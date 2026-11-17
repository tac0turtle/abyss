//! Forum topic lists in the actor (plan sections 6.1, 6.4, 6.5): opening,
//! paging, the by-id drain, flood waits, the automatic first-page limiter,
//! and the list side of live updates. The lists themselves are the pure
//! [`Topics`] store; this module dispatches the fetches they ask for, feeds
//! results and live updates back, and turns their changes into events.
//!
//! - **Generations.** Every fetch is a [`TopicsCall`]: the list's forum, its
//!   [`crate::store::TopicsGen`] and its dispatch seq. A completion applies
//!   only if the generation is current ([`Topics::current`]) *and* the seq is
//!   the list's in-flight call (`apply_page`/`apply_by_id` answer `Stale`
//!   otherwise), on top of the instance's `ClientGen`. Anything else is
//!   dropped without a trace.
//! - **Bounds.** Pages are single-flight per list with at most one queued,
//!   by-id calls one per list, and only the shown forum dispatches: at most
//!   two topics jobs per retained list are ever queued or running. By-id
//!   calls are also one at a time account-wide and at least
//!   [`TOPICS_CALL_INTERVAL`] apart (counted from the previous one's end).
//! - **Flood waits.** A `FLOOD_WAIT` on any topics call extends the
//!   account-level `not_before`, which survives navigation, eviction,
//!   backoff resets and rebuilds. No automatic topics call is dispatched
//!   before it; a user's `OpenForum`/`LoadTopics` inside it shows the
//!   retained list and is answered `FloodWait` at once.
//! - **Live updates** reach a retained list when they are *received*, also
//!   while a bootstrap buffers them for the windows: the list stamps them on
//!   its own clock, so a snapshot dispatched later wins and one dispatched
//!   earlier conflicts (plan section 6.5, "bootstrap merge").

use std::collections::HashMap;
use std::time::{Duration, Instant as StdInstant};

use super::Actor;
use super::apply::{MAX_ID, MIN_ID};
use super::boot::BootKind;
use super::jobs::{CallError, CallResult, Job, TopicsCall};
use super::lifecycle::Phase;
use crate::api::{ClientGen, Event, LoadTarget, UserError};
use crate::backend::{Backend, BackendError, TopicsById, TopicsCursor, TopicsPage, Update};
use crate::cache::CacheCmd;
use crate::model::{ChatId, Message, MsgId, ThreadKey, TopicId};
use crate::store::{
    AutoRefresh, ByIdCall, ByIdSnapshot, ForumStatus, ListChange, NotBefore, PageCall, PageKind,
    PageSnapshot, Paging, PostClosed, Refresh, Rest, TOPICS_CALL_INTERVAL, Tail, TopicEdit, Topics,
    normalize,
};

/// Most topics jobs queued at once: a page and a by-id call per retained
/// list. A guard only; dispatch never gets near it.
const MAX_TOPICS_JOBS: usize = 2 * crate::store::MAX_TOPIC_LISTS;

/// The actor's topic state: the retained lists and the account-level limits
/// that outlive them.
#[derive(Debug, Default)]
pub(super) struct TopicLists {
    pub(super) lists: Topics,
    /// The next-page cursor of each retained list (at most
    /// `MAX_TOPIC_LISTS`: entries of dropped lists are pruned).
    cursors: HashMap<ChatId, TopicsCursor>,
    /// `topics_not_before`: no automatic topics call before it.
    not_before: NotBefore,
    auto: AutoRefresh,
    /// The instance whose by-id call is in flight, if any: one at a time,
    /// account-wide. A call of an older instance is gone with it.
    by_id_flight: Option<ClientGen>,
    /// No by-id call before this ([`TOPICS_CALL_INTERVAL`] after the last ended).
    by_id_ready: Option<StdInstant>,
}

impl TopicLists {
    /// Forgets the cursors of lists that are no longer retained.
    fn prune_cursors(&mut self) {
        let lists = &self.lists;
        self.cursors.retain(|chat, _| lists.get(*chat).is_some());
    }

    /// The flood wait left, as the error a user load is answered with.
    fn flood(&self, now: StdInstant) -> Option<UserError> {
        self.not_before
            .remaining(now)
            .map(|left| UserError::FloodWait {
                seconds: whole_seconds(left),
            })
    }

    /// A by-id call ended, however (result, error or panic): the next may
    /// start [`TOPICS_CALL_INTERVAL`] later.
    fn by_id_ended(&mut self, now: StdInstant) {
        self.by_id_flight = None;
        self.by_id_ready = Some(now + TOPICS_CALL_INTERVAL);
    }

    /// Whether topics calls must wait for a flood wait to end now.
    pub(super) fn flooded(&self) -> bool {
        !self.not_before.allows(now())
    }

    /// A `FLOOD_WAIT` on a topics call: nothing automatic before it ends.
    fn note_flood(&mut self, err: CallError, now: StdInstant) {
        if let CallError::Backend(BackendError::FloodWait { seconds }) = err {
            let until = now + Duration::from_secs(u64::from(seconds));
            self.not_before.extend(until);
            self.auto.flood(until);
        }
    }
}

/// `left` in whole seconds, rounded up, at least 1.
fn whole_seconds(left: Duration) -> u32 {
    let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
    u32::try_from(secs).unwrap_or(u32::MAX).max(1)
}

/// The store's clock: the runtime's (paused in tests) as a std instant.
fn now() -> StdInstant {
    tokio::time::Instant::now().into_std()
}

/// Which kind of topics fetch a task ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fetch {
    Page,
    ById,
}

/// Whether more topics pages can be loaded.
fn has_more(paging: Paging) -> bool {
    paging == Paging::More
}

impl<B: Backend> Actor<B> {
    /// The open topic of `chat`, if the open thread is one of its topics.
    fn open_topic(&self, chat: ChatId) -> Option<TopicId> {
        self.open.filter(|t| t.chat == chat).and_then(|t| t.topic)
    }

    /// `OpenForum`: the retained list at once (Back is instant), then the
    /// first page, under a new generation. Closes any other forum view.
    pub(super) async fn open_forum(&mut self, chat: ChatId) {
        if let Some(prev) = self.topics.lists.shown().filter(|c| *c != chat) {
            self.topics.lists.close(prev);
        }
        let retained = self.topics.lists.get(chat).is_some();
        let open = self.open_topic(chat);
        // The reset below shows the list as it is after any cut.
        self.topics.lists.open(chat).set_open(open);
        self.topics.prune_cursors();
        self.note_kinds();
        if retained {
            self.emit_reset(chat).await;
            self.emit_paging(chat).await;
        }
        if let Some(wait) = self.topics.flood(now()) {
            return self.load_ended(LoadTarget::Topics(chat), Some(wait)).await;
        }
        if let Some(list) = self.topics.lists.get_mut(chat) {
            list.request_page(PageKind::First);
        }
    }

    /// `CloseForum`: in-flight fetches become stale; the list stays retained.
    pub(super) fn close_forum(&mut self, chat: ChatId) {
        self.topics.lists.close(chat);
        if self.topics.lists.shown().is_none() {
            self.topics.auto.cancel_trailing();
        }
    }

    /// `LoadTopics`: the next page of the shown forum (its first page if the
    /// list has none yet, or wants it again).
    pub(super) async fn load_topics(&mut self, chat: ChatId) {
        let target = LoadTarget::Topics(chat);
        if self.topics.lists.shown() != Some(chat) {
            return self.load_ended(target, None).await;
        }
        if let Some(wait) = self.topics.flood(now()) {
            return self.load_ended(target, Some(wait)).await;
        }
        let has_cursor = self.topics.cursors.contains_key(&chat);
        let Some(list) = self.topics.lists.get_mut(chat) else {
            return self.load_ended(target, None).await;
        };
        let pending = list.page_pending();
        let more = has_more(list.paging());
        // No cursor while more can be paged: no page was applied yet.
        let kind = if list.needs_first_page() || (more && !has_cursor && !pending) {
            PageKind::First
        } else {
            PageKind::Next
        };
        if kind == PageKind::Next && !pending && !more {
            return self.load_ended(target, None).await;
        }
        list.request_page(kind);
    }

    /// The open thread changed from `prev`: the forums involved learn their
    /// open topic (kept past the cap, first in every drain, fetched by id
    /// when unlisted), and opening another chat closes the forum view.
    pub(super) async fn follow_open_thread(&mut self, prev: Option<ThreadKey>) {
        if let Some(shown) = self.topics.lists.shown()
            && self.open.is_some_and(|t| t.chat != shown)
        {
            self.close_forum(shown);
        }
        let mut chats: Vec<ChatId> = prev.into_iter().chain(self.open).map(|t| t.chat).collect();
        chats.dedup();
        for chat in chats {
            let open = self.open_topic(chat);
            let Some(list) = self.topics.lists.get_mut(chat) else {
                continue;
            };
            if list.open() != open {
                let changes = list.set_open(open);
                self.emit_list_changes(chat, changes).await;
            }
        }
    }

    /// Whether automatic topics calls may be dispatched: on a live instance
    /// whose update baseline is established (topic snapshots are taken only
    /// after it, plan section 1).
    fn topics_ready(&self) -> bool {
        self.live.is_some()
            && match self.life.phase {
                Phase::Live => true,
                Phase::Booting => self
                    .boot
                    .as_ref()
                    .is_some_and(|b| b.baseline_deadline().is_none()),
                _ => false,
            }
    }

    /// Dispatches what the shown forum's list asks for: an automatic first
    /// page (rate-limited), a queued page, and a by-id call of the drain.
    /// Runs once per actor loop iteration.
    pub(super) async fn pump_topics(&mut self) {
        let now = now();
        if self.topics.lists.shown().is_none() {
            self.topics.auto.cancel_trailing();
        }
        if !self.topics_ready() || !self.topics.not_before.allows(now) {
            return;
        }
        let Some(chat) = self.topics.lists.shown() else {
            return;
        };
        self.auto_first_page(chat, now);
        self.dispatch_page(chat, now).await;
        self.dispatch_by_id(chat, now);
    }

    /// A list that wants its first page again (discovery overflow, an order
    /// change on a later page) gets it through the account-wide limiter.
    fn auto_first_page(&mut self, chat: ChatId, now: StdInstant) {
        let Some(list) = self.topics.lists.get_mut(chat) else {
            return;
        };
        if list.needs_first_page()
            && !list.page_pending()
            && self.topics.auto.request(now) == Refresh::Now
        {
            list.request_page(PageKind::First);
        }
    }

    /// Topics jobs join the snapshots while a bootstrap or refresh runs, else
    /// the UI queue. `Err` gives the job back when the queue is full.
    fn push_topics_job(&mut self, job: Job) -> Result<(), Job> {
        let queue = if self.boot.is_some() {
            &mut self.priority
        } else {
            &mut self.queue
        };
        let queued = queue.iter().filter(|j| j.is_topics()).count();
        if queue.len() >= super::MAX_QUEUED || queued >= MAX_TOPICS_JOBS {
            return Err(job);
        }
        queue.push_back(job);
        Ok(())
    }

    async fn dispatch_page(&mut self, chat: ChatId, now: StdInstant) {
        let Some(list) = self.topics.lists.get_mut(chat) else {
            return;
        };
        let Some(PageCall { seq, kind }) = list.dispatch_page() else {
            return;
        };
        let call = TopicsCall {
            chat,
            view: list.view(),
            seq,
        };
        let cursor = match kind {
            PageKind::First => None,
            PageKind::Next => self.topics.cursors.get(&chat).cloned(),
        };
        if self
            .push_topics_job(Job::TopicsPage { call, kind, cursor })
            .is_err()
        {
            self.fail_call(call, now);
            self.load_ended(LoadTarget::Topics(chat), Some(UserError::Busy))
                .await;
        }
    }

    fn dispatch_by_id(&mut self, chat: ChatId, now: StdInstant) {
        if self.topics.by_id_flight == Some(self.cgen)
            || self.topics.by_id_ready.is_some_and(|at| now < at)
        {
            return;
        }
        let Some(list) = self.topics.lists.get_mut(chat) else {
            return;
        };
        let Some(ByIdCall { seq, ids }) = list.dispatch_by_id(now) else {
            return;
        };
        let call = TopicsCall {
            chat,
            view: list.view(),
            seq,
        };
        match self.push_topics_job(Job::TopicsById { call, ids }) {
            Ok(()) => self.topics.by_id_flight = Some(self.cgen),
            Err(_) => self.fail_call(call, now),
        }
    }

    /// When [`Self::pump_topics`] next has something to dispatch, if it
    /// waits for time alone: the drain's next eligible call (after the call
    /// interval) or the limiter's trailing first page, never before the
    /// flood wait ends.
    /// Topics jobs held back at admission wake it when the flood wait ends.
    pub(super) fn topics_wake(&self) -> Option<tokio::time::Instant> {
        let now = now();
        let held = self
            .queue
            .iter()
            .chain(&self.priority)
            .any(Job::is_topics)
            .then(|| self.topics.not_before.deadline())
            .flatten()
            .filter(|at| *at > now)
            .map(tokio::time::Instant::from_std);
        self.drain_wake(now).into_iter().chain(held).min()
    }

    fn drain_wake(&self, now: StdInstant) -> Option<tokio::time::Instant> {
        if !self.topics_ready() {
            return None;
        }
        let list = self.topics.lists.get(self.topics.lists.shown()?)?;
        let by_id = (self.topics.by_id_flight != Some(self.cgen))
            .then(|| list.next_by_id(now))
            .flatten()
            .map(|at| self.topics.by_id_ready.map_or(at, |r| at.max(r)));
        let first = (list.needs_first_page() && !list.page_pending())
            .then(|| self.topics.auto.trailing_at().unwrap_or(now));
        let floor = self.topics.not_before.deadline();
        by_id
            .into_iter()
            .chain(first)
            .map(|at| floor.map_or(at, |f| at.max(f)))
            .min()
            .filter(|at| *at > now)
            .map(tokio::time::Instant::from_std)
    }

    /// A fetch that was never made or failed: the list's in-flight record
    /// goes, its due ids back off.
    fn fail_call(&mut self, call: TopicsCall, now: StdInstant) {
        if let Ok(list) = self.topics.lists.current(call.chat, call.view) {
            // `Err`: the list already moved on from this call.
            let _ = list.fail(call.seq, now);
        }
    }

    /// A queued topics job dropped before it ran (rebuild, bootstrap).
    pub(super) fn topics_job_purged(&mut self, call: TopicsCall) {
        self.fail_call(call, now());
    }

    /// A topics task panicked: its call failed, and a page ends the UI's load.
    pub(super) async fn topics_call_panicked(&mut self, call: TopicsCall, fetch: Fetch) {
        let now = now();
        if fetch == Fetch::ById {
            self.topics.by_id_ended(now);
        }
        let current = self.topics.lists.current(call.chat, call.view).is_ok();
        self.fail_call(call, now);
        if current && fetch == Fetch::Page {
            self.load_ended(LoadTarget::Topics(call.chat), Some(UserError::Internal))
                .await;
        }
    }

    /// A topics page completed. Dropped unless its generation and seq are
    /// still the list's; otherwise merged by the store's rule and emitted.
    pub(super) async fn on_topics_page(
        &mut self,
        call: TopicsCall,
        kind: PageKind,
        res: CallResult<TopicsPage>,
    ) {
        let now = now();
        let page = match res {
            Ok(page) => page,
            Err(e) => {
                self.topics.note_flood(e, now);
                let Ok(list) = self.topics.lists.current(call.chat, call.view) else {
                    return;
                };
                if list.fail(call.seq, now).is_ok() {
                    let target = LoadTarget::Topics(call.chat);
                    self.load_ended(target, Some(super::apply::user_error(e)))
                        .await;
                }
                return;
            }
        };
        let TopicsPage {
            order,
            topics,
            next,
            post_closed,
        } = page;
        let Ok(list) = self.topics.lists.current(call.chat, call.view) else {
            return; // a stale generation: dropped
        };
        let discarded = kind == PageKind::Next && order != list.order();
        let has_open = list
            .open()
            .is_some_and(|o| topics.iter().any(|t| t.topic.id == o));
        let snapshot = PageSnapshot {
            order,
            topics,
            rest: if next.is_some() {
                Rest::More
            } else {
                Rest::Last
            },
            post_closed,
        };
        let Ok(changes) = list.apply_page(call.seq, snapshot, now) else {
            return; // not the list's call (any more): dropped
        };
        // Every first page refreshes the open topic; one it lacks, by id.
        if kind == PageKind::First
            && !has_open
            && let Some(open) = list.open().filter(|o| list.get(*o).is_some())
        {
            list.mark_due(open, now);
        }
        if !discarded {
            match next {
                Some(cursor) => self.topics.cursors.insert(call.chat, cursor),
                None => self.topics.cursors.remove(&call.chat),
            };
        }
        self.emit_list_changes(call.chat, changes).await;
        self.emit_paging(call.chat).await;
    }

    /// A by-id result completed. Dropped unless its generation and seq are
    /// still the list's. A fresh `deleted` removes the topic: its open
    /// thread is invalidated and its cached rows go.
    pub(super) async fn on_topics_by_id(&mut self, call: TopicsCall, res: CallResult<TopicsById>) {
        let now = now();
        self.topics.by_id_ended(now);
        let found = match res {
            Ok(found) => found,
            Err(e) => {
                self.topics.note_flood(e, now);
                return self.fail_call(call, now);
            }
        };
        let TopicsById {
            found,
            deleted,
            post_closed,
        } = found;
        let Ok(list) = self.topics.lists.current(call.chat, call.view) else {
            return;
        };
        let result = ByIdSnapshot {
            found,
            deleted: deleted.clone(),
            post_closed,
        };
        let Ok(changes) = list.apply_by_id(call.seq, result, now) else {
            return;
        };
        let gone: Vec<TopicId> = changes
            .iter()
            .filter_map(|c| match c {
                ListChange::Removed(id) if deleted.contains(id) => Some(*id),
                _ => None,
            })
            .collect();
        self.emit_list_changes(call.chat, changes).await;
        for topic in gone {
            self.topic_deleted(call.chat, topic).await;
        }
    }

    /// A topic was removed by a fresh by-id `deleted`: it is no longer the
    /// list's open topic, its window is invalidated and its cached rows are
    /// deleted.
    async fn topic_deleted(&mut self, chat: ChatId, topic: TopicId) {
        if let Some(list) = self.topics.lists.get_mut(chat)
            && list.open() == Some(topic)
        {
            let changes = list.set_open(None);
            self.emit_list_changes(chat, changes).await;
        }
        let thread = ThreadKey {
            chat,
            topic: Some(topic),
        };
        // The delete is queued before the event, so a cache barrier taken
        // after `ThreadInvalidated` orders after it.
        if let Some(cache) = &self.cache {
            let cmd = CacheCmd::Reconcile {
                thread,
                lo: MIN_ID,
                hi: MAX_ID,
                messages: Vec::new(),
            };
            cache.write(cmd).await;
        }
        if self.windows.invalidate(thread).is_some() {
            self.forget_owed(thread);
            self.emit(Event::ThreadInvalidated(thread)).await;
        }
    }

    /// The list side of a live update, applied when it is received (also
    /// while a bootstrap buffers it for the windows): a retained list stamps
    /// it on its clock. Messages of a forum route by `route` (a retained
    /// list means core knows the chat is a forum).
    pub(super) async fn topics_on_update(&mut self, update: &Update) {
        let chat = match update {
            Update::NewMessage(m) | Update::Edited(m) => m.thread.chat,
            Update::Deleted {
                chat: Some(chat), ..
            }
            | Update::TopicEdited { chat, .. } => *chat,
            _ => return,
        };
        if self.topics.lists.get(chat).is_none() {
            return;
        }
        let now = now();
        let routed = |m: &Message| normalize(m.clone(), ForumStatus::Forum).1;
        let tail = match update {
            Update::NewMessage(m) if self.tail_visible(routed(m).thread) => Tail::Visible,
            _ => Tail::Hidden,
        };
        let held = match update {
            Update::Deleted { ids, .. } => self.held_topics(chat, ids),
            _ => Vec::new(),
        };
        let Some(list) = self.topics.lists.get_mut(chat) else {
            return;
        };
        let changes = match update {
            Update::NewMessage(m) => list.on_new_message(&routed(m), tail, now),
            Update::Edited(m) => list.on_edit(&routed(m), now),
            Update::Deleted { ids, .. } => list.on_deleted(ids, &held, now),
            Update::TopicEdited {
                topic,
                title,
                closed,
                visibility,
                ..
            } => {
                let edit = TopicEdit {
                    title: title.clone(),
                    closed: *closed,
                    visibility: *visibility,
                };
                list.on_topic_edited(*topic, edit, now)
            }
            _ => return,
        };
        self.emit_list_changes(chat, changes).await;
    }

    /// The topic of each of `ids` a window of `chat` holds.
    fn held_topics(&self, chat: ChatId, ids: &[MsgId]) -> Vec<(MsgId, TopicId)> {
        let windows: Vec<_> = self
            .windows
            .threads()
            .filter(|t| t.chat == chat)
            .filter_map(|t| self.windows.get(t))
            .collect();
        ids.iter()
            .filter_map(|id| {
                let m = windows.iter().find_map(|w| w.get(*id))?;
                Some((*id, m.thread.topic.unwrap_or(TopicId::GENERAL)))
            })
            .collect()
    }

    /// Bootstrap and light refresh (plan section 6.5). A full bootstrap
    /// (reconcile) drops every list (`ForumInvalidated`) and starts the shown
    /// forum's again; both reset every due topic's backoff and refetch the
    /// shown forum's first page (whose completion asks for the open topic by
    /// id when it lacks it). Inside a flood wait the page waits for its end.
    pub(super) async fn topics_snapshots(&mut self, kind: BootKind) {
        let now = now();
        if kind == BootKind::Full {
            for chat in self.topics.lists.invalidate_all() {
                self.emit(Event::ForumInvalidated(chat)).await;
            }
            self.topics.cursors.clear();
            if let Some(chat) = self.topics.lists.shown() {
                let open = self.open_topic(chat);
                self.topics.lists.open(chat).set_open(open);
            }
            self.note_kinds();
        }
        for chat in self.topics.lists.chats().to_vec() {
            if let Some(list) = self.topics.lists.get_mut(chat) {
                list.reset_backoff(now);
            }
        }
        if !self.topics.not_before.allows(now) {
            return;
        }
        if let Some(list) = self
            .topics
            .lists
            .shown()
            .and_then(|chat| self.topics.lists.get_mut(chat))
        {
            list.request_page(PageKind::First);
        }
    }

    /// Emits a list's changes as events, for the shown forum only (the UI
    /// holds no other list; opening one sends it whole).
    async fn emit_list_changes(&self, chat: ChatId, changes: Vec<ListChange>) {
        if self.topics.lists.shown() != Some(chat) {
            return;
        }
        let Some(list) = self.topics.lists.get(chat) else {
            return;
        };
        let view = list.view();
        let can_post_closed = list.post_closed() == PostClosed::Allowed;
        for change in changes {
            let event = match change {
                ListChange::Reset { order, topics } => Event::TopicsReset {
                    chat,
                    view,
                    order,
                    topics,
                    can_post_closed,
                },
                ListChange::Appended(topics) => Event::TopicsAppended { chat, view, topics },
                ListChange::Bumped(topic) => Event::TopicBumped { chat, topic },
                ListChange::Removed(topic) => Event::TopicRemoved { chat, topic },
            };
            self.emit(event).await;
        }
    }

    /// The retained list of `chat` as a `TopicsReset` of its generation.
    async fn emit_reset(&self, chat: ChatId) {
        let Some(list) = self.topics.lists.get(chat) else {
            return;
        };
        let reset = ListChange::Reset {
            order: list.order(),
            topics: list.emitted(),
        };
        self.emit_list_changes(chat, vec![reset]).await;
    }

    async fn emit_paging(&self, chat: ChatId) {
        let Some(list) = self.topics.lists.get(chat) else {
            return;
        };
        let event = Event::TopicsPaging {
            chat,
            view: list.view(),
            has_more: has_more(list.paging()),
        };
        self.emit(event).await;
    }
}
