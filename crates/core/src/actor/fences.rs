//! Forum flag changes in the actor (plan section 6.6): triggers fence a
//! known chat and queue a kind check through `peer_dialogs`
//! ([`super::badges`]); the fence holds the chat's history and cache work
//! until the check, or another authoritative summary, settles it.
//!
//! - **Triggers:** `ChatChanged`, a topic-headed message in a chat core
//!   thinks is not a forum ([`Route::ForumSuspected`](crate::store::Route)),
//!   a topics call refused as "not a forum", and an authoritative summary
//!   that changes a known chat's flag (applied at once, without a fence).
//! - **While fenced** `route` answers `ChatOnly` for the chat; on entry its
//!   windows lose continuity and are invalidated (their view generation
//!   moves, so loads in flight are stale). No history load, cache read or
//!   cache write starts for it: `OpenThread` opens the view without a load,
//!   `LoadOlder`/`LoadNewer` wait in the deferred list (`Busy` beyond it),
//!   sends wait in the pending table, and a bootstrap leaves its tail out.
//! - **Lifting**, whatever the outcome: a changed kind invalidates the
//!   chat's windows and topic list, deletes its cached messages and emits
//!   `KindChanged`, `ForumInvalidated` and, for a listed chat, `ChatBumped`
//!   (an unfenced summary refresh may send its own `ChatBumped` first). An unchanged kind (or an abandoned
//!   check) invalidates the windows and reloads the open thread's tail,
//!   whose thread-scoped reconcile alone restores continuity. Waiting
//!   loads are answered (their views are gone) and held sends go.
//! - **Retirement.** A chat that leaves the required-peer set loses its
//!   kind-check state; its fence is released (it has no windows) and its
//!   deferred commands are answered.

use std::collections::HashSet;

use super::Actor;
use super::boot::TailVia;
use super::jobs::Job;
use super::loads::Deferred;
use super::threads::kind_status;
use crate::api::{Event, UserError};
use crate::backend::{Backend, BackendError};
use crate::cache::CacheCmd;
use crate::model::{ChatId, PeerKind, ThreadKey};
use crate::store::{Fenced, ForumStatus, Settled};

/// The revisions the last retirement pass saw (sends, windows, dialogs).
pub(super) type Members = (u64, u64, u64);

impl<B: Backend> Actor<B> {
    /// Whether `chat` is known: listed, or with a window. Only known chats
    /// are fenced (they are in the required-peer set).
    fn is_known(&self, chat: ChatId) -> bool {
        self.dialogs.get(chat).is_some() || self.windows.threads().any(|t| t.chat == chat)
    }

    /// A forum-flag trigger for `chat`: fences it and queues its kind
    /// check, unless it is unknown (nothing to protect) or not a channel.
    pub(super) async fn fence_chat(&mut self, chat: ChatId) {
        if chat.kind != PeerKind::Channel || !self.is_known(chat) {
            return;
        }
        let before = self.known_status(chat);
        let fenced = self.fences.fence(chat, before, self.badges.now());
        tracing::debug!(?fenced, "forum flag trigger");
        if fenced == Fenced::New {
            self.revoke_queued(chat).await;
            self.revoke_views(chat).await;
        }
    }

    /// A topics call answered "not a forum" for a chat core thought was
    /// one: a forum-flag trigger.
    pub(super) async fn topics_refused(&mut self, chat: ChatId, err: super::jobs::CallError) {
        let not_forum = matches!(
            err,
            super::jobs::CallError::Backend(BackendError::Rejected(
                crate::backend::RejectReason::NotForum
            ))
        );
        if not_forum && self.known_status(chat) == ForumStatus::Forum {
            self.fence_chat(chat).await;
        }
    }

    /// Work queued for `chat` before its fence started doesn't run during
    /// it: its history loads are dropped with their bookkeeping (the loads
    /// the UI waits on are answered; a boot's tail snapshot completes), and
    /// its sends go back to the pending table, held until the lift.
    async fn revoke_queued(&mut self, chat: ChatId) {
        let of_chat = |job: &Job| job.fenced_chat() == Some(chat);
        let mut taken = Vec::new();
        for queue in [&mut self.queue, &mut self.priority] {
            let (out, kept): (Vec<Job>, Vec<Job>) = queue.drain(..).partition(of_chat);
            *queue = kept.into();
            taken.extend(out);
        }
        for job in taken {
            match job {
                Job::History { load, dir, .. } => {
                    let snapshot = self.is_tail_snapshot(load);
                    self.release_history(load);
                    self.covers.fetch_ended(load.thread, load.req);
                    if self.take_owed(load).is_some() {
                        self.history_ended(load.thread, dir, None).await;
                    }
                    if snapshot {
                        self.snapshot_done(super::boot::Snapshot::Tail {
                            load,
                            applied: false,
                        });
                    }
                }
                Job::Send { local_id, .. } => self.sends.unqueue(local_id),
                _ => {}
            }
        }
    }

    /// Every window of `chat` loses continuity and is invalidated: its view
    /// generation moves (loads in flight become stale) and the UI clears it.
    async fn revoke_views(&mut self, chat: ChatId) {
        self.windows.lose_continuity(chat);
        let threads: Vec<ThreadKey> = self.windows.threads().filter(|t| t.chat == chat).collect();
        for thread in threads {
            if self.windows.invalidate(thread).is_some() {
                self.forget_owed(thread);
                self.emit(Event::ThreadInvalidated(thread)).await;
            }
        }
    }

    /// A fence ended (see the module docs).
    pub(super) async fn lift_fence(&mut self, chat: ChatId, settled: Settled) {
        tracing::debug!(?settled, "fence lifted");
        match settled {
            // The stated kind is committed only now; abandonment keeps the
            // pre-fence kind, which the chat never stopped showing.
            Settled::Kind { before, after, at } => {
                let status = kind_status(after);
                // At its statement's stamp: an older snapshot landing later
                // can't revert it.
                let row = self.dialogs.commit_kind(chat, after, at).cloned();
                self.note_kind(chat, status);
                self.note_kinds();
                // The cached list was written with the pre-fence kind.
                if row.is_some() && self.cache.is_some() {
                    let all = self.dialogs.as_slice().to_vec();
                    self.cache_write(CacheCmd::PutChats(all)).await;
                }
                if before == status {
                    if let Some(summary) = row {
                        self.emit(Event::ChatBumped(summary)).await;
                    }
                    self.restore_views(chat).await;
                } else {
                    self.kind_changed(chat, status).await;
                }
            }
            Settled::Abandoned { .. } => self.restore_views(chat).await,
        }
        self.release_held_sends(chat).await;
    }

    /// An unchanged kind, or an abandoned check: continuity is still not
    /// restored. The windows are invalidated, and the open thread, if it is
    /// the chat's, reloads its tail (as a boot snapshot while a bootstrap
    /// runs past its baseline).
    async fn restore_views(&mut self, chat: ChatId) {
        self.revoke_views(chat).await;
        self.answer_deferred(chat).await;
        let Some(open) = self.open.filter(|t| t.chat == chat) else {
            return;
        };
        let Some(view) = self.windows.get(open).map(crate::store::Window::view_gen) else {
            return;
        };
        let load = self.begin_load(open, view);
        self.claim_if_booting(load);
        self.start_tail(load, TailVia::Cache).await;
    }

    /// `chat`'s forum flag changed: its thread keys changed meaning. Its
    /// cached messages go first (a cache barrier taken after the events
    /// orders after the delete), then its windows and topic list.
    pub(super) async fn kind_changed(&mut self, chat: ChatId, after: ForumStatus) {
        tracing::debug!(?after, "forum flag changed");
        self.cache_write(CacheCmd::DeleteChat(chat)).await;
        self.revoke_views(chat).await;
        self.answer_deferred(chat).await;
        self.forum_kind_changed(chat, after);
        self.note_kind(chat, after);
        self.note_kinds();
        if after != ForumStatus::Unknown {
            let forum = after;
            self.emit(Event::KindChanged { chat, forum }).await;
        }
        self.emit(Event::ForumInvalidated(chat)).await;
        if let Some(summary) = self.dialogs.get(chat).cloned() {
            self.emit(Event::ChatBumped(summary)).await;
        }
    }

    /// Answers the loads waiting for `chat`'s fence (`LoadEnded` without an
    /// error): the views they were asked for are gone, and the UI asks
    /// again from the new view (a reloaded tail ends them anyway).
    async fn answer_deferred(&mut self, chat: ChatId) {
        let waiting: Vec<Deferred> = self
            .deferred
            .extract_if(.., |d| d.thread.chat == chat)
            .collect();
        for d in waiting {
            self.history_ended(d.thread, d.dir, None).await;
        }
    }

    /// Whether a history load, cache read or cache write may start for
    /// `chat`'s threads (not while it is fenced).
    pub(super) fn is_fenced(&self, chat: ChatId) -> bool {
        self.fences.is_fenced(chat)
    }

    /// `LoadOlder`/`LoadNewer` of a fenced chat (always the open thread):
    /// it waits in the deferred list for the fence to lift. One per
    /// direction fits ([`super::loads::MAX_DEFERRED`]); a command beyond
    /// that is answered `Busy` at once.
    pub(super) async fn defer_fenced(&mut self, asked: Deferred) {
        let room = self.deferred.len() < super::loads::MAX_DEFERRED
            && self
                .deferred
                .iter()
                .all(|d| d.thread == asked.thread && d.dir != asked.dir);
        if room {
            self.defer_load(asked);
        } else {
            self.history_ended(asked.thread, asked.dir, Some(UserError::Busy))
                .await;
        }
    }

    /// The light refresh re-checks the suspect chats: each starts a new
    /// fence with fresh budgets.
    pub(super) async fn recheck_suspects(&mut self) {
        for chat in self.fences.take_suspects() {
            self.fence_chat(chat).await;
        }
    }

    /// The coalesced membership pass (run once per actor loop turn, like
    /// the peer sync, and only when sends, windows or dialogs changed):
    /// kind-check state and fences are retired for chats that left the
    /// required-peer set, and the badge state follows the retained dialogs.
    pub(super) async fn sync_members(&mut self) {
        // The pane's chat beyond the dialogs (a window only) is tracked
        // only while it is shown and unlisted: checked every turn, since
        // the pane can change without the membership revision.
        let shown = self.shown_chat();
        let dialogs = &self.dialogs;
        self.badges
            .reconcile_extra(shown, |c| dialogs.get(c).is_some());
        let members = (self.sends.rev(), self.windows.rev(), self.dialogs.rev());
        if self.members == Some(members) {
            return;
        }
        self.members = Some(members);
        let required: HashSet<ChatId> = self.required_peers().into_iter().collect();
        let listed: HashSet<ChatId> = self.dialogs.as_slice().iter().map(|c| c.id).collect();
        self.badges.retain(|c| listed.contains(&c));
        for chat in &listed {
            self.badges.track(*chat);
        }
        for chat in self.fences.retire(|c| required.contains(&c)) {
            tracing::debug!("a fenced chat left the required peers");
            self.answer_deferred(chat).await;
            self.release_held_sends(chat).await;
        }
    }

    /// Records `after` as what is known about `chat` for its views, when
    /// it has any and isn't listed (a listed chat's kind is its summary's).
    pub(super) fn note_kind(&mut self, chat: ChatId, after: ForumStatus) {
        let has_view =
            self.windows.threads().any(|t| t.chat == chat) || self.topics.lists.get(chat).is_some();
        if has_view && after != ForumStatus::Unknown {
            self.forum_kinds.set(chat, after);
        }
    }
}

#[cfg(test)]
impl<B: Backend> Actor<B> {
    /// Every kind-check record is about a chat in the required-peer set.
    pub(super) fn kind_checks_within_required(&self) -> bool {
        let required: HashSet<ChatId> = self.required_peers().into_iter().collect();
        self.fences.within(&required)
    }
}
