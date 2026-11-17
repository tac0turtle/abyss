//! macOS extended ACLs on the app's own directories (round 26, finding 3).
//!
//! A mode of 0700 says nothing about ACL entries: an inherited or existing
//! `everyone allow read,list` entry keeps granting access after `chmod`.
//! [`strip`] removes every extended entry from one directory and checks none
//! is left. It is used only on `dev.abyss` and `cache`, never on Application
//! Support. The calls are the `_link_np` variants, which act on a symlink
//! itself rather than its target (the caller has already refused symlinks).
//!
//! `libc` 0.2 has no ACL bindings for Apple targets, so the five libSystem
//! functions are declared here, with the prototypes from `<sys/acl.h>`.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// `acl_t`: an opaque handle owned by libSystem, released with `acl_free`.
type Acl = *mut c_void;
/// `ACL_TYPE_EXTENDED` (`<sys/acl.h>`): the only ACL type macOS supports.
const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;
/// `ACL_FIRST_ENTRY` (`<sys/acl.h>`).
const ACL_FIRST_ENTRY: c_int = 0;

unsafe extern "C" {
    fn acl_get_link_np(path: *const c_char, kind: c_int) -> Acl;
    fn acl_set_link_np(path: *const c_char, kind: c_int, acl: Acl) -> c_int;
    fn acl_init(count: c_int) -> Acl;
    fn acl_free(obj: *mut c_void) -> c_int;
    fn acl_get_entry(acl: Acl, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn entries(path: &CStr) -> io::Result<bool> {
    // SAFETY: `path` is a valid NUL-terminated string for the whole call.
    let acl = unsafe { acl_get_link_np(path.as_ptr(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        // No ACL at all.
        return match error.raw_os_error() {
            Some(libc::ENOENT) => Ok(false),
            _ => Err(error),
        };
    }
    let mut entry: *mut c_void = std::ptr::null_mut();
    // SAFETY: `acl` is a live handle from `acl_get_link_np`, and `entry` is a
    // valid out-pointer; the entry is only borrowed and never used after the
    // `acl_free` below.
    let found = unsafe { acl_get_entry(acl, ACL_FIRST_ENTRY, &raw mut entry) } == 0;
    // SAFETY: `acl` came from `acl_get_link_np`, is freed once, and is not
    // used afterwards.
    unsafe { acl_free(acl) };
    Ok(found)
}

/// Removes every extended ACL entry from `path`, then checks none is left.
pub(super) fn strip(path: &Path) -> io::Result<()> {
    let path = c_path(path)?;
    if !entries(&path)? {
        return Ok(());
    }
    // SAFETY: no preconditions; a null result is handled below.
    let empty = unsafe { acl_init(1) };
    if empty.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `path` is a valid NUL-terminated string and `empty` a live
    // handle from `acl_init`; the call only reads both.
    let rc = unsafe { acl_set_link_np(path.as_ptr(), ACL_TYPE_EXTENDED, empty) };
    let set = (rc == 0).then_some(()).ok_or_else(io::Error::last_os_error);
    // SAFETY: `empty` came from `acl_init`, is freed once, and is not used
    // afterwards.
    unsafe { acl_free(empty) };
    set?;
    if entries(&path)? {
        return Err(io::Error::other("ACL entries remain"));
    }
    Ok(())
}
