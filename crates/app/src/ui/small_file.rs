//! Small preference files in Application Support: the theme choice and the
//! sidebar's view.
//!
//! They are not secret and not precious: a few lines of text, readable only
//! by the user (0600 in a 0700 directory). Only the first bytes are read, so
//! a huge file is just a damaged one; the caller treats anything unexpected
//! as its defaults.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// Every file is under 50 bytes when valid; nothing longer is read.
const MAX_FILE_BYTES: u64 = 128;
const DIR: &str = "Library/Application Support/dev.abyss";

/// Where the file `name` is kept, if the home directory is known.
pub(crate) fn path(name: &str) -> Option<PathBuf> {
    Some(std::env::home_dir()?.join(DIR).join(name))
}

/// At most the file's first [`MAX_FILE_BYTES`], as text.
pub(crate) fn read(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    File::open(path)?
        .take(MAX_FILE_BYTES)
        .read_to_string(&mut text)?;
    Ok(text)
}

/// Writes `text` beside its final place, then renames it in, so a reader
/// never sees half a file.
pub(crate) fn write(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let staging = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&staging)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    fs::rename(&staging, path)
}

/// Removes the file; one that is already gone is fine.
pub(crate) fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}
