use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Component, Path, PathBuf};

#[cfg(target_os = "linux")]
#[path = "data_root/linux.rs"]
mod platform;
#[cfg(windows)]
#[path = "data_root/windows.rs"]
mod platform;

const SERVICE_DIR: &str = "rust-knowledge-server";
const DEFAULT_DIR: &str = ".rust-knowledge-server";

#[derive(Debug)]
pub(crate) struct Root {
    path: PathBuf,
    _lock: File,
}

impl Root {
    pub(crate) fn open_from_env() -> io::Result<Self> {
        let configured = env::var_os("OMP_CONFIG_ROOT");
        Self::open(&resolve_path(configured.as_deref(), env::home_dir)?)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn open(requested: &Path) -> io::Result<Self> {
        let path = prepare_directory(requested)
            .map_err(|error| path_error("cannot use data root", requested, error))?;
        let lock_path = path.join(".lock");
        let lock = platform::open_lock(&lock_path)
            .map_err(|error| path_error("cannot open data root lock", &lock_path, error))?;
        match lock.try_lock() {
            Ok(()) => Ok(Self { path, _lock: lock }),
            Err(TryLockError::WouldBlock) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "data root '{}' is already locked by another service instance",
                    path.display()
                ),
            )),
            Err(TryLockError::Error(error)) => Err(path_error(
                "cannot acquire data root lock",
                &lock_path,
                error,
            )),
        }
    }

    pub(crate) fn check_managed_storage(&self, path: &Path) -> io::Result<()> {
        // Callers reject links and reparse points before reaching here. Windows
        // mount points are reparse points; Linux bind mounts need a separate check.
        #[cfg(target_os = "linux")]
        platform::check_same_mount(&self.path, path)
            .map_err(|error| path_error("unsupported managed storage", path, error))?;
        #[cfg(windows)]
        platform::check_storage(path)
            .map_err(|error| path_error("unsupported managed storage", path, error))?;
        Ok(())
    }
}

fn resolve_path(
    configured: Option<&OsStr>,
    home_dir: impl FnOnce() -> Option<PathBuf>,
) -> io::Result<PathBuf> {
    let configured = configured.map(trim_override).unwrap_or_default();
    if Path::new(configured).is_absolute() {
        return Ok(Path::new(configured).join(SERVICE_DIR));
    }

    let home = home_dir().filter(|path| path.is_absolute()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot determine an absolute user home directory for the data root; set OMP_CONFIG_ROOT to a fully qualified path",
        )
    })?;
    if configured == "~" {
        return Ok(home.join(SERVICE_DIR));
    }
    let bytes = configured.as_encoded_bytes();
    if bytes.starts_with(b"~/") || bytes.starts_with(b"~\\") {
        let mut expanded = home.into_os_string();
        // SAFETY: splitting after the ASCII '~' preserves encoded character boundaries.
        let suffix = unsafe { OsStr::from_encoded_bytes_unchecked(&bytes[1..]) };
        #[cfg(windows)]
        if suffix.as_encoded_bytes().contains(&b'/') {
            use std::os::windows::ffi::{OsStrExt, OsStringExt};
            // Verbatim home paths interpret only backslashes as separators.
            let suffix: Vec<_> = suffix
                .encode_wide()
                .map(|unit| {
                    if unit == u16::from(b'/') {
                        u16::from(b'\\')
                    } else {
                        unit
                    }
                })
                .collect();
            expanded.push(std::ffi::OsString::from_wide(&suffix));
            return Ok(PathBuf::from(expanded).join(SERVICE_DIR));
        }
        expanded.push(suffix);
        return Ok(PathBuf::from(expanded).join(SERVICE_DIR));
    }
    Ok(home.join(DEFAULT_DIR))
}

fn trim_override(value: &OsStr) -> &OsStr {
    if let Some(text) = value.to_str() {
        return OsStr::new(text.trim());
    }
    // SAFETY: trimming only ASCII whitespace preserves encoded character boundaries.
    unsafe { OsStr::from_encoded_bytes_unchecked(value.as_encoded_bytes().trim_ascii()) }
}

fn prepare_directory(requested: &Path) -> io::Result<PathBuf> {
    #[cfg(windows)]
    platform::check_path_namespace(requested)?;
    let mut candidate = requested;
    let mut missing = Vec::new();
    let mut resolved = loop {
        match fs::symlink_metadata(candidate) {
            // Once an entry exists, failure to resolve it (including a dangling link) is fatal.
            Ok(_) => break checked_directory(candidate)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(Component::Normal(name)) = candidate.components().next_back() else {
                    return Err(path_error("cannot resolve directory", candidate, error));
                };
                missing.push(name);
                candidate = candidate.parent().expect("absolute path with a final name");
            }
            Err(error) => return Err(path_error("cannot inspect directory", candidate, error)),
        }
    };
    // Start from the checked ancestor, never the unresolved alias. Recheck every component
    // before descending, including a directory another service instance just created.
    for name in missing.into_iter().rev() {
        let child = resolved.join(name);
        match fs::create_dir(&child) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(path_error("cannot create directory", &child, error)),
        }
        let checked = checked_directory(&child)?;
        #[cfg(target_os = "linux")]
        crate::durable::sync_dir(&resolved)?;
        resolved = checked;
    }
    Ok(resolved)
}

fn checked_directory(path: &Path) -> io::Result<PathBuf> {
    let resolved = path
        .canonicalize()
        .map_err(|error| path_error("cannot resolve directory", path, error))?;
    let metadata = fs::metadata(&resolved)
        .map_err(|error| path_error("cannot inspect resolved directory", &resolved, error))?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("'{}' is not a directory", resolved.display()),
        ));
    }
    platform::check_storage(&resolved)
        .map_err(|error| path_error("unsupported data storage", &resolved, error))?;
    Ok(resolved)
}

fn path_error(action: &str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{action} '{}': {error}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_absolute_override_is_trimmed_and_needs_no_home() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let absolute = temp.path().canonicalize()?;
        let mut configured = std::ffi::OsString::from(" \t");
        configured.push(&absolute);
        configured.push(" \n");
        assert_eq!(
            resolve_path(Some(&configured), || panic!(
                "absolute override needs no home"
            ))?,
            absolute.join(SERVICE_DIR),
        );
        Ok(())
    }

    #[test]
    fn expands_only_supported_tilde_forms_and_uses_home_for_relative_paths() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().to_path_buf();
        let resolve =
            |value: Option<&str>| resolve_path(value.map(OsStr::new), || Some(home.clone()));
        assert_eq!(resolve(Some("~"))?, home.join(SERVICE_DIR));
        assert_eq!(
            resolve(Some(" ~/custom "))?,
            home.join("custom").join(SERVICE_DIR)
        );
        let mut backslash = home.clone().into_os_string();
        backslash.push(r"\custom");
        assert_eq!(
            resolve(Some(r"~\custom"))?,
            PathBuf::from(backslash).join(SERVICE_DIR)
        );
        for value in [
            None,
            Some("  "),
            Some("relative/path"),
            Some("~another/user"),
        ] {
            assert_eq!(resolve(value)?, home.join(DEFAULT_DIR));
        }
        #[cfg(windows)]
        for value in [r"C:relative", r"\root-relative", "/root-relative"] {
            assert_eq!(resolve(Some(value))?, home.join(DEFAULT_DIR));
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn expands_forward_slash_tilde_with_a_verbatim_home() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().canonicalize()?;
        let expected = home.join("nested").join("root").join(SERVICE_DIR);
        assert_eq!(
            resolve_path(Some(OsStr::new("~/nested/root")), || Some(home))?,
            expected
        );
        Ok(())
    }

    #[test]
    fn rejects_explicit_file_without_falling_back() -> io::Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        let path = resolve_path(Some(file.path().as_os_str()), || panic!("no fallback"))?;
        let error = Root::open(&path).unwrap_err();
        assert!(error.to_string().contains("cannot use data root"));
        assert!(file.path().is_file());
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn an_owned_lock_blocks_a_second_owner_and_is_not_removed_or_truncated() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let requested = temp.path().join("new-parent").join(SERVICE_DIR);
        let root = Root::open(&requested)?;
        assert_eq!(root.path(), requested.canonicalize()?);
        assert_eq!(
            Root::open(&requested).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let lock_path = root.path().join(".lock");
        drop(root);
        assert!(lock_path.is_file());
        fs::write(&lock_path, b"existing lock file")?;
        drop(Root::open(&requested)?);
        assert_eq!(fs::read(&lock_path)?, b"existing lock file");
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn refuses_a_dangling_directory_link() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let target = temp.path().join("missing-target");
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&target, &link)?;
        let error = Root::open(&link.join(SERVICE_DIR)).unwrap_err();
        assert!(error.to_string().contains("cannot resolve directory"));
        assert!(!target.exists());
        Ok(())
    }
}
