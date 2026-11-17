//! Live reducers and triggers (plan sections 6.3, 6.8). No reducer changes an
//! unread count: counts are server values only, and a live event that may
//! change one is a trigger that marks the topic due.

use std::collections::BTreeSet;
use std::time::Instant;

use super::entry::{Deletion, bound};
use super::order;
use super::{ListChange, Replay, TopicList};
use crate::model::{Freshness, Message, MsgId, Preview, TopicId, TopicOrder, Visibility};
use crate::store::bump_moves;

/// Whether the thread's tail was on screen when a message arrived
/// (`ViewingTail(Some(thread))` while the window is active).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tail {
    Visible,
    Hidden,
}

/// A live event that may change a topic's unread count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnreadTrigger {
    /// An incoming message the user hasn't seen.
    Incoming,
    /// A local read of the topic completed successfully.
    ReadLocally,
    /// A read on another device (`ReadInbox { topic: Some }`, `ReadTopicMax`).
    ReadRemotely,
    /// A message of the topic was deleted.
    Deleted,
}

impl UnreadTrigger {
    fn freshness(self) -> Freshness {
        match self {
            UnreadTrigger::Incoming => Freshness::NewSince,
            UnreadTrigger::ReadLocally => Freshness::ReadSince,
            UnreadTrigger::ReadRemotely | UnreadTrigger::Deleted => Freshness::Stale,
        }
    }
}

/// A live topic edit (`updateEditForumTopic`). `None` leaves a field alone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TopicEdit {
    /// Sanitized by the backend.
    pub title: Option<String>,
    pub closed: Option<bool>,
    pub visibility: Option<Visibility>,
}

impl TopicList {
    /// A new message routed to a topic of this forum (`msg.thread.topic` is
    /// `Some`). A known topic's activity moves forward when the id grows (and
    /// the topic moves to the top in [`TopicOrder::LastActivity`]); an unseen
    /// incoming message is an unread trigger. An unknown topic is discovered.
    pub fn on_new_message(&mut self, msg: &Message, tail: Tail, now: Instant) -> Vec<ListChange> {
        let Some(topic) = msg.thread.topic else {
            return Vec::new();
        };
        let Some(mut at) = self.position(topic) else {
            self.note_unlisted();
            self.discover(topic);
            return Vec::new();
        };
        let before = self.shown_keys();
        let seq = self.seq;
        let order = self.order;
        let Some(entry) = self.entries.get_mut(at) else {
            return Vec::new();
        };
        let mut changed = false;
        if bump_moves(entry.topic.last_id, msg.id) {
            entry.topic.last = Some(Preview::of(msg));
            entry.topic.last_date = msg.date;
            entry.topic.last_id = Some(msg.id);
            entry.last_version = msg.version();
            entry.activity.set_changed(seq);
            entry.owe(now);
            changed = true;
            if order == TopicOrder::LastActivity && !entry.topic.pinned {
                at = order::to_unpinned_top(&mut self.entries, at);
            }
        }
        if !msg.outgoing && tail == Tail::Hidden {
            if let Some(entry) = self.entries.get_mut(at) {
                entry.trigger_unread(Freshness::NewSince, seq);
            }
            self.owe(topic, now);
            changed = true;
        }
        self.checked(&before, self.bumped(changed.then_some(at)), Replay::Exact)
    }

    /// An edit routed to a topic of this forum. It only ever updates the
    /// preview (a live activity change: the topic becomes due), and only when
    /// it edits the current top message with a newer version. It never
    /// inserts, moves, or touches unread. An edit in a topic the list
    /// doesn't hold may postdate a page in flight that adds it.
    ///
    /// While the topic's activity is unsettled (a live change, such as its
    /// top message's deletion, waits for a refetch), the list doesn't know
    /// the real top message: an edit of any other message may be an edit of
    /// it. That is a live activity change too (no preview changes), so a
    /// snapshot dispatched before it conflicts and a refetch settles it.
    pub fn on_edit(&mut self, msg: &Message, now: Instant) -> Vec<ListChange> {
        let seq = self.seq;
        let Some(topic) = msg.thread.topic else {
            return Vec::new();
        };
        let Some(at) = self.position(topic) else {
            self.note_unlisted();
            return Vec::new();
        };
        let Some(entry) = self.entries.get_mut(at) else {
            return Vec::new();
        };
        let is_top = entry.topic.last_id == Some(msg.id);
        let newer = is_top && msg.version() > entry.last_version;
        if newer {
            entry.topic.last = Some(Preview::of(msg));
            entry.last_version = msg.version();
        }
        if newer || (!is_top && !entry.activity.settled()) {
            entry.activity.set_changed(seq);
            entry.owe(now);
        }
        self.bumped(newer.then_some(at))
    }

    /// An unread trigger for a known topic: the hint changes and the topic
    /// becomes due; the count is untouched.
    pub fn trigger_unread(
        &mut self,
        topic: TopicId,
        trigger: UnreadTrigger,
        now: Instant,
    ) -> Vec<ListChange> {
        let seq = self.seq;
        let Some(at) = self.position(topic) else {
            self.note_unlisted();
            return Vec::new();
        };
        if let Some(entry) = self.entries.get_mut(at) {
            entry.trigger_unread(trigger.freshness(), seq);
        }
        self.owe(topic, now);
        self.bumped(Some(at))
    }

    /// Messages of this forum were deleted. `held` names the topic of each
    /// deleted id a window held. Any other id can't be assigned to a topic,
    /// so every listed topic gets an unread trigger. Deleting a topic's top
    /// message makes its activity stale, and deleting its root only suspects
    /// a deletion. Each affected topic becomes due.
    pub fn on_deleted(
        &mut self,
        ids: &[MsgId],
        held: &[(MsgId, TopicId)],
        now: Instant,
    ) -> Vec<ListChange> {
        let seq = self.seq;
        // Any id may be a message (or the root) of a topic not in the list.
        self.note_unlisted();
        let before: Vec<_> = self.entries.iter().map(|e| e.topic.clone()).collect();
        let unassigned = ids.iter().any(|id| !held.iter().any(|(h, _)| h == id));
        let held_topics: BTreeSet<TopicId> = held.iter().map(|&(_, t)| t).collect();
        for entry in &mut self.entries {
            let id = entry.id();
            if unassigned || held_topics.contains(&id) {
                entry.trigger_unread(Freshness::Stale, seq);
                entry.owe(now);
            }
            if entry.topic.last_id.is_some_and(|last| ids.contains(&last)) {
                entry.activity.set_changed(seq);
                entry.owe(now);
            }
            if ids.contains(&MsgId(id.get())) {
                entry.deletion = Deletion::Suspected;
                entry.meta.set_changed(seq);
                entry.owe(now);
            }
        }
        self.entries
            .iter()
            .zip(&before)
            .filter(|(e, was)| e.shown() && e.topic != **was)
            .map(|(e, _)| ListChange::Bumped(e.topic.clone()))
            .collect()
    }

    /// Something says the topic may be gone (`TOPIC_DELETED` on send): a
    /// suspicion, settled by a fresh by-id result.
    pub fn suspect_deleted(&mut self, topic: TopicId, now: Instant) {
        let seq = self.seq;
        match self.entry_mut(topic) {
            Some(entry) => {
                entry.deletion = Deletion::Suspected;
                entry.meta.set_changed(seq);
                entry.owe(now);
            }
            None => self.note_unlisted(),
        }
    }

    /// The server's state may differ (`TOPIC_CLOSED` with a stale list):
    /// refetch it.
    pub fn mark_due(&mut self, topic: TopicId, now: Instant) {
        if self.entry(topic).is_some() {
            self.owe(topic, now);
        } else {
            self.note_unlisted();
        }
    }

    /// A live topic edit: a live meta change (the topic becomes due).
    /// Becoming hidden removes the topic from the emitted list; becoming
    /// shown adds it.
    pub fn on_topic_edited(
        &mut self,
        topic: TopicId,
        edit: TopicEdit,
        now: Instant,
    ) -> Vec<ListChange> {
        let seq = self.seq;
        let before = self.shown_keys();
        let Some(entry) = self.entry_mut(topic) else {
            self.note_unlisted();
            return Vec::new();
        };
        let was_shown = entry.shown();
        if let Some(title) = edit.title {
            entry.topic.title = title;
        }
        if let Some(closed) = edit.closed {
            entry.topic.closed = closed;
        }
        if let Some(visibility) = edit.visibility {
            entry.visibility = visibility;
        }
        bound(&mut entry.topic);
        entry.meta.set_changed(seq);
        entry.owe(now);
        let changes = match (was_shown, entry.shown()) {
            (true, false) => vec![ListChange::Removed(topic)],
            (false, true) => {
                // Re-shown: placed at its rank among the shown topics, as
                // the consumer places an `Appended` topic it doesn't hold.
                let shown = vec![ListChange::Appended(vec![entry.topic.clone()])];
                if let Some(at) = self.position(topic) {
                    order::replace(&mut self.entries, self.order, at);
                }
                shown
            }
            (true, true) => vec![ListChange::Bumped(entry.topic.clone())],
            (false, false) => Vec::new(),
        };
        self.checked(&before, changes, Replay::Exact)
    }

    /// `Bumped` for the changed entry at `at`, if it is shown.
    fn bumped(&self, at: Option<usize>) -> Vec<ListChange> {
        match at.and_then(|at| self.entries.get(at)) {
            Some(entry) if entry.shown() => vec![ListChange::Bumped(entry.topic.clone())],
            _ => Vec::new(),
        }
    }
}
