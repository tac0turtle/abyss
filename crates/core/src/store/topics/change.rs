//! What the store tells the UI, and how the UI applies it.

use super::order::{Key, slot};
use crate::model::{MsgId, Topic, TopicGone, TopicId, TopicOrder};

/// What an operation changed in the emitted (shown) list.
///
/// **Consumer contract.** A consumer holding the emitted topics in display
/// order applies each change in sequence with [`ListChange::apply`], and then
/// holds exactly the store's emitted list (the store checks this for every
/// change it emits that isn't a reset, and emits a reset otherwise):
/// - `Reset`: replace the list and the order.
/// - `Removed(id, _)`: drop the topic if present (the reason is for the
///   consumer's other state, never for the list). It may name a topic the
///   consumer doesn't hold (a deletion of an unlisted or just-evicted
///   topic, an open General fetched already hidden): a no-op for the rows.
///   A deletion or hiding is never only implied by a reset: its `Removed`
///   comes first, also when a `Reset` follows that omits the topic anyway.
/// - `Bumped(t)`: replace the copy in place. In `LastActivity` order, an
///   unpinned topic whose `last_id` grew then moves to the top of the
///   unpinned part (a live message, as in the chat list).
/// - `Appended(ts)`, one topic after another: a present copy whose pinned flag and rank key
///   (`last_id` in `LastActivity`, `created` in `Created`) are unchanged is
///   replaced in place. Otherwise the old copy (if any) is dropped and the
///   topic is inserted at the end of the pinned prefix if pinned, else
///   before the first unpinned topic it ranks above (after ties), else last.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListChange {
    /// Replace the copy (`TopicsReset`).
    Reset {
        order: TopicOrder,
        topics: Vec<Topic>,
    },
    /// Added or changed, in the order the store applied them
    /// (`TopicsAppended`).
    Appended(Vec<Topic>),
    /// Changed by a live event (`TopicBumped`).
    Bumped(Topic),
    /// Deleted, hidden or cut by the cap (`TopicRemoved`), as the second
    /// field says.
    Removed(TopicId, TopicGone),
}

impl ListChange {
    /// Applies the change to a consumer's copy (see the contract above).
    pub fn apply(&self, order: &mut TopicOrder, shown: &mut Vec<Topic>) {
        apply_rows(self, order, shown);
    }
}

/// A row a consumer keeps: a full topic (the UI) or just its key (the
/// store's replay check).
pub(super) trait Row: Sized {
    fn key(&self) -> Key;
    fn row(topic: &Topic) -> Self;
}

impl Row for Topic {
    fn key(&self) -> Key {
        Key::of(self)
    }

    fn row(topic: &Topic) -> Self {
        topic.clone()
    }
}

impl Row for Key {
    fn key(&self) -> Key {
        *self
    }

    fn row(topic: &Topic) -> Self {
        Key::of(topic)
    }
}

pub(super) fn apply_rows<R: Row>(change: &ListChange, order: &mut TopicOrder, rows: &mut Vec<R>) {
    let find = |rows: &[R], id: TopicId| rows.iter().position(|r| r.key().id == id);
    match change {
        ListChange::Reset { order: o, topics } => {
            *order = *o;
            *rows = topics.iter().map(R::row).collect();
        }
        ListChange::Removed(id, _) => {
            if let Some(at) = find(rows, *id) {
                rows.remove(at);
            }
        }
        ListChange::Bumped(t) => {
            let Some(at) = find(rows, t.id) else {
                return;
            };
            let old = rows.remove(at).key();
            let grew = t
                .last_id
                .is_some_and(|new| old.last_id.is_none_or(|o: MsgId| new > o));
            let to = if *order == TopicOrder::LastActivity && !t.pinned && grew {
                rows.iter().take_while(|r| r.key().pinned).count()
            } else {
                at
            };
            rows.insert(to, R::row(t));
        }
        ListChange::Appended(ts) => {
            for t in ts {
                let new = Key::of(t);
                let found = find(rows, t.id);
                let in_place = found
                    .and_then(|at| rows.get_mut(at))
                    .filter(|r| r.key().places_like(&new, *order));
                if let Some(r) = in_place {
                    *r = R::row(t);
                } else {
                    if let Some(at) = found {
                        rows.remove(at);
                    }
                    let keys: Vec<Key> = rows.iter().map(Row::key).collect();
                    rows.insert(slot(*order, &keys, &new), R::row(t));
                }
            }
        }
    }
}
