use std::ffi::CString;
use std::fs::{self, DirBuilder, File};
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub(super) fn prepare_directory(path: &Path) -> io::Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| unsupported("data root has no parent"))?;
    if parent.canonicalize()? != parent {
        return Err(unsupported("data root parent must not be redirected"));
    }
    check_storage(parent)?;
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    check_private_directory(path)?;
    check_same_mount(parent, path)?;
    Ok(path.to_path_buf())
}

pub(crate) fn check_private_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    super::check_directory(path, &metadata)?;
    // SAFETY: geteuid has no arguments or memory side effects.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "data root must be owned by the service user and private",
        ));
    }
    Ok(())
}

fn check_storage(path: &Path) -> io::Result<()> {
    let name = CString::new(path.as_os_str().as_bytes())?;
    let mut stats = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: name is NUL-terminated; stats has space for a statfs result.
    if unsafe { libc::statfs(name.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful statfs initialized the result.
    let stats = unsafe { stats.assume_init() };
    if stats.f_type != libc::TMPFS_MAGIC {
        return Err(unsupported(format!(
            "filesystem type {:#x} is not tmpfs",
            stats.f_type
        )));
    }
    Ok(())
}

pub(super) fn check_same_mount(root: &Path, path: &Path) -> io::Result<()> {
    if fs::metadata(root)?.dev() != fs::metadata(path)?.dev() {
        return Err(unsupported(
            "managed directory must be on the data root device",
        ));
    }

    // Same-device bind mounts also redirect writes and recursive cleanup.
    // mountinfo escapes these four bytes in mount point paths.
    let mut escaped = Vec::new();
    for &byte in path.as_os_str().as_bytes() {
        match byte {
            b' ' => escaped.extend_from_slice(br"\040"),
            b'\t' => escaped.extend_from_slice(br"\011"),
            b'\n' => escaped.extend_from_slice(br"\012"),
            b'\\' => escaped.extend_from_slice(br"\134"),
            byte => escaped.push(byte),
        }
    }
    let mounts = fs::read("/proc/self/mountinfo")?;
    for line in mounts
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let mount = line
            .split(|&byte| byte == b' ')
            .nth(4)
            .ok_or_else(|| unsupported("cannot read mount point from mountinfo"))?;
        if mount
            .strip_prefix(escaped.as_slice())
            .is_some_and(|tail| tail.is_empty() || tail.starts_with(b"/"))
        {
            return Err(unsupported(
                "mount points are not allowed in managed storage",
            ));
        }
    }
    Ok(())
}

pub(super) fn open_lock(path: &Path) -> io::Result<File> {
    let lock = File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !lock.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "lock must be a regular file",
        ));
    }
    Ok(lock)
}

fn unsupported(reason: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, reason.into())
}
