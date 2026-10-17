//! File facts Node exposes through `lstat` that stable Rust does not.

use std::io;
use std::path::Path;

/// The number of hard links to the file at `path` (Node's `lstat().nlink`). A database reachable
/// through a second name could be changed behind NOSH's back, so backups refuse one.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn hard_link_count(path: &Path) -> io::Result<u32> {
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
    Ok(unsafe { info.assume_init() }.nNumberOfLinks)
}

#[cfg(unix)]
pub fn hard_link_count(path: &Path) -> io::Result<u32> {
    use std::os::unix::fs::MetadataExt;
    Ok(u32::try_from(std::fs::metadata(path)?.nlink()).unwrap_or(u32::MAX))
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
}
