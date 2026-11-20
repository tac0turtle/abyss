//! Where the encrypted message cache lives (plan section 16, tasks 9b, 9c;
//! Stage 3 plan section 2.2).
//!
//! - **Real backend:** `<support>/dev.abyss/cache`, where `<support>` is the
//!   user's Application Support directory from [`support_dir`]
//!   (`NSFileManager URLsForDirectory:inDomains:`). Foundation resolves the
//!   directory through the user database and, in the App Sandbox, the
//!   container, never through `HOME`. It must be absolute and already exist.
//! - **Two regimes.** The bundle runs sandboxed, so `<support>` is
//!   `~/Library/Containers/dev.abyss/Data/Library/Application Support`. A bare
//!   `cargo dev` or `cargo run` binary is not sandboxed and gets the real
//!   `~/Library/Application Support`. Each regime has its own Keychain
//!   namespace. If a key remains after its database is removed, core opens
//!   a new cache (`a_keychain_key_with_no_database_...`).
//! - **`--fake`:** no cache, unless `ABYSS_FAKE_CACHE=1`. Then each run gets
//!   a fresh directory `$TMPDIR/dev.abyss.fake/run-<pid>-<nanos>/cache`,
//!   created exclusively, so a fake run never adopts existing files (such as
//!   a `cache.db` symlink planted there). A `TMPDIR` that resolves inside the
//!   real app directory is refused. Fake runs are dev runs and stay
//!   unsandboxed. The fake keeps its key in memory, so the opt-in exercises
//!   open, bind, writes and the exit drain under the real
//!   GPUI quit, not persistence (tested in core,
//!   `cached_history_survives_a_core_restart`).
//!
//! [`prepare`] creates the components below the root one at a time with mode
//! 0700, refuses a component that is a symlink, not a directory, or not ours
//! (the real uid), narrows a wider mode to 0700 and strips extended ACL
//! entries (`acl`). Core's `Cache::open` recreates the leaf after a logout's
//! erase with the same mode and symlink refusal, and opens the database with
//! `SQLITE_OPEN_NOFOLLOW`. A failure is logged and the run continues without
//! a cache; it never blocks login. Paths are never logged (plan section 11).
//!
//! **Threat model.** The user's Application Support (or the container) is trusted:
//! they belong to the user, and no hostile process runs as the same uid (one
//! that did could read the Keychain item's owner's memory anyway). The checks
//! here catch mistakes and stale or planted state from *other* users or
//! earlier runs; the time between a check and a use (TOCTOU) is not defended
//! against a same-uid attacker.

#[cfg(target_os = "macos")]
use abyss_core::secret::acl;

use std::fmt;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use abyss_core::CacheConfig;

/// The app id: the directory in Application Support.
pub(crate) const APP_ID: &str = "dev.abyss";
/// The fake's directory under `TMPDIR`; never [`APP_ID`].
#[cfg(feature = "fake")]
const FAKE_APP_ID: &str = "dev.abyss.fake";
/// The cache directory inside the app's.
const CACHE_DIR: &str = "cache";
/// The per-run directory of a fake cache, in errors.
#[cfg(feature = "fake")]
const RUN_DIR: &str = "run";
/// `TMPDIR`, the per-user temporary directory macOS sets.
#[cfg(feature = "fake")]
pub(crate) const TMPDIR_VAR: &str = "TMPDIR";
/// Opt-in for a cache in `--fake` runs; only the value `1` enables it.
#[cfg(feature = "fake")]
pub(crate) const FAKE_CACHE_VAR: &str = "ABYSS_FAKE_CACHE";
/// The private mode of every directory we create.
const PRIVATE: u32 = 0o700;

/// Where a cache goes: an existing root, and the directories created below
/// it (app id, a fake's run, then cache).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CacheLocation {
    root: PathBuf,
    app: &'static str,
    #[cfg(feature = "fake")]
    fake: Option<FakeRun>,
}

/// A fake run's fresh directory and the real tree it must stay out of.
#[cfg(feature = "fake")]
#[derive(Clone, Debug, PartialEq, Eq)]
struct FakeRun {
    run: String,
    /// The real app directory, which the fake root must stay out of.
    real_app: PathBuf,
}

impl CacheLocation {
    /// The real backend's cache, under `support`, the Application Support
    /// directory from [`support_dir`] (the container's when sandboxed).
    pub(crate) fn real(support: Option<PathBuf>) -> Result<Self, StorageError> {
        let support = support
            .filter(|s| s.is_absolute())
            .ok_or(StorageError::NoRoot)?;
        Ok(Self {
            root: support,
            app: APP_ID,
            #[cfg(feature = "fake")]
            fake: None,
        })
    }

    /// The fake's cache: `None` unless `enabled` is exactly `1`, or without
    /// an absolute `tmpdir`. `support` is the real Application Support, which
    /// locates the real tree to stay out of; without it isolation can't be
    /// checked, so there is no fake cache (round 29).
    #[cfg(feature = "fake")]
    pub(crate) fn fake(
        enabled: Option<&std::ffi::OsStr>,
        tmpdir: Option<std::ffi::OsString>,
        support: Option<PathBuf>,
    ) -> Option<Self> {
        if enabled != Some(std::ffi::OsStr::new("1")) {
            return None;
        }
        let Some(root) = tmpdir.map(PathBuf::from).filter(|p| p.is_absolute()) else {
            tracing::warn!("{FAKE_CACHE_VAR} is set but TMPDIR is not absolute; no cache");
            return None;
        };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let Ok(real) = Self::real(support) else {
            tracing::warn!(
                "{FAKE_CACHE_VAR} is set but the Application Support directory is unknown; no cache"
            );
            return None;
        };
        let real_app = real.app_dir();
        Some(Self {
            root,
            app: FAKE_APP_ID,
            fake: Some(FakeRun {
                run: format!("run-{}-{nanos}", std::process::id()),
                real_app,
            }),
        })
    }

    /// The app directory (`dev.abyss` or `dev.abyss.fake`).
    fn app_dir(&self) -> PathBuf {
        self.root.join(self.app)
    }

    /// The directory that holds `cache`: the app's, or a fake's run.
    fn parent(&self) -> PathBuf {
        #[cfg(feature = "fake")]
        if let Some(fake) = &self.fake {
            return self.app_dir().join(&fake.run);
        }
        self.app_dir()
    }

    /// The cache directory this location names.
    pub(crate) fn dir(&self) -> PathBuf {
        self.parent().join(CACHE_DIR)
    }
}

/// The app's directory in Application Support, under the root from
/// [`support_dir`]: the one place its path is built. The theme, colourway,
/// sidebar and Keychain-preference files live in it, and the real cache below
/// it.
pub(crate) fn app_dir() -> Option<PathBuf> {
    support_dir().map(|support| support.join(APP_ID))
}

/// Select the bundle's storage even if the full notification guard fails.
#[cfg(target_os = "macos")]
pub(crate) fn has_app_bundle_id() -> bool {
    objc2_foundation::NSBundle::mainBundle()
        .bundleIdentifier()
        .is_some_and(|id| id.to_string() == APP_ID)
}

/// The user's Application Support directory, or `None`: the container's
/// `Library/Application Support` when the process is sandboxed, the real one
/// when it is not. Foundation answers from the user database and the
/// container, so `HOME` is not consulted. The path is absolute; it is not
/// created or checked here ([`prepare`] checks everything below it).
#[cfg(target_os = "macos")]
pub(crate) fn support_dir() -> Option<PathBuf> {
    use objc2_foundation::{NSFileManager, NSSearchPathDirectory, NSSearchPathDomainMask};
    let urls = NSFileManager::defaultManager().URLsForDirectory_inDomains(
        NSSearchPathDirectory::ApplicationSupportDirectory,
        NSSearchPathDomainMask::UserDomainMask,
    );
    let url = urls.firstObject()?;
    if !url.isFileURL() {
        return None;
    }
    let path = PathBuf::from(url.path()?.to_string());
    path.is_absolute().then_some(path)
}

/// Only macOS has the lookup; elsewhere there is no cache.
#[cfg(not(target_os = "macos"))]
pub(crate) fn support_dir() -> Option<PathBuf> {
    None
}

/// Why there is no cache this run. Names a component, never a path.
#[derive(Debug)]
pub(crate) enum StorageError {
    /// No absolute Application Support directory.
    NoRoot,
    /// The root is missing or not a directory.
    Root(io::Error),
    /// A fake root inside the real app directory.
    #[cfg(feature = "fake")]
    Overlap,
    /// A component is a symlink.
    Symlink(&'static str),
    /// A component exists but is not a directory.
    NotADirectory(&'static str),
    /// A component is owned by another user.
    Owner(&'static str),
    /// A fake run's directory already existed.
    #[cfg(feature = "fake")]
    Exists(&'static str),
    /// Creating, inspecting, narrowing or stripping a component failed.
    Io(&'static str, io::Error),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRoot => f.write_str("no absolute Application Support directory"),
            Self::Root(e) => write!(f, "the cache's parent directory is unusable: {e}"),
            #[cfg(feature = "fake")]
            Self::Overlap => f.write_str("the fake cache would be inside the real one"),
            Self::Symlink(c) => write!(f, "{c:?} is a symlink"),
            Self::NotADirectory(c) => write!(f, "{c:?} is not a directory"),
            Self::Owner(c) => write!(f, "{c:?} has another owner"),
            #[cfg(feature = "fake")]
            Self::Exists(c) => write!(f, "{c:?} already exists"),
            Self::Io(c, e) => write!(f, "{c:?}: {e}"),
        }
    }
}

/// Creates the location's directories (0700, no symlinks, no ACL entries)
/// and returns the cache directory.
pub(crate) fn prepare(location: &CacheLocation) -> Result<PathBuf, StorageError> {
    let root = std::fs::metadata(&location.root).map_err(StorageError::Root)?;
    if !root.is_dir() {
        return Err(StorageError::Root(io::Error::from(
            io::ErrorKind::NotADirectory,
        )));
    }
    // SAFETY: `getuid` has no preconditions and cannot fail.
    let me = unsafe { libc::getuid() };
    #[cfg(feature = "fake")]
    if let Some(fake) = &location.fake {
        refuse_overlap(&location.root, &fake.real_app)?;
    }
    private_dir(&location.app_dir(), location.app, me)?;
    #[cfg(feature = "fake")]
    if location.fake.is_some() {
        fresh_dir(&location.parent(), me)?;
    }
    let cache = location.dir();
    private_dir(&cache, CACHE_DIR, me)?;
    Ok(cache)
}

/// A fake root must not resolve inside the real app directory. Only a real
/// directory that does not exist (`NotFound`) is known not to contain it;
/// any other error resolving it refuses (round 29).
#[cfg(feature = "fake")]
fn refuse_overlap(root: &Path, real_app: &Path) -> Result<(), StorageError> {
    let root = std::fs::canonicalize(root).map_err(StorageError::Root)?;
    match std::fs::canonicalize(real_app) {
        Ok(real) if root.starts_with(&real) => Err(StorageError::Overlap),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(StorageError::Overlap),
    }
}

/// Creates a fake run's directory, which must not exist yet.
#[cfg(feature = "fake")]
fn fresh_dir(path: &Path, owner: u32) -> Result<(), StorageError> {
    match std::fs::DirBuilder::new().mode(PRIVATE).create(path) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(StorageError::Exists(RUN_DIR)),
        Err(e) => Err(StorageError::Io(RUN_DIR, e)),
        Ok(()) => private_dir(path, RUN_DIR, owner),
    }
}

/// Creates `path` with mode 0700 if missing, then checks it is a directory
/// (not a symlink to one) owned by `owner`, narrows its mode to 0700 and
/// strips its ACL entries.
fn private_dir(path: &Path, name: &'static str, owner: u32) -> Result<(), StorageError> {
    match std::fs::DirBuilder::new().mode(PRIVATE).create(path) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => {
            return Err(StorageError::Io(name, e));
        }
        _ => {}
    }
    // `symlink_metadata` does not follow a symlink in the last component.
    let meta = std::fs::symlink_metadata(path).map_err(|e| StorageError::Io(name, e))?;
    if meta.file_type().is_symlink() {
        return Err(StorageError::Symlink(name));
    }
    if !meta.is_dir() {
        return Err(StorageError::NotADirectory(name));
    }
    if meta.uid() != owner {
        return Err(StorageError::Owner(name));
    }
    if meta.permissions().mode() & 0o777 != PRIVATE {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE))
            .map_err(|e| StorageError::Io(name, e))?;
    }
    #[cfg(target_os = "macos")]
    acl::strip(path).map_err(|e| StorageError::Io(name, e))?;
    Ok(())
}

/// The core cache config for a location, or `None` (logged): the run then
/// goes on without a disk cache.
pub(crate) fn config(location: Result<CacheLocation, StorageError>) -> Option<CacheConfig> {
    let prepared = location.and_then(|l| {
        let dir = prepare(&l)?;
        #[cfg(feature = "fake")]
        if l.fake.is_some() {
            start_fake_cleanup(l.parent(), dir.clone());
        }
        Ok(dir)
    });
    match prepared {
        Ok(dir) => {
            tracing::info!("disk cache enabled");
            Some(CacheConfig { dir })
        }
        Err(error) => {
            tracing::warn!(%error, "no disk cache this run");
            None
        }
    }
}

/// A cleanup request: the worker answers on it once it is done.
#[cfg(feature = "fake")]
type CleanupDone = futures::channel::oneshot::Sender<()>;

/// Wakes this process's fake cleanup worker, once the fake cache is
/// configured (round 30).
#[cfg(feature = "fake")]
static FAKE_CLEANUP: std::sync::OnceLock<std::sync::mpsc::SyncSender<CleanupDone>> =
    std::sync::OnceLock::new();
/// How long the cleanup worker waits for the fake cache's workers before it
/// gives up (GPUI gives `on_app_quit` 200 ms in all).
#[cfg(feature = "fake")]
const CLEANUP_WAIT: std::time::Duration = std::time::Duration::from_millis(100);

/// Starts the worker that removes this run's fake directory `run` at quit,
/// once nothing has `cache` open (round 29, finding 2). It sleeps until
/// [`request_fake_cleanup`]. Without it (no thread), the directory is kept,
/// with a warning; it is never reused (each run creates its own).
#[cfg(feature = "fake")]
pub(crate) fn start_fake_cleanup(run: PathBuf, cache: PathBuf) {
    let (wake, requests) = std::sync::mpsc::sync_channel::<CleanupDone>(1);
    let worker = std::thread::Builder::new()
        .name("abyss-fake-cleanup".into())
        .spawn(move || {
            if let Ok(done) = requests.recv() {
                remove_run(&run, &cache, CLEANUP_WAIT);
                let _ = done.send(());
            }
        });
    match worker {
        // One fake cache per process; a second `set` cannot happen.
        Ok(_) => drop(FAKE_CLEANUP.set(wake)),
        Err(error) => tracing::warn!(%error, "no fake cleanup; the run directory is kept"),
    }
}

/// At quit, after `begin_exit`: asks the cleanup worker to remove the fake
/// run directory, and returns at once (round 30). The census wait, the
/// removal and any warning happen on the worker, never on the quit thread:
/// none of them is bounded in wall-clock time (a held census lock, a slow
/// disk, a blocked stderr). The returned future resolves when the worker is
/// done, or at once without a worker; GPUI awaits it within its own
/// 200 ms quit bound. Best effort: whatever is left when the process ends
/// stays, as before.
#[cfg(feature = "fake")]
pub(crate) fn request_fake_cleanup() -> impl std::future::Future<Output = ()> + 'static {
    let (done, finished) = futures::channel::oneshot::channel();
    if let Some(wake) = FAKE_CLEANUP.get() {
        // Full or gone: an earlier request is in hand, or the worker died.
        let _ = wake.try_send(done);
    }
    async move {
        let _ = finished.await;
    }
}

/// Removes `run` once nothing has `cache` open within `wait`; whether it
/// was removed. `remove_dir_all` removes links, never their targets.
#[cfg(feature = "fake")]
fn remove_run(run: &Path, cache: &Path, wait: std::time::Duration) -> bool {
    if !abyss_core::cache::wait_closed(Some(cache), wait) {
        tracing::warn!("the fake cache is still open; its run directory is kept");
        return false;
    }
    match std::fs::remove_dir_all(run) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, "could not remove the fake run directory");
            false
        }
    }
}

#[cfg(test)]
mod tests;
