use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use qdrant_edge::EdgeShard;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{data_root::Root, durable, model_http::Embedding, shard::Shard};

const CATALOG: &str = "catalog.json";
const FORMAT: u32 = 1;
const BUILD_PREFIX: &str = ".build-";
const MAX_NAME_BYTES: usize = 1024;
pub(crate) const INVALID_NAME: &str =
    "library name must be non-blank, without control characters, and at most 1024 UTF-8 bytes";

#[derive(Debug)]
pub(crate) enum CatalogError {
    InvalidName,
    AlreadyExists,
    NotFound,
    StaleBuild,
    OperationCommitted,
    Storage(io::Error),
    CommitUncertain(io::Error),
    Unavailable,
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => f.write_str(INVALID_NAME),
            Self::AlreadyExists => f.write_str("library already exists"),
            Self::NotFound => f.write_str("library does not exist"),
            Self::StaleBuild => f.write_str("library changed while building the import"),
            Self::OperationCommitted => f.write_str("operation already committed"),
            Self::Storage(err) => write!(f, "catalog storage error: {err}"),
            Self::CommitUncertain(err) => write!(
                f,
                "catalog commit outcome is uncertain; restart required: {err}"
            ),
            Self::Unavailable => f.write_str("catalog is unavailable until restart"),
        }
    }
}

impl Error for CatalogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Storage(err) | Self::CommitUncertain(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for CatalogError {
    fn from(error: io::Error) -> Self {
        Self::Storage(error)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum Receipt {
    Created { library_id: Uuid },
    Deleted { library_id: Uuid },
    Published { library_id: Uuid, version_id: Uuid },
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Library {
    pub(crate) name: String,
    pub(crate) id: Uuid,
    pub(crate) version_id: Option<Uuid>,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct LibraryRecord {
    id: Uuid,
    version_id: Option<Uuid>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Table {
    format: u32,
    libraries: BTreeMap<String, LibraryRecord>,
    receipts: BTreeMap<Uuid, Receipt>,
}

struct State {
    table: Table,
    versions: BTreeMap<Uuid, Arc<Version>>,
    uncertain: bool,
}

pub(crate) struct Catalog {
    root: Arc<Root>,
    table_path: PathBuf,
    versions_root: PathBuf,
    stage_root: PathBuf,
    state: Mutex<State>,
}

pub(crate) struct Build {
    root: Arc<Root>,
    name: String,
    base: LibraryRecord,
    version_id: Uuid,
    path: PathBuf,
    cleanup: bool,
}

impl Build {
    pub(crate) fn index_path(&self) -> PathBuf {
        self.path.join("index")
    }

    pub(crate) fn version_id(&self) -> Uuid {
        self.version_id
    }
}

impl Drop for Build {
    fn drop(&mut self) {
        if self.cleanup {
            // Failed removal (for example, a still-open builder handle on
            // Windows) is reclaimed on the next startup.
            remove_owned_directory(&self.path);
        }
    }
}

pub(crate) struct Version {
    shard: Option<Shard>,
    id: Uuid,
    retired: AtomicBool,
    // A retained snapshot must also keep the exclusive root lock alive.
    _root: Arc<Root>,
}

impl Version {
    fn new(root: Arc<Root>, id: Uuid, path: PathBuf) -> Self {
        Self {
            shard: Some(Shard::published(path)),
            id,
            retired: AtomicBool::new(false),
            _root: root,
        }
    }

    #[cfg(test)]
    pub(crate) fn index_path(&self) -> PathBuf {
        self.shard().path().join("index")
    }

    pub(crate) fn open(&self) -> Result<&EdgeShard, &io::Error> {
        self.shard().open()
    }

    pub(crate) fn for_embedding(
        &self,
        provider: &Embedding,
    ) -> Result<Option<&EdgeShard>, &io::Error> {
        self.shard().for_embedding(self.id, provider)
    }

    fn shard(&self) -> &Shard {
        self.shard.as_ref().expect("live version has a shard")
    }

    fn retire(&self) {
        self.retired.store(true, Ordering::Release);
    }
}

impl Drop for Version {
    fn drop(&mut self) {
        if self.retired.load(Ordering::Acquire)
            && let Some(shard) = self.shard.take()
        {
            // Releases Edge handles before Windows can delete the directory.
            remove_owned_directory(&shard.into_path());
        }
    }
}

pub(crate) type Snapshot = Vec<(String, Arc<Version>)>;

impl Catalog {
    pub(crate) fn open(root: Root) -> Result<Self, CatalogError> {
        let root = Arc::new(root);
        let table_path = root.path().join(CATALOG);
        let versions_root = root.path().join("versions");
        let stage_root = root.path().join("imports");
        let has_versions = check_managed_directory(&root, &versions_root)?;
        let has_stage = check_managed_directory(&root, &stage_root)?;
        let audit = root.path().join("audit");
        let has_audit = check_managed_directory(&root, &audit)?;
        let (table, referenced) = match load_table(&table_path)? {
            Some(loaded) => loaded,
            None => {
                for (path, exists) in [
                    (&versions_root, has_versions),
                    (&stage_root, has_stage),
                    (&audit, has_audit),
                ] {
                    if exists && has_entries(path)? {
                        return Err(invalid_data(
                            &table_path,
                            format_args!(
                                "catalog is missing but {} contains service data; no files were reclaimed",
                                path.display()
                            ),
                        )
                        .into());
                    }
                }
                let table = Table {
                    format: FORMAT,
                    libraries: BTreeMap::new(),
                    receipts: BTreeMap::new(),
                };
                write_table(&table_path, &table).map_err(io::Error::from)?;
                (table, BTreeSet::new())
            }
        };
        for (path, exists) in [(&versions_root, has_versions), (&stage_root, has_stage)] {
            if !exists {
                fs::create_dir(path).map_err(|err| path_error(path, err))?;
            }
        }
        #[cfg(target_os = "linux")]
        if !has_versions || !has_stage {
            durable::sync_dir(root.path())?;
        }

        clean_entries(&versions_root, true, |name| {
            if let Some(build) = name.strip_prefix(BUILD_PREFIX) {
                return canonical_uuid(build).is_some();
            }
            canonical_uuid(name).is_some_and(|id| !referenced.contains(&id))
        })?;
        clean_entries(&stage_root, true, |name| {
            // `import::stage` uses tempfile::Builder with prefix `import-`
            // and the default six ASCII alphanumeric random characters.
            name.strip_prefix("import-").is_some_and(|suffix| {
                suffix.len() == 6 && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })
        })?;
        clean_entries(root.path(), false, |name| {
            name.strip_prefix(".catalog.json-")
                .and_then(|suffix| suffix.strip_suffix(".tmp"))
                .and_then(canonical_uuid)
                .is_some()
        })?;
        let versions = referenced
            .into_iter()
            .map(|id| {
                let version = Version::new(Arc::clone(&root), id, id_path(&versions_root, id));
                (id, Arc::new(version))
            })
            .collect();
        Ok(Self {
            root,
            table_path,
            versions_root,
            stage_root,
            state: Mutex::new(State {
                table,
                versions,
                uncertain: false,
            }),
        })
    }

    pub(crate) fn root_path(&self) -> &Path {
        self.root.path()
    }

    pub(crate) fn stage_root(&self) -> &Path {
        &self.stage_root
    }

    pub(crate) fn list(&self) -> Result<Vec<Library>, CatalogError> {
        let state = self.lock()?;
        Ok(state
            .table
            .libraries
            .iter()
            .map(|(name, entry)| Library {
                name: name.clone(),
                id: entry.id,
                version_id: entry.version_id,
            })
            .collect())
    }

    pub(crate) fn create(
        &self,
        name: &str,
        operation: Option<Uuid>,
    ) -> Result<Receipt, CatalogError> {
        let mut state = self.lock()?;
        state.check_operation(operation)?;
        if name.len() > MAX_NAME_BYTES || !valid_name(name) {
            return Err(CatalogError::InvalidName);
        }
        if state.table.libraries.contains_key(name) {
            return Err(CatalogError::AlreadyExists);
        }
        let id = Uuid::new_v4();
        state.table.libraries.insert(
            name.to_owned(),
            LibraryRecord {
                id,
                version_id: None,
            },
        );
        let receipt = Receipt::Created { library_id: id };
        self.commit_operation(&mut state, operation, receipt, |table| {
            table.libraries.remove(name);
        })
    }

    pub(crate) fn delete(
        &self,
        name: &str,
        operation: Option<Uuid>,
    ) -> Result<Receipt, CatalogError> {
        let mut state = self.lock()?;
        state.check_operation(operation)?;
        let (name, removed) = state
            .table
            .libraries
            .remove_entry(name)
            .ok_or(CatalogError::NotFound)?;
        let receipt = Receipt::Deleted {
            library_id: removed.id,
        };
        self.commit_operation(&mut state, operation, receipt, |table| {
            table.libraries.insert(name, removed);
        })?;
        retire_version(state, removed.version_id);
        Ok(receipt)
    }

    pub(crate) fn start_build(&self, name: &str) -> Result<Build, CatalogError> {
        let state = self.lock()?;
        let base = *state
            .table
            .libraries
            .get(name)
            .ok_or(CatalogError::NotFound)?;
        let version_id = Uuid::new_v4();
        let path = self
            .versions_root
            .join(format!("{BUILD_PREFIX}{version_id}"));
        fs::create_dir(&path).map_err(|err| path_error(&path, err))?;
        Ok(Build {
            root: Arc::clone(&self.root),
            name: name.to_owned(),
            base,
            version_id,
            path,
            cleanup: true,
        })
    }

    /// The caller must flush and drop all build-time Edge handles first.
    pub(crate) fn publish(
        &self,
        mut build: Build,
        operation: Option<Uuid>,
    ) -> Result<Receipt, CatalogError> {
        if !Arc::ptr_eq(&self.root, &build.root) {
            return Err(CatalogError::StaleBuild);
        }
        self.lock()?.check_build(&build, operation)?;

        // The fresh generation is invisible to queries until the table commit.
        // Open, flush and hash it without holding the catalog lock.
        let path = id_path(&self.versions_root, build.version_id);
        durable::seal_version(&build.path, &path)?;
        build.path = path;

        let mut state = self.lock()?;
        state.check_build(&build, operation)?;

        state
            .table
            .libraries
            .get_mut(&build.name)
            .expect("checked build lease")
            .version_id = Some(build.version_id);
        let receipt = Receipt::Published {
            library_id: build.base.id,
            version_id: build.version_id,
        };
        let result = self.commit_operation(&mut state, operation, receipt, |table| {
            table
                .libraries
                .get_mut(&build.name)
                .expect("checked build lease")
                .version_id = build.base.version_id;
        });
        if matches!(result, Err(CatalogError::CommitUncertain(_))) {
            // The disk table may already reference this version. Only startup
            // reconciliation can safely reclaim it.
            build.cleanup = false;
        }
        result?;
        build.cleanup = false;
        state.versions.insert(
            build.version_id,
            Arc::new(Version::new(
                Arc::clone(&self.root),
                build.version_id,
                std::mem::take(&mut build.path),
            )),
        );
        retire_version(state, build.base.version_id);
        Ok(receipt)
    }

    /// Pins all selected generations under one lock without opening storage.
    pub(crate) fn snapshot(&self, library: Option<&str>) -> Result<Snapshot, CatalogError> {
        let state = self.lock()?;
        let pin = |(name, entry): (&String, &LibraryRecord)| {
            let id = entry.version_id?;
            Some((name.clone(), Arc::clone(&state.versions[&id])))
        };
        if let Some(name) = library {
            let entry = state
                .table
                .libraries
                .get_key_value(name)
                .ok_or(CatalogError::NotFound)?;
            return Ok(pin(entry).into_iter().collect());
        }
        Ok(state.table.libraries.iter().filter_map(pin).collect())
    }

    pub(crate) fn receipt(&self, operation: Uuid) -> Result<Option<Receipt>, CatalogError> {
        Ok(self.lock()?.table.receipts.get(&operation).copied())
    }

    /// Only call once the corresponding audit result has been synced to disk.
    pub(crate) fn clear_receipt(&self, operation: Uuid) -> Result<(), CatalogError> {
        let mut state = self.lock()?;
        if let Some(receipt) = state.table.receipts.remove(&operation) {
            self.commit(&mut state, |table| {
                table.receipts.insert(operation, receipt);
            })?;
        }
        Ok(())
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, CatalogError> {
        let state = self.state.lock().map_err(|_| CatalogError::Unavailable)?;
        if state.uncertain {
            return Err(CatalogError::Unavailable);
        }
        Ok(state)
    }

    fn commit_operation(
        &self,
        state: &mut State,
        operation: Option<Uuid>,
        receipt: Receipt,
        rollback: impl FnOnce(&mut Table),
    ) -> Result<Receipt, CatalogError> {
        if let Some(operation) = operation {
            state.table.receipts.insert(operation, receipt);
        }
        self.commit(state, |table| {
            if let Some(operation) = operation {
                table.receipts.remove(&operation);
            }
            rollback(table);
        })?;
        Ok(receipt)
    }

    fn commit(
        &self,
        state: &mut State,
        rollback: impl FnOnce(&mut Table),
    ) -> Result<(), CatalogError> {
        match write_table(&self.table_path, &state.table) {
            Ok(()) => Ok(()),
            Err(durable::CommitError::Unchanged(error)) => {
                rollback(&mut state.table);
                Err(path_error(&self.table_path, error).into())
            }
            Err(durable::CommitError::Uncertain(error)) => {
                state.uncertain = true;
                Err(CatalogError::CommitUncertain(path_error(
                    &self.table_path,
                    error,
                )))
            }
        }
    }
}

impl State {
    fn check_operation(&self, operation: Option<Uuid>) -> Result<(), CatalogError> {
        if operation.is_some_and(|id| self.table.receipts.contains_key(&id)) {
            Err(CatalogError::OperationCommitted)
        } else {
            Ok(())
        }
    }

    fn check_build(&self, build: &Build, operation: Option<Uuid>) -> Result<(), CatalogError> {
        self.check_operation(operation)?;
        if self.table.libraries.get(&build.name) != Some(&build.base) {
            return Err(CatalogError::StaleBuild);
        }
        Ok(())
    }
}

fn retire_version(mut state: MutexGuard<'_, State>, id: Option<Uuid>) {
    let old = id.and_then(|id| state.versions.remove(&id));
    drop(state);
    if let Some(old) = old {
        old.retire();
    }
}

fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && !name.chars().any(char::is_control)
}

fn write_table(path: &Path, table: &Table) -> Result<(), durable::CommitError> {
    let bytes = serde_json::to_vec(table).map_err(io::Error::other)?;
    durable::replace(path, &bytes)
}

fn load_table(path: &Path) -> io::Result<Option<(Table, BTreeSet<Uuid>)>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(path_error(path, error)),
    };
    if is_link(&metadata) || !metadata.is_file() {
        return Err(invalid_data(path, "expected a regular catalog file"));
    }
    let bytes = fs::read(path).map_err(|err| path_error(path, err))?;
    let table: Table = serde_json::from_slice(&bytes)
        .map_err(|err| invalid_data(path, format_args!("invalid catalog: {err}")))?;
    if table.format != FORMAT {
        return Err(invalid_data(path, "unsupported catalog format"));
    }
    let mut libraries = BTreeSet::new();
    let mut referenced = BTreeSet::new();
    for (name, library) in &table.libraries {
        if !valid_name(name) || !libraries.insert(library.id) {
            return Err(invalid_data(path, "invalid or duplicated library record"));
        }
        if let Some(id) = library.version_id
            && !referenced.insert(id)
        {
            return Err(invalid_data(path, "duplicate published version reference"));
        }
    }
    Ok(Some((table, referenced)))
}

fn check_managed_directory(root: &Root, path: &Path) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(path_error(path, error)),
    };
    if is_link(&metadata) || !metadata.is_dir() {
        return Err(invalid_data(path, "expected a service-managed directory"));
    }
    root.check_managed_storage(path)?;
    Ok(true)
}

fn has_entries(path: &Path) -> io::Result<bool> {
    fs::read_dir(path)
        .and_then(|mut entries| entries.next().transpose())
        .map(|entry| entry.is_some())
        .map_err(|err| path_error(path, err))
}

fn clean_entries(root: &Path, directories: bool, owns: impl Fn(&str) -> bool) -> io::Result<()> {
    for entry in fs::read_dir(root).map_err(|err| path_error(root, err))? {
        let entry = entry.map_err(|err| path_error(root, err))?;
        if !entry.file_name().to_str().is_some_and(&owns) {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|err| path_error(&path, err))?;
        if is_link(&metadata) {
            continue;
        }
        let result = if directories && metadata.is_dir() {
            fs::remove_dir_all(&path)
        } else if !directories && metadata.is_file() {
            fs::remove_file(&path)
        } else {
            continue;
        };
        result.map_err(|err| path_error(&path, err))?;
    }
    Ok(())
}

fn remove_owned_directory(path: &Path) {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir() && !is_link(&metadata)) {
        let _ = fs::remove_dir_all(path);
    }
}

fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn canonical_uuid(name: &str) -> Option<Uuid> {
    let id = Uuid::parse_str(name).ok()?;
    (id.hyphenated().encode_lower(&mut Uuid::encode_buffer()) == name).then_some(id)
}

fn id_path(root: &Path, id: Uuid) -> PathBuf {
    root.join(id.hyphenated().encode_lower(&mut Uuid::encode_buffer()))
}

fn invalid_data(path: &Path, message: impl fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: {message}", path.display()),
    )
}

fn path_error(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests;
