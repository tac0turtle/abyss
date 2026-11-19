//! Small preference files in the app's directory: the theme and colourway
//! choices and the sidebar's view.
//!
//! They are not secret and not precious: a few lines of text, readable only
//! by the user (0600 in a 0700 directory). Only the first bytes are read, so
//! a huge file is just a damaged one; the caller treats anything unexpected
//! as its defaults.
//!
//! The directory gets the same checks as the Keychain preference's
//! (colourways plan, section 3): a write refuses a symlinked, non-directory
//! or foreign-owned app directory, narrows its mode to 0700, and never
//! creates `Library/Application Support`. The file is staged under a random
//! name and renamed in, so a reader never sees half a file and nothing
//! planted at a predictable name is followed.

use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use abyss_core::secret::{private_directory, write_private};

/// Every file is under 64 bytes when valid; nothing longer is read.
const MAX_FILE_BYTES: u64 = 128;

/// Where the file `name` is kept, if the home directory is known.
pub(crate) fn path(name: &str) -> Option<PathBuf> {
    Some(crate::storage::app_dir()?.join(name))
}

/// At most the file's first [`MAX_FILE_BYTES`], as text. Only a regular
/// file in an owned app directory is read: a symlink (even a dangling one)
/// or anything else at `path` or in the directory's place is an error,
/// never [`io::ErrorKind::NotFound`], and a FIFO cannot block.
pub(crate) fn read(path: &Path) -> io::Result<String> {
    if !owned_dir(path)? {
        return Err(io::Error::from(io::ErrorKind::NotFound));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a regular file",
        ));
    }
    let mut text = String::new();
    file.take(MAX_FILE_BYTES).read_to_string(&mut text)?;
    Ok(text)
}

/// Replaces the file with `text`, after checking its directory (see the
/// module docs). On an error the file may hold the old or the new text,
/// never part of either.
pub(crate) fn write(path: &Path, text: &str) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    private_directory(dir)?;
    write_private(path, text.as_bytes())
}

/// Removes the file; one that is already gone, or whose directory is, is
/// fine. A symlinked, non-directory or foreign-owned directory is refused
/// and nothing is removed.
pub(crate) fn remove(path: &Path) -> io::Result<()> {
    if !owned_dir(path)? {
        return Ok(());
    }
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Whether the directory holding `path` exists: `Ok(false)` if it is
/// missing, an error if a symlink, a non-directory or a directory owned by
/// someone else stands in its place. Nothing is created or followed.
fn owned_dir(path: &Path) -> io::Result<bool> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let meta = match fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    // SAFETY: getuid has no preconditions and cannot fail.
    if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe app directory",
        ));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    /// A trusted parent (standing in for Application Support) holding a
    /// not-yet-created app directory.
    fn app_dir() -> (tempfile::TempDir, PathBuf) {
        let parent = tempfile::tempdir().unwrap();
        let app = parent.path().join("dev.abyss");
        (parent, app)
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// Nothing but `name` in `dir`: no staging file was left behind.
    fn only(dir: &Path, name: &str) {
        let names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, [name], "leftovers in {dir:?}");
    }

    #[test]
    fn writes_privately_and_reads_back() {
        let (_parent, app) = app_dir();
        let file = app.join("theme");
        write(&file, "one\n").unwrap();
        assert_eq!(read(&file).unwrap(), "one\n");
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&app), 0o700);
        write(&file, "two\n").unwrap();
        assert_eq!(read(&file).unwrap(), "two\n");
        only(&app, "theme");
    }

    #[test]
    fn a_loose_app_directory_is_narrowed() {
        let (_parent, app) = app_dir();
        fs::create_dir(&app).unwrap();
        fs::set_permissions(&app, fs::Permissions::from_mode(0o755)).unwrap();
        write(&app.join("theme"), "x\n").unwrap();
        assert_eq!(mode(&app), 0o700);
    }

    #[test]
    fn a_symlinked_or_file_app_directory_is_refused() {
        let (parent, app) = app_dir();
        let elsewhere = parent.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::write(elsewhere.join("theme"), "planted\n").unwrap();
        symlink(&elsewhere, &app).unwrap();
        let file = app.join("theme");
        assert!(write(&file, "x\n").is_err());
        assert!(remove(&file).is_err());
        assert_ne!(read(&file).unwrap_err().kind(), io::ErrorKind::NotFound);
        let planted = fs::read_to_string(elsewhere.join("theme")).unwrap();
        assert_eq!(planted, "planted\n", "followed");
        // A dangling link is not a missing directory either.
        fs::remove_file(&app).unwrap();
        symlink(parent.path().join("nowhere"), &app).unwrap();
        assert_ne!(read(&file).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(remove(&file).is_err());
        fs::remove_file(&app).unwrap();
        fs::write(&app, "").unwrap();
        assert!(write(&file, "x\n").is_err());
        assert!(remove(&file).is_err());
        assert_ne!(read(&file).unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_missing_parent_is_not_created() {
        let parent = tempfile::tempdir().unwrap();
        let app = parent.path().join("Application Support").join("dev.abyss");
        assert!(write(&app.join("theme"), "x\n").is_err());
        assert!(!app.parent().unwrap().exists());
    }

    #[test]
    fn planted_symlinks_are_not_followed() {
        let (parent, app) = app_dir();
        fs::create_dir(&app).unwrap();
        let target = parent.path().join("target");
        fs::write(&target, "untouched").unwrap();
        // At the old fixed staging name, and at the file itself.
        let file = app.join("theme");
        symlink(&target, file.with_extension("tmp")).unwrap();
        write(&file, "one\n").unwrap();
        fs::remove_file(&file).unwrap();
        symlink(&target, &file).unwrap();
        write(&file, "two\n").unwrap();
        assert_eq!(read(&file).unwrap(), "two\n");
        assert_eq!(fs::read_to_string(&target).unwrap(), "untouched");
    }

    #[test]
    fn only_regular_files_are_read() {
        let (parent, app) = app_dir();
        fs::create_dir(&app).unwrap();
        let file = app.join("colourway");
        assert_eq!(read(&file).unwrap_err().kind(), io::ErrorKind::NotFound);
        let missing_dir = parent.path().join("gone").join("colourway");
        assert_eq!(
            read(&missing_dir).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        symlink(parent.path().join("nowhere"), &file).unwrap();
        assert_ne!(read(&file).unwrap_err().kind(), io::ErrorKind::NotFound);
        fs::remove_file(&file).unwrap();
        let fifo = std::ffi::CString::new(file.to_str().unwrap()).unwrap();
        // SAFETY: a valid, NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(
            read(&file).is_err(),
            "a FIFO is not read, and doesn't block"
        );
    }

    #[test]
    fn removal_tolerates_what_is_already_gone() {
        let (_parent, app) = app_dir();
        remove(&app.join("theme")).unwrap();
        write(&app.join("theme"), "x\n").unwrap();
        remove(&app.join("theme")).unwrap();
        remove(&app.join("theme")).unwrap();
        assert_eq!(fs::read_dir(&app).unwrap().count(), 0);
    }
}
