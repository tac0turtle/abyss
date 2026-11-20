//! Bounded automatic retries. Something the UI still wants from core (a
//! focus change, a dialogs page, history, a frame's read mark) can fail: the
//! command queue is full, or core reports the load failed. Retrying on the
//! next redraw would loop at frame rate (a notice redraws), and retrying only
//! on the next redraw can stall forever (nothing redraws). So each such
//! intent keeps a [`Backoff`]: the next attempt waits (doubling from
//! [`FIRST_DELAY`] up to [`MAX_DELAY`]), the shell wakes itself once when it
//! is due (`shell::chats`), and after [`MAX_FAILURES`] in a row it waits for
//! a fresh trigger instead (a new core event or user action resets it).

use std::time::{Duration, Instant};

/// Wait after the first failure.
pub(crate) const FIRST_DELAY: Duration = Duration::from_millis(500);
/// Longest wait between attempts.
pub(crate) const MAX_DELAY: Duration = Duration::from_secs(8);
/// Failures in a row after which automatic retries stop.
pub(crate) const MAX_FAILURES: u8 = 5;
/// Shortest time between two automatic restarts of retries that gave up
/// (window activation; task 10e). Activation can toggle at the user's pace
/// (or an app switcher's), so without it every toggle would buy
/// [`MAX_FAILURES`] more attempts. At this gap a full queue sees at most
/// `MAX_FAILURES` attempts per 30 s per retry, while a user returning to the
/// window still gets an automatic recovery. The user's Retry is not limited.
pub(crate) const AUTO_RESTART_GAP: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Backoff {
    /// Failures since the last success or reset.
    failures: u8,
    /// No attempt before this.
    until: Option<Instant>,
}

impl Backoff {
    /// An attempt failed at `now`.
    pub(crate) fn failed(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1).min(MAX_FAILURES);
        let shift = u32::from(self.failures.saturating_sub(1));
        let delay = FIRST_DELAY
            .checked_mul(1 << shift.min(8))
            .map_or(MAX_DELAY, |d| d.min(MAX_DELAY));
        self.until = now.checked_add(delay);
    }

    /// An attempt failed at `now`, and the intent must never give up (a
    /// withdrawal that core relies on, review A76): the delay caps at
    /// [`MAX_DELAY`], which bounds the rate, and the count stays below
    /// [`MAX_FAILURES`], so it stays ready once the delay passed.
    pub(crate) fn failed_persistent(&mut self, now: Instant) {
        self.failed(now);
        if self.gave_up() {
            self.failures = MAX_FAILURES - 1;
        }
    }

    /// An attempt succeeded, or a fresh trigger allows trying again at once.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Automatic recovery: retries that gave up start over. One that still
    /// waits keeps its deadline, so a recovery never brings an attempt
    /// forward.
    pub(crate) fn restart_if_exhausted(&mut self) {
        if self.gave_up() {
            self.reset();
        }
    }

    /// Whether automatic retries stopped (until a reset).
    pub(crate) fn gave_up(&self) -> bool {
        self.failures >= MAX_FAILURES
    }

    /// Whether an attempt may be made at `now`.
    pub(crate) fn ready(&self, now: Instant) -> bool {
        !self.gave_up() && self.until.is_none_or(|u| now >= u)
    }

    /// When a waiting retry becomes due, if one waits past `now`.
    pub(crate) fn due(&self, now: Instant) -> Option<Instant> {
        self.until.filter(|u| *u > now && !self.gave_up())
    }
}

/// The earlier of two optional instants.
pub(crate) fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_caps_and_gives_up() {
        let now = Instant::now();
        let mut b = Backoff::default();
        assert!(b.ready(now) && b.due(now).is_none());
        let mut waits = Vec::new();
        while !b.gave_up() {
            b.failed(now);
            waits.push(b.until.unwrap() - now);
        }
        assert_eq!(
            waits,
            [500, 1_000, 2_000, 4_000, 8_000].map(Duration::from_millis)
        );
        assert!(!b.ready(now + MAX_DELAY), "stopped until a reset");
        assert_eq!(b.due(now), None, "no wake-up once given up");
        b.reset();
        assert!(b.ready(now));
        b.failed(now);
        assert!(!b.ready(now));
        assert_eq!(b.due(now), Some(now + FIRST_DELAY));
        assert!(b.ready(now + FIRST_DELAY));
        assert_eq!(b.due(now + FIRST_DELAY), None, "due: nothing waits");
    }

    /// A withdrawal never gives up, and its rate stays bounded (review A76).
    #[test]
    fn persistent_failures_cap_the_delay_and_never_give_up() {
        let mut now = Instant::now();
        let mut b = Backoff::default();
        let mut waits = Vec::new();
        for _ in 0..8 {
            b.failed_persistent(now);
            assert!(!b.gave_up());
            let until = b.until.unwrap();
            waits.push(until - now);
            assert!(!b.ready(until.checked_sub(Duration::from_millis(1)).unwrap()));
            now = until;
            assert!(b.ready(now), "ready again once the delay passed");
        }
        assert_eq!(
            waits,
            [500, 1_000, 2_000, 4_000, 8_000, 8_000, 8_000, 8_000].map(Duration::from_millis)
        );
    }
}
