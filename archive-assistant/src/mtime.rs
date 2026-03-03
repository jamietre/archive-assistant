use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::Result;

/// Read the mtime of a file as seconds since UNIX epoch.
pub fn get_mtime(path: &Path) -> Result<u64> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta.modified()?;
    let secs = mtime.duration_since(UNIX_EPOCH)?.as_secs();
    Ok(secs)
}

/// Set the mtime of a file to `secs` seconds since UNIX epoch.
/// atime is set to the same value.
pub fn set_mtime(path: &Path, secs: u64) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let times = [
        libc::timespec { tv_sec: secs as libc::time_t, tv_nsec: 0 },
        libc::timespec { tv_sec: secs as libc::time_t, tv_nsec: 0 },
    ];
    let ret = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// Convenience: set mtime to original + 60 seconds.
pub fn bump_mtime(path: &Path, original_mtime: u64) -> Result<()> {
    set_mtime(path, original_mtime + 60)
}
