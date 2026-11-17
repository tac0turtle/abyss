//! Display order of a topic list (plan section 6.1): pinned topics first in
//! server order, then the unpinned ones by the list's [`TopicOrder`].
//!
//! One placement rule ([`slot`]) serves the store, the consumer of
//! [`super::ListChange`]s and the store's check that the consumer's replay
//! reproduces its order.

use std::cmp::Ordering;

use super::entry::TopicEntry;
use crate::model::{MsgId, Topic, TopicId, TopicOrder};

/// What placement looks at in a topic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Key {
    pub(super) id: TopicId,
    pub(super) pinned: bool,
    pub(super) last_id: Option<MsgId>,
    pub(super) created: i64,
}

impl Key {
    pub(super) fn of(t: &Topic) -> Self {
        Self {
            id: t.id,
            pinned: t.pinned,
            last_id: t.last_id,
            created: t.created,
        }
    }

    /// Whether `self` and `other` place alike (pinned flag and rank key).
    pub(super) fn places_like(&self, other: &Key, order: TopicOrder) -> bool {
        self.pinned == other.pinned
            && match order {
                TopicOrder::LastActivity => self.last_id == other.last_id,
                TopicOrder::Created => self.created == other.created,
            }
    }
}

/// How `a` ranks against `b` in the unpinned part: `Less` when `a` comes first.
pub(super) fn rank_keys(order: TopicOrder, a: &Key, b: &Key) -> Ordering {
    match order {
        // Newest message first; a topic with no known message last.
        TopicOrder::LastActivity => b.last_id.cmp(&a.last_id),
        // Newest created first, ties by id descending.
        TopicOrder::Created => (b.created, b.id).cmp(&(a.created, a.id)),
    }
}

pub(super) fn rank(order: TopicOrder, a: &Topic, b: &Topic) -> Ordering {
    rank_keys(order, &Key::of(a), &Key::of(b))
}

/// Where `k` goes among `list` (in display order): a pinned topic at the end
/// of the pinned prefix, an unpinned one before the first unpinned topic it
/// ranks above (after any it ties with), else at the end.
pub(super) fn slot(order: TopicOrder, list: &[Key], k: &Key) -> usize {
    let pinned = list.iter().take_while(|e| e.pinned).count();
    if k.pinned {
        return pinned;
    }
    list.iter()
        .skip(pinned)
        .position(|e| rank_keys(order, k, e) == Ordering::Less)
        .map_or(list.len(), |i| pinned + i)
}

fn keys(list: &[TopicEntry]) -> Vec<Key> {
    list.iter().map(|e| Key::of(&e.topic)).collect()
}

pub(super) fn pinned_len(list: &[TopicEntry]) -> usize {
    list.iter().take_while(|e| e.topic.pinned).count()
}

/// Inserts `entry` at its [`slot`]. Returns its index. A shown topic's slot
/// is taken among the shown topics only, exactly as a consumer of the
/// emitted list places it, so hidden entries can't make the two disagree.
pub(super) fn insert_ranked(
    list: &mut Vec<TopicEntry>,
    order: TopicOrder,
    entry: TopicEntry,
) -> usize {
    let key = Key::of(&entry.topic);
    let shown: Vec<usize> = list
        .iter()
        .enumerate()
        .filter(|(_, e)| e.shown())
        .map(|(i, _)| i)
        .collect();
    let at = match shown.last() {
        Some(&end) if entry.shown() => {
            let shown_keys: Vec<Key> = shown
                .iter()
                .filter_map(|&i| list.get(i))
                .map(|e| Key::of(&e.topic))
                .collect();
            let s = slot(order, &shown_keys, &key);
            let at = shown.get(s).copied().unwrap_or(end + 1);
            // Keep the pinned prefix across hidden entries; this never moves
            // the topic past a shown one.
            let pinned = pinned_len(list);
            if key.pinned {
                at.min(pinned)
            } else {
                at.max(pinned)
            }
        }
        _ => slot(order, &keys(list), &key),
    };
    list.insert(at, entry);
    at
}

/// Moves the entry at `at` to its [`slot`] among the others.
pub(super) fn replace(list: &mut Vec<TopicEntry>, order: TopicOrder, at: usize) -> usize {
    if at >= list.len() {
        return at;
    }
    let entry = list.remove(at);
    insert_ranked(list, order, entry)
}

/// Moves the unpinned entry at `at` to the top of the unpinned part (a live
/// message that grew its `last_id`). Returns its new index.
pub(super) fn to_unpinned_top(list: &mut Vec<TopicEntry>, at: usize) -> usize {
    if at >= list.len() {
        return at;
    }
    let entry = list.remove(at);
    let top = pinned_len(list);
    list.insert(top, entry);
    top
}

/// Restores the pinned prefix, keeping relative order within each part.
pub(super) fn pinned_first(list: &mut [TopicEntry]) {
    list.sort_by_key(|e| !e.topic.pinned);
}
