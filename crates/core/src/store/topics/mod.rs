//! Bounded, ordered topic lists of forums (plan sections 6.1, 6.3, 6.4).
//!
//! Pure state: core's actor dispatches the fetches a [`TopicList`] asks for,
//! feeds their results and live events back, and turns the returned
//! [`ListChange`]s into events. Time is always passed in, never read.

mod apply;
mod change;
mod due;
mod entry;
mod lists;
mod live;
mod order;
mod stamp;
mod tombstones;

#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

pub use change::ListChange;
pub use due::{MAX_DUE_BACKOFF, MAX_TOPICS_BY_ID};
pub use lists::{MAX_TOPIC_LISTS, Topics, TopicsGen};
pub use live::{Tail, TopicEdit, UnreadTrigger};
pub use stamp::FetchSeq;
pub use tombstones::MAX_TOMBSTONES;

use self::entry::TopicEntry;
use self::tombstones::Tombstones;
use super::dialogs::Rest;
use crate::model::{Topic, TopicGone, TopicId, TopicOrder, TopicSnapshot};

/// Most topics a list holds, plus the open topic if it would otherwise be cut.
pub const MAX_TOPICS_PER_FORUM: usize = 500;
/// Most unknown topic ids waiting for a by-id fetch, per list.
pub const MAX_DISCOVERY: usize = 64;
/// Entries a list may hold: the cap plus the open topic.
pub(super) const MAX_ENTRIES: usize = MAX_TOPICS_PER_FORUM + 1;
/// Topics per page request (`limit`).
pub const TOPICS_PAGE: u32 = 50;
/// Least time between by-id calls (shown forum only).
pub const TOPICS_CALL_INTERVAL: Duration = Duration::from_secs(1);

/// Which page a fetch asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageKind {
    First,
    Next,
}

/// Whether more topics can be paged in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Paging {
    More,
    /// The server's list ended.
    Complete,
    /// [`MAX_TOPICS_PER_FORUM`] reached: paging stops ("topic limit for this view").
    Capped,
}

/// Whether the user may post in closed topics (`creator || admin_rights.manage_topics`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostClosed {
    Allowed,
    Refused,
}

/// Whether a list wants its first page refetched (discovery overflow, an
/// order change on a later page). Automatic refreshes go through the
/// account-wide [`crate::store::AutoRefresh`] limiter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FirstPage {
    Fresh,
    Needed,
}

/// How an operation's changes must relate to the consumer's replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Replay {
    /// The consumer's rule reproduces the store's steps exactly.
    Exact,
    /// A later page keeps server order, which the rank rule may not
    /// reproduce: a reset is then deliberate.
    ServerOrder,
}

/// What became of an unknown topic id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discovered {
    /// Queued for a by-id fetch.
    Queued,
    /// Listed, or already being fetched.
    Known,
    /// The discovery set overflowed and was cleared; the first page is needed.
    Overflowed,
}

/// A page fetch to dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageCall {
    pub seq: FetchSeq,
    pub kind: PageKind,
}

/// A by-id fetch to dispatch (at most [`MAX_TOPICS_BY_ID`] ids).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ByIdCall {
    pub seq: FetchSeq,
    pub ids: Vec<TopicId>,
}

/// A topics page as the backend returned it.
#[derive(Clone, Debug)]
pub struct PageSnapshot {
    /// The order the server reported for this page.
    pub order: TopicOrder,
    pub topics: Vec<TopicSnapshot>,
    pub rest: Rest,
    pub post_closed: PostClosed,
}

/// A by-id result as the backend returned it.
#[derive(Clone, Debug)]
pub struct ByIdSnapshot {
    pub found: Vec<TopicSnapshot>,
    pub deleted: Vec<TopicId>,
    pub post_closed: PostClosed,
}

/// One forum's topics, in display order, with the merge and refetch state.
#[derive(Debug)]
pub struct TopicList {
    view: TopicsGen,
    order: TopicOrder,
    entries: Vec<TopicEntry>,
    paging: Paging,
    post_closed: PostClosed,
    /// The dispatch seq of the result that set `post_closed`: an older
    /// result completing later doesn't overwrite it.
    post_closed_seen: FetchSeq,
    /// The list's clock: advanced on every dispatch.
    seq: FetchSeq,
    page: Option<PageCall>,
    queued_page: Option<PageKind>,
    by_id: Option<ByIdCall>,
    /// Unknown ids, disposable.
    discovery: BTreeSet<TopicId>,
    first_page: FirstPage,
    /// The last non-open id the drain took (due state is on the entries).
    drain_cursor: Option<TopicId>,
    tombstones: Tombstones,
    /// The open topic: never cut by the cap, first in every drain.
    open: Option<TopicId>,
    /// When the open topic, while unlisted, may be fetched by id again:
    /// it backs off when a call fails or leaves it unanswered.
    open_retry: due::Retry,
    /// The clock at the last live event that touched a topic not in the
    /// list (or a deletion, whose ids may be such a topic's): results
    /// dispatched at or before it can't be trusted for topics they add.
    unlisted_change: FetchSeq,
}

impl TopicList {
    pub fn new(view: TopicsGen) -> Self {
        Self {
            view,
            order: TopicOrder::LastActivity,
            entries: Vec::new(),
            paging: Paging::More,
            post_closed: PostClosed::Refused,
            post_closed_seen: FetchSeq::default(),
            seq: FetchSeq::default(),
            page: None,
            queued_page: None,
            by_id: None,
            discovery: BTreeSet::new(),
            first_page: FirstPage::Needed,
            drain_cursor: None,
            tombstones: Tombstones::default(),
            open: None,
            open_retry: due::Retry::default(),
            unlisted_change: FetchSeq::default(),
        }
    }

    pub fn view(&self) -> TopicsGen {
        self.view
    }

    pub fn order(&self) -> TopicOrder {
        self.order
    }

    pub fn paging(&self) -> Paging {
        self.paging
    }

    /// A forum badge can be derived only from the entire settled list.
    /// Partial/capped lists, outstanding discovery and pending refreshes
    /// cannot prove that unseen topics are read. Hidden topics count too.
    pub fn confirmed_unread_topics(&self) -> Option<u32> {
        if self.paging != Paging::Complete
            || self.first_page != FirstPage::Fresh
            || self.page.is_some()
            || !self.discovery.is_empty()
            || self.unlisted_open().is_some()
            || self.entries.iter().any(|e| !e.settled() || e.due.is_some())
        {
            return None;
        }
        u32::try_from(self.entries.iter().filter(|e| e.topic.unread > 0).count()).ok()
    }

    pub fn post_closed(&self) -> PostClosed {
        self.post_closed
    }

    pub fn needs_first_page(&self) -> bool {
        self.first_page == FirstPage::Needed
    }

    /// The pinned topics changed (`TopicsPinned`): the list wants its first
    /// page again, which the automatic limiter fetches for the shown forum
    /// and the next open fetches for any other.
    pub fn want_first_page(&mut self) {
        self.first_page = FirstPage::Needed;
    }

    /// Whether a page is in flight or queued.
    pub fn page_pending(&self) -> bool {
        self.page.is_some() || self.queued_page.is_some()
    }

    /// Every retained topic, hidden ones included (routing, the closed check).
    pub fn get(&self, id: TopicId) -> Option<&Topic> {
        self.entry(id).map(|e| &e.topic)
    }

    /// The emitted topics: shown ones, in display order.
    pub fn topics(&self) -> impl Iterator<Item = &Topic> {
        self.entries.iter().filter(|e| e.shown()).map(|e| &e.topic)
    }

    pub fn emitted(&self) -> Vec<Topic> {
        self.topics().cloned().collect()
    }

    /// Retained topics, hidden ones included.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn open(&self) -> Option<TopicId> {
        self.open
    }

    /// Sets the open topic. An unlisted one is fetched by id in the next
    /// call's reserved slot. Closing one that sat past the cap cuts the
    /// lowest-ranked topic.
    /// A new open intent may fetch an unlisted open topic at once.
    pub fn set_open(&mut self, open: Option<TopicId>) -> Vec<ListChange> {
        let before = self.shown_keys();
        self.open = open;
        // Discovery keeps an open topic's id; dispatch skips it while it
        // is open (its slot has its own backoff), and it is fetched as
        // discovery again once another topic is open.
        self.open_retry = due::Retry::default();
        let cut = self.enforce_cap();
        let changes = cut
            .into_iter()
            .map(|id| ListChange::Removed(id, TopicGone::Evicted))
            .collect();
        self.checked(&before, changes, Replay::Exact)
    }

    /// Asks for a page. Pages are single-flight with at most one queued; a
    /// queued first page absorbs a next-page request.
    pub fn request_page(&mut self, kind: PageKind) {
        self.queued_page = match (self.queued_page, kind) {
            (Some(PageKind::First), _) | (_, PageKind::First) => Some(PageKind::First),
            _ => Some(PageKind::Next),
        };
    }

    /// The page fetch to dispatch now, if one is queued and none is in flight.
    pub fn dispatch_page(&mut self) -> Option<PageCall> {
        if self.page.is_some() {
            return None;
        }
        let kind = self.queued_page.take()?;
        if kind == PageKind::Next && self.paging != Paging::More {
            return None;
        }
        self.seq = self.seq.next();
        let call = PageCall {
            seq: self.seq,
            kind,
        };
        if kind == PageKind::First {
            self.first_page = FirstPage::Fresh;
        }
        self.page = Some(call);
        Some(call)
    }

    /// The by-id fetch to dispatch now: the open topic in its reserved slot,
    /// due topics round-robin, then discovery ids in any room left. `None`
    /// when one is in flight or nothing is eligible. Rate limits, flood waits
    /// and "shown forum only" are the caller's.
    pub fn dispatch_by_id(&mut self, now: Instant) -> Option<ByIdCall> {
        if self.by_id.is_some() {
            return None;
        }
        let seq = self.seq.next();
        let mut ids = Vec::with_capacity(MAX_TOPICS_BY_ID);
        if let Some(open) = self.unlisted_open()
            && self.open_retry.eligible(now)
        {
            ids.push(open);
        }
        let room = MAX_TOPICS_BY_ID - ids.len();
        ids.extend(self.take_due(now, room, seq));
        let room = MAX_TOPICS_BY_ID - ids.len();
        let discovered: Vec<TopicId> = self
            .discovery
            .iter()
            .filter(|id| Some(**id) != self.open && !ids.contains(id))
            .take(room)
            .copied()
            .collect();
        for id in discovered {
            self.discovery.remove(&id);
            ids.push(id);
        }
        if ids.is_empty() {
            return None;
        }
        self.seq = seq;
        let call = ByIdCall { seq, ids };
        self.by_id = Some(call.clone());
        Some(call)
    }

    /// A fetch failed. Its due ids back off, its discovery ids return.
    pub fn fail(&mut self, seq: FetchSeq, now: Instant) -> Result<(), super::Stale> {
        if self.page.is_some_and(|p| p.seq == seq) {
            self.page = None;
        } else if let Some(call) = self.by_id.take_if(|c| c.seq == seq) {
            for id in call.ids {
                if Some(id) == self.unlisted_open() {
                    self.open_retry.back_off(now);
                } else if self.is_due(id) {
                    self.back_off(id, now);
                } else {
                    self.rediscover(id);
                }
            }
        } else {
            return Err(super::Stale);
        }
        self.prune_tombstones();
        Ok(())
    }

    /// Queues an unknown topic id for a by-id fetch. On overflow the set is
    /// cleared and the list wants its first page: nothing required is lost,
    /// because an unknown topic has no entry to keep correct.
    pub fn discover(&mut self, id: TopicId) -> Discovered {
        let in_flight = self.by_id.as_ref().is_some_and(|c| c.ids.contains(&id));
        if self.entry(id).is_some() || in_flight || self.discovery.contains(&id) {
            return Discovered::Known;
        }
        self.discovery.insert(id);
        if self.discovery.len() > MAX_DISCOVERY {
            self.discovery.clear();
            self.first_page = FirstPage::Needed;
            return Discovered::Overflowed;
        }
        Discovered::Queued
    }

    /// A live event touched a topic the list doesn't hold.
    fn note_unlisted(&mut self) {
        self.unlisted_change = self.seq;
    }

    /// The open topic, if the list doesn't hold it.
    fn unlisted_open(&self) -> Option<TopicId> {
        self.open.filter(|o| self.entry(*o).is_none())
    }

    fn rediscover(&mut self, id: TopicId) {
        if self.entry(id).is_none() && self.discovery.len() < MAX_DISCOVERY {
            self.discovery.insert(id);
        }
    }

    fn position(&self, id: TopicId) -> Option<usize> {
        self.entries.iter().position(|e| e.id() == id)
    }

    fn entry(&self, id: TopicId) -> Option<&TopicEntry> {
        self.entries.iter().find(|e| e.id() == id)
    }

    fn entry_mut(&mut self, id: TopicId) -> Option<&mut TopicEntry> {
        self.entries.iter_mut().find(|e| e.id() == id)
    }

    fn oldest_in_flight(&self) -> Option<FetchSeq> {
        let page = self.page.map(|p| p.seq);
        let by_id = self.by_id.as_ref().map(|c| c.seq);
        page.into_iter().chain(by_id).min()
    }

    fn prune_tombstones(&mut self) {
        self.tombstones.prune(self.oldest_in_flight());
    }

    /// Records a removal at `seq`. At the cap, every in-flight fetch older
    /// than the oldest tombstone is invalidated and re-queued first, so
    /// discarding that tombstone can't let an older result re-insert its topic.
    fn tombstone(&mut self, id: TopicId, seq: FetchSeq) {
        if self.tombstones.is_full()
            && let Some(oldest) = self.tombstones.oldest()
        {
            self.invalidate_older_than(oldest);
            self.prune_tombstones();
        }
        self.tombstones.record(id, seq);
    }

    fn invalidate_older_than(&mut self, seq: FetchSeq) {
        if let Some(page) = self.page.take_if(|p| p.seq < seq) {
            self.request_page(page.kind);
        }
        if let Some(call) = self.by_id.take_if(|c| c.seq < seq) {
            self.requeue(Some(call.seq));
            for id in call.ids {
                if !self.is_due(id) {
                    self.rediscover(id);
                }
            }
        }
    }

    /// Topics other than the open one.
    fn capped_len(&self) -> usize {
        self.entries.len() - usize::from(self.open.is_some_and(|o| self.entry(o).is_some()))
    }

    /// Cuts the lowest-ranked topics past [`MAX_TOPICS_PER_FORUM`], never the
    /// open one, and stops paging. Returns the shown topics it cut. Also
    /// releases capacity the list grew past its bound while changing.
    fn enforce_cap(&mut self) -> Vec<TopicId> {
        let mut cut = Vec::new();
        while self.capped_len() > MAX_TOPICS_PER_FORUM {
            let Some(at) = self.entries.iter().rposition(|e| Some(e.id()) != self.open) else {
                break;
            };
            let gone = self.entries.remove(at);
            if gone.shown() {
                cut.push(gone.id());
            }
        }
        if self.capped_len() >= MAX_TOPICS_PER_FORUM {
            self.paging = Paging::Capped;
        }
        if self.entries.capacity() > MAX_ENTRIES {
            self.entries.shrink_to(MAX_ENTRIES);
        }
        cut
    }

    /// Applies a result's posting permission unless a later-dispatched
    /// result already set it.
    fn set_post_closed(&mut self, seq: FetchSeq, post_closed: PostClosed) {
        if seq > self.post_closed_seen {
            self.post_closed_seen = seq;
            self.post_closed = post_closed;
        }
    }

    fn shown_keys(&self) -> Vec<order::Key> {
        self.topics().map(order::Key::of).collect()
    }

    /// `changes` if a consumer that held the emitted list `before` reaches
    /// the current one by applying them (the [`ListChange`] contract);
    /// otherwise their semantic removals (`Deleted`, `Hidden`) followed by
    /// one reset. For [`Replay::Exact`] operations a divergence is a bug:
    /// debug builds (and so tests) fail on it, release builds repair it
    /// with the reset.
    fn checked(
        &self,
        before: &[order::Key],
        changes: Vec<ListChange>,
        replay: Replay,
    ) -> Vec<ListChange> {
        if changes
            .iter()
            .any(|c| matches!(c, ListChange::Reset { .. }))
        {
            return changes;
        }
        let mut rows = before.to_vec();
        let mut order = self.order;
        for change in &changes {
            change::apply_rows(change, &mut order, &mut rows);
        }
        let shown = self.shown_keys();
        if rows == shown {
            return changes;
        }
        if replay == Replay::Exact {
            debug_assert_eq!(rows, shown, "incremental replay diverged");
        }
        changes
            .into_iter()
            .filter(|c| {
                matches!(
                    c,
                    ListChange::Removed(_, TopicGone::Deleted | TopicGone::Hidden)
                )
            })
            .chain([self.reset()])
            .collect()
    }

    fn reset(&self) -> ListChange {
        ListChange::Reset {
            order: self.order,
            topics: self.emitted(),
        }
    }

    /// Bumps the generation (open, close): in-flight results become stale.
    fn set_view(&mut self, view: TopicsGen) {
        self.view = view;
        self.page = None;
        self.by_id = None;
        self.queued_page = None;
        self.requeue(None);
        self.prune_tombstones();
    }
}
