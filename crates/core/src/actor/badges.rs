//! The forum badge and the account's `peer_dialogs` scheduler in the actor
//! (plan section 7). The pure parts are [`crate::store::Badges`] and
//! [`crate::store::PeerScheduler`]; this module feeds them and applies
//! what comes back.
//!
//! - **Set only from the server.** A forum's `unread` (its unread-topic
//!   count) is set from a dialogs page containing it, a `peer_dialogs`
//!   result, or a chat-level read update. It is a dialog row field like the
//!   kind and the folder, ordered by the badge clock: a snapshot sets it
//!   only if dispatched after the field's last source (`Dialogs::guard`).
//!   Nothing adjusts it locally.
//! - **Triggers** (an incoming message, a remote topic read, a deletion in
//!   the forum; a completed local read in task 9) set the chat's dirty
//!   stamp. The scheduler drains dirty chats through `peer_dialogs`, and only
//!   a result dispatched after the stamp clears it.
//! - **One call per account** in flight, [`crate::store::PEER_CALL_INTERVAL`]
//!   apart, never before `peer_not_before`; batches split into reserved
//!   slots for kind checks, the shown forum and the badge cursor. Dispatch
//!   waits for the update baseline, like the topic snapshots.
//! - **Snapshots.** A `peer_dialogs` result is a dialog snapshot: it passes
//!   the same per-field guard as a dialogs page (stamped at admission), so
//!   it never reverts a newer value (a live archive move, a read, a newer
//!   page's count or kind), and an older page never reverts it.

use super::Actor;
use super::jobs::{CallError, CallResult, Job, Stamps};
use super::threads::kind_status;
use crate::api::Event;
use crate::backend::{Backend, BackendError};
use crate::cache::CacheCmd;
use crate::model::{ChatId, ChatKind, ChatSummary};
use crate::store::{BadgeSeq, Completed, ForumStatus, Guarded, MAX_PEER_DIALOGS, Verdict};

/// The store's clock: the runtime's (paused in tests) as a std instant.
fn now() -> std::time::Instant {
    tokio::time::Instant::now().into_std()
}

impl<B: Backend> Actor<B> {
    /// Whether `chat` is a forum for its badge: what core knows, a fence
    /// aside (a fenced forum's badge still follows the server).
    pub(super) fn is_forum(&self, chat: ChatId) -> bool {
        self.known_status(chat) == ForumStatus::Forum
    }

    /// Something may have changed a forum's count: refresh its badge.
    pub(super) fn badge_trigger(&mut self, chat: ChatId) {
        if self.is_forum(chat) {
            self.badges.trigger(chat);
        }
    }

    /// A chat-level read on another device (`ReadInbox { topic: None }`):
    /// a live change of the row's count, which no snapshot dispatched
    /// before it can revert (for a forum, its unread-topic count).
    pub(super) async fn chat_read(&mut self, chat: ChatId, still_unread: u32) {
        let now = self.badges.now();
        if let Some(summary) = self.dialogs.set_unread(chat, still_unread, now).cloned() {
            self.emit(Event::ChatBumped(summary)).await;
        }
    }

    /// Guards a dialog snapshot dispatched at `stamp` before it is applied:
    /// every row field by its accepted stamp (`Dialogs::guard`; a fenced
    /// chat's kind is held), and every forum it lists is tracked. Settling
    /// dirty badge stamps is separate: any result for a chat dispatched
    /// after its trigger answers it, whatever value was taken.
    pub(super) fn guard_snapshot(
        &mut self,
        chats: &mut [ChatSummary],
        stamp: BadgeSeq,
    ) -> crate::store::Guarded {
        let fences = &self.fences;
        let guarded = self.dialogs.guard(chats, stamp, |c| fences.before(c));
        if guarded != crate::store::Guarded::Discard {
            for chat in chats.iter() {
                if kind_status(chat.kind) == ForumStatus::Forum {
                    self.badges.track(chat.id);
                }
                self.badges.settle(chat.id, stamp);
            }
        }
        guarded
    }

    /// Plans and queues the next `peer_dialogs` call, if one is due: once
    /// the baseline is established, one in flight, spaced, outside a flood
    /// wait. Kind checks take their slots from the FIFO's head.
    pub(super) fn pump_peer_dialogs(&mut self) {
        if !self.topics_ready() {
            return;
        }
        let kinds = self.fences.head(MAX_PEER_DIALOGS);
        let shown = self.topics.lists.shown();
        let Some(batch) = self.peer.reserve(now(), &kinds, shown, &self.badges) else {
            return;
        };
        self.fences.dispatch(batch.kind_checks);
        // Stamped, and its interval counted, when it is admitted
        // (`Self::peer_admitted`).
        let job = Job::PeerDialogs {
            chats: batch.chats,
            stamps: Stamps::default(),
        };
        // Like the topic snapshots: with the bootstrap's while one runs.
        let queue = if self.boot.is_some() {
            &mut self.priority
        } else {
            &mut self.queue
        };
        if queue.len() >= super::MAX_QUEUED {
            return self.peer_job_dropped();
        }
        queue.push_back(job);
    }

    /// The reserved call was dropped before it ran (a purged job, a full
    /// queue): its kind checks go back to the head of the FIFO, unspent.
    pub(super) fn peer_job_dropped(&mut self) {
        self.fences.return_flight();
        self.peer.drop_in_flight();
    }

    /// When [`Self::pump_peer_dialogs`] next has a call to make, if it waits
    /// for time alone (the call interval, a flood wait).
    pub(super) fn peer_wake(&self) -> Option<tokio::time::Instant> {
        if !self.topics_ready() {
            return None;
        }
        let work = self.fences.queued() > 0 || self.badges.dirty().next().is_some();
        let now = now();
        let at = self.peer.ready_at(now).filter(|_| work)?;
        (at > now).then(|| tokio::time::Instant::from_std(at))
    }

    /// The call stamped `stamp` ran without an answer for its checks (a
    /// flood wait): they go back to the head of the FIFO, unspent.
    fn peer_call_lost(&mut self, stamp: BadgeSeq) {
        self.fences.return_call(stamp);
        self.peer.complete(stamp);
    }

    /// A `peer_dialogs` call ended. A flood wait pauses the scheduler and
    /// costs the kind checks nothing; any other failure counts against each
    /// one's budget. A result is applied as a dialog snapshot, then settles
    /// the checks it answers.
    pub(super) async fn on_peer_dialogs(
        &mut self,
        chats: Vec<ChatId>,
        stamps: Stamps,
        res: CallResult<Vec<ChatSummary>>,
    ) {
        let stamp = stamps.badge;
        if let Err(CallError::Backend(BackendError::FloodWait { seconds })) = res {
            self.peer
                .flood(now() + std::time::Duration::from_secs(u64::from(seconds)));
            return self.peer_call_lost(stamp);
        }
        self.peer.complete(stamp);
        let Completed { live, retired } = self.fences.complete(stamp);
        let checks = live;
        let summaries = match res {
            // Nothing about a chat whose check was retired is applied: it
            // left the required peers, and may since have come back with
            // newer state than this response.
            Ok(mut summaries) => {
                summaries.retain(|s| !retired.contains(&s.id));
                summaries
            }
            Err(e) => {
                tracing::debug!(?e, "peer_dialogs failed");
                for chat in checks {
                    if let Some(verdict) = self.fences.failed(chat) {
                        self.after_verdict(chat, verdict).await;
                    }
                }
                return;
            }
        };
        let asked: Vec<ChatId> = chats.into_iter().filter(|c| !retired.contains(c)).collect();
        let stated = self.apply_peer_summaries(&asked, stamps, summaries).await;
        for &chat in &checks {
            let verdict = match stated.iter().find(|s| s.chat == chat) {
                Some(s) => self.fences.succeeded(chat, stamp, s.kind),
                // No dialog came back for it: not an answer.
                None => self.fences.failed(chat),
            };
            if let Some(verdict) = verdict {
                self.after_verdict(chat, verdict).await;
            }
        }
        // Chats in badge slots: their summary is authoritative for a kind
        // too (a queued fence it postdates, or a change of a known chat).
        for s in stated.into_iter().filter(|s| !checks.contains(&s.chat)) {
            self.stated_kind(s, stamp).await;
        }
    }

    /// A `peer_dialogs` task panicked: a failure of each of its checks.
    pub(super) async fn peer_call_panicked(&mut self, stamp: BadgeSeq) {
        self.peer.complete(stamp);
        for chat in self.fences.complete(stamp).live {
            if let Some(verdict) = self.fences.failed(chat) {
                self.after_verdict(chat, verdict).await;
            }
        }
    }

    async fn after_verdict(&mut self, chat: ChatId, verdict: Verdict) {
        if let Verdict::Settled(settled) = verdict {
            self.lift_fence(chat, settled).await;
        }
    }

    /// Applies `peer_dialogs` summaries like a dialogs page: each field
    /// under its accepted stamp ([`Self::guard_snapshot`]), each listed
    /// chat's row refreshed in place (moved only by a newer message).
    /// Returns each returned chat's stated kind. A chat asked for without a
    /// dialog coming back settles its badge without a value.
    async fn apply_peer_summaries(
        &mut self,
        asked: &[ChatId],
        stamps: Stamps,
        mut summaries: Vec<ChatSummary>,
    ) -> Vec<Stated> {
        let stamp = stamps.badge;
        let stated: Vec<Stated> = summaries
            .iter()
            .map(|s| Stated {
                chat: s.id,
                before: self.known_status(s.id),
                kind: s.kind,
            })
            .collect();
        let guarded = self.guard_snapshot(&mut summaries, stamp);
        let mut changed = Vec::new();
        if guarded != Guarded::Discard {
            for fresh in summaries {
                if let Some(row) = self.dialogs.refresh(fresh) {
                    changed.push(row.clone());
                }
            }
        }
        for chat in asked
            .iter()
            .filter(|c| !stated.iter().any(|s| s.chat == **c))
        {
            self.badges.settle(*chat, stamp);
        }
        self.note_kinds();
        if !changed.is_empty() && self.cache.is_some() {
            let all = self.dialogs.as_slice().to_vec();
            self.cache_write(CacheCmd::PutChats(all)).await;
        }
        for summary in changed {
            self.emit(Event::ChatBumped(summary)).await;
        }
        if guarded != Guarded::Apply {
            self.refetch_first_dialogs().await;
        }
        stated
    }

    /// An authoritative summary dispatched at `stamp` stated a kind, and
    /// was applied: it is kept by a fence (settling a queued one it
    /// postdates), and a known, unfenced chat whose forum flag changed by
    /// it goes through the change path without a check (plan section 6.6).
    /// The change is the row's as applied: an older snapshot the field
    /// stamps refused changes nothing.
    pub(super) async fn stated_kind(&mut self, stated: Stated, stamp: BadgeSeq) {
        let Stated { chat, before, kind } = stated;
        if self.fences.is_fenced(chat) {
            if let Some(settled) = self.fences.settle(chat, stamp, kind) {
                self.lift_fence(chat, settled).await;
            }
            return;
        }
        let after = self.known_status(chat);
        if before != ForumStatus::Unknown && after != ForumStatus::Unknown && before != after {
            self.kind_changed(chat, after).await;
        }
    }
}

/// What an authoritative summary said about a chat's kind, and what core
/// knew before applying it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Stated {
    pub(super) chat: ChatId,
    pub(super) before: ForumStatus,
    /// The stated kind, kept by a fence and committed when it settles it.
    pub(super) kind: ChatKind,
}
