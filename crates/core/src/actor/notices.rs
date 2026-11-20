//! Notice eligibility and bounded aggregation. Message flags are supplied by
//! updates only, so fetching or displaying history cannot produce notices.

mod lookup;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::Instant;

use super::Actor;
use super::boot::Snapshot;
use super::jobs::{CallError, CallResult, Job, Stamps};
use super::lifecycle::Phase;
use crate::api::{Event, Notice, UserError};
use crate::backend::{Backend, BackendError, NewMessage, NotifyScope, NotifyTarget};
use crate::model::{
    Body, ChatId, ChatKind, ChatSummary, Message, MsgId, PeerKind, ThreadKey, TopicId,
};
use crate::sanitize::display_name;
use crate::store::{BadgeSeq, MAX_DIALOGS, PeerMute, Route};
use lookup::{Deferred, Lookups, MuteEvidence};

const NOTIFY_FRESHNESS: i64 = 5 * 60;
const NOTIFY_COALESCE: Duration = Duration::from_secs(2);
/// Cap on ids in each of the pending, deferred and recent sets.
const MAX_NOTICE_MESSAGES: usize = 4096;

pub(super) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn fresh(date: i64, now: i64) -> bool {
    now.abs_diff(date) <= NOTIFY_FRESHNESS.unsigned_abs()
}

#[derive(Debug, Default)]
pub(super) struct Notices {
    pub(super) booted: bool,
    scopes: Scopes,
    pending: HashMap<ChatId, Pending>,
    pending_count: usize,
    next: Option<Instant>,
    pub(super) difference: Difference,
    catch_up_before: Option<usize>,
    pub(super) catch_up_reconciled: bool,
    lookups: Lookups,
    mute_evidence: MuteEvidence,
    seen: HashSet<(ChatId, MsgId)>,
    seen_order: VecDeque<(ChatId, MsgId)>,
}

/// Whether a difference is still delivering updates, and whether a
/// reconcile has already been scheduled for it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Difference {
    #[default]
    Idle,
    Replaying,
    Reconciling,
}

#[derive(Clone, Copy, Debug, Default)]
struct Scopes {
    users: i64,
    groups: i64,
}

impl Scopes {
    fn deadline(self, scope: NotifyScope) -> i64 {
        match scope {
            NotifyScope::Users => self.users,
            NotifyScope::Groups => self.groups,
        }
    }
}

#[derive(Debug)]
struct Pending {
    at: Instant,
    messages: BTreeMap<MsgId, Contributor>,
}

#[derive(Debug)]
struct Contributor {
    topic: Option<TopicId>,
    scope: NotifyScope,
    mentioned: bool,
    date: i64,
    chat_title: String,
    sender: Option<String>,
}

impl Contributor {
    fn into_notice(self, chat: ChatId, msg: MsgId, count: u32) -> Notice {
        Notice::Message {
            chat,
            topic: self.topic,
            msg,
            chat_title: self.chat_title,
            sender: self.sender,
            count,
        }
    }
}

/// What the update and the current dialog tell us at the point of arrival.
#[derive(Clone, Copy)]
struct Candidate<'a> {
    message: &'a Message,
    thread: ThreadKey,
    kind: ChatKind,
    title: &'a str,
}

#[derive(Clone, Copy)]
pub(super) struct Flags {
    silent: bool,
    mentioned: bool,
}

impl From<&NewMessage> for Flags {
    fn from(new: &NewMessage) -> Self {
        Self {
            silent: new.silent,
            mentioned: new.mentioned,
        }
    }
}

impl Flags {
    fn eligible(self, message: &Message, now: i64) -> bool {
        !message.outgoing
            && !matches!(message.body, Body::Unsupported)
            && !self.silent
            && fresh(message.date, now)
    }
}

impl Candidate<'_> {
    fn contributor(&self, scope: NotifyScope, flags: Flags) -> Contributor {
        Contributor {
            topic: self.thread.topic,
            scope,
            mentioned: flags.mentioned,
            date: self.message.date,
            chat_title: display_name(self.title),
            sender: self
                .message
                .sender
                .as_ref()
                .filter(|_| scope == NotifyScope::Groups)
                .map(|s| display_name(&s.name)),
        }
    }
}

impl Notices {
    fn remember(&mut self, chat: ChatId, id: MsgId) -> bool {
        let key = (chat, id);
        if self.seen.contains(&key)
            || self
                .pending
                .get(&chat)
                .is_some_and(|entry| entry.messages.contains_key(&id))
            || self.lookups.contains(chat, id)
        {
            return false;
        }
        if self.seen_order.len() >= MAX_NOTICE_MESSAGES
            && let Some(oldest) = self.seen_order.pop_front()
        {
            self.seen.remove(&oldest);
        }
        self.seen.insert(key);
        self.seen_order.push_back(key);
        true
    }

    fn queue(&mut self, chat: ChatId, id: MsgId, message: Contributor, now: Instant) {
        if self.pending_count >= MAX_NOTICE_MESSAGES
            || self.pending.len() >= MAX_DIALOGS && !self.pending.contains_key(&chat)
        {
            return;
        }
        let pending = self.pending.entry(chat).or_insert_with(|| Pending {
            at: now + NOTIFY_COALESCE,
            messages: BTreeMap::new(),
        });
        self.pending_count += usize::from(pending.messages.insert(id, message).is_none());
        pending.at = pending.at.min(now + NOTIFY_COALESCE);
        self.next = Some(self.next.map_or(pending.at, |next| next.min(pending.at)));
    }

    fn due(
        &mut self,
        now: Instant,
        wall: i64,
        peer_mute: impl Fn(ChatId) -> Option<PeerMute>,
    ) -> Vec<(ChatId, Notice)> {
        let mut due = Vec::new();
        for (chat, mut pending) in self.pending.extract_if(|_, pending| pending.at <= now) {
            self.pending_count -= pending.messages.len();
            let mute = self.mute_evidence.resolve(chat, peer_mute(chat));
            if mute.is_none() {
                // A gap may have invalidated a mute since this was queued.
                let at = pending.at - NOTIFY_COALESCE;
                let ordinary = pending
                    .messages
                    .extract_if(.., |_, message| !message.mentioned);
                for (id, contributor) in ordinary.filter(|(_, message)| fresh(message.date, wall)) {
                    self.lookups.push(chat, id, Deferred { at, contributor });
                }
            }
            let until = mute.and_then(PeerMute::until);
            pending.messages.retain(|_, message| {
                fresh(message.date, wall)
                    && (message.mentioned
                        || until.unwrap_or_else(|| self.scopes.deadline(message.scope)) <= wall)
            });
            let count = u32::try_from(pending.messages.len()).unwrap_or(u32::MAX);
            if let Some((id, message)) = pending.messages.pop_last() {
                due.push((chat, message.into_notice(chat, id, count)));
            }
        }
        self.next = self.pending.values().map(|p| p.at).min();
        due
    }

    fn discard_pending(
        &mut self,
        scope: Option<ChatId>,
        discard: impl Fn(Option<TopicId>, MsgId) -> bool,
    ) {
        let mut count = self.pending_count;
        let covers = |chat: ChatId| {
            scope.map_or_else(|| chat.kind.has_account_unique_ids(), |scope| scope == chat)
        };
        self.pending.retain(|&chat, pending| {
            if covers(chat) {
                count -= pending.messages.len();
                pending
                    .messages
                    .retain(|&id, message| !discard(message.topic, id));
                count += pending.messages.len();
            }
            !pending.messages.is_empty()
        });
        if count != self.pending_count {
            self.pending_count = count;
            self.next = self.pending.values().map(|pending| pending.at).min();
        }
        self.lookups.discard(covers, discard);
    }

    /// A replacement stream may resume beyond contributors already accepted.
    /// Keep them and their replay ids, and retry any interrupted peer lookup.
    pub(super) fn retire(&mut self, cutoff: BadgeSeq) {
        self.difference = Difference::Idle;
        self.invalidate_peer_mutes(cutoff);
        self.lookups.retire();
    }

    pub(super) fn reset_account(&mut self) {
        *self = Self::default();
    }

    pub(super) fn scope_mute(&self, kind: ChatKind) -> i64 {
        self.scopes.deadline(NotifyScope::of(kind))
    }

    pub(super) fn invalidate_peer_mutes(&mut self, cutoff: BadgeSeq) {
        self.mute_evidence.invalidate(cutoff);
    }

    pub(super) fn confirm_snapshot_mutes(&mut self, rows: &[ChatSummary], stamp: BadgeSeq) {
        if self.mute_evidence.accepts(stamp) {
            for row in rows {
                self.mute_evidence.confirm(row.id, row.muted_until);
            }
        }
    }

    pub(super) fn newest_contributor(&self, thread: ThreadKey) -> Option<MsgId> {
        self.pending
            .get(&thread.chat)
            .and_then(|p| {
                p.messages.iter().rev().find_map(|(&id, contributor)| {
                    (contributor.topic == thread.topic).then_some(id)
                })
            })
            .max(self.lookups.newest_contributor(thread))
    }
}

impl<B: Backend> Actor<B> {
    pub(super) async fn notify_inbox_read(&mut self, thread: ThreadKey, max_id: MsgId) {
        self.notices
            .discard_pending(Some(thread.chat), |topic, id| {
                id <= max_id && (thread.topic.is_none() || thread.topic == topic)
            });
        if self.events.has_notifications() {
            self.emit(Event::InboxRead { thread, max_id }).await;
        }
    }

    pub(super) async fn notify_deleted(&mut self, chat: Option<ChatId>, ids: &[MsgId]) {
        if ids.is_empty() {
            return;
        }
        let deleted: HashSet<_> = ids.iter().copied().collect();
        self.notices
            .discard_pending(chat, |_, id| deleted.contains(&id));
        if self.events.has_notifications() {
            self.emit(Event::NotifyDeleted {
                chat,
                ids: ids.to_vec(),
            })
            .await;
        }
    }

    /// A rebuild can drop the window while pending and deferred contributors
    /// survive. Check their ids against the server tail even in that case.
    pub(super) fn reconcile_notice_tail(
        &mut self,
        thread: ThreadKey,
        range: Option<(MsgId, MsgId)>,
        messages: &[Message],
    ) {
        let Some((lo, top)) = range else {
            return;
        };
        self.notices
            .discard_pending(Some(thread.chat), |topic, id| {
                topic == thread.topic
                    && (lo..=top).contains(&id)
                    && messages
                        .binary_search_by_key(&id, |message| message.id)
                        .is_err()
            });
    }

    pub(super) fn consider_notice(
        &mut self,
        message: &Message,
        route: Route,
        flags: Flags,
        peer: Option<&(ChatKind, String)>,
    ) {
        if self.logout_gate.hides_account() || !self.auth.signed_in {
            return;
        }
        let chat = message.thread.chat;
        let row = self.dialogs.get(chat);
        let listed = row.map(|row| (row.kind, row.title.as_str()));
        let mapped = peer.map(|(kind, title)| (*kind, title.as_str()));
        let Some((kind, title)) = listed.or(mapped).or_else(|| fallback(message)) else {
            return;
        };
        let thread = match route {
            Route::Thread(thread) => thread,
            // An unlisted forum can still be identified by the update's
            // directory. Its omitted topic is General, as in window routing.
            _ if matches!(
                mapped.map_or(kind, |(kind, _)| kind),
                ChatKind::Supergroup { forum: true, .. }
            ) =>
            {
                ThreadKey {
                    chat,
                    topic: Some(message.thread.topic.unwrap_or(TopicId::GENERAL)),
                }
            }
            _ => message.thread,
        };
        let candidate = Candidate {
            message,
            thread,
            kind,
            title,
        };

        let now = unix_now();
        let chat = candidate.thread.chat;
        let id = candidate.message.id;
        if !flags.eligible(message, now) || !self.notices.remember(chat, id) {
            return;
        }
        let scope = NotifyScope::of(candidate.kind);
        let until = if flags.mentioned {
            Some(0)
        } else {
            self.notice_peer_mute(chat).map(|mute| {
                mute.until()
                    .unwrap_or_else(|| self.notices.scopes.deadline(scope))
            })
        };
        if until.is_some_and(|until| until > now) {
            return;
        }
        let at = Instant::now();
        let contributor = candidate.contributor(scope, flags);
        if until.is_none() {
            self.notices
                .lookups
                .push(chat, id, Deferred { at, contributor });
        } else {
            self.notices.queue(chat, id, contributor, at);
        }
    }

    /// A replay can reappear on the live stream after a channel is forgotten.
    pub(super) fn remember_replayed_notice(&mut self, message: &Message, flags: Flags) {
        if flags.eligible(message, unix_now()) {
            self.notices.remember(message.thread.chat, message.id);
        }
    }

    pub(super) fn pump_notice_lookup(&mut self) {
        if self.life.phase != Phase::Live
            || self.gap.pending()
            || self.logout_gate.hides_account()
            || self.queue.len() >= super::MAX_QUEUED
        {
            return;
        }
        if let Some(chat) = self.notices.lookups.start() {
            self.queue.push_back(Job::PeerNotifySettings {
                chat,
                stamps: Stamps::default(),
            });
        }
    }

    pub(super) async fn on_notify_snapshot(
        &mut self,
        target: NotifyTarget,
        stamp: BadgeSeq,
        res: CallResult<Option<i64>>,
    ) {
        if let Err(
            error @ CallError::Backend(BackendError::Migrated | BackendError::Unauthorized),
        ) = res
        {
            if error == CallError::Backend(BackendError::Migrated) {
                self.error(UserError::AccountMigrated).await;
            }
            self.inspect_call(error).await;
            return;
        }
        match target {
            NotifyTarget::Scope(scope) => self.on_scope_snapshot(scope, res).await,
            NotifyTarget::Peer(chat) => self.on_peer_mute_snapshot(chat, stamp, res).await,
        }
    }

    async fn on_peer_mute_snapshot(
        &mut self,
        chat: ChatId,
        stamp: BadgeSeq,
        res: CallResult<Option<i64>>,
    ) {
        if !self.notices.mute_evidence.accepts(stamp) {
            // Retry the queued candidates after the gap reconcile.
            self.notices.lookups.retire();
            return;
        }
        if let Ok(until) = res {
            let row = self
                .dialogs
                .set_peer_mute_snapshot(chat, until, stamp)
                .cloned();
            if self
                .dialogs
                .peer_mute(chat)
                .is_some_and(|mute| mute.until() == until)
            {
                self.notices.mute_evidence.confirm(chat, until);
            }
            if let Some(row) = row {
                self.emit(Event::ChatBumped(row)).await;
                self.cache_dialogs().await;
            }
        }
        self.finish_notice_lookup(chat);
    }

    fn notice_peer_mute(&self, chat: ChatId) -> Option<PeerMute> {
        self.notices
            .mute_evidence
            .resolve(chat, self.dialogs.peer_mute(chat))
    }

    pub(super) fn finish_notice_lookup(&mut self, chat: ChatId) {
        let pending = self.notices.lookups.finish(chat);
        if let Some(mute) = self.notice_peer_mute(chat) {
            let now = unix_now();
            for (id, entry) in pending {
                let until = mute
                    .until()
                    .unwrap_or_else(|| self.notices.scopes.deadline(entry.contributor.scope));
                if until > now || !fresh(entry.contributor.date, now) {
                    continue;
                }
                self.notices.queue(chat, id, entry.contributor, entry.at);
            }
        }
    }

    pub(super) fn notice_wake(&self) -> Option<Instant> {
        self.notices.next.filter(|_| {
            self.life.phase == Phase::Live
                && self.boot.is_none()
                && self.notices.difference == Difference::Idle
        })
    }

    pub(super) async fn flush_notices(&mut self) {
        let dialogs = &self.dialogs;
        let due = self
            .notices
            .due(Instant::now(), unix_now(), |chat| dialogs.peer_mute(chat));
        if !self.logout_gate.hides_account() && self.auth.signed_in {
            for (chat, notice) in due {
                self.emit(Event::Notify(notice)).await;
                self.notices.lookups.after_delivery(chat, Instant::now());
            }
        }
    }

    fn unread_chats(&self) -> usize {
        let now = unix_now();
        self.dialogs
            .as_slice()
            .iter()
            .filter(|chat| chat.has_confirmed_unread() && !chat.muted_at(now))
            .count()
    }

    pub(super) fn begin_catch_up(&mut self) {
        if self.notices.booted && self.notices.catch_up_before.is_none() {
            self.notices.catch_up_before = Some(self.unread_chats());
            self.notices.catch_up_reconciled = false;
        }
    }

    pub(super) async fn flush_catch_up(&mut self) {
        if self.boot.is_none()
            && !self.gap.pending()
            && self.notices.difference == Difference::Idle
            && let Some(before) = self.notices.catch_up_before.take()
            && self.notices.catch_up_reconciled
            && !self.logout_gate.hides_account()
            && self.auth.signed_in
            && self.unread_chats() > before
        {
            self.emit(Event::Notify(Notice::Summary)).await;
        }
    }

    pub(super) async fn on_notify_settings(&mut self, target: NotifyTarget, until: Option<i64>) {
        match target {
            NotifyTarget::Peer(chat) => {
                self.notices.mute_evidence.confirm(chat, until);
                if let Some(row) = self
                    .dialogs
                    .set_mute(chat, until, self.badges.now())
                    .cloned()
                {
                    self.emit(Event::ChatBumped(row)).await;
                    self.cache_dialogs().await;
                }
            }
            NotifyTarget::Scope(scope) => self.apply_scope_mute(scope, until).await,
        }
    }

    async fn apply_scope_mute(&mut self, scope: NotifyScope, until: Option<i64>) {
        let scopes = &mut self.notices.scopes;
        match scope {
            NotifyScope::Users => scopes.users = until.unwrap_or_default(),
            NotifyScope::Groups => scopes.groups = until.unwrap_or_default(),
        }
        let scopes = *scopes;
        if self
            .dialogs
            .set_scope_mutes(|kind| scopes.deadline(NotifyScope::of(kind)))
        {
            self.emit(Event::DialogsReset(self.dialogs.as_slice().to_vec()))
                .await;
        }
    }

    pub(super) async fn on_scope_snapshot(
        &mut self,
        scope: NotifyScope,
        result: CallResult<Option<i64>>,
    ) {
        if let Ok(until) = result {
            self.apply_scope_mute(scope, until).await;
        } else {
            tracing::warn!("account notification settings could not be read");
            // A failed required snapshot leaves the checkpoint pending.
            // A regular startup retains its unmuted fallback.
            if !self.gap.pending() {
                self.apply_scope_mute(scope, None).await;
            }
        }
        self.snapshot_done(Snapshot::NotifySettings {
            applied: result.is_ok(),
        });
    }
}

fn fallback(message: &Message) -> Option<(ChatKind, &str)> {
    match message.thread.chat.kind {
        PeerKind::User => Some((
            ChatKind::Private { contact: false },
            message
                .sender
                .as_ref()
                .map_or("Unknown", |s| s.name.as_str()),
        )),
        PeerKind::Chat => Some((ChatKind::Group, "Unknown")),
        PeerKind::Channel => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MediaKind;
    use crate::rich_text::RichText;
    use proptest::prelude::*;

    fn candidate_message(body: Body) -> Message {
        Message {
            id: MsgId(19),
            thread: ThreadKey::whole(ChatId {
                kind: PeerKind::User,
                id: 71,
            }),
            sender: None,
            date: 1000,
            edit_date: None,
            outgoing: false,
            reply_to: None,
            body,
        }
    }

    proptest! {
        #[test]
        fn no_notice_carries_message_text(content in any::<String>(), caption in any::<bool>()) {
            let rich = RichText::plain(&content);
            let body = if caption { Body::Media { kind: MediaKind::Photo, caption: rich } }
                else { Body::Text(rich) };
            let message = candidate_message(body);
            let mut notices = Notices::default();
            notices.mute_evidence.confirm(message.thread.chat, None);
            let now = Instant::now();
            let candidate = Candidate {
                message: &message,
                thread: message.thread,
                kind: ChatKind::Private { contact: false },
                title: "Peer",
            };
            let contributor = candidate.contributor(NotifyScope::Users, Flags {
                silent: false,
                mentioned: false,
            });
            notices.queue(message.thread.chat, message.id, contributor, now);
            let due = notices.due(now + NOTIFY_COALESCE, message.date, |_| Some(PeerMute::Inherit));
            prop_assert_eq!(due, vec![(message.thread.chat, Notice::Message {
                chat: message.thread.chat,
                topic: None,
                msg: message.id,
                chat_title: "Peer".into(),
                sender: None,
                count: 1,
            })]);
        }
    }
}
