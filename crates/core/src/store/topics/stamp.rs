//! The list's logical clock and the per-group merge rule (plan section 6.4).

/// A topic list's logical clock. Every topics fetch (page or by-id) takes the
/// next value when it is *dispatched*, so results are ordered by dispatch,
/// whatever order they complete in. A value is never reused within a list:
/// 64 bits can't be exhausted (by-id calls are at least a second apart).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FetchSeq(u64);

impl FetchSeq {
    pub(super) fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// What a snapshot dispatched at some seq may do to one field group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Dispatched after everything the client knows about the group: authoritative.
    Apply,
    /// A snapshot dispatched at or after this one already set the group.
    Stale,
    /// A live change or trigger happened after its dispatch: keep the live
    /// state and refetch.
    Conflict,
}

/// One field group's stamps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Stamp {
    /// The dispatch seq of the snapshot that last set the group.
    seen: FetchSeq,
    /// The list's seq at the group's last live change or trigger.
    changed: FetchSeq,
}

impl Stamp {
    /// A group just set by a snapshot dispatched at `seq`.
    pub(super) fn applied(seq: FetchSeq) -> Self {
        Self {
            seen: seq,
            changed: FetchSeq::default(),
        }
    }

    /// Apply iff `seq > max(seen, changed)`.
    pub(super) fn judge(self, seq: FetchSeq) -> Verdict {
        if seq <= self.seen {
            Verdict::Stale
        } else if seq <= self.changed {
            Verdict::Conflict
        } else {
            Verdict::Apply
        }
    }

    pub(super) fn set_seen(&mut self, seq: FetchSeq) {
        self.seen = self.seen.max(seq);
    }

    /// The last applied snapshot was dispatched after the group's last live
    /// change: nothing about it is waiting for a refetch.
    pub(super) fn settled(self) -> bool {
        self.seen > self.changed
    }

    /// Records a live change or trigger while the list's clock reads `now`.
    pub(super) fn set_changed(&mut self, now: FetchSeq) {
        self.changed = self.changed.max(now);
    }

    #[cfg(test)]
    pub(super) fn parts(self) -> (FetchSeq, FetchSeq) {
        (self.seen, self.changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts_follow_the_rule() {
        let mut s = Stamp::applied(FetchSeq(3));
        assert_eq!(s.judge(FetchSeq(3)), Verdict::Stale);
        assert_eq!(s.judge(FetchSeq(4)), Verdict::Apply);
        assert!(s.settled());
        s.set_changed(FetchSeq(5));
        assert!(!s.settled());
        assert_eq!(s.judge(FetchSeq(2)), Verdict::Stale);
        assert_eq!(s.judge(FetchSeq(5)), Verdict::Conflict);
        assert_eq!(s.judge(FetchSeq(6)), Verdict::Apply);
        // Stamps only move forward.
        s.set_changed(FetchSeq(1));
        s.set_seen(FetchSeq(1));
        assert_eq!(s.parts(), (FetchSeq(3), FetchSeq(5)));
    }
}
