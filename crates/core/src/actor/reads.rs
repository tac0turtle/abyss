//! Read marks (plan section 6.8). The highest displayed id of each thread is
//! a read intent, coalesced and flushed every [`super::READ_FLUSH`] as one
//! read per thread, and kept until a read covering it lands.
//!
//! - **Admission** changes no count and schedules nothing. Stage 1's
//!   cleared badge is a display overlay: a chat's row carries the id its
//!   outstanding read goes up to (`ChatSummary::reading`), projected from
//!   the intents whenever the row is installed, replaced or emitted, and
//!   whenever an intent is admitted, grows or settles (stage 2 plan,
//!   section 17.6.1). The intents are the only record of pending reads.
//! - **A successful read** (under the current instance) settles only its
//!   own `max_id`: it proves the server's pointer is at least that high,
//!   and no count. A chat's pointer rises to it, unstamped, and its row is
//!   made dirty for a `peer_dialogs` refresh, which brings the server's
//!   pair. A topic's pointer rises to it, the topic is due with
//!   `ReadSince`, and the forum's badge is dirty. Only now, so no refetch
//!   can be answered from state that predates the read.
//! - **A failed read** schedules nothing, and its intent stays:
//!   - an unknown outcome (transient, time-out, dropped by a rebuild, a
//!     migration) is retried after a backoff doubling from
//!     [`FIRST_BACKOFF`] up to [`MAX_BACKOFF`];
//!   - a flood wait is retried after its deadline;
//!   - a rejection is final, and changes no read state.
//!
//!   A newer mark of the thread coalesces into the intent and waits for the
//!   same deadline. Only a read's first time-out counts for stall
//!   detection (`apply::call_error`), so a read that keeps timing out can't
//!   keep rebuilding the instance.
//! - **Bounds.** At most [`MAX_READS`] intents, each with at most one read
//!   queued or running, and none more often than its backoff allows.

use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;

use super::Actor;
use super::jobs::{CallError, CallResult, Job};
use super::lifecycle::Phase;
use crate::api::{ClientGen, Event, UserError};
use crate::backend::{Backend, BackendError};
use crate::model::{ChatId, ChatSummary, MsgId, ThreadKey};
use crate::store::{MAX_DIALOGS, MAX_TOPICS_PER_FORUM};

/// Threads with a read intent: every listed chat plus a forum's topics.
pub(super) const MAX_READS: usize = MAX_DIALOGS + MAX_TOPICS_PER_FORUM;
/// The wait before a failed read's first retry; it doubles per failure.
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
/// The longest wait between retries of a read.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// The shortest flood wait honoured.
const MIN_FLOOD_WAIT: Duration = Duration::from_secs(1);

/// A thread's read intent, until a read covering it lands.
#[derive(Debug)]
struct ReadMark {
    /// The highest id the user saw.
    max_id: MsgId,
    /// The read queued or running: its instance and id.
    flight: Option<(ClientGen, MsgId)>,
    /// Failed attempts since the last read that landed.
    failures: u8,
    /// No read before this (a backoff or a flood wait).
    not_before: Option<Instant>,
}

impl ReadMark {
    fn new(max_id: MsgId) -> Self {
        Self {
            max_id,
            flight: None,
            failures: 0,
            not_before: None,
        }
    }

    /// Whether a read goes at the flush: none is in flight on `cgen`, and
    /// no deadline holds it.
    fn due(&self, cgen: ClientGen, now: Instant) -> bool {
        self.flight.is_none_or(|(at, _)| at != cgen) && self.not_before.is_none_or(|t| t <= now)
    }

    /// A failed attempt at `now`: the next waits `wait`.
    fn hold(&mut self, wait: Duration, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        self.not_before = Some(now + wait);
    }

    /// The doubling backoff after this many failures.
    fn backoff(&self) -> Duration {
        let doublings = u32::from(self.failures.min(8));
        FIRST_BACKOFF
            .saturating_mul(1 << doublings)
            .min(MAX_BACKOFF)
    }
}

/// How a read's attempt ended, for its intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// The server took it.
    Landed,
    /// A flood wait: again after this long.
    FloodWait(Duration),
    /// Unknown (transient, time-out, dropped, a migration): again after a
    /// backoff.
    Unknown,
    /// The server refused it: final.
    Refused,
}

impl Outcome {
    fn of(res: CallResult<()>) -> Self {
        match res {
            Ok(()) => Self::Landed,
            Err(CallError::Backend(BackendError::FloodWait { seconds })) => {
                Self::FloodWait(Duration::from_secs(u64::from(seconds)).max(MIN_FLOOD_WAIT))
            }
            Err(
                CallError::Backend(BackendError::Transient | BackendError::Migrated)
                | CallError::Timeout
                | CallError::Dropped,
            ) => Self::Unknown,
            Err(CallError::Backend(_)) => Self::Refused,
        }
    }
}

/// No room for another thread's intent (at most [`MAX_READS`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Full;

/// The read intents, one per thread, until a read covering each lands.
#[derive(Debug, Default)]
pub(super) struct Reads {
    marks: HashMap<ThreadKey, ReadMark>,
}

impl Reads {
    /// The user saw `thread` up to `max_id`: the intent grows to it.
    pub(super) fn admit(&mut self, thread: ThreadKey, max_id: MsgId) -> Result<(), Full> {
        if self.marks.len() >= MAX_READS && !self.marks.contains_key(&thread) {
            return Err(Full);
        }
        let mark = self
            .marks
            .entry(thread)
            .or_insert_with(|| ReadMark::new(max_id));
        mark.max_id = mark.max_id.max(max_id);
        Ok(())
    }

    /// The id an outstanding read of the whole of `chat` goes up to: what
    /// its row's overlay shows (`ChatSummary::reading`).
    pub(super) fn outstanding(&self, chat: ChatId) -> Option<MsgId> {
        let thread = ThreadKey { chat, topic: None };
        self.marks.get(&thread).map(|m| m.max_id)
    }

    /// Sets the overlay of every summary in `chats` from the intents: a
    /// snapshot never supplies or clears it.
    pub(super) fn project(&self, chats: &mut [ChatSummary]) {
        for chat in chats {
            chat.reading = self.outstanding(chat.id);
        }
    }

    /// The intents a read goes for at a flush on `cgen` at `now`, with
    /// their failures so far.
    fn due(&self, cgen: ClientGen, now: Instant) -> Vec<(ThreadKey, MsgId, u8)> {
        self.marks
            .iter()
            .filter(|(_, m)| m.due(cgen, now))
            .map(|(t, m)| (*t, m.max_id, m.failures))
            .collect()
    }

    /// The read up to `max_id` was queued on `cgen`.
    fn queued(&mut self, thread: ThreadKey, cgen: ClientGen, max_id: MsgId) {
        if let Some(mark) = self.marks.get_mut(&thread) {
            mark.flight = Some((cgen, max_id));
        }
    }

    /// The read couldn't be queued at `now`: it waits a backoff.
    fn unqueued(&mut self, thread: ThreadKey, now: Instant) {
        if let Some(mark) = self.marks.get_mut(&thread) {
            let wait = mark.backoff();
            mark.hold(wait, now);
        }
    }

    /// A queued read was dropped unrun: its intent goes again.
    fn dropped(&mut self, thread: ThreadKey, max_id: MsgId) {
        if let Some(mark) = self.marks.get_mut(&thread)
            && mark.flight.is_some_and(|(_, id)| id == max_id)
        {
            mark.flight = None;
        }
    }

    /// The read up to `max_id` on `cgen` ended at `now` with `outcome`.
    /// `false` when it isn't its intent's read in flight (the intent was
    /// cleared by a logout, or sent again after a rebuild): nothing
    /// changes. A landed or refused read settles only its own `max_id`: an
    /// intent that grew meanwhile stays, and goes at the next flush.
    pub(super) fn ended(
        &mut self,
        cgen: ClientGen,
        thread: ThreadKey,
        max_id: MsgId,
        outcome: Outcome,
        now: Instant,
    ) -> bool {
        let Some(mark) = self.marks.get_mut(&thread) else {
            return false;
        };
        if mark.flight != Some((cgen, max_id)) {
            return false;
        }
        mark.flight = None;
        match outcome {
            Outcome::Landed | Outcome::Refused => {
                if mark.max_id <= max_id {
                    self.marks.remove(&thread);
                } else {
                    *mark = ReadMark::new(mark.max_id);
                }
            }
            Outcome::FloodWait(wait) => mark.hold(wait, now),
            Outcome::Unknown => {
                let wait = mark.backoff();
                mark.hold(wait, now);
            }
        }
        true
    }

    pub(super) fn clear(&mut self) {
        self.marks.clear();
    }
}

impl<B: Backend> Actor<B> {
    pub(super) async fn mark_read(&mut self, thread: ThreadKey, max_id: MsgId) {
        if self.reads.admit(thread, max_id).is_err() {
            return self.error(UserError::Busy).await;
        }
        self.project_reading(thread.chat).await;
    }

    /// Re-derives `chat`'s row overlay from the intents, and emits the row
    /// if that changed it.
    async fn project_reading(&mut self, chat: ChatId) {
        let reading = self.reads.outstanding(chat);
        if let Some(summary) = self.dialogs.set_reading(chat, reading).cloned() {
            self.emit(Event::ChatBumped(summary)).await;
        }
    }

    /// Queues a read for every due intent. While no instance is live they
    /// keep coalescing instead of piling up in the queue.
    pub(super) async fn flush_reads(&mut self) {
        if self.life.phase != Phase::Live {
            return;
        }
        let (cgen, now) = (self.cgen, Instant::now());
        for (thread, max_id, retry) in self.reads.due(cgen, now) {
            let queued = self
                .enqueue(Job::Read {
                    thread,
                    max_id,
                    retry,
                })
                .await;
            if queued {
                self.reads.queued(thread, cgen, max_id);
            } else {
                self.reads.unqueued(thread, now);
            }
        }
    }

    /// A queued read was dropped unrun (the instance went): its intent goes
    /// again at a later flush.
    pub(super) fn read_job_dropped(&mut self, thread: ThreadKey, max_id: MsgId) {
        self.reads.dropped(thread, max_id);
    }

    /// A read ended, on any instance (see the module docs).
    pub(super) async fn on_read(
        &mut self,
        cgen: ClientGen,
        thread: ThreadKey,
        max_id: MsgId,
        res: CallResult<()>,
    ) {
        let outcome = Outcome::of(res);
        if !self
            .reads
            .ended(cgen, thread, max_id, outcome, Instant::now())
        {
            return;
        }
        let chat = thread.chat;
        let mut raised = false;
        if outcome == Outcome::Landed && cgen == self.cgen {
            self.notify_inbox_read(thread, max_id).await;
            if thread.topic.is_some() {
                self.topic_read(thread, max_id).await;
                self.badge_trigger(chat).await;
            } else {
                // The pointer rises, unstamped; the read proves no count,
                // so the row is refreshed from the server. The pane's chat
                // is refreshed even when it isn't listed (a window only).
                let now = self.badges.now();
                raised = self.dialogs.read_landed(chat, max_id, now).is_some();
                if self.dialogs.get(chat).is_none() && self.shown_chat() == Some(chat) {
                    let dialogs = &self.dialogs;
                    self.badges.track_extra(chat, |c| dialogs.get(c).is_some());
                }
                self.badges.trigger(chat);
            }
        }
        let reading = self.reads.outstanding(chat);
        let projected = self.dialogs.set_reading(chat, reading).is_some();
        if (raised || projected)
            && let Some(summary) = self.dialogs.get(chat).cloned()
        {
            self.emit(Event::ChatBumped(summary)).await;
        }
    }
}

#[cfg(test)]
mod tests;
