//! Device-local secret storage (plan sections 3 and 16).
//!
//! Session state, the cache key, API credentials and unfinished logout state
//! live in a private local file by default. [`SwitchingStore`] migrates them
//! to/from optional macOS Keychain storage; tests use memory and fault stores.
//!
//! Every [`SecretStore`] call is **blocking**. After startup the only caller is
//! the persistence worker thread (`crate::session::PersistHandle`), so no
//! Keychain call ever runs on an async runtime thread.

#[cfg(target_os = "macos")]
pub mod acl;
mod cache_key;
#[cfg(any(test, feature = "fake"))]
mod clock_gate;
#[cfg(target_os = "macos")]
mod keychain;
mod local;
#[cfg(any(test, feature = "fake"))]
mod memory;
#[cfg(test)]
mod tests;

use zeroize::Zeroizing;

pub use cache_key::{CacheKeyError, create_cache_key, delete_cache_key, load_or_create_cache_key};
#[cfg(any(test, feature = "fake"))]
pub use clock_gate::{Busy, ClockGate};
#[cfg(target_os = "macos")]
pub use keychain::{KeychainLocation, KeychainStore};
pub use local::{
    SecretStorageConfig, SwitchingStore, private_directory, read_private, write_private,
};
#[cfg(any(test, feature = "fake"))]
pub use memory::{FaultControl, FaultyStore, MemoryStore, StoreOp};

/// Which secret an operation targets. Each maps to one Keychain item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SecretItem {
    /// Versioned postcard blob of the critical session state.
    Session,
    /// Raw 32-byte SQLCipher key of the message cache.
    CacheKey,
    /// The entered API credentials (`dev.abyss.apicreds`, plan section 17.1).
    /// Survives logout; deleted only by `ForgetStoredCredentials`.
    ApiCredentials,
    /// Present while a logout has begun and not finished deleting (review
    /// round 15, A): startup resumes the logout before using the session.
    /// One byte, the remote result so far.
    LogoutPending,
}

impl SecretItem {
    /// Suffix appended to the store's namespace to form the Keychain service.
    pub(crate) const fn suffix(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::CacheKey => "cachekey",
            Self::ApiCredentials => "apicreds",
            Self::LogoutPending => "logoutpending",
        }
    }
}

/// Error from a secret store. Never carries secret material; the message is the
/// OS status description at most.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The store refused or failed the operation (Keychain `OSStatus`).
    Os { code: i32, message: String },
    /// A local file operation failed. Paths and secrets are never included.
    Local(std::io::ErrorKind),
    /// The stored value has an unexpected type or shape.
    Malformed,
    /// Injected by `FaultyStore`.
    Injected,
    /// The persistence worker is gone (its thread exited or the runtime is
    /// shutting down).
    Stopped,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Os { code, message } => write!(f, "keychain error {code}: {message}"),
            Self::Local(kind) => write!(f, "local secret storage error: {kind:?}"),
            Self::Malformed => f.write_str("stored secret is malformed"),
            Self::Injected => f.write_str("injected store failure"),
            Self::Stopped => f.write_str("persistence worker stopped"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Local(error.kind())
    }
}

/// Blocking key-value store for device-local secrets.
///
/// Contract:
/// - `get` returns `Ok(None)` when the item does not exist.
/// - `set` creates or replaces the item atomically (a failed `set` leaves the
///   previous value, if any, in place, except when a file replacement succeeds
///   but its directory sync fails; that error leaves durability uncertain).
/// - `delete` succeeds when the item does not exist, so a retried logout is
///   idempotent.
/// - Returned secrets are in zeroizing buffers; implementations must not keep
///   unwiped copies of their own.
pub trait SecretStore: Send + 'static {
    fn get(&self, item: SecretItem) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError>;
    fn set(&self, item: SecretItem, secret: &[u8]) -> Result<(), StoreError>;
    fn delete(&self, item: SecretItem) -> Result<(), StoreError>;

    /// Whether legacy secrets exist. Keychain overrides this with metadata
    /// queries that cannot display an authentication prompt. An error is
    /// unknown, never proof that there are no secrets to migrate.
    fn has_items(&self) -> Result<bool, StoreError> {
        for item in [
            SecretItem::Session,
            SecretItem::CacheKey,
            SecretItem::ApiCredentials,
            SecretItem::LogoutPending,
        ] {
            if self.get(item)?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Tests: the gate that keeps a paused clock still while this store's
    /// thread works (`clock_gate`).
    #[cfg(test)]
    fn clock_gate(&self) -> Option<ClockGate> {
        None
    }
}
