//! Validates and stages a complete Markdown batch before handing it to the builder.
//!
//! Multipart `path` fields are relative metadata, never filesystem locations.
//! Local path imports enter through a separate, authorized caller. Both modes
//! snapshot raw bytes; the batch owns cleanup of the temporary copies.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::str;

use axum::extract::Multipart;
use axum::extract::multipart::{Field, MultipartError};
use axum::http::StatusCode;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

pub(crate) const MAX_IMPORT_BYTES: usize = 1024 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 16 * 1024;
const PATH_TOO_LONG: &str = "path must not exceed 16 KiB (16384 UTF-8 bytes)";

#[derive(Debug)]
pub(crate) enum ImportError {
    InvalidInput {
        document: Option<usize>,
        reason: &'static str,
    },
    PayloadTooLarge,
    Storage {
        operation: &'static str,
        source: io::Error,
    },
}

impl ImportError {
    fn invalid(reason: &'static str) -> Self {
        Self::InvalidInput {
            document: None,
            reason,
        }
    }

    fn at_document(self, number: usize) -> Self {
        match self {
            Self::InvalidInput { reason, .. } => Self::InvalidInput {
                document: Some(number),
                reason,
            },
            other => other,
        }
    }

    fn multipart(error: MultipartError) -> Self {
        if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
            Self::PayloadTooLarge
        } else {
            Self::invalid("malformed or truncated multipart upload")
        }
    }
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput { document, reason } => {
                if let Some(number) = document {
                    write!(f, "document {number}: ")?;
                }
                f.write_str(reason)
            }
            Self::PayloadTooLarge => f.write_str("import exceeds the 1 GiB size limit"),
            Self::Storage { operation, .. } => write!(f, "cannot {operation}"),
        }
    }
}

impl std::error::Error for ImportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Storage { source, .. } => Some(source),
            Self::InvalidInput { .. } | Self::PayloadTooLarge => None,
        }
    }
}

pub(crate) struct Batch {
    documents: Vec<Document>,
    _directory: TempDir,
}

impl Batch {
    pub(crate) fn documents(&self) -> &[Document] {
        &self.documents
    }
}

pub(crate) struct Document {
    pub(crate) relative_path: String,
    pub(crate) sha256: String,
    staged_path: PathBuf,
}

impl Document {
    /// For the blocking builder: read one staged document at a time.
    pub(crate) fn read_body(&self) -> Result<String, ImportError> {
        std::fs::read_to_string(&self.staged_path).map_err(|source| ImportError::Storage {
            operation: "read a staged Markdown document",
            source,
        })
    }
}

/// Checks relative path metadata shared by uploads and query scopes. A query
/// scope may refer to a directory, so Markdown extensions are checked separately.
pub(crate) fn validate_path(path: &str) -> Result<(), ImportError> {
    if path.len() > MAX_PATH_BYTES {
        return Err(ImportError::invalid(PATH_TOO_LONG));
    }
    if path.trim().is_empty() {
        return Err(ImportError::invalid("path must not be blank"));
    }
    if path.starts_with('/')
        || matches!(path.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic())
    {
        return Err(ImportError::invalid(
            "path must be relative and have no drive prefix",
        ));
    }
    if path.contains('\\') || path.chars().any(char::is_control) {
        return Err(ImportError::invalid(
            "path must not contain backslashes or control characters",
        ));
    }
    if path
        .split('/')
        .any(|segment| matches!(segment, "" | "." | ".."))
    {
        return Err(ImportError::invalid(
            "path must not contain empty, '.' or '..' segments",
        ));
    }
    Ok(())
}

/// Stage one complete upload under an existing, server-approved root. The
/// caller controls the request body limit through the `Multipart` extractor.
pub(crate) async fn stage(
    mut multipart: Multipart,
    stage_root: &Path,
) -> Result<Batch, ImportError> {
    let directory = staging_directory(stage_root)?;
    let mut documents = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(ImportError::multipart)?
    {
        let number = documents.len() + 1;
        if field.name() != Some("path") {
            return Err(ImportError::invalid("expected a 'path' field").at_document(number));
        }
        let relative_path = read_path(field)
            .await
            .map_err(|error| error.at_document(number))?;
        validate_path(&relative_path).map_err(|error| error.at_document(number))?;
        if !is_markdown(
            relative_path
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .as_bytes(),
        ) {
            return Err(ImportError::invalid("path must name a .md file").at_document(number));
        }

        let field = multipart
            .next_field()
            .await
            .map_err(|error| ImportError::multipart(error).at_document(number))?
            .filter(|field| field.name() == Some("file"))
            .ok_or_else(|| {
                ImportError::invalid("expected a 'file' field after 'path'").at_document(number)
            })?;
        let staged_path = directory.path().join(number.to_string());
        let mut file =
            File::create_new(&staged_path)
                .await
                .map_err(|source| ImportError::Storage {
                    operation: "create a staged Markdown document",
                    source,
                })?;
        let result = write_document(field, &mut file).await;
        // Finish pending I/O even if a later chunk was invalid so the staged
        // file can be closed and the directory removed on error.
        let flushed = file.flush().await;
        let sha256 = result.map_err(|error| error.at_document(number))?;
        flushed.map_err(|source| ImportError::Storage {
            operation: "finish writing a staged Markdown document",
            source,
        })?;
        documents.push(Document {
            relative_path,
            sha256,
            staged_path,
        });
    }
    if documents.is_empty() {
        return Err(ImportError::invalid(
            "upload must contain at least one 'path'/'file' pair",
        ));
    }
    // Use borrowed keys; the builder receives documents in upload order and no
    // copy of the path is needed to validate exact duplicate metadata.
    let mut paths = HashSet::with_capacity(documents.len());
    for (index, document) in documents.iter().enumerate() {
        if !paths.insert(document.relative_path.as_str()) {
            return Err(ImportError::invalid("duplicate path in upload").at_document(index + 1));
        }
    }
    Ok(Batch {
        documents,
        _directory: directory,
    })
}

/// Stage one file or recurse through a directory on the machine running the
/// service. Only the staged snapshot and slash-separated relative names reach
/// the builder; the chosen directory's name is retained, like a browser upload.
pub(crate) fn stage_local(path: &str, stage_root: &Path) -> Result<Batch, ImportError> {
    let source = Path::new(path);
    if !source.is_absolute()
        || path.len() > MAX_PATH_BYTES
        || path.chars().any(char::is_control)
        || source.components().any(|part| part == Component::ParentDir)
    {
        return Err(ImportError::invalid(
            "enter an absolute path without '..' or control characters (at most 16 KiB)",
        ));
    }
    #[cfg(windows)]
    if !matches!(
        source.components().next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), std::path::Prefix::Disk(_))
    ) || path.as_bytes()[2..].contains(&b':')
    {
        return Err(ImportError::invalid("enter an absolute Windows drive path"));
    }
    // Strip trailing separators before checking the final entry, so a trailing
    // slash does not turn a symbolic link into a directory lookup on Linux.
    let source: PathBuf = source.components().collect();
    let metadata = fs::symlink_metadata(&source).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => ImportError::invalid("服务器上不存在该路径"),
        _ => ImportError::invalid("服务器无法读取该路径"),
    })?;
    if is_link(&metadata) || !(metadata.is_dir() || metadata.is_file()) {
        return Err(ImportError::invalid(
            "the local path must be a regular file or directory, not a link",
        ));
    }
    let relative_to = source.parent().unwrap_or(&source);
    let directory = staging_directory(stage_root)?;
    let mut documents = Vec::new();
    let mut remaining = MAX_IMPORT_BYTES;
    let mut buffer = [0; 64 * 1024];
    let mut stage = |file: &Path| {
        let number = documents.len() + 1;
        let relative_path = file
            .strip_prefix(relative_to)
            .expect("source files are below their import base")
            .to_str()
            .ok_or_else(|| ImportError::invalid("relative path must be UTF-8"))
            .map_err(|error| error.at_document(number))?;
        #[cfg(windows)]
        let relative_path = relative_path.replace('\\', "/");
        #[cfg(not(windows))]
        let relative_path = relative_path.to_owned();
        validate_path(&relative_path).map_err(|error| error.at_document(number))?;
        let staged_path = directory.path().join(number.to_string());
        let sha256 = copy_local_document(
            file,
            &staged_path,
            &mut buffer,
            &mut remaining,
            relative_path.len(),
        )
        .map_err(|error| error.at_document(number))?;
        documents.push(Document {
            relative_path,
            sha256,
            staged_path,
        });
        Ok::<_, ImportError>(())
    };
    if metadata.is_file() {
        if !source
            .file_name()
            .is_some_and(|name| is_markdown(name.as_encoded_bytes()))
        {
            return Err(ImportError::invalid("path must name a .md file"));
        }
        stage(&source)?;
    } else {
        let read_dir = |path: &Path| {
            fs::read_dir(path).map_err(|_| ImportError::invalid("cannot read a local directory"))
        };
        let mut stack = vec![read_dir(&source)?];
        while let Some(directory) = stack.last_mut() {
            let Some(entry) = directory.next() else {
                stack.pop();
                continue;
            };
            let unreadable =
                |_| ImportError::invalid("cannot read an entry in the local directory");
            let entry = entry.map_err(unreadable)?;
            let kind = entry.file_type().map_err(unreadable)?;
            if !kind.is_dir()
                && !(kind.is_file() && is_markdown(entry.file_name().as_encoded_bytes()))
            {
                continue;
            }
            // Windows junctions and other reparse points can look like normal
            // directories or files in FileType.
            #[cfg(windows)]
            if is_link(&entry.metadata().map_err(unreadable)?) {
                continue;
            }
            if kind.is_dir() {
                stack.push(read_dir(&entry.path())?);
            } else {
                stage(&entry.path())?;
            }
        }
        if documents.is_empty() {
            return Err(ImportError::invalid(
                "the local directory contains no Markdown files",
            ));
        }
    }
    Ok(Batch {
        documents,
        _directory: directory,
    })
}

fn staging_directory(stage_root: &Path) -> Result<TempDir, ImportError> {
    tempfile::Builder::new()
        .prefix("import-")
        .tempdir_in(stage_root)
        .map_err(|source| ImportError::Storage {
            operation: "create the import staging directory",
            source,
        })
}

fn is_markdown(name: &[u8]) -> bool {
    name.iter()
        .rposition(|&byte| byte == b'.')
        .is_some_and(|dot| dot != 0 && name[dot + 1..].eq_ignore_ascii_case(b"md"))
}

fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.is_symlink()
    }
}

fn copy_local_document(
    source: &Path,
    staged: &Path,
    buffer: &mut [u8],
    remaining: &mut usize,
    path_bytes: usize,
) -> Result<String, ImportError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let unreadable = |_| ImportError::invalid("cannot read the local Markdown file");
    let mut input = options.open(source).map_err(unreadable)?;
    let metadata = input.metadata().map_err(unreadable)?;
    if !metadata.is_file() || is_link(&metadata) {
        return Err(ImportError::invalid(
            "the local Markdown source is not a regular file",
        ));
    }
    *remaining = remaining
        .checked_sub(path_bytes)
        .ok_or(ImportError::PayloadTooLarge)?;
    if metadata.len() > *remaining as u64 {
        return Err(ImportError::PayloadTooLarge);
    }
    let mut output = fs::File::create_new(staged).map_err(|source| ImportError::Storage {
        operation: "create a staged Markdown document",
        source,
    })?;
    let mut utf8 = Utf8Tail::default();
    let mut hash = Sha256::new();
    loop {
        let read = input.read(buffer).map_err(unreadable)?;
        if read == 0 {
            break;
        }
        *remaining = remaining
            .checked_sub(read)
            .ok_or(ImportError::PayloadTooLarge)?;
        let chunk = &buffer[..read];
        utf8.check(chunk)?;
        hash.update(chunk);
        output
            .write_all(chunk)
            .map_err(|source| ImportError::Storage {
                operation: "write a staged Markdown document",
                source,
            })?;
    }
    finish_document(utf8, hash)
}

async fn read_path(mut field: Field<'_>) -> Result<String, ImportError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(ImportError::multipart)? {
        if chunk.len() > MAX_PATH_BYTES - bytes.len() {
            return Err(ImportError::invalid(PATH_TOO_LONG));
        }
        let len = bytes.len() + chunk.len();
        if len > bytes.capacity() {
            // Grow geometrically without letting spare capacity exceed the limit.
            bytes.reserve_exact(len.next_power_of_two() - bytes.len());
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_| ImportError::invalid("path must be UTF-8"))
}

async fn write_document(mut field: Field<'_>, file: &mut File) -> Result<String, ImportError> {
    let mut utf8 = Utf8Tail::default();
    let mut hash = Sha256::new();
    while let Some(chunk) = field.chunk().await.map_err(ImportError::multipart)? {
        utf8.check(&chunk)?;
        hash.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|source| ImportError::Storage {
                operation: "write a staged Markdown document",
                source,
            })?;
    }
    finish_document(utf8, hash)
}

fn finish_document(utf8: Utf8Tail, hash: Sha256) -> Result<String, ImportError> {
    if utf8.len != 0 {
        return Err(ImportError::invalid("file must be UTF-8"));
    }
    let mut hex = String::with_capacity(64);
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in hash.finalize().into_iter() {
        hex.push(char::from(DIGITS[usize::from(byte >> 4)]));
        hex.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    Ok(hex)
}

/// Carry only an incomplete UTF-8 scalar across multipart chunk boundaries.
#[derive(Default)]
struct Utf8Tail {
    bytes: [u8; 4],
    len: usize,
}

impl Utf8Tail {
    fn check(&mut self, mut chunk: &[u8]) -> Result<(), ImportError> {
        if self.len != 0 {
            // The previous call ended on a valid but incomplete scalar.
            let width = match self.bytes[0] {
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                _ => 4,
            };
            let take = (width - self.len).min(chunk.len());
            let (prefix, rest) = chunk.split_at(take);
            self.bytes[self.len..self.len + take].copy_from_slice(prefix);
            self.len += take;
            if self.len < width {
                return Ok(());
            }
            str::from_utf8(&self.bytes[..width])
                .map_err(|_| ImportError::invalid("file must be UTF-8"))?;
            self.len = 0;
            chunk = rest;
        }
        if let Err(error) = str::from_utf8(chunk) {
            if error.error_len().is_some() {
                return Err(ImportError::invalid("file must be UTF-8"));
            }
            let tail = &chunk[error.valid_up_to()..];
            self.bytes[..tail.len()].copy_from_slice(tail);
            self.len = tail.len();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::FromRequest;
    use axum::http::{Request, header};

    const BOUNDARY: &str = "multipart-test-boundary";

    fn append_field(body: &mut Vec<u8>, name: &str, filename: Option<&str>, bytes: &[u8]) {
        body.extend_from_slice(
            format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"").as_bytes(),
        );
        if let Some(filename) = filename {
            body.extend_from_slice(format!("; filename=\"{filename}\"").as_bytes());
        }
        body.extend_from_slice(b"\r\n\r\n");
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }

    fn append_document(body: &mut Vec<u8>, path: &[u8], file: &[u8]) {
        append_field(body, "path", None, path);
        append_field(body, "file", Some("ignored/browser.txt"), file);
    }

    fn close(body: &mut Vec<u8>) {
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    }

    async fn multipart(body: Vec<u8>) -> Multipart {
        let request = Request::post("/test")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(Body::from(body))
            .unwrap();
        Multipart::from_request(request, &()).await.unwrap()
    }

    #[test]
    fn validates_relative_scopes_without_requiring_markdown_extensions() {
        for path in ["指南/v1", "a.txt", "guide/std::vector"] {
            validate_path(path).unwrap();
        }
        for path in [
            "",
            " \u{3000}",
            "/abs.md",
            "//host/a.md",
            "C:/a.md",
            "c:a.md",
            "a\\b.md",
            "a//b.md",
            "a/",
            "./a.md",
            "a/../b.md",
            "../b.md",
            "a/./b.md",
            "a\0.md",
            "a\u{0085}b.md",
        ] {
            assert!(
                matches!(validate_path(path), Err(ImportError::InvalidInput { .. })),
                "{path:?}",
            );
        }
    }

    #[tokio::test]
    async fn stages_original_bytes_and_cleans_up_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let original = "\u{feff}# Title\r\n中文\r\n";
        let mut body = Vec::new();
        append_document(
            &mut body,
            "指南/nested/Title.MD".as_bytes(),
            original.as_bytes(),
        );
        append_field(&mut body, "path", None, "指南/nested/title.md".as_bytes());
        append_field(&mut body, "file", None, b"");
        close(&mut body);

        let batch = stage(multipart(body).await, root.path()).await.unwrap();
        let [doc, empty] = batch.documents() else {
            panic!("expected two documents");
        };
        assert_eq!(doc.relative_path, "指南/nested/Title.MD");
        assert_eq!(doc.read_body().unwrap().as_bytes(), original.as_bytes());
        assert_eq!(
            doc.sha256,
            "0b13afdb79052eb4fe6bbdc67931fc687016638eefef81c731dcb31000a876c3",
        );
        assert_eq!(empty.relative_path, "指南/nested/title.md");
        assert_eq!(empty.read_body().unwrap(), "");
        assert_eq!(
            empty.sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        assert_eq!(root.path().read_dir().unwrap().count(), 1);
        drop(batch);
        assert!(root.path().read_dir().unwrap().next().is_none());
    }

    #[tokio::test]
    async fn rejects_entire_batch_after_valid_prefix() {
        let root = tempfile::tempdir().unwrap();
        for (path, file, reason) in [
            (&b"../unsafe.md"[..], &b"data"[..], "segments"),
            (b"valid/nested.md", b"new data", "duplicate path"),
            (b"bad.md", b"private text\xff", "file must be UTF-8"),
            (b"bad.md", b"\xe4\xb8", "file must be UTF-8"),
            (b"path-\xff.md", b"data", "path must be UTF-8"),
            (b"guide.md/nested.txt", b"data", "must name a .md file"),
        ] {
            let mut body = Vec::new();
            append_document(&mut body, b"valid/nested.md", b"valid");
            append_document(&mut body, path, file);
            close(&mut body);
            let error = stage(multipart(body).await, root.path())
                .await
                .err()
                .expect("invalid batch");
            assert!(matches!(
                &error,
                ImportError::InvalidInput {
                    document: Some(2),
                    ..
                }
            ));
            let message = error.to_string();
            assert!(message.contains(reason), "{message}");
            assert!(!message.contains("private text"));
            assert!(
                root.path().read_dir().unwrap().next().is_none(),
                "{message}"
            );
        }
    }

    #[tokio::test]
    async fn rejects_upload_and_query_paths_over_the_utf8_byte_limit() {
        let root = tempfile::tempdir().unwrap();
        let at_limit = format!("{}x/a.md", "é".repeat((MAX_PATH_BYTES - 6) / 2));
        assert_eq!(at_limit.len(), MAX_PATH_BYTES);
        validate_path(&at_limit).unwrap();
        let over_limit = format!("x{at_limit}");
        assert_eq!(
            validate_path(&over_limit).unwrap_err().to_string(),
            PATH_TOO_LONG,
        );

        let mut body = Vec::new();
        append_document(&mut body, b"valid/nested.md", b"valid");
        append_document(&mut body, over_limit.as_bytes(), b"not staged");
        close(&mut body);
        assert!(matches!(
            stage(multipart(body).await, root.path()).await,
            Err(ImportError::InvalidInput {
                document: Some(2),
                reason: PATH_TOO_LONG,
            })
        ));
        assert!(root.path().read_dir().unwrap().next().is_none());
    }

    #[tokio::test]
    async fn rejects_empty_unpaired_reordered_and_truncated_multipart() {
        let root = tempfile::tempdir().unwrap();
        let mut empty = Vec::new();
        close(&mut empty);
        let mut unpaired = Vec::new();
        append_document(&mut unpaired, b"good.md", b"good");
        append_field(&mut unpaired, "path", None, b"unpaired.md");
        close(&mut unpaired);
        let mut reorder = Vec::new();
        append_field(&mut reorder, "file", None, b"not first");
        append_field(&mut reorder, "path", None, b"not-second.md");
        close(&mut reorder);
        let mut truncated = Vec::new();
        append_document(&mut truncated, b"good.md", b"good");
        append_document(&mut truncated, b"truncated.md", b"incomplete");
        for body in [empty, unpaired, reorder, truncated] {
            assert!(matches!(
                stage(multipart(body).await, root.path()).await,
                Err(ImportError::InvalidInput { .. })
            ));
            assert!(root.path().read_dir().unwrap().next().is_none());
        }
    }

    #[tokio::test]
    async fn reports_storage_failure_separately() {
        let root = tempfile::tempdir().unwrap();
        let mut body = Vec::new();
        append_document(&mut body, b"good.md", b"valid");
        close(&mut body);
        assert!(matches!(
            stage(multipart(body).await, &root.path().join("missing")).await,
            Err(ImportError::Storage { .. })
        ));
        assert!(root.path().read_dir().unwrap().next().is_none());
    }

    #[test]
    fn validates_multibyte_utf8_across_chunks() {
        let mut utf8 = Utf8Tail::default();
        for byte in "\u{feff}é中🦀\r\n".as_bytes() {
            utf8.check(std::slice::from_ref(byte)).unwrap();
            utf8.check(b"").unwrap();
        }
        assert_eq!(utf8.len, 0);
        utf8.check(b"\xe4\xb8").unwrap();
        assert!(utf8.check(b"x").is_err());
    }
}
