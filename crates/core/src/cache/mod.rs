//! Encrypted on-disk message cache (SQLCipher, AES-256, CommonCrypto on macOS).
//!
//! Security properties:
//! - The database, its WAL and journal are encrypted with a random 256-bit key held
//!   by the caller (Keychain, device-only). Temp storage is in memory only.
//! - The directory is created 0700 and excluded from backups.
//! - Logout is crypto-erase via [`Cache::erase`]: close the connection, delete the
//!   persistent key, then unlink the files.
//! - Only our own validated domain model is stored. Rows are size-checked before
//!   decoding, and decoding re-validates invariants (see `RichText`); bad rows are
//!   deleted, never surfaced.
//! - Bounded: [`MAX_CACHE_BYTES`] total, [`MAX_CACHED_PER_CHAT`] per chat.
//!
//! The cache is disposable: a wrong key or unknown schema means "destroy and
//! recreate", never "fail the app".

pub(crate) mod backup;
mod db;

use std::path::{Path, PathBuf};

use zeroize::Zeroizing;

mod census;
mod worker;
#[cfg(feature = "fake")]
#[doc(hidden)]
pub use census::leak_lock_for_tests;
pub use census::{EXIT_CACHE_BUSY, EXIT_WAIT, InitError, Refused, begin_exit, init, wait_closed};
pub(crate) use census::{close_admission, hold};
pub use db::{Binding, Cache, EraseError};
pub use worker::{CACHE_QUEUE, CacheHandle, DeleteKey, EraseFailure, KeyError, WorkerError};
pub(crate) use worker::{CacheCmd, open_blocking};

/// Maximum logical bytes of cached message bodies.
pub const MAX_CACHE_BYTES: u64 = 512 * 1024 * 1024;
/// Maximum cached messages per chat.
pub const MAX_CACHED_PER_CHAT: u32 = 50_000;
/// Rows larger than this are treated as corrupt and deleted without decoding.
pub const MAX_ROW_BYTES: usize = 64 * 1024;

const DB_FILE: &str = "cache.db";
const SIDE_FILES: [&str; 3] = ["cache.db-wal", "cache.db-shm", "cache.db-journal"];

/// 256-bit cache encryption key. Zeroized on drop.
pub struct CacheKey(Zeroizing<[u8; 32]>);

impl std::fmt::Debug for CacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CacheKey(<redacted>)")
    }
}

impl CacheKey {
    pub fn generate() -> Result<Self, CacheError> {
        let mut key = Zeroizing::new([0u8; 32]);
        getrandom::fill(key.as_mut_slice()).map_err(|_| CacheError::Random)?;
        Ok(Self(key))
    }

    /// Takes ownership of key material read from the Keychain. The caller passes a
    /// zeroizing buffer so no unwiped copy remains on its side.
    pub fn from_bytes(bytes: Zeroizing<[u8; 32]>) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// `x'..'` raw-key literal for `sqlite3_key_v2`, which skips SQLCipher's KDF.
    /// Built in a buffer preallocated to its exact final size, so it never
    /// reallocates and leaves no unwiped intermediate copies.
    fn hex_literal(&self) -> Zeroizing<Vec<u8>> {
        let mut s = Zeroizing::new(Vec::with_capacity(2 + 64 + 1));
        s.extend_from_slice(b"x'");
        for b in self.0.iter() {
            s.push(hex_digit(b >> 4));
            s.push(hex_digit(b & 0x0f));
        }
        s.push(b'\'');
        debug_assert_eq!(s.len(), s.capacity());
        s
    }
}

/// Lowercase hex digit for a nibble (`n < 16`).
fn hex_digit(n: u8) -> u8 {
    debug_assert!(n < 16);
    if n < 10 { b'0' + n } else { b'a' + (n - 10) }
}

#[derive(Debug)]
pub enum CacheError {
    /// The key does not decrypt the existing database. Caller should destroy it.
    WrongKey,
    /// The database was written by an unknown schema version. Caller should destroy it.
    UnknownSchema(i64),
    /// SQLCipher is not using the expected crypto provider.
    WrongProvider(String),
    /// Eviction could not bring the cache under its byte budget.
    OverBudget,
    Random,
    /// SQLCipher could not be readied for safe use (see `census::init`).
    Init(InitError),
    /// Admission is closed (core shutdown, process exit) or full.
    Refused(Refused),
    Io(std::io::Error),
    /// A SQLite error, without its message (round 29).
    Sql(SqlError),
    Encode(postcard::Error),
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongKey => f.write_str("cache key does not match"),
            Self::UnknownSchema(v) => write!(f, "unknown cache schema version {v}"),
            Self::WrongProvider(p) => write!(f, "unexpected crypto provider {p:?}"),
            Self::OverBudget => f.write_str("cache is over its byte budget"),
            Self::Random => f.write_str("system random number generator failed"),
            Self::Init(e) => write!(f, "cache unavailable: {e}"),
            Self::Refused(Refused::Closing) => f.write_str("cache closed for shutdown"),
            Self::Refused(Refused::Full) => f.write_str("too many open cache directories"),
            Self::Io(e) => write!(f, "cache io: {e}"),
            Self::Sql(e) => write!(f, "cache sql: {e}"),
            Self::Encode(e) => write!(f, "cache encode: {e}"),
        }
    }
}

impl std::error::Error for CacheError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Encode(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for CacheError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(SqlError::from(&e))
    }
}

/// What is kept of a `rusqlite::Error` (round 29): its SQLite result codes
/// only. Its message is dropped, because SQLite names the database file in
/// some (`unable to open database file: <path>`), and paths hold the user
/// name (plan section 11). `None` codes: an error raised by rusqlite itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SqlError {
    pub code: Option<rusqlite::ErrorCode>,
    pub extended: Option<i32>,
}

impl From<&rusqlite::Error> for SqlError {
    fn from(e: &rusqlite::Error) -> Self {
        match e {
            rusqlite::Error::SqliteFailure(f, _) => Self {
                code: Some(f.code),
                extended: Some(f.extended_code),
            },
            _ => Self {
                code: None,
                extended: None,
            },
        }
    }
}

impl std::fmt::Display for SqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.code, self.extended) {
            (Some(code), Some(ext)) => write!(f, "{code:?} ({ext})"),
            _ => f.write_str("rusqlite error"),
        }
    }
}

impl From<std::io::Error> for CacheError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<postcard::Error> for CacheError {
    fn from(e: postcard::Error) -> Self {
        Self::Encode(e)
    }
}

fn db_path(dir: &Path) -> PathBuf {
    dir.join(DB_FILE)
}

/// Removes a cache that could not be opened (wrong key, unknown schema). For an
/// open cache use [`Cache::erase`], which also closes the connection first.
pub fn destroy_unopened(dir: &Path) -> std::io::Result<()> {
    remove_files(dir)
}

fn remove_files(dir: &Path) -> std::io::Result<()> {
    for name in std::iter::once(DB_FILE).chain(SIDE_FILES) {
        match std::fs::remove_file(dir.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    match std::fs::remove_dir(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Creates the cache directory (0700) and excludes it from backups. A
/// symlink in its place is refused (task 9b): the mode and the xattr would
/// otherwise land on the link's target.
fn prepare_dir(dir: &Path) -> Result<(), CacheError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    if std::fs::symlink_metadata(dir)?.file_type().is_symlink() {
        return Err(CacheError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the cache directory is a symlink",
        )));
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    backup::exclude_from_backup(dir)?;
    Ok(())
}

#[cfg(test)]
mod tests;
