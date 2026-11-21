use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use qdrant_edge::{EdgeConfig, EdgeShard, WAL_PATH};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::index::{EMBEDDING_GENERATION, EmbeddingGeneration};

const INDEX: &str = "index";
const MANIFEST: &str = "integrity.json";
const EDGE_CONFIG: &str = "edge_config.json";
pub(crate) const SOURCE_PATHS: &str = "source-paths.json";

pub(crate) enum CommitError {
    Unchanged(io::Error),
    Uncertain(io::Error),
}

impl From<io::Error> for CommitError {
    fn from(error: io::Error) -> Self {
        Self::Unchanged(error)
    }
}

impl From<CommitError> for io::Error {
    fn from(error: CommitError) -> Self {
        match error {
            CommitError::Unchanged(error) | CommitError::Uncertain(error) => error,
        }
    }
}

/// Leave the uniquely named temporary on failure for startup recovery. In particular, an
/// error syncing the parent after the move does not mean the previous file is still current.
pub(crate) fn replace(path: &Path, bytes: &[u8]) -> Result<(), CommitError> {
    let parent = containing_dir(path)?;
    let name = file_name(path)?;
    let mut temp_name = OsString::from(".");
    temp_name.push(name);
    temp_name.push(format!("-{}.tmp", Uuid::new_v4()));
    let temp = parent.join(temp_name);
    let mut file = at(&temp, "create replacement", File::create_new(&temp))?;
    at(&temp, "write replacement", file.write_all(bytes))?;
    at(&temp, "sync replacement", file.sync_all())?;
    drop(file);
    commit_move(&temp, path, true)
}

/// Seal a flushed, closed Edge shard in a private build directory and publish the directory
/// under a fresh name. The caller's exclusive root and unique build paths own this namespace.
pub(crate) fn seal_version(build_dir: &Path, published_dir: &Path) -> io::Result<()> {
    let parent = containing_dir(build_dir)?;
    if parent != containing_dir(published_dir)? || build_dir == published_dir {
        return Err(invalid(
            published_dir,
            "version paths must be distinct siblings",
        ));
    }
    check_root_entries(build_dir, false)?;
    let index = build_dir.join(INDEX);
    walk(&index, None, &mut |_, _| Ok(()))?;
    require_edge_layout(&index)?;

    // Pinned Edge's load fills unspecified config fields from the segments and rewrites
    // edge_config.json. It can also finish pending local index housekeeping. Let it do
    // that before sealing, while this directory is still private and can be discarded.
    let shard = EdgeShard::load(&index, None).map_err(|error| {
        io::Error::other(format!("normalize Edge index {}: {error}", index.display()))
    })?;
    shard.flush().map_err(|error| {
        io::Error::other(format!(
            "flush normalized Edge index {}: {error}",
            index.display()
        ))
    })?;
    drop(shard);

    let mut entries = BTreeMap::new();
    let mut buffer = [0; 64 * 1024];
    walk(&index, None, &mut |path, size| {
        let entry = if let Some(size) = size {
            Entry::File {
                size,
                sha256: hash_file(path, size, true, &mut buffer)?,
            }
        } else {
            // The walker visits directories after all children, including empty ones.
            #[cfg(target_os = "linux")]
            sync_dir(path)?;
            Entry::Directory
        };
        if path != index.as_path() {
            entries.insert(relative_name(&index, path)?, entry);
        }
        Ok(())
    })?;
    require_edge_layout(&index)?;

    let manifest = build_dir.join(MANIFEST);
    let mut file = at(
        &manifest,
        "create integrity manifest",
        File::create_new(&manifest),
    )?;
    {
        let mut writer = BufWriter::new(&mut file);
        serde_json::to_writer(&mut writer, &entries)
            .map_err(|error| context(&manifest, "write integrity manifest", error.into()))?;
        at(&manifest, "flush integrity manifest", writer.flush())?;
    }
    at(&manifest, "sync integrity manifest", file.sync_all())?;
    drop(file);
    #[cfg(target_os = "linux")]
    sync_dir(build_dir)?;

    commit_move(build_dir, published_dir, false).map_err(io::Error::from)
}

/// Check exact bytes and the required Edge layout without following links or repairing anything.
/// Edge must not be invoked on an unverified published version: it can recreate absent files.
pub(crate) fn verify_version(
    published_dir: &Path,
) -> io::Result<Option<EmbeddingGeneration<String>>> {
    let mut entries = read_manifest(published_dir)?;

    let index = published_dir.join(INDEX);
    let embedding = entries.remove(EMBEDDING_GENERATION);
    let mut buffer = [0; 64 * 1024];
    walk(&index, Some(EMBEDDING_GENERATION), &mut |path, size| {
        if path == index.as_path() {
            return Ok(());
        }
        let name = relative_name(&index, path)?;
        let expected = entries
            .remove(&name)
            .ok_or_else(|| invalid(path, "not in the integrity manifest"))?;
        match (size, expected) {
            (None, Entry::Directory) => Ok(()),
            (
                Some(size),
                Entry::File {
                    size: expected,
                    sha256,
                },
            ) => {
                if size != expected {
                    return Err(invalid(path, "file size does not match integrity manifest"));
                }
                if hash_file(path, size, false, &mut buffer)? != sha256 {
                    return Err(invalid(path, "SHA-256 does not match integrity manifest"));
                }
                Ok(())
            }
            _ => Err(invalid(path, "file type does not match integrity manifest")),
        }
    })?;
    if let Some((missing, _)) = entries.first_key_value() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "missing integrity manifest entry {missing:?} under {}",
                index.display()
            ),
        ));
    }
    require_edge_layout(&index)?;
    Ok(embedding.and_then(|entry| verify_embedding(&index.join(EMBEDDING_GENERATION), entry)))
}

/// Read only the sealed path list; browsing a library need not load its search index.
pub(crate) fn read_source_paths(published_dir: &Path) -> io::Result<Option<Vec<String>>> {
    let Some(entry) = read_manifest(published_dir)?.remove(SOURCE_PATHS) else {
        return Ok(None);
    };
    let index = published_dir.join(INDEX);
    require_dir(&index)?;
    let path = index.join(SOURCE_PATHS);
    let bytes = read_verified_file(&path, entry)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| context(&path, "parse source path list", error.into()))
}

fn read_manifest(published_dir: &Path) -> io::Result<BTreeMap<String, Entry>> {
    check_root_entries(published_dir, true)?;
    let manifest = published_dir.join(MANIFEST);
    require_file(&manifest)?;
    let file = at(&manifest, "open integrity manifest", File::open(&manifest))?;
    serde_json::from_reader(BufReader::new(file))
        .map_err(|error| context(&manifest, "parse integrity manifest", error.into()))
}

fn verify_embedding(path: &Path, entry: Entry) -> Option<EmbeddingGeneration<String>> {
    let bytes = read_verified_file(path, entry).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn read_verified_file(path: &Path, entry: Entry) -> io::Result<Vec<u8>> {
    let Entry::File { size, sha256 } = entry else {
        return Err(invalid(path, "not a file in the integrity manifest"));
    };
    let metadata = checked_metadata(path)?;
    if !metadata.is_file() || metadata.len() != size {
        return Err(invalid(
            path,
            "file type or size does not match integrity manifest",
        ));
    }
    let bytes = fs::read(path)?;
    if <[u8; 32]>::from(Sha256::digest(&bytes)) != sha256 {
        return Err(invalid(path, "SHA-256 does not match integrity manifest"));
    }
    Ok(bytes)
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Entry {
    Directory,
    File { size: u64, sha256: [u8; 32] },
}

fn check_root_entries(path: &Path, has_manifest: bool) -> io::Result<()> {
    require_dir(path)?;
    for entry in at(path, "list version directory", fs::read_dir(path))? {
        let entry = at(path, "read version directory entry", entry)?;
        let name = entry.file_name();
        if name != INDEX && !(has_manifest && name == MANIFEST) {
            return Err(invalid(&entry.path(), "unexpected version directory entry"));
        }
    }
    Ok(())
}

fn walk(
    path: &Path,
    skip_file: Option<&str>,
    visit: &mut impl FnMut(&Path, Option<u64>) -> io::Result<()>,
) -> io::Result<()> {
    require_dir(path)?;
    walk_dir(path, skip_file, visit)
}

fn walk_dir(
    path: &Path,
    skip_file: Option<&str>,
    visit: &mut impl FnMut(&Path, Option<u64>) -> io::Result<()>,
) -> io::Result<()> {
    for entry in at(path, "list index directory", fs::read_dir(path))? {
        let entry = at(path, "read index directory entry", entry)?;
        if skip_file.is_some_and(|name| entry.file_name() == name) {
            continue;
        }
        let child = entry.path();
        let metadata = checked_metadata(&child)?;
        if metadata.is_dir() {
            if atomic_write_temp(&entry.file_name()) {
                check_atomic_write_temp(&child)?;
            } else {
                walk_dir(&child, None, visit)?;
            }
        } else if metadata.is_file() {
            visit(&child, Some(metadata.len()))?;
        } else {
            return Err(invalid(&child, "not a regular file or directory"));
        }
    }
    visit(path, None)
}

// Edge's pinned atomicwrites dependency can leave an empty temp dir or a tmpfile.tmp if
// interrupted during load or drop. The scratch file is never live Edge data, nor is it cleaned
// up during verification. Recognize only that reserved layout, and still reject links in it.
fn atomic_write_temp(name: &OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix(".atomicwrite"))
        .is_some_and(|suffix| {
            suffix.len() == 6 && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
        })
}

fn check_atomic_write_temp(path: &Path) -> io::Result<()> {
    for entry in at(
        path,
        "list Edge atomic write temp directory",
        fs::read_dir(path),
    )? {
        let entry = at(path, "read Edge atomic write temp entry", entry)?;
        let child = entry.path();
        if entry.file_name() != "tmpfile.tmp" {
            return Err(invalid(&child, "unexpected Edge atomic write temp entry"));
        }
        require_file(&child)?;
    }
    Ok(())
}

fn require_edge_layout(index: &Path) -> io::Result<()> {
    let config_path = index.join(EDGE_CONFIG);
    require_file(&config_path)?;
    let config = EdgeConfig::load(index)
        .ok_or_else(|| invalid(&config_path, "missing Edge config"))?
        .map_err(|error| invalid(&config_path, &format!("cannot parse Edge config: {error}")))?;
    let wal = index.join(WAL_PATH);
    if config.wal_enabled() || at(&wal, "check for Edge WAL", wal.try_exists())? {
        return Err(invalid(&wal, "published Edge versions must not use WAL"));
    }
    let segments = index.join("segments");
    require_dir(&segments)?;
    let mut has_segment = false;
    for entry in at(&segments, "list Edge segments", fs::read_dir(&segments))? {
        let entry = at(&segments, "read Edge segments entry", entry)?;
        let segment = entry.path();
        let name = entry.file_name();
        if !checked_metadata(&segment)?.is_dir() || atomic_write_temp(&name) {
            continue;
        }
        if name
            .to_str()
            .and_then(|name| Uuid::try_parse(name).ok())
            .is_none()
        {
            return Err(invalid(&segment, "invalid Edge segment directory name"));
        }
        require_file(&segment.join("version.info"))?;
        require_file(&segment.join("segment.json"))?;
        has_segment = true;
    }
    if !has_segment {
        return Err(invalid(&segments, "no persisted Edge segment"));
    }
    Ok(())
}

fn relative_name(index: &Path, path: &Path) -> io::Result<String> {
    let relative = path
        .strip_prefix(index)
        .map_err(|_| invalid(path, "not inside Edge index"))?;
    let mut name = String::new();
    for part in relative.iter() {
        let part = part
            .to_str()
            .ok_or_else(|| invalid(path, "non-UTF-8 Edge path"))?;
        if !name.is_empty() {
            name.push('/');
        }
        name.push_str(part);
    }
    Ok(name)
}

fn hash_file(path: &Path, size: u64, sync: bool, buffer: &mut [u8]) -> io::Result<[u8; 32]> {
    // Windows file flush requires a handle with write access. Verification stays read-only.
    let mut file = at(
        path,
        "open index file",
        File::options()
            .read(true)
            .write(sync && cfg!(windows))
            .open(path),
    )?;
    let mut hash = Sha256::new();
    let mut read = 0_u64;
    loop {
        let len = match file.read(buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            read => at(path, "read index file", read)?,
        };
        if len == 0 {
            break;
        }
        hash.update(&buffer[..len]);
        read += len as u64;
    }
    if read != size {
        return Err(invalid(path, "index file changed size while hashing"));
    }
    if sync {
        at(path, "sync index file", file.sync_all())?;
    }
    Ok(hash.finalize().into())
}

fn checked_metadata(path: &Path) -> io::Result<Metadata> {
    let metadata = at(path, "read metadata", fs::symlink_metadata(path))?;
    if metadata.is_symlink() || is_reparse_point(&metadata) {
        return Err(invalid(path, "symlink or reparse point in version"));
    }
    Ok(metadata)
}

fn require_file(path: &Path) -> io::Result<()> {
    if checked_metadata(path)?.is_file() {
        Ok(())
    } else {
        Err(invalid(path, "not a regular file"))
    }
}

fn require_dir(path: &Path) -> io::Result<()> {
    if checked_metadata(path)?.is_dir() {
        Ok(())
    } else {
        Err(invalid(path, "not a regular directory"))
    }
}

fn containing_dir(path: &Path) -> io::Result<&Path> {
    path.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| invalid(path, "missing containing directory"))
}

fn file_name(path: &Path) -> io::Result<&OsStr> {
    path.file_name()
        .ok_or_else(|| invalid(path, "missing file name"))
}

fn at<T>(path: &Path, stage: &str, result: io::Result<T>) -> io::Result<T> {
    result.map_err(|error| context(path, stage, error))
}

fn invalid(path: &Path, description: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{description}: {}", path.display()),
    )
}

fn context(path: &Path, stage: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{stage} {}: {error}", path.display()))
}

#[cfg(target_os = "linux")]
fn is_reparse_point(_: &Metadata) -> bool {
    false
}

#[cfg(windows)]
fn is_reparse_point(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(target_os = "linux")]
fn commit_move(source: &Path, dest: &Path, overwrite: bool) -> Result<(), CommitError> {
    if !overwrite {
        // Only this catalog owns the root, and concurrent builders have disjoint generation
        // names. The check prevents a rename from replacing even an empty existing directory.
        match fs::symlink_metadata(dest) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("published version already exists: {}", dest.display()),
                )
                .into());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(context(dest, "check destination for version publish", error).into());
            }
        }
    }
    fs::rename(source, dest).map_err(|error| {
        CommitError::Uncertain(context(
            dest,
            &format!("rename {} to", source.display()),
            error,
        ))
    })?;
    let parent = dest.parent().expect("validated move directory");
    sync_dir(parent).map_err(CommitError::Uncertain)
}

#[cfg(target_os = "linux")]
pub(crate) fn sync_dir(path: &Path) -> io::Result<()> {
    let dir = at(path, "open directory for sync", File::open(path))?;
    at(path, "sync directory", dir.sync_all())
}

#[cfg(windows)]
fn commit_move(source: &Path, dest: &Path, overwrite: bool) -> Result<(), CommitError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let parent = containing_dir(dest)?;
    let canonical = at(parent, "canonicalize move directory", parent.canonicalize())?;
    // Both paths are in the same directory. Keep Rust's canonical verbatim namespace and
    // never canonicalize the destination entry itself (it may not exist yet).
    let wide = |path: &Path| -> io::Result<Vec<u16>> {
        let mut name: Vec<u16> = canonical
            .join(file_name(path)?)
            .as_os_str()
            .encode_wide()
            .collect();
        if name.contains(&0) {
            return Err(invalid(path, "path contains NUL"));
        }
        name.push(0);
        Ok(name)
    };
    let from = wide(source)?;
    let to = wide(dest)?;
    let flags = MOVEFILE_WRITE_THROUGH
        | if overwrite {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    // SAFETY: both buffers remain live and are NUL-terminated. No COPY_ALLOWED: a cross-volume
    // move must fail, never silently copy or publish a partially copied destination.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), flags) } == 0 {
        return Err(CommitError::Uncertain(context(
            dest,
            &format!("MoveFileExW {} to", source.display()),
            io::Error::last_os_error(),
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use qdrant_edge::{
        Distance, EdgeConfigBuilder, EdgeSparseVectorParamsBuilder, EdgeVectorParamsBuilder,
        Memory, Modifier, PointInsertOperations, PointOperations, PointStruct, UpdateOperation,
        Vector, Vectors,
    };
    use serde_json::json;

    use super::*;
    use crate::shard::Shard;

    #[test]
    fn sealed_edge_reopens_and_detects_same_length_damage() -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let generation = Uuid::new_v4();
        let build = root.path().join(format!(".build-{generation}"));
        let published = root.path().join(generation.to_string());
        let config = EdgeConfigBuilder::new()
            .vector(
                "semantic",
                EdgeVectorParamsBuilder::new(2, Distance::Cosine)
                    .memory(Memory::Pinned)
                    .build(),
            )
            .sparse_vector(
                "text",
                EdgeSparseVectorParamsBuilder::new()
                    .memory(Memory::Pinned)
                    .modifier(Modifier::Idf)
                    .build(),
            )
            .max_search_threads(1)
            .wal_enabled(false)
            .build();
        let shard = EdgeShard::new(&build.join(INDEX), config)?;
        let point = PointStruct::new(
            1_u64,
            Vectors::new_named([
                ("semantic", Vector::new_dense(vec![1.0, 0.0])),
                ("text", Vector::new_sparse(vec![1], vec![1.5])?),
            ]),
            json!({ "path": "a.md" }),
        );
        shard.update(UpdateOperation::PointOperation(
            PointOperations::UpsertPoints(PointInsertOperations::PointsList(vec![point.into()])),
        ))?;
        shard.flush()?;
        drop(shard);
        seal_version(&build, &published)?;

        // A killed Edge opener may leave an atomicwrites temp dir, but its contents
        // were never published and reopening must not remove it or use it as data.
        let scratch = published.join(INDEX).join(".atomicwriteAb01Cd");
        fs::create_dir(&scratch)?;
        fs::write(scratch.join("tmpfile.tmp"), b"interrupted write")?;
        let shard = Shard::published(published.clone());
        shard
            .open()
            .map_err(|error| io::Error::new(error.kind(), error.to_string()))?;
        drop(shard);
        verify_version(&published)?;
        assert_eq!(
            fs::read(scratch.join("tmpfile.tmp"))?.as_slice(),
            b"interrupted write"
        );

        let marker = fs::read_dir(published.join(INDEX).join("segments"))?
            .next()
            .expect("one persisted segment")?
            .path()
            .join("version.info");
        let mut bytes = fs::read(&marker)?;
        bytes[0] ^= 1;
        fs::write(&marker, &bytes)?;
        let broken = Shard::published(published);
        let Err(error) = broken.open() else {
            panic!("same-length damage passed the integrity check");
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("SHA-256"));
        assert!(error.to_string().contains("version.info"));
        assert_eq!(fs::read(&marker)?, bytes);
        Ok(())
    }
}
