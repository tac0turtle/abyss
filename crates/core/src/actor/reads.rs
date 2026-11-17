//! Read marks (plan section 6.8). The highest displayed id of each thread is
//! a read intent, coalesced and flushed every [`super::READ_FLUSH`] as one
//! read per thread, and kept until a read covering it lands.
//!
//! - **Admission** changes no forum count and schedules nothing. A
//!   non-forum chat's row is cleared at once, as in Stage 1.
//! - **A successful topic read** (under the current instance) makes the
//!   topic due with `ReadSince` and the forum's badge dirty. Only now, so
//!   neither refetch can be answered from state that predates the read.
//! - **A failed read** schedules nothing, and its intent stays:
//!   - an unknown outcome (transient, time-out, dropped by a rebuild, a
//!     migration) is retried after a backoff doubling from
//!     [`FIRST_BACKOFF`] up to [`MAX_BACKOFF`];
//!   - a flood wait is retried after its deadline;
//!   - a rejection is final.
//!
//!   A newer mark of the thread coalesces into the intent and waits for the
//!   same deadline. Only a read's first time-out counts for stall
//!   detection (`apply::call_error`), so a read that keeps timing out can't
//!   keep rebuilding the instance.
//! - **Bounds.** At most [`MAX_READS`] intents, each with at most one read
//!   queued or running, and none more often than its backoff allows.

use std::time::Duration;

use tokio::time::Instant;

use super::Actor;
use super::jobs::{CallError, CallResult, Job};
use super::lifecycle::Phase;
use crate::api::{ClientGen, Event, UserError};
use crate::backend::{Backend, BackendError};
use crate::model::{MsgId, ThreadKey};
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
pub(super) struct ReadMark {
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

    /// A failed attempt: the next waits `wait`.
    fn hold(&mut self, wait: Duration) {
        self.failures = self.failures.saturating_add(1);
        self.not_before = Some(Instant::now() + wait);
    }

    /// The doubling backoff after this many failures.
    fn backoff(&self) -> Duration {
        let doublings = u32::from(self.failures.min(8));
        FIRST_BACKOFF
            .saturating_mul(1 << doublings)
            .min(MAX_BACKOFF)
    }
}

impl<B: Backend> Actor<B> {
    pub(super) async fn mark_read(&mut self, thread: ThreadKey, max_id: MsgId) {
        if self.reads.len() >= MAX_READS && !self.reads.contains_key(&thread) {
            return self.error(UserError::Busy).await;
        }
        let mark = self
            .reads
            .entry(thread)
            .or_insert_with(|| ReadMark::new(max_id));
        mark.max_id = mark.max_id.max(max_id);
        // A forum's badge is the server's only (plan section 7).
        if self.is_forum(thread.chat) {
            return;
        }
        let now = self.badges.now();
        if let Some(summary) = self.dialogs.set_unread(thread.chat, 0, now).cloned() {
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
        let due: Vec<(ThreadKey, MsgId, u8)> = self
            .reads
            .iter()
            .filter(|(_, m)| m.due(cgen, now))
            .map(|(t, m)| (*t, m.max_id, m.failures))
            .collect();
        for (thread, max_id, retry) in due {
            let queued = self
                .enqueue(Job::Read {
                    thread,
                    max_id,
                    retry,
                })
                .await;
            let Some(mark) = self.reads.get_mut(&thread) else {
                continue;
            };
            if queued {
                mark.flight = Some((cgen, max_id));
            } else {
                let wait = mark.backoff();
                mark.hold(wait);
            }
        }
    }

    /// A queued read was dropped unrun (the instance went): its intent goes
    /// again at a later flush.
    pub(super) fn read_job_dropped(&mut self, thread: ThreadKey, max_id: MsgId) {
        if let Some(mark) = self.reads.get_mut(&thread)
            && mark.flight.is_some_and(|(_, id)| id == max_id)
        {
            mark.flight = None;
        }
    }

    /// A read ended, on any instance (see the module docs). One that isn't
    /// its intent's read in flight (the intent was cleared by a logout, or
    /// sent again after a rebuild) changes nothing.
    pub(super) async fn on_read(
        &mut self,
        cgen: ClientGen,
        thread: ThreadKey,
        max_id: MsgId,
        res: CallResult<()>,
    ) {
        let Some(mark) = self.reads.get_mut(&thread) else {
            return;
        };
        if mark.flight != Some((cgen, max_id)) {
            return;
        }
        mark.flight = None;
        match res {
            Ok(()) => {
                self.read_settled(thread, max_id);
                if thread.topic.is_some() && cgen == self.cgen {
                    self.topic_read(thread).await;
                    self.badge_trigger(thread.chat);
                }
            }
            Err(CallError::Backend(BackendError::FloodWait { seconds })) => {
                let wait = Duration::from_secs(u64::from(seconds)).max(MIN_FLOOD_WAIT);
                mark.hold(wait);
            }
            Err(
                CallError::Backend(BackendError::Transient | BackendError::Migrated)
                | CallError::Timeout
                | CallError::Dropped,
            ) => {
                let wait = mark.backoff();
                mark.hold(wait);
            }
            // Final: the server refused it.
            Err(CallError::Backend(_)) => self.read_settled(thread, max_id),
        }
    }

    /// A read up to `max_id` is done with: the intent goes, unless a newer
    /// mark joined it meanwhile, which goes at the next flush.
    fn read_settled(&mut self, thread: ThreadKey, max_id: MsgId) {
        let Some(mark) = self.reads.get_mut(&thread) else {
            return;
        };
        if mark.max_id <= max_id {
            self.reads.remove(&thread);
        } else {
            *mark = ReadMark::new(mark.max_id);
        }
    }
}
