//! Account-wide rate limits for automatic fetches (plan sections 6.4, 7):
//! flood-wait deadlines and the automatic first-page refresh limiter.

use std::time::{Duration, Instant};

/// Least time between two automatic first-page topics fetches, account-wide.
pub const AUTO_REFRESH_COOLDOWN: Duration = Duration::from_secs(30);

/// A flood-wait deadline (`topics_not_before`, `peer_not_before`). It only
/// ever extends, and survives navigation, backoff resets and rebuilds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NotBefore(Option<Instant>);

impl NotBefore {
    /// Moves the deadline to `until` if that is later.
    pub fn extend(&mut self, until: Instant) {
        self.0 = Some(self.0.map_or(until, |d| d.max(until)));
    }

    pub fn allows(self, now: Instant) -> bool {
        self.0.is_none_or(|d| now >= d)
    }

    /// The time left before the deadline, if it is in the future.
    pub fn remaining(self, now: Instant) -> Option<Duration> {
        self.0.filter(|d| *d > now).map(|d| d - now)
    }

    pub fn deadline(self) -> Option<Instant> {
        self.0
    }
}

/// Whether to run an automatic refresh now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refresh {
    Now,
    /// Inside the cooldown: one coalesced trailing request runs when it ends.
    Later,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Trailing {
    #[default]
    None,
    Pending,
}

/// At most one automatic first-page topics fetch per
/// [`AUTO_REFRESH_COOLDOWN`] (overflow, `TopicsPinned`, a page order change),
/// plus one coalesced trailing request run when the cooldown ends, for
/// whichever forum is shown then. A flood wait extends the cooldown.
/// Requests are only made for the shown forum; other lists just keep
/// `needs_first_page` for their next open, which isn't limited.
#[derive(Debug, Default)]
pub struct AutoRefresh {
    cooldown: NotBefore,
    trailing: Trailing,
}

impl AutoRefresh {
    pub fn request(&mut self, now: Instant) -> Refresh {
        if self.cooldown.allows(now) {
            self.cooldown.extend(now + AUTO_REFRESH_COOLDOWN);
            self.trailing = Trailing::None;
            Refresh::Now
        } else {
            self.trailing = Trailing::Pending;
            Refresh::Later
        }
    }

    /// Runs the trailing request once the cooldown has ended.
    pub fn poll(&mut self, now: Instant) -> Refresh {
        if self.trailing == Trailing::Pending && self.cooldown.allows(now) {
            self.request(now)
        } else {
            Refresh::Later
        }
    }

    /// Drops the trailing request (no forum is shown any more).
    pub fn cancel_trailing(&mut self) {
        self.trailing = Trailing::None;
    }

    /// A flood wait on a topics call extends the cooldown.
    pub fn flood(&mut self, until: Instant) {
        self.cooldown.extend(until);
    }

    /// When a trailing request becomes due, if one is pending.
    pub fn trailing_at(&self) -> Option<Instant> {
        match self.trailing {
            Trailing::Pending => self.cooldown.deadline(),
            Trailing::None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn deadlines_only_extend() {
        let t0 = Instant::now();
        let mut d = NotBefore::default();
        assert!(d.allows(t0));
        d.extend(t0 + Duration::from_secs(60));
        d.extend(t0 + Duration::from_secs(5));
        assert_eq!(d.deadline(), Some(t0 + Duration::from_secs(60)));
        assert!(!d.allows(t0 + Duration::from_secs(59)));
        assert_eq!(d.remaining(t0), Some(Duration::from_secs(60)));
        assert!(d.allows(t0 + Duration::from_secs(60)));
    }

    #[test]
    fn a_flood_wait_extends_the_cooldown() {
        let t0 = Instant::now();
        let mut limiter = AutoRefresh::default();
        assert_eq!(limiter.request(t0), Refresh::Now);
        limiter.flood(t0 + Duration::from_secs(90));
        assert_eq!(
            limiter.request(t0 + Duration::from_secs(31)),
            Refresh::Later
        );
        assert_eq!(limiter.poll(t0 + Duration::from_secs(89)), Refresh::Later);
        assert_eq!(limiter.trailing_at(), Some(t0 + Duration::from_secs(90)));
        assert_eq!(limiter.poll(t0 + Duration::from_secs(90)), Refresh::Now);
        assert_eq!(
            limiter.poll(t0 + Duration::from_secs(200)),
            Refresh::Later,
            "one trailing only"
        );
    }

    proptest! {
        /// Repeated overflow and pin triggers from several lists, polled
        /// between completed calls: at most one fetch per cooldown, and any
        /// request inside a cooldown is followed by exactly one trailing run.
        #[test]
        fn at_most_one_fetch_per_cooldown(
            steps in proptest::collection::vec((0u64..20_000, any::<bool>()), 1..200),
        ) {
            let t0 = Instant::now();
            let mut limiter = AutoRefresh::default();
            let mut runs: Vec<Instant> = Vec::new();
            let mut at = t0;
            let mut deferred = false;
            for (gap_ms, is_request) in steps {
                at += Duration::from_millis(gap_ms);
                let decision = if is_request { limiter.request(at) } else { limiter.poll(at) };
                match decision {
                    Refresh::Now => {
                        runs.push(at);
                        deferred = false;
                    }
                    Refresh::Later => deferred |= is_request,
                }
            }
            for pair in runs.windows(2) {
                prop_assert!(pair[1] - pair[0] >= AUTO_REFRESH_COOLDOWN);
            }
            if deferred {
                let end = at + AUTO_REFRESH_COOLDOWN;
                prop_assert_eq!(limiter.poll(end), Refresh::Now, "the trailing request runs");
                prop_assert_eq!(limiter.poll(end + AUTO_REFRESH_COOLDOWN), Refresh::Later);
            }
        }
    }
}
