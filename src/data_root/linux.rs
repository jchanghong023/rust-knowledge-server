use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

pub(super) fn check_storage(path: &Path) -> io::Result<()> {
    let name = CString::new(path.as_os_str().as_bytes())?;
    let mut stats = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: name is NUL-terminated; stats has space for a statfs result.
    if unsafe { libc::statfs(name.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful statfs initialized the result.
    let stats = unsafe { stats.assume_init() };
    if !matches!(stats.f_type, libc::EXT4_SUPER_MAGIC | libc::XFS_SUPER_MAGIC) {
        return Err(unsupported(format!(
            "filesystem type {:#x} is not ext or XFS",
            stats.f_type
        )));
    }

    let device = fs::metadata(path)?.dev();
    let number = format!("{}:{}", libc::major(device), libc::minor(device));
    let sysfs = Path::new("/sys/dev/block").join(&number);
    require_backing_device(&sysfs, 8).map_err(|error| {
        unsupported(format!(
            "cannot confirm persistent block backing for device {number}: {error}"
        ))
    })
}

pub(super) fn check_same_mount(root: &Path, path: &Path) -> io::Result<()> {
    check_storage(path)?;
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

fn require_backing_device(path: &Path, layers_left: u8) -> io::Result<()> {
    let sysfs = path.canonicalize()?;
    if !sysfs.starts_with("/sys/devices") {
        return Err(unsupported("block device is not registered in sysfs"));
    }
    if !sysfs.starts_with("/sys/devices/virtual") {
        return Ok(());
    }

    // Permit device-mapper and MD only if all their backing devices are known. Other
    // virtual devices (loop, RAM, zram, NBD, etc.) cannot establish local persistence.
    // sysfs cannot tell whether a hypervisor or storage controller uses durable media.
    let name = sysfs.file_name().unwrap_or_default().as_bytes();
    if layers_left == 0 || !(name.starts_with(b"dm-") || name.starts_with(b"md")) {
        return Err(unsupported(format!(
            "unsupported virtual block device '{}'",
            sysfs.display()
        )));
    }
    if sysfs.join("partition").try_exists()? {
        let parent = sysfs
            .parent()
            .ok_or_else(|| unsupported("block partition has no parent device"))?;
        return require_backing_device(parent, layers_left - 1);
    }
    let mut slaves = fs::read_dir(sysfs.join("slaves"))?;
    let first = slaves
        .next()
        .ok_or_else(|| unsupported("no backing devices found in sysfs"))??;
    require_backing_device(&first.path(), layers_left - 1)?;
    for slave in slaves {
        require_backing_device(&slave?.path(), layers_left - 1)?;
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
