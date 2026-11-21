use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, Prefix};
use std::ptr::null_mut;

use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetDriveTypeW, GetVolumeInformationW, GetVolumePathNameW,
};
use windows_sys::Win32::System::WindowsProgramming::{DRIVE_FIXED, DRIVE_REMOTE};

pub(super) fn check_path_namespace(path: &Path) -> io::Result<()> {
    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::UNC(..) | Prefix::VerbatimUNC(..) => {
                Err(unsupported("UNC/network paths are not allowed"))
            }
            Prefix::Disk(_) | Prefix::VerbatimDisk(_) => Ok(()),
            // A mounted volume without a drive letter can resolve to a volume GUID path.
            Prefix::Verbatim(name) if is_volume_guid(name.as_encoded_bytes()) => Ok(()),
            _ => Err(unsupported("not a drive or volume GUID path")),
        },
        _ => Err(unsupported("not a fully qualified volume path")),
    }
}

fn is_volume_guid(name: &[u8]) -> bool {
    let Some(name) = name.strip_suffix(b"}") else {
        return false;
    };
    if name.len() != 43 || !name[..7].eq_ignore_ascii_case(b"Volume{") {
        return false;
    }
    name[7..]
        .iter()
        .enumerate()
        .all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

pub(super) fn check_storage(path: &Path) -> io::Result<()> {
    check_path_namespace(path)?;
    // Keep the canonical verbatim path: stripping its prefix can change how Windows
    // interprets its components. Size the root buffer for the entire extended path.
    let mut name: Vec<u16> = path.as_os_str().encode_wide().collect();
    if name.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains NUL",
        ));
    }
    name.push(0);
    let mut volume = vec![0; name.len() + 1];
    // SAFETY: name is NUL-terminated, and volume is writable for its stated length.
    if unsafe { GetVolumePathNameW(name.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) } == 0 {
        return Err(api_error("GetVolumePathNameW"));
    }
    let root_len = volume.iter().position(|&c| c == 0).unwrap_or(volume.len());
    if root_len == volume.len() || !volume[..root_len].ends_with(&[u16::from(b'\\')]) {
        return Err(unsupported(
            "volume lookup returned no absolute volume root",
        ));
    }
    let volume_path = OsString::from_wide(&volume[..root_len]);
    check_path_namespace(Path::new(&volume_path))?;

    // SAFETY: volume is NUL-terminated and ends with a backslash.
    match unsafe { GetDriveTypeW(volume.as_ptr()) } {
        DRIVE_FIXED => {}
        DRIVE_REMOTE => return Err(unsupported("network volumes are not allowed")),
        drive_type => {
            return Err(unsupported(format!(
                "volume is not a fixed drive (type {drive_type})"
            )));
        }
    }
    let mut filesystem = [0u16; 261];
    // SAFETY: volume is still NUL-terminated; the filesystem buffer has the stated capacity.
    if unsafe {
        GetVolumeInformationW(
            volume.as_ptr(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    } == 0
    {
        return Err(api_error("GetVolumeInformationW"));
    }
    let end = filesystem
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(filesystem.len());
    let filesystem = &filesystem[..end];
    if !["NTFS", "ReFS"]
        .iter()
        .any(|name| filesystem.iter().copied().eq(name.encode_utf16()))
    {
        return Err(unsupported(format!(
            "volume filesystem '{}' is not NTFS or ReFS",
            String::from_utf16_lossy(filesystem)
        )));
    }
    Ok(())
}

pub(super) fn open_lock(path: &Path) -> io::Result<File> {
    let lock = File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = lock.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "lock must be a regular file, not a reparse point",
        ));
    }
    Ok(lock)
}

fn api_error(operation: &str) -> io::Error {
    let error = io::Error::last_os_error();
    io::Error::new(
        error.kind(),
        format!("{operation} could not classify volume: {error}"),
    )
}

fn unsupported(reason: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, reason.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unc_and_device_namespaces() {
        for path in [
            r"\\server\share\dir",
            r"\\?\UNC\server\share\dir",
            r"\\.\C:\dir",
        ] {
            assert_eq!(
                check_path_namespace(Path::new(path)).unwrap_err().kind(),
                io::ErrorKind::Unsupported
            );
        }
    }
}
