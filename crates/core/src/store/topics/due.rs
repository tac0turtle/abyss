//! Required refetches (plan section 6.4): due state lives on the entries,
//! with backoff, and is drained round-robin in stable id order, independent
//! of display order.

use std::time::{Duration, Instant};

use super::TopicList;
use super::entry::TopicEntry;
use super::stamp::FetchSeq;
use crate::model::TopicId;

/// Most topic ids in one by-id call (`MAX_TOPICS_BY_ID`).
pub const MAX_TOPICS_BY_ID: usize = 20;
/// The longest wait between refetches of a topic that keeps conflicting.
pub const MAX_DUE_BACKOFF: Duration = Duration::from_secs(30);

/// The wait before attempt `step`: 0 s, then 1 s, 2 s, 4 s, … up to 30 s.
pub(super) fn backoff(step: u8) -> Duration {
    match step {
        0 => Duration::ZERO,
        n => Duration::from_secs(1u64 << (n - 1).min(5)).min(MAX_DUE_BACKOFF),
    }
}

/// A topic's owed refetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Due {
    next_try: Instant,
    step: u8,
    /// The by-id call serving it, if one is in flight.
    flight: Option<FetchSeq>,
}

impl Due {
    fn new(now: Instant) -> Self {
        Self {
            next_try: now,
            step: 0,
            flight: None,
        }
    }

    fn eligible(&self, now: Instant) -> bool {
        self.flight.is_none() && self.next_try <= now
    }
}

impl TopicEntry {
    /// Makes the topic due. One already due keeps its backoff and flight.
    pub(super) fn owe(&mut self, now: Instant) {
        self.due.get_or_insert(Due::new(now));
    }
}

impl TopicList {
    /// Makes a listed topic due.
    pub(super) fn owe(&mut self, id: TopicId, now: Instant) {
        if let Some(entry) = self.entry_mut(id) {
            entry.owe(now);
        }
    }

    /// The call `seq` settled `id`: it is no longer due, unless a newer call
    /// is serving it.
    pub(super) fn settle(&mut self, id: TopicId, seq: FetchSeq) {
        if let Some(entry) = self.entry_mut(id)
            && entry.due.is_some_and(|d| d.flight.is_none_or(|f| f == seq))
        {
            entry.due = None;
        }
    }

    /// A call conflicted or failed for `id`: it stays due and waits longer.
    pub(super) fn back_off(&mut self, id: TopicId, now: Instant) {
        if let Some(entry) = self.entry_mut(id) {
            let due = entry.due.get_or_insert(Due::new(now));
            due.step = due.step.saturating_add(1).min(8);
            due.next_try = now + backoff(due.step);
            due.flight = None;
        }
    }

    /// The by-id call `seq` was dropped before its result (`None`: every
    /// call): its ids are eligible again at once.
    pub(super) fn requeue(&mut self, seq: Option<FetchSeq>) {
        for due in self.entries.iter_mut().filter_map(|e| e.due.as_mut()) {
            if due.flight.is_some_and(|f| seq.is_none_or(|s| s == f)) {
                due.flight = None;
            }
        }
    }

    /// Light refresh and reconcile: every due topic may be tried now.
    pub fn reset_backoff(&mut self, now: Instant) {
        for due in self.entries.iter_mut().filter_map(|e| e.due.as_mut()) {
            due.step = 0;
            due.next_try = now;
        }
    }

    pub fn is_due(&self, id: TopicId) -> bool {
        self.entry(id).is_some_and(|e| e.due.is_some())
    }

    pub fn due_len(&self) -> usize {
        self.entries.iter().filter(|e| e.due.is_some()).count()
    }

    /// Takes the due ids for one by-id call dispatched as `seq`, at most
    /// `room`: the open topic in its reserved slot when it is due, then the
    /// other eligible ids in ascending order after the drain cursor,
    /// wrapping around.
    pub(super) fn take_due(&mut self, now: Instant, room: usize, seq: FetchSeq) -> Vec<TopicId> {
        let eligible = |e: &TopicEntry| e.due.is_some_and(|d| d.eligible(now));
        let mut ids = Vec::with_capacity(room);
        if room == 0 {
            return ids;
        }
        if let Some(open) = self.open
            && self.entry(open).is_some_and(eligible)
        {
            ids.push(open);
        }
        let mut rest: Vec<TopicId> = self
            .entries
            .iter()
            .filter(|e| eligible(e) && Some(e.id()) != self.open)
            .map(TopicEntry::id)
            .collect();
        rest.sort_unstable();
        let start = self
            .drain_cursor
            .map_or(0, |c| rest.partition_point(|id| *id <= c));
        rest.rotate_left(start);
        rest.truncate(room - ids.len());
        if let Some(last) = rest.last() {
            self.drain_cursor = Some(*last);
        }
        ids.extend(rest);
        for id in &ids {
            if let Some(due) = self.entry_mut(*id).and_then(|e| e.due.as_mut()) {
                due.flight = Some(seq);
            }
        }
        ids
    }
}
