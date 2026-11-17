//! A wall-clock watchdog for the harness waits (review round 32).
//!
//! A wait used to check wall time between slices of the test's clock. While
//! the store's [`ClockGate`] holds a paused clock still, no slice ends, so a
//! store operation that never answered hung the run. The watchdog runs on
//! its own OS thread: past its bound it abandons the gate (the clock moves
//! again) and wakes the wait, which fails the test.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::secret::ClockGate;

#[derive(Debug, Default)]
struct State {
    /// When the armed wait fails (`None`: no wait armed).
    deadline: Option<Instant>,
    stop: bool,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    fired: CancellationToken,
    gate: ClockGate,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The watchdog thread: waits for a deadline to pass while armed.
    fn watch(&self) {
        let mut s = self.lock();
        while !s.stop && !self.fired.is_cancelled() {
            let Some(deadline) = s.deadline else {
                s = self.changed.wait(s).unwrap_or_else(PoisonError::into_inner);
                continue;
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                self.gate.abandon();
                self.fired.cancel();
                return;
            }
            s = self
                .changed
                .wait_timeout(s, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// See the module docs. One per harness; dropping it stops its thread.
#[derive(Debug)]
pub(super) struct Watchdog {
    shared: Arc<Shared>,
    bound: Duration,
    thread: Option<JoinHandle<()>>,
}

/// An armed wait; dropping it disarms the watchdog.
pub(super) struct Armed(Arc<Shared>);

impl Drop for Armed {
    fn drop(&mut self) {
        self.0.lock().deadline = None;
        self.0.changed.notify_all();
    }
}

impl Watchdog {
    pub(super) fn new(gate: ClockGate, bound: Duration) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::default(),
            changed: Condvar::new(),
            fired: CancellationToken::new(),
            gate,
        });
        let watcher = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("harness-watchdog".into())
            .spawn(move || watcher.watch())
            .ok();
        assert!(thread.is_some(), "no watchdog thread");
        Self {
            shared,
            bound,
            thread,
        }
    }

    /// Arms the watchdog for one wait of at most the bound.
    pub(super) fn arm(&self) -> Armed {
        self.shared.lock().deadline = Some(Instant::now() + self.bound);
        self.shared.changed.notify_all();
        Armed(Arc::clone(&self.shared))
    }

    /// Fires once an armed wait outlives its bound.
    pub(super) fn fired(&self) -> CancellationToken {
        self.shared.fired.clone()
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.shared.lock().stop = true;
        self.shared.changed.notify_all();
        if let Some(thread) = self.thread.take() {
            // The thread only waits on the condvar, so it ends promptly.
            drop(thread.join());
        }
    }
}
