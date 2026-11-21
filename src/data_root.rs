use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
#[path = "data_root/linux.rs"]
mod platform;
#[cfg(windows)]
#[path = "data_root/windows.rs"]
mod platform;

#[cfg(target_os = "linux")]
pub(crate) const LINUX_ROOT: &str = "/dev/shm/rust-knowledge-server";
#[cfg(target_os = "linux")]
pub(crate) use platform::check_private_directory;

#[derive(Debug)]
pub(crate) struct Root {
    path: PathBuf,
    _lock: File,
}

impl Root {
    pub(crate) fn open_default() -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        return Self::open(Path::new(LINUX_ROOT));
        #[cfg(windows)]
        return Self::open(&platform::default_path()?);
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn open(requested: &Path) -> io::Result<Self> {
        if !requested.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "data root must be an absolute path",
            ));
        }
        let path = platform::prepare_directory(requested)
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

fn check_directory(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    #[cfg(windows)]
    let is_link = platform::is_reparse_point(metadata);
    #[cfg(target_os = "linux")]
    let is_link = metadata.is_symlink();
    if is_link || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("'{}' is not a real directory", path.display()),
        ));
    }
    Ok(())
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
    fn refuses_a_file_without_overwriting_it() -> io::Result<()> {
        let file = tempfile::NamedTempFile::new_in(test_parent())?;
        fs::write(file.path(), b"existing file")?;
        let error = Root::open(file.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotADirectory);
        assert_eq!(fs::read(file.path())?, b"existing file");
        Ok(())
    }

    fn test_parent() -> PathBuf {
        #[cfg(target_os = "linux")]
        return PathBuf::from("/dev/shm");
        #[cfg(windows)]
        return std::env::temp_dir();
    }

    #[test]
    fn an_owned_lock_blocks_a_second_owner_and_is_not_removed_or_truncated() -> io::Result<()> {
        let temp = tempfile::tempdir_in(test_parent())?;
        let requested = temp.path().join("knowledge");
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
    fn refuses_redirected_or_nonprivate_tmpfs_roots() -> io::Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir_in(test_parent())?;
        let target = temp.path().join("target");
        fs::create_dir(&target)?;
        let link = temp.path().join("link");
        symlink(&target, &link)?;
        assert_eq!(
            Root::open(&link).unwrap_err().kind(),
            io::ErrorKind::NotADirectory
        );
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
        assert_eq!(
            Root::open(&target).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(!target.join(".lock").exists());
        Ok(())
    }
}
