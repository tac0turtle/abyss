//! Excluding the cache directory from Time Machine and other backups.

use std::io;
use std::path::Path;

/// xattr name used by `tmutil addexclusion` for sticky exclusions.
#[cfg(target_os = "macos")]
const EXCLUDE_XATTR: &std::ffi::CStr = c"com.apple.metadata:com_apple_backup_excludeItem";

/// Binary plist encoding the string "com.apple.backupd", byte-for-byte what
/// `tmutil addexclusion` writes (verified on macOS 27).
#[cfg(target_os = "macos")]
const EXCLUDE_VALUE: [u8; 61] = [
    0x62, 0x70, 0x6C, 0x69, 0x73, 0x74, 0x30, 0x30, 0x5F, 0x10, 0x11, 0x63, 0x6F, 0x6D, 0x2E, 0x61,
    0x70, 0x70, 0x6C, 0x65, 0x2E, 0x62, 0x61, 0x63, 0x6B, 0x75, 0x70, 0x64, 0x08, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1C,
];

/// Marks `dir` as excluded from backups. The exclusion is an xattr on the
/// directory, so it covers every file inside it.
#[cfg(target_os = "macos")]
pub(crate) fn exclude_from_backup(dir: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: `path` and `EXCLUDE_XATTR` are valid NUL-terminated C strings and
    // `EXCLUDE_VALUE` is a live buffer of the given length for the whole call.
    let rc = unsafe {
        libc::setxattr(
            path.as_ptr(),
            EXCLUDE_XATTR.as_ptr(),
            EXCLUDE_VALUE.as_ptr().cast(),
            EXCLUDE_VALUE.len(),
            0,
            0,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn exclude_from_backup(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn tmutil_reports_excluded() {
        let dir = tempfile::tempdir().unwrap();
        exclude_from_backup(dir.path()).unwrap();
        let out = std::process::Command::new("tmutil")
            .arg("isexcluded")
            .arg(dir.path())
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("[Excluded]"));
    }
}
