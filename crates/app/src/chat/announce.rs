//! What the open chat tells a screen reader as messages arrive: one polite
//! announcement, "New message from Ana", at most once per [`MIN_GAP`]. The
//! rest of a burst is dropped, not queued: the list holds every message, and
//! a reader that fell behind would otherwise speak minutes of stale news.
//! Sender names are announced and never logged.

use std::time::{Duration, Instant};

use abyss_core::model::ThreadKey;
use gpui::SharedString;

/// The least time between two announcements.
pub(crate) const MIN_GAP: Duration = Duration::from_secs(2);

/// The announcement on offer to the live region.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Announcement {
    /// A new one for each announcement, so one that repeats the last is
    /// still a new node, and so announced.
    pub(crate) seq: u64,
    pub(crate) text: SharedString,
    thread: ThreadKey,
}

#[derive(Debug, Default)]
pub(crate) struct Announcer {
    last: Option<Instant>,
    seq: u64,
    current: Option<Announcement>,
}

impl Announcer {
    /// A message arrived in `thread` at `now`. Returns whether it is
    /// announced: not within [`MIN_GAP`] of the last announcement.
    pub(crate) fn offer(&mut self, thread: ThreadKey, text: SharedString, now: Instant) -> bool {
        if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < MIN_GAP)
        {
            return false;
        }
        self.last = Some(now);
        self.seq = self.seq.wrapping_add(1);
        self.current = Some(Announcement {
            seq: self.seq,
            text,
            thread,
        });
        true
    }

    /// What the live region holds while `thread` is open: an announcement
    /// about another thread is never read out in this one.
    pub(crate) fn current(&self, thread: ThreadKey) -> Option<&Announcement> {
        self.current.as_ref().filter(|a| a.thread == thread)
    }

    /// The open thread changed: nothing said before it is still news.
    pub(crate) fn clear(&mut self) {
        self.current = None;
    }
}

#[cfg(test)]
mod tests {
    use abyss_core::model::{ChatId, PeerKind};

    use super::*;

    fn thread(id: i64) -> ThreadKey {
        ThreadKey::whole(ChatId {
            kind: PeerKind::User,
            id,
        })
    }

    #[test]
    fn one_announcement_per_gap() {
        let mut a = Announcer::default();
        let t0 = Instant::now();
        assert!(a.offer(thread(1), "New message from Ana".into(), t0));
        for ms in [1, 500, 1_999] {
            let at = t0 + Duration::from_millis(ms);
            assert!(!a.offer(thread(1), "New message from Bo".into(), at));
        }
        assert_eq!(a.current(thread(1)).unwrap().text, "New message from Ana");
        assert!(a.offer(thread(1), "New message from Bo".into(), t0 + MIN_GAP));
        assert_eq!(a.current(thread(1)).unwrap().text, "New message from Bo");
    }

    #[test]
    fn a_repeat_is_a_new_announcement() {
        let mut a = Announcer::default();
        let t0 = Instant::now();
        a.offer(thread(1), "New message from Ana".into(), t0);
        let first = a.current(thread(1)).unwrap().seq;
        a.offer(thread(1), "New message from Ana".into(), t0 + MIN_GAP);
        assert_ne!(a.current(thread(1)).unwrap().seq, first);
    }

    #[test]
    fn it_belongs_to_its_thread_and_leaves_with_it() {
        let mut a = Announcer::default();
        a.offer(thread(1), "New message from Ana".into(), Instant::now());
        assert!(a.current(thread(2)).is_none());
        a.clear();
        assert!(a.current(thread(1)).is_none());
    }
}
