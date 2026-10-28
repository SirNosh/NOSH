//! File facts Node exposes through `lstat` that stable Rust does not.

use std::io;
use std::path::Path;

/// The number of hard links to the file at `path` (Node's `lstat().nlink`). A database reachable
/// through a second name could be changed behind NOSH's back, so backups refuse one.
#[cfg(windows)]
pub fn hard_link_count(path: &Path) -> io::Result<u32> {
    Ok(file_information(path)?.nNumberOfLinks)
}

/// The volume and file index of the file at `path` (Node's `lstat()` `dev` and `ino`): a file
/// replaced under the same name has a new identity even when its size and times match.
#[cfg(windows)]
pub fn file_identity(path: &Path) -> io::Result<(u64, u64)> {
    let info = file_information(path)?;
    Ok((
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn file_information(
    path: &Path,
) -> io::Result<windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let file = std::fs::File::open(path)?;
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: `file` stays open for the whole call, so its handle is valid, and `info` points to
    // writable memory of the structure's size, which the call fills in on success.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded, so it initialised every field.
    Ok(unsafe { info.assume_init() })
}

#[cfg(unix)]
pub fn hard_link_count(path: &Path) -> io::Result<u32> {
    use std::os::unix::fs::MetadataExt;
    Ok(u32::try_from(std::fs::metadata(path)?.nlink()).unwrap_or(u32::MAX))
}

#[cfg(unix)]
pub fn file_identity(path: &Path) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_hard_links() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.sqlite");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(hard_link_count(&file).unwrap(), 1);
        std::fs::hard_link(&file, dir.path().join("b.sqlite")).unwrap();
        assert_eq!(hard_link_count(&file).unwrap(), 2);
        assert!(hard_link_count(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn a_replaced_file_has_a_new_identity() {
        let dir = tempfile::tempdir().unwrap();
        let (file, other) = (dir.path().join("a.txt"), dir.path().join("b.txt"));
        std::fs::write(&file, "x").unwrap();
        std::fs::write(&other, "x").unwrap();
        let before = file_identity(&file).unwrap();
        assert_eq!(file_identity(&file).unwrap(), before);
        assert_ne!(file_identity(&other).unwrap(), before);
        std::fs::rename(&other, &file).unwrap();
        assert_ne!(file_identity(&file).unwrap(), before);
    }
}
