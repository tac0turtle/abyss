//! Echoes: the UI's copy of the user's own sends until the server's message
//! takes their place (plan section 9). Core owns the outcome (the pending-send
//! table, `random_id`, retries); this only shows it and offers Retry and
//! Discard where core accepts them. Every change that follows a command is
//! made only after core admitted the command (`ChatShell`), and every change
//! of what core keeps only on core's correlated answer (task 6j, [`Phase`]).
//!
//! One capacity rule: every echo counts toward [`MAX_ECHOES`], whatever its
//! phase (a submission core has not answered, a discard it has not
//! answered, text it refused). Core's table never holds a send the UI has no
//! echo for, so it never holds more than the UI counts.

use std::time::{Duration, Instant};

use abyss_core::api::SendState;
use abyss_core::model::{LocalId, MsgId, ThreadKey};

use super::discarded::{Lost, Obsolete};

/// Same bound as core's whole pending-send table (256 ordinary sends plus 32
/// kept `Failed` for Retry, review rounds 31 to 33). No echo core keeps is
/// ever dropped, and core keeps every send while its table has room, so a
/// send the UI admits is kept unless core says otherwise (`SendRefused`,
/// then the text stays here as [`Phase::Returned`]). A `SentUnconfirmed`
/// echo (the server has the message; only its id is unknown, and core's
/// table no longer holds it) gives way to a new send at the cap.
pub(crate) const MAX_ECHOES: usize = 256 + 32;

/// Where an echo stands with core (task 6j).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// `SendText` queued; core has not said whether it keeps the send.
    Submitted,
    /// Core keeps the send (`SendAdmitted`).
    Kept,
    /// `DiscardSend` queued; core has not answered. Still counted.
    Discarding,
    /// Core holds nothing for it (`SendRefused`): its text exists only
    /// here, offered back to the composer, never for Retry.
    Returned,
    /// Core stopped before it answered or resolved the send (its event
    /// stream closed, task 6k): the fate is unknown unless its last state
    /// says otherwise. Its text is kept for Copy, never for Retry.
    Stranded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Echo {
    pub(crate) local_id: LocalId,
    pub(crate) thread: ThreadKey,
    pub(crate) text: String,
    pub(crate) state: SendState,
    /// Unix seconds when the user sent it.
    pub(crate) date: i64,
    pub(crate) phase: Phase,
    /// When core resends a flood-waiting send (`SendState::FloodWait`).
    resend_at: Option<Instant>,
    /// Its thread can no longer be sent to, and why: it is never shown
    /// again, and leaves as soon as core holds nothing for it
    /// ([`Echoes::take_obsolete`]).
    obsolete: Option<Obsolete>,
}

impl Echo {
    pub(crate) fn new(local_id: LocalId, thread: ThreadKey, text: String, date: i64) -> Self {
        Self {
            local_id,
            thread,
            text,
            state: SendState::Sending,
            date,
            phase: Phase::Submitted,
            resend_at: None,
            obsolete: None,
        }
    }

    /// Retry is offered only where core accepts it: a send core keeps.
    pub(crate) fn can_retry(&self) -> bool {
        self.phase == Phase::Kept
            && matches!(self.state, SendState::Failed | SendState::Unknown { .. })
    }

    /// Discard is offered only where core accepts it: also while a send
    /// waits out a flood wait.
    pub(crate) fn can_discard(&self) -> bool {
        self.can_retry()
            || (self.phase == Phase::Kept && matches!(self.state, SendState::FloodWait { .. }))
    }

    /// Text core refused, offered back to the composer or dismissed.
    pub(crate) fn is_returned(&self) -> bool {
        self.phase == Phase::Returned
    }

    /// Text left when core stopped, offered for Copy or dismissed.
    pub(crate) fn is_stranded(&self) -> bool {
        self.phase == Phase::Stranded
    }

    /// The server has the message (`Sent`, `SentUnconfirmed`): no later
    /// answer may offer its text for sending again.
    fn delivered(&self) -> bool {
        matches!(
            self.state,
            SendState::Sent { .. } | SendState::SentUnconfirmed
        )
    }

    /// Whole seconds until core resends a flood-waiting send, rounded up.
    pub(crate) fn wait_left(&self, now: Instant) -> Option<u64> {
        let left = self.resend_at?.saturating_duration_since(now);
        Some(left.as_secs() + u64::from(left.subsec_nanos() > 0))
    }
}

#[derive(Debug, Default)]
pub(crate) struct Echoes {
    /// Oldest first.
    list: Vec<Echo>,
}

/// The echo table is full; the composer keeps the text.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Full;

impl Echoes {
    /// Whether a new send has no room: the table is at its cap (echoes of
    /// every phase count) and no `SentUnconfirmed` echo can give way.
    pub(crate) fn is_full(&self) -> bool {
        self.list.len() >= MAX_ECHOES && self.oldest_unconfirmed().is_none()
    }

    fn oldest_unconfirmed(&self) -> Option<usize> {
        self.list
            .iter()
            .position(|e| e.state == SendState::SentUnconfirmed)
    }

    pub(crate) fn push(&mut self, echo: Echo) -> Result<(), Full> {
        if self.list.len() >= MAX_ECHOES {
            let at = self.oldest_unconfirmed().ok_or(Full)?;
            self.list.remove(at);
        }
        self.list.push(echo);
        debug_assert!(self.list.len() <= MAX_ECHOES);
        Ok(())
    }

    pub(crate) fn get(&self, local_id: LocalId) -> Option<&Echo> {
        self.list.iter().find(|e| e.local_id == local_id)
    }

    /// The echoes `thread` shows: an obsolete one is never shown.
    pub(crate) fn for_thread(&self, thread: ThreadKey) -> impl Iterator<Item = &Echo> {
        self.list
            .iter()
            .filter(move |e| e.thread == thread && e.obsolete.is_none())
    }

    pub(crate) fn count(&self, thread: ThreadKey) -> usize {
        self.for_thread(thread).count()
    }

    /// Whether any echo shows a flood-wait countdown.
    pub(crate) fn any_waiting(&self) -> bool {
        self.list.iter().any(|e| e.resend_at.is_some())
    }

    /// Applies a `SendState` event received at `now`. Returns the echo's
    /// thread if it changed.
    pub(crate) fn set_state(
        &mut self,
        local_id: LocalId,
        state: SendState,
        now: Instant,
    ) -> Option<ThreadKey> {
        let echo = self
            .list
            .iter_mut()
            .find(|e| e.local_id == local_id && e.phase != Phase::Returned)?;
        echo.state = state;
        echo.resend_at = match state {
            SendState::FloodWait { seconds } => {
                now.checked_add(Duration::from_secs(u64::from(seconds)))
            }
            _ => None,
        };
        Some(echo.thread)
    }

    fn find_mut(&mut self, local_id: LocalId, phase: Phase) -> Option<&mut Echo> {
        self.list
            .iter_mut()
            .find(|e| e.local_id == local_id && e.phase == phase)
    }

    /// Core's queue took a retry with the original `random_id`: shows
    /// "Sending…" until core answers it (it answers every retry: the
    /// send's state, or `SendRefused`).
    pub(crate) fn retried(&mut self, local_id: LocalId) {
        if let Some(echo) = self.find_mut(local_id, Phase::Kept) {
            echo.state = SendState::Sending;
            echo.resend_at = None;
        }
    }

    /// Core keeps a submitted send (`SendAdmitted`). Returns its thread.
    pub(crate) fn admitted(&mut self, local_id: LocalId) -> Option<ThreadKey> {
        let echo = self.find_mut(local_id, Phase::Submitted)?;
        echo.phase = Phase::Kept;
        Some(echo.thread)
    }

    /// Core holds nothing for this send (`SendRefused`): its text stays
    /// here as [`Phase::Returned`]; a discard waiting on it is done.
    /// Returns its thread. A send the server already has keeps its state
    /// (task 6k): a Retry queued before a late confirmation is answered
    /// `SendRefused` once core has dropped the sent entry, and that answer
    /// is older than the confirmation it follows.
    pub(crate) fn refused(&mut self, local_id: LocalId) -> Option<ThreadKey> {
        let at = self.list.iter().position(|e| e.local_id == local_id)?;
        let echo = self.list.get_mut(at)?;
        if echo.delivered() {
            return None;
        }
        let thread = echo.thread;
        match echo.phase {
            Phase::Discarding if echo.obsolete.is_none() => drop(self.list.remove(at)),
            Phase::Submitted | Phase::Kept | Phase::Discarding | Phase::Returned => {
                echo.phase = Phase::Returned;
                echo.resend_at = None;
            }
            Phase::Stranded => {}
        }
        Some(thread)
    }

    /// Core's event stream closed while it ran (task 6k): no answer will
    /// come, so every pending phase is finalized (task 6l) and none stays
    /// `Submitted` or `Discarding`. A delivered echo is `Kept` (it shows
    /// its delivered state, with no Retry or Put back); any other echo
    /// still waiting on core is stranded with its last state; returned and
    /// stranded ones stay. Returns whether any changed.
    pub(crate) fn strand(&mut self) -> bool {
        let mut changed = false;
        for echo in &mut self.list {
            let next = match echo.phase {
                Phase::Returned | Phase::Stranded => continue,
                Phase::Submitted | Phase::Kept | Phase::Discarding if echo.delivered() => {
                    Phase::Kept
                }
                Phase::Submitted | Phase::Kept | Phase::Discarding => Phase::Stranded,
            };
            if next != echo.phase {
                echo.phase = next;
                echo.resend_at = None;
                changed = true;
            }
        }
        debug_assert!(
            self.list
                .iter()
                .all(|e| !matches!(e.phase, Phase::Submitted | Phase::Discarding))
        );
        changed
    }

    /// Core's queue took a discard: the echo (and its slot) stays until
    /// core answers.
    pub(crate) fn discarding(&mut self, local_id: LocalId) {
        if let Some(echo) = self.find_mut(local_id, Phase::Kept) {
            echo.phase = Phase::Discarding;
        }
    }

    /// Core discarded the send (`SendDiscarded`): the echo goes. Returns
    /// its thread. An obsolete echo's text now exists only here: it stays,
    /// returned, for [`Self::take_obsolete`].
    pub(crate) fn discarded(&mut self, local_id: LocalId) -> Option<ThreadKey> {
        let at = self
            .list
            .iter()
            .position(|e| e.local_id == local_id && e.phase != Phase::Returned)?;
        let echo = self.list.get_mut(at)?;
        if echo.obsolete.is_some() {
            echo.phase = Phase::Returned;
            return Some(echo.thread);
        }
        Some(self.list.remove(at).thread)
    }

    /// Core refused a discard and keeps the send in `state`
    /// (`DiscardRefused`). Returns its thread.
    pub(crate) fn discard_refused(
        &mut self,
        local_id: LocalId,
        state: SendState,
        now: Instant,
    ) -> Option<ThreadKey> {
        self.find_mut(local_id, Phase::Discarding)?.phase = Phase::Kept;
        self.set_state(local_id, state, now)
    }

    /// Takes a returned or stranded echo out (the user put its text back
    /// or dismissed it). Core holds nothing for either.
    pub(crate) fn take_returned(&mut self, local_id: LocalId) -> Option<Echo> {
        let at = self.list.iter().position(|e| {
            e.local_id == local_id && matches!(e.phase, Phase::Returned | Phase::Stranded)
        })?;
        Some(self.list.remove(at))
    }

    /// `why` made threads unsendable: their echoes are marked, so none is
    /// shown again or offered for Retry, and each leaves once core holds
    /// nothing for it. Returns whether any was marked.
    pub(crate) fn obsolete(&mut self, why: Obsolete) -> bool {
        let mut marked = false;
        for echo in self.list.iter_mut().filter(|e| why.covers(e.thread)) {
            echo.obsolete.get_or_insert(why);
            marked = true;
        }
        marked
    }

    /// Obsolete sends core keeps and would discard: the UI discards them
    /// itself, since nobody can see them any more.
    pub(crate) fn obsolete_to_discard(&self) -> Vec<LocalId> {
        self.discardable_obsolete().map(|e| e.local_id).collect()
    }

    /// Whether an obsolete send waits to be discarded.
    pub(crate) fn any_obsolete_to_discard(&self) -> bool {
        self.discardable_obsolete().next().is_some()
    }

    fn discardable_obsolete(&self) -> impl Iterator<Item = &Echo> {
        self.list
            .iter()
            .filter(|e| e.obsolete.is_some() && e.can_discard())
    }

    /// Takes out the obsolete echoes core holds nothing for: a delivered
    /// one goes quietly (the server has it), and the text of any other
    /// (refused, discarded, stranded) is returned, oldest first, with why
    /// its thread can't be sent to and whether it may have been sent, for
    /// the discarded-drafts notice.
    pub(crate) fn take_obsolete(&mut self) -> Vec<(Obsolete, Lost, ThreadKey, String)> {
        let (gone, kept): (Vec<Echo>, Vec<Echo>) =
            std::mem::take(&mut self.list).into_iter().partition(|e| {
                e.obsolete.is_some()
                    && (e.delivered() || matches!(e.phase, Phase::Returned | Phase::Stranded))
            });
        self.list = kept;
        gone.into_iter()
            .filter(|e| !e.delivered())
            .filter_map(|e| {
                // An unknown outcome, or a send in flight when core stopped.
                let maybe_sent = matches!(e.state, SendState::Unknown { .. })
                    || (e.phase == Phase::Stranded && e.state == SendState::Sending);
                let lost = if maybe_sent {
                    Lost::UnconfirmedSends
                } else {
                    Lost::Sends
                };
                Some((e.obsolete?, lost, e.thread, e.text))
            })
            .collect()
    }

    /// Drops sent echoes of `thread` whose server copy is now shown. Returns
    /// whether any went.
    pub(crate) fn resolve(&mut self, thread: ThreadKey, shown: impl Fn(MsgId) -> bool) -> bool {
        let before = self.list.len();
        self.list.retain(|e| {
            let SendState::Sent { id } = e.state else {
                return true;
            };
            e.thread != thread || !shown(id)
        });
        self.list.len() != before
    }

    /// Core reconciled `thread` with the server up to `newest`
    /// (`Event::Reconciled`). `shown` answers whether the reconciled view
    /// holds an id, when that view is the one on screen. A sent echo at or
    /// below `newest` that the view lacks was deleted. An unconfirmed echo
    /// is never matched by text (plan section 9), nor by how many tails
    /// arrived: it leaves only on core's evidence ([`Self::covered`]).
    /// Returns whether any echo went.
    pub(crate) fn reconciled(
        &mut self,
        thread: ThreadKey,
        newest: Option<MsgId>,
        shown: Option<&dyn Fn(MsgId) -> bool>,
    ) -> bool {
        let (Some(shown), Some(newest)) = (shown, newest) else {
            return false;
        };
        let before = self.list.len();
        self.list.retain(|e| match e.state {
            SendState::Sent { id } if e.thread == thread => id > newest || shown(id),
            _ => true,
        });
        self.list.len() != before
    }

    /// Core showed server history covering these unconfirmed sends
    /// (`Event::SendsCovered`): their messages are on screen (or deleted).
    /// Returns whether any echo went.
    pub(crate) fn covered(&mut self, sends: &[LocalId]) -> bool {
        let before = self.list.len();
        self.list
            .retain(|e| e.state != SendState::SentUnconfirmed || !sends.contains(&e.local_id));
        self.list.len() != before
    }
}

#[cfg(test)]
mod tests {
    use abyss_core::model::{ChatId, PeerKind};

    use super::*;

    const A: ThreadKey = ThreadKey::whole(ChatId {
        kind: PeerKind::User,
        id: 1,
    });
    const B: ThreadKey = ThreadKey::whole(ChatId {
        kind: PeerKind::User,
        id: 2,
    });

    fn echo(n: u64, thread: ThreadKey) -> Echo {
        Echo::new(LocalId(n), thread, format!("t{n}"), 0)
    }

    fn states(e: &Echoes, thread: ThreadKey) -> Vec<SendState> {
        e.for_thread(thread).map(|e| e.state).collect()
    }

    fn set(e: &mut Echoes, n: u64, state: SendState) {
        e.set_state(LocalId(n), state, Instant::now());
    }

    #[test]
    fn retry_and_discard_only_where_core_accepts_them() {
        let mut e = Echoes::default();
        e.push(echo(1, A)).unwrap();
        set(&mut e, 1, SendState::Failed);
        let get = |e: &Echoes| e.get(LocalId(1)).unwrap().clone();
        assert!(
            !get(&e).can_retry() && !get(&e).can_discard(),
            "core has not said it keeps the send"
        );
        assert_eq!(e.admitted(LocalId(1)), Some(A));
        set(&mut e, 1, SendState::Sending);
        let get = |e: &Echoes| e.get(LocalId(1)).unwrap().clone();
        assert!(!get(&e).can_retry(), "in flight");
        assert!(!get(&e).can_discard(), "in flight");
        set(&mut e, 1, SendState::Failed);
        assert!(get(&e).can_retry() && get(&e).can_discard());
        set(&mut e, 1, SendState::FloodWait { seconds: 30 });
        assert!(!get(&e).can_retry(), "core resends it by itself");
        assert!(get(&e).can_discard(), "a waiting send can be dropped");
        // Flood waits exhausted (4c): Failed, with Retry and Discard again
        // and no countdown left over.
        set(&mut e, 1, SendState::Failed);
        assert!(get(&e).can_retry() && get(&e).can_discard());
        assert_eq!(get(&e).wait_left(Instant::now()), None);
        assert!(!e.any_waiting());
        e.retried(LocalId(1));
        assert_eq!(states(&e, A), [SendState::Sending]);
        set(&mut e, 1, SendState::Failed);
        e.discarding(LocalId(1));
        assert!(!get(&e).can_retry() && !get(&e).can_discard(), "discarding");
        assert_eq!(
            e.count(A),
            1,
            "a discard core has not answered keeps its slot"
        );
        e.discarded(LocalId(1));
        assert_eq!(e.count(A), 0);
        assert_eq!(
            e.set_state(LocalId(1), SendState::Failed, Instant::now()),
            None,
            "late event"
        );
    }

    #[test]
    fn flood_wait_counts_down() {
        let mut e = Echoes::default();
        e.push(echo(1, A)).unwrap();
        let now = Instant::now();
        e.set_state(LocalId(1), SendState::FloodWait { seconds: 3 }, now);
        let w = e.get(LocalId(1)).unwrap();
        assert_eq!(w.wait_left(now), Some(3));
        assert_eq!(w.wait_left(now + Duration::from_millis(1_500)), Some(2));
        assert_eq!(w.wait_left(now + Duration::from_secs(9)), Some(0));
        assert!(e.any_waiting());
        e.set_state(LocalId(1), SendState::Sending, now);
        assert!(!e.any_waiting(), "resent: no countdown");
    }

    #[test]
    fn sent_echo_gives_way_to_the_server_copy() {
        let mut e = Echoes::default();
        e.push(echo(1, A)).unwrap();
        e.push(echo(2, A)).unwrap();
        e.push(echo(3, B)).unwrap();
        set(&mut e, 1, SendState::Sent { id: MsgId(10) });
        set(&mut e, 3, SendState::Sent { id: MsgId(10) });
        assert!(!e.resolve(A, |id| id == MsgId(11)));
        assert!(e.resolve(A, |id| id == MsgId(10)));
        assert_eq!(states(&e, A), [SendState::Sending]);
        assert_eq!(e.count(B), 1, "other chats are untouched");
    }

    #[test]
    fn reconcile_drops_deleted_sends_but_not_unconfirmed_ones() {
        let mut e = Echoes::default();
        for n in 1..=4 {
            e.push(echo(n, A)).unwrap();
        }
        set(&mut e, 1, SendState::SentUnconfirmed);
        set(&mut e, 2, SendState::Sent { id: MsgId(5) });
        set(&mut e, 3, SendState::Sent { id: MsgId(50) });
        let shown = |_: MsgId| false;
        assert!(e.reconciled(A, Some(MsgId(20)), Some(&shown)));
        assert!(!e.reconciled(A, Some(MsgId(20)), None), "not on screen");
        for _ in 0..3 {
            e.reconciled(A, Some(MsgId(20)), Some(&shown));
        }
        assert_eq!(
            states(&e, A),
            [
                SendState::SentUnconfirmed,
                SendState::Sent { id: MsgId(50) },
                SendState::Sending
            ],
            "5 was deleted; reconciles alone never prove an unconfirmed send"
        );
        assert!(!e.covered(&[LocalId(3)]), "only unconfirmed sends");
        assert!(e.covered(&[LocalId(1)]));
        assert_eq!(e.count(A), 2);
    }

    #[test]
    fn bounded() {
        let mut e = Echoes::default();
        for n in 0..MAX_ECHOES {
            e.push(echo(u64::try_from(n).unwrap(), A)).unwrap();
        }
        assert!(e.is_full());
        assert_eq!(e.push(echo(9999, A)), Err(Full));
        // An accepted send whose id is unknown gives way at the cap.
        set(&mut e, 7, SendState::SentUnconfirmed);
        assert!(!e.is_full());
        e.push(echo(9999, A)).unwrap();
        assert!(e.get(LocalId(7)).is_none());
        assert_eq!(e.count(A), MAX_ECHOES);
        assert!(e.is_full());
    }
}
