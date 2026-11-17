//! The kind-check queue and the fence (plan section 6.6): one deduplicated
//! FIFO of chats whose forum flag is being re-checked through
//! `peer_dialogs`, each fenced until its check settles.
//!
//! - **Bounds.** A chat is fenced only while it is in the required-peer set
//!   (the caller retires the rest, [`KindChecks::retire`]), so the FIFO, the
//!   in-flight batch and every per-chat mark (recheck, failures, suspect)
//!   are bounded by `MAX_PINNED_PEERS`.
//! - **Budgets.** Each fence has a recheck budget of one and a cumulative
//!   failure budget of three: the third failure abandons it. Attempts
//!   discarded by a rebuild or a flood wait consume neither. So a fence
//!   ends after at most four completed attempts.
//! - **Rechecks.** A trigger while the chat's check is in flight marks a
//!   recheck: the result may predate it, so the chat is queued once more.
//!   A trigger during that recheck marks the chat suspect instead, for the
//!   next light refresh to re-check.

use std::collections::{HashMap, HashSet, VecDeque};

use super::{BadgeSeq, ForumStatus};
use crate::model::{ChatId, ChatKind};

/// Failures that abandon a fence (the third).
pub const FENCE_FAILURES: u8 = 3;

/// One fenced chat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fence {
    /// What core knew about the chat when the fence started.
    before: ForumStatus,
    /// The badge clock at the last trigger: an authoritative summary
    /// dispatched after it settles a queued fence.
    triggered: BadgeSeq,
    failures: u8,
    recheck: Recheck,
    /// Its check is in the in-flight batch.
    in_flight: bool,
    /// The newest authoritative statement of its kind seen while fenced
    /// (a check's result, a dialogs page, a `peer_dialogs` badge slot), by
    /// dispatch stamp. Kept even while it can't settle the fence yet.
    stated: Option<(BadgeSeq, ChatKind)>,
}

impl Fence {
    /// Keeps `kind`, stated by a source dispatched at `stamp`, if it is the
    /// newest statement so far.
    fn state(&mut self, stamp: BadgeSeq, kind: ChatKind) {
        if self.stated.is_none_or(|(seen, _)| stamp > seen) {
            self.stated = Some((stamp, kind));
        }
    }

    /// The kept statement, if it was dispatched after the last trigger: it
    /// answers the check.
    fn answer(&self) -> Option<(BadgeSeq, ChatKind)> {
        self.stated.filter(|(stamp, _)| *stamp > self.triggered)
    }
}

/// The fence's recheck budget (one per fence).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recheck {
    Unused,
    /// A trigger crossed the in-flight check: check once more.
    Pending,
    Used,
}

/// What a trigger did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fenced {
    /// A new fence: the caller revokes the chat's continuity.
    New,
    /// Already fenced; the check queued or in flight covers it.
    Already,
    /// The check in flight may predate the trigger: it is checked again.
    Recheck,
    /// The recheck budget is spent: the chat is marked suspect.
    Suspect,
}

/// How a fence ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settled {
    /// The check (or another authoritative summary) stated the kind, which
    /// the caller commits only now: while the fence stood, the chat kept
    /// what core knew before it.
    Kind {
        before: ForumStatus,
        after: ChatKind,
        /// The dispatch stamp of the statement: the commit's.
        at: BadgeSeq,
    },
    /// The third failure: the old kind is kept and the chat is suspect.
    Abandoned { before: ForumStatus },
}

/// What a check's result did to its fence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Queued again (a recheck, or a failure within the budget).
    Requeued,
    Settled(Settled),
}

/// The checks in the call reserved or in flight: its badge stamp once it is
/// admitted, and its chats, each live or retired (left the required-peer
/// set; its result is ignored).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Flight {
    stamp: Option<BadgeSeq>,
    chats: Vec<(ChatId, Slot)>,
}

/// The checks of a completed call.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Completed {
    /// Checks whose results count.
    pub live: Vec<ChatId>,
    /// Checks of chats that left the required-peer set meanwhile: nothing
    /// in the result about them may be applied.
    pub retired: Vec<ChatId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Live,
    Retired,
}

/// The kind-check FIFO, its fences and the suspect marks.
#[derive(Debug, Default)]
pub struct KindChecks {
    fifo: VecDeque<ChatId>,
    fences: HashMap<ChatId, Fence>,
    flight: Option<Flight>,
    /// Abandoned (or re-triggered past the budget): re-checked by the next
    /// light refresh. Topic-headed messages of a chat thought not to be a
    /// forum don't fence it again meanwhile.
    suspects: HashSet<ChatId>,
}

impl KindChecks {
    /// A trigger for `chat` (a known chat), at badge clock `now`. `before`
    /// is what core knows about it, kept for the comparison at the end.
    pub fn fence(&mut self, chat: ChatId, before: ForumStatus, now: BadgeSeq) -> Fenced {
        let Some(fence) = self.fences.get_mut(&chat) else {
            self.suspects.remove(&chat);
            self.fences.insert(
                chat,
                Fence {
                    before,
                    triggered: now,
                    failures: 0,
                    recheck: Recheck::Unused,
                    in_flight: false,
                    stated: None,
                },
            );
            self.fifo.push_back(chat);
            return Fenced::New;
        };
        fence.triggered = now;
        if !fence.in_flight {
            return Fenced::Already;
        }
        match fence.recheck {
            Recheck::Unused => {
                fence.recheck = Recheck::Pending;
                Fenced::Recheck
            }
            Recheck::Pending => Fenced::Already,
            Recheck::Used => {
                self.suspects.insert(chat);
                Fenced::Suspect
            }
        }
    }

    pub fn is_fenced(&self, chat: ChatId) -> bool {
        self.fences.contains_key(&chat)
    }

    pub fn is_suspect(&self, chat: ChatId) -> bool {
        self.suspects.contains(&chat)
    }

    /// Fenced chats (queued or in flight).
    pub fn len(&self) -> usize {
        self.fences.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fences.is_empty()
    }

    /// Fenced chats, in no particular order.
    pub fn fenced(&self) -> impl Iterator<Item = ChatId> + '_ {
        self.fences.keys().copied()
    }

    /// The first `n` queued chats, head first.
    pub fn head(&self, n: usize) -> Vec<ChatId> {
        self.fifo.iter().take(n).copied().collect()
    }

    pub fn queued(&self) -> usize {
        self.fifo.len()
    }

    /// What core knew about a fenced chat when its fence started.
    pub fn before(&self, chat: ChatId) -> Option<ForumStatus> {
        self.fences.get(&chat).map(|f| f.before)
    }

    /// The call being reserved takes the first `n` queued chats.
    pub fn dispatch(&mut self, n: usize) {
        debug_assert!(self.flight.is_none(), "one peer_dialogs call at a time");
        let n = n.min(self.fifo.len());
        let chats: Vec<(ChatId, Slot)> = self.fifo.drain(..n).map(|c| (c, Slot::Live)).collect();
        for (chat, _) in &chats {
            if let Some(fence) = self.fences.get_mut(chat) {
                fence.in_flight = true;
            }
        }
        self.flight = Some(Flight { stamp: None, chats });
    }

    /// The reserved call was admitted and stamped `stamp`.
    pub fn stamp_flight(&mut self, stamp: BadgeSeq) {
        if let Some(flight) = self.flight.as_mut() {
            flight.stamp = Some(stamp);
        }
    }

    /// The call stamped `stamp` completed: its live checks, whose results
    /// the caller reports one by one ([`Self::succeeded`],
    /// [`Self::failed`]), and its retired ones. Empty for another call.
    pub fn complete(&mut self, stamp: BadgeSeq) -> Completed {
        let Some(flight) = self.flight.take_if(|f| f.stamp == Some(stamp)) else {
            return Completed::default();
        };
        let (live, retired): (Vec<_>, Vec<_>) = flight
            .chats
            .into_iter()
            .partition(|(_, slot)| *slot == Slot::Live);
        Completed {
            live: live.into_iter().map(|(c, _)| c).collect(),
            retired: retired.into_iter().map(|(c, _)| c).collect(),
        }
    }

    /// The call in flight was dropped without a result (a rebuild, a
    /// purged job, a flood wait): its checks return to the head of the
    /// FIFO, in order, and consume no budget.
    pub fn return_flight(&mut self) {
        let Some(flight) = self.flight.take() else {
            return;
        };
        for (chat, slot) in flight.chats.into_iter().rev() {
            if slot == Slot::Retired {
                continue;
            }
            if let Some(fence) = self.fences.get_mut(&chat) {
                fence.in_flight = false;
                if fence.recheck == Recheck::Pending {
                    // The next attempt is dispatched after the trigger.
                    fence.recheck = Recheck::Unused;
                }
                self.fifo.push_front(chat);
            }
        }
    }

    /// [`Self::return_flight`] for the call stamped `stamp` only.
    pub fn return_call(&mut self, stamp: BadgeSeq) {
        if self.flight.as_ref().is_some_and(|f| f.stamp == Some(stamp)) {
            self.return_flight();
        }
    }

    /// The live check of `chat` stated `after`.
    /// The live check of `chat`, dispatched at `stamp`, stated `kind`. It
    /// settles with the newest statement kept (a newer dialogs page wins
    /// over it), unless a trigger crossed it and nothing newer answers that
    /// trigger: then the chat is checked once more.
    pub fn succeeded(&mut self, chat: ChatId, stamp: BadgeSeq, kind: ChatKind) -> Option<Verdict> {
        let fence = self.fences.get_mut(&chat)?;
        fence.in_flight = false;
        fence.state(stamp, kind);
        if fence.recheck == Recheck::Pending && fence.answer().is_none() {
            fence.recheck = Recheck::Used;
            self.fifo.push_back(chat);
            return Some(Verdict::Requeued);
        }
        let newest = fence.stated.unwrap_or((stamp, kind));
        Some(Verdict::Settled(self.finish(chat, newest)))
    }

    /// Ends `chat`'s fence with the statement `(at, after)`.
    fn finish(&mut self, chat: ChatId, (at, after): (BadgeSeq, ChatKind)) -> Settled {
        let before = self
            .fences
            .remove(&chat)
            .map_or(ForumStatus::Unknown, |f| f.before);
        self.fifo.retain(|c| *c != chat);
        Settled::Kind { before, after, at }
    }

    /// The live check of `chat` failed: queued again, or abandoned on the
    /// third failure of its fence.
    /// A statement newer than the last trigger, kept meanwhile, answers it
    /// instead: the fence settles with it and no failure is counted.
    pub fn failed(&mut self, chat: ChatId) -> Option<Verdict> {
        let fence = self.fences.get_mut(&chat)?;
        fence.in_flight = false;
        if let Some(after) = fence.answer() {
            return Some(Verdict::Settled(self.finish(chat, after)));
        }
        fence.failures = fence.failures.saturating_add(1);
        if fence.failures >= FENCE_FAILURES {
            let before = fence.before;
            self.fences.remove(&chat);
            self.suspects.insert(chat);
            return Some(Verdict::Settled(Settled::Abandoned { before }));
        }
        if fence.recheck == Recheck::Pending {
            // The retry is dispatched after the trigger: it covers it.
            fence.recheck = Recheck::Unused;
        }
        self.fifo.push_back(chat);
        Some(Verdict::Requeued)
    }

    /// An authoritative summary of `chat` dispatched at `stamp` (a dialogs
    /// page, or a `peer_dialogs` result in a badge slot) stated `after`.
    /// It is kept as the fence's newest statement if it is, and settles a
    /// queued fence it postdates; one in flight waits for its own result,
    /// which the newest statement then overrides.
    pub fn settle(&mut self, chat: ChatId, stamp: BadgeSeq, after: ChatKind) -> Option<Settled> {
        let fence = self.fences.get_mut(&chat)?;
        fence.state(stamp, after);
        if fence.in_flight {
            return None;
        }
        let after = fence.answer()?;
        Some(self.finish(chat, after))
    }

    /// Keeps only the chats `keep` accepts (the required-peer set). A chat
    /// that left loses its queue entry, its marks and its fence; its
    /// in-flight slot is retired. Returns the fences released.
    pub fn retire(&mut self, mut keep: impl FnMut(ChatId) -> bool) -> Vec<ChatId> {
        let mut gone: Vec<ChatId> = self.fences.keys().copied().filter(|c| !keep(*c)).collect();
        gone.sort_unstable();
        for chat in &gone {
            self.fences.remove(chat);
        }
        if !gone.is_empty() {
            self.fifo.retain(|c| !gone.contains(c));
            if let Some(flight) = self.flight.as_mut() {
                for (chat, slot) in &mut flight.chats {
                    if gone.contains(chat) {
                        *slot = Slot::Retired;
                    }
                }
            }
        }
        self.suspects.retain(|c| keep(*c));
        gone
    }

    /// The suspect chats, for the light refresh to re-check; the marks go.
    pub fn take_suspects(&mut self) -> Vec<ChatId> {
        let mut out: Vec<ChatId> = self.suspects.drain().collect();
        out.sort_unstable();
        out
    }

    /// Forgets everything (a new account).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Every per-chat record (fences, queue entries, in-flight slots and
    /// suspect marks) is about one of `chats`.
    #[cfg(test)]
    pub(crate) fn within(&self, chats: &HashSet<ChatId>) -> bool {
        let flight = self.flight.iter().flat_map(|f| f.chats.iter());
        self.fences.keys().all(|c| chats.contains(c))
            && self.fifo.iter().all(|c| chats.contains(c))
            && self.suspects.iter().all(|c| chats.contains(c))
            && flight
                .filter(|(_, s)| *s == Slot::Live)
                .all(|(c, _)| chats.contains(c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PeerKind;

    fn c(id: i64) -> ChatId {
        ChatId {
            kind: PeerKind::Channel,
            id,
        }
    }

    const NOT: ForumStatus = ForumStatus::NotForum;
    const FORUM: ForumStatus = ForumStatus::Forum;
    const GROUP_KIND: ChatKind = ChatKind::Supergroup {
        forum: false,
        gigagroup: false,
    };
    const FORUM_KIND: ChatKind = ChatKind::Supergroup {
        forum: true,
        gigagroup: false,
    };

    fn stamp(n: u64) -> BadgeSeq {
        BadgeSeq::at(n)
    }

    /// One attempt of `chat` in a call stamped `n`.
    fn attempt(k: &mut KindChecks, n: u64) -> Vec<ChatId> {
        k.dispatch(1);
        k.stamp_flight(stamp(n));
        k.complete(stamp(n)).live
    }

    #[test]
    fn deduplicated_fifo_and_settling() {
        let mut k = KindChecks::default();
        assert_eq!(k.fence(c(1), NOT, stamp(0)), Fenced::New);
        assert_eq!(k.fence(c(2), NOT, stamp(0)), Fenced::New);
        assert_eq!(k.fence(c(1), NOT, stamp(0)), Fenced::Already);
        assert_eq!(k.head(10), vec![c(1), c(2)]);
        assert_eq!(attempt(&mut k, 1), vec![c(1)]);
        assert_eq!(
            k.succeeded(c(1), stamp(1), FORUM_KIND),
            Some(Verdict::Settled(Settled::Kind {
                before: NOT,
                after: FORUM_KIND,
                at: stamp(1)
            }))
        );
        assert!(!k.is_fenced(c(1)) && k.is_fenced(c(2)));
    }

    /// Fail, fail, success with a recheck pending, then success;
    /// a trigger during the recheck gets no second recheck.
    #[test]
    fn budgets_fail_fail_recheck_success() {
        let mut k = KindChecks::default();
        k.fence(c(1), NOT, stamp(0));
        for n in 1..=2 {
            attempt(&mut k, n);
            assert_eq!(k.failed(c(1)), Some(Verdict::Requeued));
        }
        k.dispatch(1);
        k.stamp_flight(stamp(3));
        assert_eq!(k.fence(c(1), NOT, stamp(3)), Fenced::Recheck);
        k.complete(stamp(3));
        assert_eq!(
            k.succeeded(c(1), stamp(3), GROUP_KIND),
            Some(Verdict::Requeued)
        );
        k.dispatch(1);
        k.stamp_flight(stamp(4));
        assert_eq!(k.fence(c(1), NOT, stamp(4)), Fenced::Suspect);
        k.complete(stamp(4));
        assert!(matches!(
            k.succeeded(c(1), stamp(4), GROUP_KIND),
            Some(Verdict::Settled(Settled::Kind { .. }))
        ));
        assert!(k.is_suspect(c(1)));
    }

    /// … then the third failure abandons, and a dropped attempt (rebuild)
    /// consumes neither budget.
    #[test]
    fn budgets_third_failure_abandons_and_drops_are_free() {
        let mut k = KindChecks::default();
        k.fence(c(1), FORUM, stamp(0));
        attempt(&mut k, 1);
        k.failed(c(1));
        k.dispatch(1);
        k.stamp_flight(stamp(2));
        k.return_flight();
        assert_eq!(k.head(1), vec![c(1)], "back at the head");
        attempt(&mut k, 3);
        k.failed(c(1));
        k.dispatch(1);
        k.stamp_flight(stamp(4));
        k.fence(c(1), FORUM, stamp(4));
        k.complete(stamp(4));
        assert_eq!(
            k.failed(c(1)),
            Some(Verdict::Settled(Settled::Abandoned { before: FORUM }))
        );
        assert!(k.is_suspect(c(1)) && !k.is_fenced(c(1)));
        assert_eq!(k.take_suspects(), vec![c(1)]);
        assert!(!k.is_suspect(c(1)));
    }

    #[test]
    fn an_authoritative_summary_settles_a_queued_fence_it_postdates() {
        let mut k = KindChecks::default();
        k.fence(c(1), NOT, stamp(2));
        assert_eq!(k.settle(c(1), stamp(2), FORUM_KIND), None, "not newer");
        assert!(k.settle(c(1), stamp(3), FORUM_KIND).is_some());
        assert_eq!(k.head(5), Vec::new());
        k.fence(c(2), NOT, stamp(3));
        k.dispatch(1);
        k.stamp_flight(stamp(4));
        assert_eq!(k.settle(c(2), stamp(5), FORUM_KIND), None, "in flight");
        // The older check's result loses to that newer statement.
        assert_eq!(
            k.succeeded(c(2), stamp(4), GROUP_KIND),
            Some(Verdict::Settled(Settled::Kind {
                before: NOT,
                after: FORUM_KIND,
                at: stamp(5)
            }))
        );
    }

    #[test]
    fn retirement_drops_every_mark_and_ignores_the_result() {
        let mut k = KindChecks::default();
        for id in 1..=3 {
            k.fence(c(id), NOT, stamp(0));
        }
        k.dispatch(2);
        k.stamp_flight(stamp(1));
        let gone = k.retire(|chat| chat == c(2));
        assert_eq!(gone, vec![c(1), c(3)]);
        assert_eq!(k.fenced().collect::<Vec<_>>(), vec![c(2)]);
        let done = k.complete(stamp(1));
        assert_eq!(done.live, vec![c(2)]);
        assert_eq!(done.retired, vec![c(1)], "c(1)'s slot is retired");
        let keep: HashSet<ChatId> = [c(2)].into_iter().collect();
        assert!(k.within(&keep));
        assert_eq!(k.fence(c(1), NOT, stamp(1)), Fenced::New, "starts fresh");
    }
}
