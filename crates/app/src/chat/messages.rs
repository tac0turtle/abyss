//! The open chat's message window as the UI holds it: a mirror of core's
//! sliding window (plan sections 8 and 16), plus the row ops that keep a
//! GPUI `ListState` in step with it, so prepending keeps the scroll anchor.

use std::collections::VecDeque;
use std::ops::Range;
use std::time::Instant;

use abyss_core::api::{HistoryDir, UserError};
use abyss_core::model::{Message, MsgId, ThreadKey};
use abyss_core::store::{MAX_MESSAGES_PER_CHAT, ViewGen};

use super::retry::{Backoff, earliest};

/// Rows from either end of the window at which the next page is requested.
pub(crate) const LOAD_MARGIN: usize = 10;

/// A change to the list rows, recorded in application order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RowOp {
    /// `ListState::splice(range, count)`: rows replaced by `count` new ones.
    /// The row after them may change its day separator, so callers remeasure
    /// it too.
    Splice { range: Range<usize>, count: usize },
    /// `ListState::remeasure_items(ix..ix + 1)`: same row, new content.
    Remeasure(usize),
}

/// What a change did to the rows.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Applied {
    /// Stale or for another view: nothing changed.
    Ignored,
    /// A new view: the rows were replaced.
    Reset,
    Changed(Vec<RowOp>),
}

/// A history event's payload.
#[derive(Debug)]
pub(crate) struct Batch {
    pub(crate) view: ViewGen,
    pub(crate) dir: HistoryDir,
    pub(crate) messages: Vec<Message>,
    pub(crate) removed: Vec<MsgId>,
    pub(crate) has_more: bool,
}

/// Row ops with adjacent inserts and removals coalesced.
#[derive(Debug, Default)]
struct Splices(Vec<RowOp>);

impl Splices {
    fn insert(&mut self, at: usize) {
        if let Some(RowOp::Splice { range, count }) = self.0.last_mut()
            && range.start == range.end
            && at == range.start + *count
        {
            *count += 1;
            return;
        }
        self.0.push(RowOp::Splice {
            range: at..at,
            count: 1,
        });
    }

    fn remove(&mut self, at: usize) {
        if let Some(RowOp::Splice { range, count: 0 }) = self.0.last_mut()
            && at == range.start
        {
            range.end += 1;
            return;
        }
        self.0.push(RowOp::Splice {
            range: at..at + 1,
            count: 0,
        });
    }

    fn replace(&mut self, at: usize) {
        self.0.push(RowOp::Remeasure(at));
    }
}

/// Which end of the window to trim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Oldest,
    Newest,
}

/// One direction's history loading.
#[derive(Clone, Copy, Debug, Default)]
struct DirLoad {
    /// Core admitted a load and has not answered it: `History` in this
    /// direction or `LoadEnded` (a new view forgets it). A `Reconciled`
    /// does not end it; core still runs a load it deferred.
    pending: bool,
    /// Core said there was nothing to load: wait for the next history
    /// event of the window before asking again, so a disagreement about
    /// what is left cannot loop.
    parked: bool,
    /// Failed loads: bounded, spaced retries.
    retry: Backoff,
}

impl DirLoad {
    fn may_ask(&self, now: Instant) -> bool {
        !self.pending && !self.parked && self.retry.ready(now)
    }

    /// Core answered this direction with history.
    fn answered(&mut self) {
        *self = Self::default();
    }

    fn ended(&mut self, error: Option<UserError>, now: Instant) {
        if !self.pending {
            return; // not ours (a load core started itself)
        }
        self.pending = false;
        match error {
            None => self.parked = true,
            Some(_) => self.retry.failed(now),
        }
    }

    fn due(&self, now: Instant) -> Option<Instant> {
        (!self.pending && !self.parked)
            .then(|| self.retry.due(now))
            .flatten()
    }
}

/// History loads asked of core, per direction.
#[derive(Clone, Copy, Debug, Default)]
struct Pending {
    older: DirLoad,
    newer: DirLoad,
}

impl Pending {
    fn dir(&mut self, dir: HistoryDir) -> Option<&mut DirLoad> {
        match dir {
            HistoryDir::Older => Some(&mut self.older),
            HistoryDir::Newer => Some(&mut self.newer),
            HistoryDir::Tail | HistoryDir::Around => None,
        }
    }

    /// The window changed: a parked direction may ask again.
    fn unpark(&mut self) {
        self.older.parked = false;
        self.newer.parked = false;
    }
}

#[derive(Debug)]
pub(crate) struct MessageWindow {
    thread: ThreadKey,
    /// Core's view this mirrors; `None` until the first tail arrives.
    view: Option<ViewGen>,
    /// Ascending by id, unique, at most [`MAX_MESSAGES_PER_CHAT`].
    messages: VecDeque<Message>,
    has_older: bool,
    has_newer: bool,
    pending: Pending,
}

impl MessageWindow {
    pub(crate) fn new(thread: ThreadKey) -> Self {
        Self {
            thread,
            view: None,
            messages: VecDeque::new(),
            has_older: true,
            has_newer: false,
            pending: Pending::default(),
        }
    }

    pub(crate) fn thread(&self) -> ThreadKey {
        self.thread
    }

    pub(crate) fn len(&self) -> usize {
        self.messages.len()
    }

    pub(crate) fn get(&self, ix: usize) -> Option<&Message> {
        self.messages.get(ix)
    }

    pub(crate) fn contains(&self, id: MsgId) -> bool {
        self.find(id).is_ok()
    }

    /// The row of message `id`, if the window holds it.
    pub(crate) fn index_of(&self, id: MsgId) -> Option<usize> {
        self.find(id).ok()
    }

    pub(crate) fn by_id(&self, id: MsgId) -> Option<&Message> {
        self.messages.get(self.find(id).ok()?)
    }

    pub(crate) fn newest(&self) -> Option<MsgId> {
        self.messages.back().map(|m| m.id)
    }

    /// The row of the first message above `after`, if the window holds one.
    pub(crate) fn first_above(&self, after: MsgId) -> Option<usize> {
        let ix = self.messages.partition_point(|m| m.id <= after);
        (ix < self.messages.len()).then_some(ix)
    }

    /// The row the divider of read position `after` sits on: the first
    /// message above it, when the window also holds what comes right
    /// before that message (a message at or below `after`, or the start
    /// of history). Above the window's oldest message the divider's place
    /// is unknown until older history loads.
    pub(crate) fn divider_row(&self, after: MsgId) -> Option<usize> {
        self.first_above(after)
            .filter(|ix| *ix > 0 || !self.has_older)
    }

    /// Core's view this window mirrors, once a tail arrived.
    pub(crate) fn view(&self) -> Option<ViewGen> {
        self.view
    }

    /// Whether core has answered with this thread's history at least once.
    pub(crate) fn is_loaded(&self) -> bool {
        self.view.is_some()
    }

    pub(crate) fn has_older(&self) -> bool {
        self.has_older
    }

    /// Detached from the live tail (scrolled far back).
    pub(crate) fn has_newer(&self) -> bool {
        self.has_newer
    }

    fn find(&self, id: MsgId) -> Result<usize, usize> {
        self.messages.binary_search_by_key(&id, |m| m.id)
    }

    /// Applies a `History` event. A newer view replaces the window; an older
    /// one is stale. Until the first view arrives only one that starts a
    /// view (a tail, or a page around a read position) is accepted, so late
    /// events from a previous opening of the thread cannot seed it.
    pub(crate) fn apply(&mut self, b: Batch) -> Applied {
        match self.view {
            None if !matches!(b.dir, HistoryDir::Tail | HistoryDir::Around) => {
                return Applied::Ignored;
            }
            Some(cur) if b.view < cur => return Applied::Ignored,
            _ => {}
        }
        let reset = self.view != Some(b.view);
        if reset {
            self.messages.clear();
            self.view = Some(b.view);
            self.has_older = true;
            self.has_newer = false;
            // Loads asked of the old view are over with it.
            self.pending = Pending::default();
        }
        // An answer in a direction ends its load; core queues a load asked
        // while another runs, so the other direction is still answered.
        // Any change to the window lets a parked direction ask again.
        if let Some(load) = self.pending.dir(b.dir) {
            load.answered();
        }
        self.pending.unpark();
        let mut splices = Splices::default();
        let trimmed = !b.removed.is_empty();
        self.remove_ids(&b.removed, &mut splices);
        for m in b.messages {
            self.upsert(m, &mut splices);
        }
        match b.dir {
            HistoryDir::Tail => {
                self.has_older = b.has_more;
                self.has_newer = false;
            }
            HistoryDir::Older => {
                self.has_older = b.has_more;
                self.has_newer |= trimmed;
            }
            HistoryDir::Newer => {
                self.has_newer = b.has_more;
                self.has_older |= trimmed;
            }
            // A detached window around the read position of an anchored
            // open (`OpenThreadAt`): `LoadNewer` pages towards the tail.
            HistoryDir::Around => {
                self.has_older = true;
                self.has_newer = b.has_more;
            }
        }
        let end = if b.dir == HistoryDir::Older {
            End::Newest
        } else {
            End::Oldest
        };
        self.trim(end, &mut splices);
        if reset {
            Applied::Reset
        } else {
            Applied::Changed(splices.0)
        }
    }

    fn upsert(&mut self, m: Message, splices: &mut Splices) {
        match self.find(m.id) {
            Ok(at) => {
                if let Some(slot) = self.messages.get_mut(at) {
                    *slot = m;
                    splices.replace(at);
                }
            }
            Err(at) => {
                self.messages.insert(at, m);
                splices.insert(at);
            }
        }
    }

    fn remove_ids(&mut self, ids: &[MsgId], splices: &mut Splices) {
        for id in ids {
            if let Ok(at) = self.find(*id) {
                self.messages.remove(at);
                splices.remove(at);
            }
        }
    }

    /// Defensive bound: core already trims to the same cap.
    fn trim(&mut self, end: End, splices: &mut Splices) {
        while self.messages.len() > MAX_MESSAGES_PER_CHAT {
            match end {
                End::Oldest => {
                    self.messages.pop_front();
                    self.has_older = true;
                    splices.remove(0);
                }
                End::Newest => {
                    self.messages.pop_back();
                    self.has_newer = true;
                    splices.remove(self.messages.len());
                }
            }
        }
    }

    /// A live edit of a message in the window. Matched by chat: forum chats
    /// are still shown as one stream, whatever topic the server stated.
    pub(crate) fn edit(&mut self, m: Message) -> Applied {
        if m.thread.chat != self.thread.chat {
            return Applied::Ignored;
        }
        match self.find(m.id) {
            Ok(at) => {
                let mut splices = Splices::default();
                self.upsert(m, &mut splices);
                debug_assert_eq!(splices.0.first(), Some(&RowOp::Remeasure(at)));
                Applied::Changed(splices.0)
            }
            Err(_) => Applied::Ignored,
        }
    }

    pub(crate) fn delete(&mut self, ids: &[MsgId]) -> Applied {
        let mut splices = Splices::default();
        self.remove_ids(ids, &mut splices);
        Applied::Changed(splices.0)
    }

    /// Core cleared the window (rebuild); the next tail carries a new view.
    pub(crate) fn invalidate(&mut self) -> Applied {
        self.messages.clear();
        self.has_older = true;
        self.has_newer = false;
        self.pending = Pending::default();
        Applied::Reset
    }

    /// A fresh trigger (the user's Retry, reconnection, activation): a
    /// direction that waits out a backoff, gave up or is parked may ask
    /// again at once. A load core still runs stays pending.
    pub(crate) fn retry_now(&mut self) {
        for load in [&mut self.pending.older, &mut self.pending.newer] {
            if !load.pending {
                *load = DirLoad::default();
            }
        }
    }

    /// Automatic recovery: a direction whose retries gave up may ask again;
    /// one that waits out a backoff keeps its deadline, and a parked one
    /// stays parked (see [`Backoff::restart_if_exhausted`]).
    pub(crate) fn recover(&mut self) {
        for load in [&mut self.pending.older, &mut self.pending.newer] {
            if !load.pending {
                load.retry.restart_if_exhausted();
            }
        }
    }

    /// Whether either direction stopped retrying its failed loads.
    pub(crate) fn exhausted(&self) -> bool {
        self.pending.older.retry.gave_up() || self.pending.newer.retry.gave_up()
    }

    /// Whether a direction with history left stopped retrying its failed
    /// loads (until [`Self::retry_now`]).
    pub(crate) fn gave_up(&self) -> bool {
        (self.has_older && self.pending.older.retry.gave_up())
            || (self.has_newer && self.pending.newer.retry.gave_up())
    }

    /// A load in `dir` ended without history (`Event::LoadEnded`): nothing
    /// to load parks the direction until the window changes; a failure
    /// waits out a backoff.
    pub(crate) fn load_ended(&mut self, dir: HistoryDir, error: Option<UserError>, now: Instant) {
        if let Some(load) = self.pending.dir(dir) {
            load.ended(error, now);
        }
    }

    /// When a failed load may be asked again, if one waits past `now`.
    pub(crate) fn retry_due(&self, now: Instant) -> Option<Instant> {
        earliest(self.pending.older.due(now), self.pending.newer.due(now))
    }

    /// Whether to ask for older history with row `first_visible` on screen.
    pub(crate) fn wants_older(&self, first_visible: usize, now: Instant) -> bool {
        self.is_loaded()
            && self.has_older
            && self.pending.older.may_ask(now)
            && first_visible < LOAD_MARGIN
    }

    /// Whether to ask for newer history with message row `last_visible` on
    /// screen (only while detached from the live tail).
    pub(crate) fn wants_newer(&self, last_visible: usize, now: Instant) -> bool {
        self.is_loaded()
            && self.has_newer
            && self.pending.newer.may_ask(now)
            && last_visible.saturating_add(LOAD_MARGIN) >= self.messages.len()
    }

    /// Core admitted a load in `dir`; no other until it is answered.
    pub(crate) fn requested(&mut self, dir: HistoryDir) {
        if let Some(load) = self.pending.dir(dir) {
            load.pending = true;
        }
    }
}

#[cfg(test)]
mod tests;
