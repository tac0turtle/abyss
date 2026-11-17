//! Private local secrets by default, with recoverable migration to/from Keychain.
//!
//! The manifest is authoritative once created. Never fall back to old Keychain
//! items after that: doing so could resurrect a logged-out session. A move to
//! local storage commits all four items together before deleting their Keychain
//! copies; cleanup is recorded and retried before serving any operation.

use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use super::{SecretItem, SecretStore, StoreError};

const FILE: &str = "secrets";
const HEADER: &[u8] = b"abyss-secrets 1\n";
const MAX_BYTES: u64 = 64 * 1024;
const ITEMS: [SecretItem; 4] = [
    SecretItem::Session,
    SecretItem::CacheKey,
    SecretItem::ApiCredentials,
    SecretItem::LogoutPending,
];

/// Storage selected at launch. Changes take effect on the next launch.
#[derive(Debug)]
pub struct SecretStorageConfig {
    /// Private app directory, separate from the erasable message cache.
    pub dir: PathBuf,
    /// Opt in to Keychain storage. Defaults to false in the app.
    pub use_keychain: bool,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct State {
    keychain: bool,
    cleanup: bool,
    values: [Option<Vec<u8>>; 4],
}

impl Drop for State {
    fn drop(&mut self) {
        self.values.zeroize();
    }
}

/// A single persistence-worker owner; initialization and migration are lazy so
/// no Keychain or file IO runs on the UI thread. Debug never includes secrets.
pub struct SwitchingStore<S> {
    config: SecretStorageConfig,
    keychain: S,
    state: RefCell<Option<State>>,
    lock: RefCell<Option<File>>,
}

impl<S> std::fmt::Debug for SwitchingStore<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwitchingStore")
            .field("use_keychain", &self.config.use_keychain)
            .finish_non_exhaustive()
    }
}

impl<S: SecretStore> SwitchingStore<S> {
    pub fn new(config: SecretStorageConfig, keychain: S) -> Self {
        Self {
            config,
            keychain,
            state: RefCell::new(None),
            lock: RefCell::new(None),
        }
    }

    fn initialize(&self) -> Result<(), StoreError> {
        if self.state.borrow().is_some() {
            return Ok(());
        }
        private_directory(&self.config.dir).map_err(StoreError::from)?;
        crate::cache::backup::exclude_from_backup(&self.config.dir).map_err(StoreError::from)?;
        if self.lock.borrow().is_none() {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(self.config.dir.join("lock"))
                .map_err(StoreError::from)?;
            let meta = file.metadata().map_err(StoreError::from)?;
            // SAFETY: getuid has no preconditions and cannot fail.
            if !meta.is_file() || meta.nlink() != 1 || meta.uid() != unsafe { libc::getuid() } {
                return Err(StoreError::Local(io::ErrorKind::PermissionDenied));
            }
            // SAFETY: file is a live descriptor; flock retains no pointers.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(StoreError::from(io::Error::last_os_error()));
            }
            *self.lock.borrow_mut() = Some(file);
        }
        remove_staging(&self.config.dir).map_err(StoreError::from)?;
        let path = self.config.dir.join(FILE);
        let mut state = match read_private(&path).map_err(StoreError::from)? {
            Some(bytes) => decode(&bytes)?,
            None => {
                // Legacy versions stored everything in Keychain. Import once;
                // an unreadable item blocks startup rather than losing it.
                State {
                    keychain: self.config.use_keychain || self.keychain.has_items()?,
                    cleanup: false,
                    values: Default::default(),
                }
            }
        };
        self.cleanup(&mut state)?;
        if state.keychain == self.config.use_keychain {
            // Also establish an authoritative manifest for a legacy Keychain
            // installation before its first subsequent write or logout.
            self.save(&state)?;
        } else {
            self.migrate(&mut state)?;
        }
        *self.state.borrow_mut() = Some(state);
        Ok(())
    }

    fn migrate(&self, state: &mut State) -> Result<(), StoreError> {
        if self.config.use_keychain {
            // Record possible destination copies before writing any. If a
            // write fails and the user cancels the switch, local startup must
            // still delete those copies before a later logout can succeed.
            let mut pending = state.clone();
            pending.cleanup = true;
            self.save(&pending)?;
            *state = pending;
            for (item, value) in ITEMS.into_iter().zip(&state.values) {
                match value {
                    Some(bytes) => self.keychain.set(item, bytes)?,
                    None => self.keychain.delete(item)?,
                }
            }
            // Only remove the local copies after every destination write has
            // succeeded. On failure the old manifest remains authoritative.
            let next = State {
                keychain: true,
                cleanup: false,
                values: Default::default(),
            };
            self.save(&next)?;
            *state = next;
        } else {
            let mut next = State {
                cleanup: true,
                keychain: false,
                values: Default::default(),
            };
            for (item, value) in ITEMS.into_iter().zip(&mut next.values) {
                *value = self
                    .keychain
                    .get(item)?
                    .map(|mut bytes| std::mem::take(&mut *bytes));
            }
            self.save(&next)?;
            *state = next;
            self.cleanup(state)?;
        }
        Ok(())
    }

    fn cleanup(&self, state: &mut State) -> Result<(), StoreError> {
        if state.cleanup {
            for item in ITEMS {
                self.keychain.delete(item)?;
            }
            let mut next = state.clone();
            next.cleanup = false;
            self.save(&next)?;
            *state = next;
        }
        Ok(())
    }

    fn save(&self, state: &State) -> Result<(), StoreError> {
        let mut bytes = Zeroizing::new(HEADER.to_vec());
        // Serialize into our zeroizing buffer, including on encode failure.
        let mut encoded = Zeroizing::new(vec![
            0;
            usize::try_from(MAX_BYTES)
                .map_err(|_| StoreError::Malformed)?
        ]);
        let data = postcard::to_slice(state, &mut encoded).map_err(|_| StoreError::Malformed)?;
        bytes.extend_from_slice(data);
        if u64::try_from(bytes.len()).map_err(|_| StoreError::Malformed)? > MAX_BYTES {
            return Err(StoreError::Malformed);
        }
        write_private(&self.config.dir.join(FILE), &bytes).map_err(StoreError::from)
    }
}

impl<S: SecretStore> SecretStore for SwitchingStore<S> {
    fn get(&self, item: SecretItem) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
        self.initialize()?;
        let state = self.state.borrow();
        let state = state.as_ref().ok_or(StoreError::Stopped)?;
        if state.keychain {
            self.keychain.get(item)
        } else {
            Ok(state
                .values
                .get(index(item))
                .ok_or(StoreError::Malformed)?
                .clone()
                .map(Zeroizing::new))
        }
    }

    fn set(&self, item: SecretItem, secret: &[u8]) -> Result<(), StoreError> {
        self.change(item, Some(secret))
    }

    fn delete(&self, item: SecretItem) -> Result<(), StoreError> {
        self.change(item, None)
    }
}

impl<S: SecretStore> SwitchingStore<S> {
    fn change(&self, item: SecretItem, secret: Option<&[u8]>) -> Result<(), StoreError> {
        self.initialize()?;
        let mut current = self.state.borrow_mut();
        let state = current.as_ref().ok_or(StoreError::Stopped)?;
        if state.keychain {
            return match secret {
                Some(bytes) => self.keychain.set(item, bytes),
                None => self.keychain.delete(item),
            };
        }
        let mut next = state.clone();
        let value = next
            .values
            .get_mut(index(item))
            .ok_or(StoreError::Malformed)?;
        value.zeroize();
        *value = secret.map(<[u8]>::to_vec);
        if let Err(error) = self.save(&next) {
            // A directory sync can fail after rename. Reload before another
            // operation so an uncertain deletion is never undone from memory.
            *current = None;
            return Err(error);
        }
        *current = Some(next);
        Ok(())
    }
}

const fn index(item: SecretItem) -> usize {
    match item {
        SecretItem::Session => 0,
        SecretItem::CacheKey => 1,
        SecretItem::ApiCredentials => 2,
        SecretItem::LogoutPending => 3,
    }
}

fn decode(bytes: &[u8]) -> Result<State, StoreError> {
    let bytes = bytes.strip_prefix(HEADER).ok_or(StoreError::Malformed)?;
    let (state, rest): (State, _) =
        postcard::take_from_bytes(bytes).map_err(|_| StoreError::Malformed)?;
    if !rest.is_empty()
        || (state.keychain && (state.cleanup || state.values.iter().any(Option::is_some)))
    {
        return Err(StoreError::Malformed);
    }
    Ok(state)
}

/// Creates and checks an app-owned directory. The parent must already exist.
pub fn private_directory(path: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let meta = fs::symlink_metadata(path)?;
    // SAFETY: getuid has no preconditions and cannot fail.
    if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe secret directory",
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    #[cfg(target_os = "macos")]
    super::acl::strip(path)?;
    // A synced manifest alone does not make a newly created directory's
    // entry durable. Sync its parent before migration can delete the source.
    // Repeat on retry, including if an earlier directory sync failed.
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

/// Bounded, no-follow reads of private, regular, single-link files.
pub fn read_private(path: &Path) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    // SAFETY: getuid has no preconditions and cannot fail.
    if !meta.is_file() || meta.nlink() != 1 || meta.uid() != unsafe { libc::getuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe secret file",
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    #[cfg(target_os = "macos")]
    super::acl::strip(path)?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized secret file",
        ));
    }
    Ok(Some(bytes))
}

/// Atomic replacement with a private, exclusively created staging file. A
/// crash before rename leaves the original intact; after rename it leaves the
/// complete replacement. Never follow a leftover staging symlink.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut nonce = [0; 16];
    getrandom::fill(&mut nonce).map_err(io::Error::other)?;
    let name = format!("{:032x}", u128::from_le_bytes(nonce));
    let stem = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let staging = dir.join(format!(".{stem}-{name}"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staging)?;
    let result = (|| {
        #[cfg(target_os = "macos")]
        super::acl::strip(&staging)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&staging, path)?;
        File::open(dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staging);
    }
    result
}

// Only the secrets manifest uses this prefix. Preference saves may be in
// flight on another worker, and must keep their own staging files.
fn remove_staging(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(nonce) = name.to_str().and_then(|n| n.strip_prefix(".secrets-")) else {
            continue;
        };
        if nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
