//! Keeps a paused test clock still while the persistence thread works
//! (review round 31, tests only).
//!
//! The persistence worker is an OS thread, which a paused tokio clock cannot
//! see: an actor waiting on it looks idle, and the runtime auto-advances to
//! the next timer, so hundreds of virtual seconds (a call deadline, the
//! shutdown deadline, a test's wait budget) can pass while the store answers
//! in a few milliseconds. While a request is awaited ([`ClockGate::busy`]) a
//! blocking task runs, and tokio does not auto-advance while one does. A test
//! hold ([`super::FaultControl::hold`]) is meant to let time pass, so it
//! lifts the gate until [`super::FaultControl::release`], or until it
//! expires on its own (then the gate holds the clock again, review round
//! 32). A test's wall-clock watchdog [`ClockGate::abandon`]s the gate so a
//! stuck test fails instead of hanging.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use tokio::runtime::Handle;

#[derive(Debug, Default)]
struct State {
    /// Requests awaited on the store thread.
    busy: u32,
    /// Store operations blocked on a test hold.
    held: u32,
    /// Bumped by a release: holds that began before it are over.
    epoch: u64,
    /// The blocking task that keeps the clock still is running.
    waiter: bool,
    /// A watchdog gave up on the test: the clock is never held again.
    abandoned: bool,
    /// The runtime whose clock is held, from the last request: a hold
    /// that expires on the store thread has no current runtime.
    runtime: Option<Handle>,
}

#[derive(Debug, Default)]
struct Inner {
    state: Mutex<State>,
    changed: Condvar,
}

/// See the module docs. Clones share the gate.
#[derive(Clone, Debug, Default)]
pub struct ClockGate(Arc<Inner>);

/// A request awaited on the store thread; dropping it ends it.
#[derive(Debug)]
pub struct Busy(ClockGate);

impl Drop for Busy {
    fn drop(&mut self) {
        let mut s = self.0.lock();
        s.busy = s.busy.saturating_sub(1);
        drop(s);
        self.0.0.changed.notify_all();
    }
}

impl ClockGate {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A request is about to be awaited (async code, on the runtime).
    pub fn busy(&self) -> Busy {
        let mut s = self.lock();
        s.busy += 1;
        if let Ok(runtime) = Handle::try_current() {
            s.runtime = Some(runtime);
        }
        drop(s);
        self.ensure_waiter();
        Busy(self.clone())
    }

    /// Starts the blocking task that holds the clock, unless one runs, no
    /// request is awaited, or a test hold lets time pass. Runs on the
    /// runtime of the last request, from any thread.
    fn ensure_waiter(&self) {
        let mut s = self.lock();
        if s.waiter || s.busy == 0 || s.held > 0 || s.abandoned {
            return;
        }
        let Some(runtime) = s.runtime.clone() else {
            return;
        };
        s.waiter = true;
        drop(s);
        let gate = self.clone();
        drop(runtime.spawn_blocking(move || gate.hold_clock()));
    }

    fn hold_clock(&self) {
        let mut s = self.lock();
        while s.busy > 0 && s.held == 0 && !s.abandoned {
            s = self
                .0
                .changed
                .wait(s)
                .unwrap_or_else(PoisonError::into_inner);
        }
        s.waiter = false;
    }

    /// A store operation blocks on a test hold (store thread): time may
    /// pass. Returns the epoch for [`Self::hold_ended`].
    pub(super) fn hold_began(&self) -> u64 {
        let mut s = self.lock();
        s.held += 1;
        let epoch = s.epoch;
        drop(s);
        self.0.changed.notify_all();
        epoch
    }

    /// A hold ended (store thread): unless a release already counted it,
    /// its bound passed, and the clock is held again while requests are
    /// awaited.
    pub(super) fn hold_ended(&self, epoch: u64) {
        let mut s = self.lock();
        if s.epoch != epoch {
            return;
        }
        s.held = s.held.saturating_sub(1);
        drop(s);
        self.ensure_waiter();
    }

    /// Stops holding the clock for good (any thread): a test watchdog
    /// fired, and the test must reach its failure rather than wait on the
    /// store.
    pub fn abandon(&self) {
        self.lock().abandoned = true;
        self.0.changed.notify_all();
    }

    /// Whether the blocking task that holds the clock runs.
    #[cfg(test)]
    fn holds_clock(&self) -> bool {
        self.lock().waiter
    }

    /// The test released every hold (async code): the clock is held again
    /// while the released operations finish.
    pub(super) fn released(&self) {
        let mut s = self.lock();
        s.held = 0;
        s.epoch = s.epoch.wrapping_add(1);
        drop(s);
        self.ensure_waiter();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::ClockGate;

    /// Whether the clock-holding task should run.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Holder {
        Running,
        Gone,
    }

    /// Waits (wall time, bounded) until the clock-holding task is `want`.
    fn await_holder(gate: &ClockGate, want: Holder) {
        let start = Instant::now();
        let now = || {
            if gate.holds_clock() {
                Holder::Running
            } else {
                Holder::Gone
            }
        };
        while now() != want {
            assert!(start.elapsed() < Duration::from_secs(10), "holder stuck");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// An abandoned gate lets the clock go and never holds it again.
    #[tokio::test(start_paused = true)]
    async fn an_abandoned_gate_never_holds_the_clock() {
        let gate = ClockGate::default();
        let busy = gate.busy();
        await_holder(&gate, Holder::Running);
        gate.abandon();
        await_holder(&gate, Holder::Gone);
        let again = gate.busy();
        assert!(!gate.holds_clock());
        tokio::time::sleep(Duration::from_secs(600)).await;
        drop((busy, again));
    }
}
