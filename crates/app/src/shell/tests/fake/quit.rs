//! Round 30: the quit callback only signals. Run in a child process (this
//! test binary, re-run on `child`), since `begin_exit` is one-way and the
//! scenarios wedge the census for the rest of the process.
//!
//! In each scenario the fake cleanup worker can never finish: the census
//! lock is held forever, or the cache stays open and stderr (a pipe nobody
//! reads) is full, so the worker's warning blocks. Before round 30 the
//! callback ran that cleanup itself and never returned.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use abyss_core::cache::{Cache, CacheKey};
use futures::FutureExt;

const CHILD: &str = "ABYSS_QUIT_CHILD";
/// How long a child may run. A regression never ends.
const WATCHDOG: Duration = Duration::from_secs(30);
/// What "promptly" means, generous for a loaded machine: the callback
/// does a few atomic operations.
const PROMPT: Duration = Duration::from_secs(2);
/// Child status: the callback returned promptly and its cleanup future was
/// still pending (the worker, not the callback, is stuck).
const RETURNED: i32 = 0;
/// Child status: the callback took longer than `PROMPT`.
const SLOW: i32 = 3;
/// Child status: the cleanup future was already done.
const NOT_DEFERRED: i32 = 4;

/// Fills stderr until a write would block, then makes it blocking again:
/// from now on every stderr write blocks forever.
fn fill_stderr() {
    // SAFETY: reads fd 2's status flags.
    let flags = unsafe { libc::fcntl(2, libc::F_GETFL) };
    assert!(flags >= 0);
    // SAFETY: sets fd 2 non-blocking, so the fill below stops when full.
    assert!(unsafe { libc::fcntl(2, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0);
    let junk = [b'x'; 4096];
    let mut full = false;
    for _ in 0..100_000 {
        // SAFETY: `junk` is a live buffer of `junk.len()` bytes.
        let n = unsafe { libc::write(2, junk.as_ptr().cast(), junk.len()) };
        if n < 0 {
            full = true;
            break;
        }
    }
    // SAFETY: restores fd 2's original (blocking) flags.
    assert!(unsafe { libc::fcntl(2, libc::F_SETFL, flags) } >= 0);
    assert!(full, "stderr is a pipe that filled up");
}

/// The child side. Does nothing unless run by `run`.
#[test]
fn child() {
    let Some(scenario) = std::env::var_os(CHILD) else {
        return;
    };
    abyss_core::cache::init().unwrap();
    let run = tempfile::tempdir().unwrap().keep();
    let cache = run.join("cache");
    let key = CacheKey::generate().unwrap();
    let open = Cache::open(&cache, &key).unwrap();
    crate::storage::start_fake_cleanup(run.clone(), cache);
    match scenario.to_str().unwrap() {
        "clean" => return clean_child(open, &run),
        "ack" => return ack_child(open, &run),
        _ => {}
    }
    let subscriber = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();
    match scenario.to_str().unwrap() {
        "locked" => abyss_core::cache::leak_lock_for_tests(),
        "stderr" => fill_stderr(),
        other => panic!("unknown scenario {other}"),
    }
    let start = Instant::now();
    let cleanup = crate::shell::quit_signals(&|| None);
    let took = start.elapsed();
    let status = if took > PROMPT {
        SLOW
    } else if cleanup.now_or_never().is_some() {
        NOT_DEFERRED
    } else {
        RETURNED
    };
    // The cache is still open: skip the exit hook's wait for it.
    std::mem::forget(open);
    // SAFETY: ends the child at once with `status`.
    unsafe { libc::_exit(status) }
}

/// The cleanup still works, off the quit thread: with the cache closed,
/// awaiting the returned future sees the run directory removed.
fn clean_child(open: Cache, run: &std::path::Path) {
    drop(open);
    futures::executor::block_on(crate::shell::quit_signals(&|| None));
    assert!(!run.exists(), "the run directory was removed");
}

/// The cleanup waits for core's ack when this quit is the first request, as an
/// OS-initiated quit is (N48): core gets GPUI's quit budget to save, not just
/// a cancel.
fn ack_child(open: Cache, run: &std::path::Path) {
    use std::future::Future;
    use std::task::Context;

    drop(open);
    let (done, ack) = futures::channel::oneshot::channel();
    let ack: crate::shell::bridge::ShutdownAck = Box::pin(async move { ack.await.ok() });
    let ack = std::cell::Cell::new(Some(ack));
    let mut quit = Box::pin(crate::shell::quit_signals(&|| ack.take()));
    // Long enough for the cleanup worker to have finished: only core's
    // ack can still be holding the future.
    std::thread::sleep(Duration::from_millis(300));
    let mut cx = Context::from_waker(futures::task::noop_waker_ref());
    assert!(quit.as_mut().poll(&mut cx).is_pending(), "waits for core");
    done.send(abyss_core::ShutdownReport::UNCONFIRMED).unwrap();
    futures::executor::block_on(quit);
    assert!(!run.exists(), "the run directory was removed after the ack");
}

/// Runs `child` for `scenario`; its exit status, or `None` if the watchdog
/// killed it. Its stderr is a pipe nobody reads until it has ended.
fn run(scenario: &str) -> Option<i32> {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "shell::tests::fake::quit::child",
            "--test-threads=1",
        ])
        .env(CHILD, scenario)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let code = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code();
        }
        if start.elapsed() > WATCHDOG {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut stderr = Vec::new();
    let _ = child.stderr.take().unwrap().read_to_end(&mut stderr);
    code
}

#[test]
fn quit_cleanup_still_removes_the_fake_run() {
    assert_eq!(run("clean"), Some(0));
}

#[test]
fn quit_waits_for_cores_ack_before_the_cleanup() {
    assert_eq!(run("ack"), Some(0));
}

#[test]
fn quit_returns_while_the_census_lock_is_held() {
    assert_eq!(run("locked"), Some(RETURNED));
}

#[test]
fn quit_returns_while_stderr_is_blocked() {
    assert_eq!(run("stderr"), Some(RETURNED));
}
