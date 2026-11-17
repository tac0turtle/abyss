//! Which cache databases are open, by directory, and SQLCipher's lifetime
//! around process exit (plan section 13, exit SIGSEGV fix; rounds 23, 24,
//! 27 and 30).
//!
//! SQLCipher tears down its global state (crypto provider, private heap) in
//! an exit-time handler (`sqlcipher_atexit`, plus a `__mod_term_func`). A
//! thread still inside SQLCipher at that moment, typically a worker closing
//! its connection (`sqlite3_close` -> `sqlite3FreeCodecArg`), touches freed
//! memory and the process dies with SIGSEGV.
//!
//! The rules:
//! - [`init`] initializes SQLite, which runs `sqlcipher_extra_init` and so
//!   registers SQLCipher's exit handler, and only then registers ours. Exit
//!   handlers run in reverse order of registration, so ours runs first.
//!   `Cache::open` refuses to open anything until `init` has succeeded, and
//!   core calls it at startup before any background work exists.
//! - Every thread that may run SQLCipher code holds an [`Open`] entry. A
//!   `Cache` holds one for as long as its connection lives (the entry is its
//!   last field, so it drops after the connection, on unwinding too), and a
//!   caller takes one before it schedules an open on another thread, so a
//!   queued open is counted before it starts.
//! - Admission is closed atomically with draining: [`close_admission`] for
//!   one directory while core shuts down, [`begin_exit`] for the whole
//!   process. A closed admission refuses new entries, so an empty census
//!   stays empty.
//! - The exit hook closes admission and waits, bounded, for every entry. If
//!   one is still open at the deadline, it ends the process with `_exit`
//!   rather than return into SQLCipher's teardown.
//!
//! Process exit uses only two static atomics, [`EXITING`] and [`OPEN`]
//! (round 27). Neither needs initializing or allocating, and neither can
//! block. Their protocol (all `SeqCst`), each side a store then a load:
//! - `enter` increments `OPEN` (a reservation), then loads `EXITING`. If
//!   it is set, `enter` gives the reservation back and refuses, before
//!   anything touches SQLCipher.
//! - The exit hook stores `EXITING`, then loads `OPEN`.
//!
//! `SeqCst` puts all four in one total order consistent with each thread's
//! program order. If `enter`'s load sees `EXITING` clear, that load comes
//! before the hook's store, so `enter`'s increment, earlier still, comes
//! before the hook's load: the hook sees the count and waits for the entry.
//! Otherwise `enter` sees `EXITING` and refuses. Round 27 had `enter` load
//! first and increment after, which let the hook's store and load both fall
//! between them: an uncounted entry (round 30). A refused reservation can
//! make the hook poll once more; it is never counted for longer than
//! `enter` holds it. The per-directory lists stay under the [`STATE`]
//! mutex, which process exit never takes.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use rusqlite::ffi;

/// How long process exit waits for open caches to close.
pub const EXIT_WAIT: Duration = Duration::from_secs(3);
/// The process status when exit gives up waiting for a cache to close and
/// skips the rest of exit (`EX_TEMPFAIL`).
pub const EXIT_CACHE_BUSY: i32 = 75;
/// Distinct cache directories the census tracks at once. A process uses
/// one; tests use one per test. Beyond this, opens are refused.
const MAX_DIRS: usize = 256;
/// How often the exit hook looks at the census again.
const POLL: Duration = Duration::from_millis(1);
/// Upper bound on those looks (the deadline ends them first).
const MAX_POLLS: u32 = 10_000;

#[derive(Debug)]
struct State {
    /// Open entries per directory, each directory listed at most once.
    open: Vec<(PathBuf, usize)>,
    /// Directories whose admission is closed, with how many closers hold
    /// each (core shutdowns in progress).
    closed: Vec<(PathBuf, usize)>,
}

static STATE: Mutex<State> = Mutex::new(State {
    open: Vec::new(),
    closed: Vec::new(),
});
static CLOSED: Condvar = Condvar::new();
static INIT: OnceLock<Result<(), InitError>> = OnceLock::new();
/// Process exit has begun: nothing is admitted any more. Never reset.
static EXITING: AtomicBool = AtomicBool::new(false);
/// Entries open in every directory (the sum of `State::open`), plus
/// reservations `enter` has not yet kept or given back. Changed only under
/// `STATE`, read without it by the exit hook.
static OPEN: AtomicUsize = AtomicUsize::new(0);

fn lock() -> MutexGuard<'static, State> {
    // The state is updated in single steps, so a panic elsewhere never
    // leaves it half-written; a poisoned lock is still consistent.
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Why SQLCipher could not be readied. Cache opens are refused after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    /// `sqlite3_initialize` failed with this code.
    Sqlite(i32),
    /// `atexit` could not register the exit hook.
    Atexit,
}

impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(rc) => write!(f, "sqlite3_initialize failed ({rc})"),
            Self::Atexit => f.write_str("could not register the exit hook"),
        }
    }
}

/// Why a census entry was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Process exit, or a shutdown of this directory, has begun.
    Closing,
    /// [`MAX_DIRS`] directories are open already.
    Full,
}

/// Initializes SQLite and SQLCipher, then registers the exit hook after
/// SQLCipher's own exit handler. Idempotent; concurrent callers wait for
/// the first. Core calls it at startup, on the thread that starts core,
/// before any cache can be opened; `Cache::open` calls it too.
///
/// `sqlite3_initialize` runs `sqlcipher_extra_init` (SQLCipher is built
/// with `SQLITE_EXTRA_INIT=sqlcipher_extra_init`), whose first run calls
/// `atexit(sqlcipher_atexit)` before anything else (SQLCipher 4.20.0,
/// `sqlcipher.c`). If SQLite was already initialized, that registration
/// happened then. Either way it precedes ours.
pub fn init() -> Result<(), InitError> {
    *INIT.get_or_init(init_once)
}

fn init_once() -> Result<(), InitError> {
    // SAFETY: `sqlite3_initialize` takes no arguments, is thread-safe and
    // idempotent, and is called before any SQLite configuration we rely on.
    let rc = unsafe { ffi::sqlite3_initialize() };
    if rc != ffi::SQLITE_OK {
        tracing::error!(rc, "SQLite failed to initialize; running without a cache");
        return Err(InitError::Sqlite(rc));
    }
    // The exit hook never takes `STATE`, but warm it anyway: std's mutex
    // may allocate its OS lock lazily on first use, and anything that
    // touches the census near exit should not be the first to do so.
    drop(lock());
    // SAFETY: `exit_hook` is an `extern "C" fn()` that never unwinds (its
    // body runs under `catch_unwind`), as `atexit` requires.
    let rc = unsafe { libc::atexit(exit_hook) };
    if rc != 0 {
        tracing::error!(
            rc,
            "could not register the cache exit hook; running without a cache"
        );
        return Err(InitError::Atexit);
    }
    Ok(())
}

/// One thread that may be inside SQLCipher for the cache at `dir`.
/// Dropped once that thread is done with the database.
#[derive(Debug)]
pub(crate) struct Open {
    dir: PathBuf,
}

impl Open {
    /// Admits one more thread to the cache at `dir`, unless admission is
    /// closed. Atomic with [`begin_exit`] (see the module docs) and with
    /// [`close_admission`] (same lock): after either, no entry is admitted.
    pub(crate) fn enter(dir: &Path) -> Result<Self, Refused> {
        let mut state = lock();
        // Reserve before checking (round 30): see the module docs.
        let reservation = Reservation::take();
        let closed = state.closed.iter().any(|(d, _)| d == dir);
        if EXITING.load(Ordering::SeqCst) || closed {
            return Err(Refused::Closing);
        }
        let full = state.open.len() >= MAX_DIRS;
        match state.open.iter_mut().find(|(d, _)| d == dir) {
            Some((_, n)) => *n = n.saturating_add(1),
            None if full => return Err(Refused::Full),
            None => state.open.push((dir.to_path_buf(), 1)),
        }
        reservation.keep();
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }
}

/// One count in [`OPEN`], taken before `enter` checks [`EXITING`]. Dropped
/// on every path that refuses (or unwinds), which gives the count back;
/// kept, it becomes the entry's, given back by `Open`'s drop.
struct Reservation(());

impl Reservation {
    fn take() -> Self {
        OPEN.fetch_add(1, Ordering::SeqCst);
        Self(())
    }

    fn keep(self) {
        std::mem::forget(self);
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        OPEN.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        let mut state = lock();
        if let Some(i) = state.open.iter().position(|(d, _)| *d == self.dir) {
            let left = state.open.get_mut(i).map_or(0, |(_, n)| {
                *n = n.saturating_sub(1);
                *n
            });
            if left == 0 {
                state.open.swap_remove(i);
            }
            OPEN.fetch_sub(1, Ordering::SeqCst);
        }
        drop(state);
        CLOSED.notify_all();
    }
}

/// Keeps `dir` in the census across a close followed by a reopen (logout,
/// re-binding), so a drain cannot finish in the gap between them. `None`
/// when admission is closed, in which case the reopen is refused too.
pub(crate) fn hold(dir: &Path) -> Option<Open> {
    Open::enter(dir).ok()
}

/// Admission to one directory is closed while this lives (core shutdown).
/// Entries already admitted are unaffected; drain them with
/// [`wait_closed`]. Dropping it reopens the directory once no other closer
/// holds it, so a later core can use the directory again.
#[derive(Debug)]
pub(crate) struct AdmissionClosed {
    dir: PathBuf,
}

/// Closes admission to the cache at `dir` (see [`AdmissionClosed`]).
pub(crate) fn close_admission(dir: &Path) -> AdmissionClosed {
    let mut state = lock();
    match state.closed.iter_mut().find(|(d, _)| d == dir) {
        Some((_, n)) => *n = n.saturating_add(1),
        None => state.closed.push((dir.to_path_buf(), 1)),
    }
    AdmissionClosed {
        dir: dir.to_path_buf(),
    }
}

impl Drop for AdmissionClosed {
    fn drop(&mut self) {
        let mut state = lock();
        if let Some(i) = state.closed.iter().position(|(d, _)| *d == self.dir) {
            let left = state.closed.get_mut(i).map_or(0, |(_, n)| {
                *n = n.saturating_sub(1);
                *n
            });
            if left == 0 {
                state.closed.swap_remove(i);
            }
        }
    }
}

/// Closes admission to every cache for the rest of the process: process
/// exit has begun. Open caches keep working until they close. Called by the
/// exit hook, and earlier by the app when it starts to quit. Never blocks
/// and never allocates: one atomic store (round 27).
pub fn begin_exit() {
    EXITING.store(true, Ordering::SeqCst);
}

fn any_open(state: &State, dir: Option<&Path>) -> bool {
    state
        .open
        .iter()
        .any(|(d, _)| dir.is_none_or(|dir| d == dir))
}

/// Waits on `state` until no cache at `dir` (every cache, for `None`) is
/// open, or `deadline` passes. Whether they all closed.
fn drain(mut state: MutexGuard<'static, State>, dir: Option<&Path>, deadline: Instant) -> bool {
    while any_open(&state, dir) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        state = CLOSED
            .wait_timeout(state, left)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
    true
}

/// Blocks until no cache at `dir` (every cache, for `None`) is open, or
/// `timeout` passes. Whether they all closed. Never call it on an async
/// runtime thread. Pair it with closed admission ([`close_admission`],
/// [`begin_exit`]) when nothing may open after it returns.
///
/// `timeout` bounds the waiting, not the call: the deadline is set before
/// the census lock is taken, but taking it, at the start and again after
/// each condvar wait, has no bound (a thread holding it stalls this one).
/// So never call it on a thread that must stay responsive: the UI thread,
/// a quit callback, or the exit path (round 30). Those use atomics only.
pub fn wait_closed(dir: Option<&Path>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    drain(lock(), dir, deadline)
}

/// Test support for the app's quit regression (round 30), fake builds
/// only: takes the census lock on a thread that then ends, so the lock
/// stays held for the rest of the process. Every later census call other
/// than [`begin_exit`] and the exit hook blocks forever.
#[cfg(feature = "fake")]
pub fn leak_lock_for_tests() {
    let leaker = std::thread::spawn(|| std::mem::forget(lock()));
    let _ = leaker.join();
}

/// Closes admission and polls, bounded by [`EXIT_WAIT`], until every open
/// cache has closed. Whether they all did.
///
/// It touches only the static atomics `EXITING` and `OPEN`, the monotonic
/// clock and `nanosleep`: no lock, no allocation, no logging. So nothing
/// another thread holds can delay it, and nothing it does can block. (A
/// condvar wait would re-acquire the lock after its timeout with no bound,
/// round 24; a log write can block on a full or locked stderr, round 24;
/// std's mutex may allocate on first use, round 27.) It returns no earlier
/// than the deadline, and later only by how far one `sleep(POLL)` and the
/// scheduler overshoot it.
///
/// It stores `EXITING` before its first load of `OPEN`, the order the
/// proof in the module docs needs. Once a load sees zero, the census stays
/// empty: any later `enter` reserves after that load, so its check comes
/// after the store and refuses (round 30).
fn drain_at_exit() -> bool {
    let deadline = Instant::now() + EXIT_WAIT;
    begin_exit();
    for _ in 0..MAX_POLLS {
        let open = OPEN.load(Ordering::SeqCst);
        if open == 0 {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
    false
}

/// Runs at process exit, before SQLCipher's own exit handler (see
/// [`init`]).
///
/// SAFETY contract (it is an `atexit` callback):
/// - It must never unwind: a panic out of an `extern "C" fn` aborts. The
///   body runs under `catch_unwind`. A caught panic counts as "not
///   drained", and its payload is forgotten, never dropped, because a
///   payload destructor could panic again outside the catch.
/// - It is bounded by [`EXIT_WAIT`] and cannot block: see
///   [`drain_at_exit`]. It neither locks nor allocates nor logs. Exit
///   status [`EXIT_CACHE_BUSY`] is the diagnostic.
/// - `exit` is called by the main thread (GPUI's terminate, or `main`
///   returning in tests), which never holds an entry: caches are opened and
///   closed on core's runtime, blocking and `abyss-cache` threads, and no
///   cache code calls `exit`. Were a thread holding an entry to call `exit`
///   anyway, the wait would time out on its own entry and end in `_exit`
///   below: a delay, not a crash or a hang.
/// - If anything is still open at the deadline, returning would run
///   SQLCipher's teardown under a live connection. So it ends the process
///   with `_exit(EXIT_CACHE_BUSY)` instead. That skips the remaining
///   `atexit` handlers and C stdio flushing (our logs go unbuffered to
///   stderr). SQLite durability is unaffected: committed transactions are
///   durable, and the WAL is recovered on the next open.
extern "C" fn exit_hook() {
    let outcome = std::panic::catch_unwind(drain_at_exit);
    match outcome {
        Ok(true) => {}
        Ok(false) => abandon_exit(),
        Err(payload) => {
            std::mem::forget(payload);
            abandon_exit();
        }
    }
}

fn abandon_exit() -> ! {
    // SAFETY: `_exit` takes a plain status and never returns; it skips
    // exit handlers by design (see `exit_hook`).
    unsafe { libc::_exit(EXIT_CACHE_BUSY) }
}

#[cfg(test)]
mod tests;
