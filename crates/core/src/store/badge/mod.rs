//! The forum badge (plan section 7): set only from authoritative sources,
//! ordered by one actor-wide dispatch clock, refreshed through dirty stamps
//! that are never dropped.

mod batch;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::time::Instant;

pub use batch::{
    CURSOR_SLOTS, KIND_SLOTS, MAX_PEER_DIALOGS, PEER_CALL_INTERVAL, PEER_REFRESH_INTERVAL,
    PeerBatch, PeerScheduler, SHOWN_SLOTS,
};

use crate::model::ChatId;

/// The badge clock: advanced at every dialogs or `peer_dialogs` dispatch and
/// at every chat-level read update applied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BadgeSeq(u64);

/// Where a badge value came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadgeSource {
    /// A dialogs page containing the chat, or a `peer_dialogs` result:
    /// stamped with its dispatch. Clears a dirty stamp older than it.
    Fetched(BadgeSeq),
    /// A chat-level read update (`ReadInbox { topic: None }`), stamped when
    /// applied. Doesn't clear a dirty stamp.
    ChatRead,
}

/// Whether to show a source's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offer {
    Apply,
    /// The shown value comes from a source dispatched later.
    Ignore,
}

/// Inline bytes of one chat's badge state, for the memory-bound test.
#[cfg(test)]
pub(crate) const CHAT_BADGE_BYTES: usize = std::mem::size_of::<ChatBadge>();

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ChatBadge {
    /// The stamp of the source of the value shown.
    shown: BadgeSeq,
    /// Set by a trigger to the clock's value then; `None` when clean.
    dirty: Option<BadgeSeq>,
    /// When the chat last went out in a `peer_dialogs` call.
    dispatched: Option<Instant>,
}

/// Badge stamps of the retained dialogs (at most `MAX_DIALOGS`): leaving the
/// list drops a chat's state.
#[derive(Debug, Default)]
pub struct Badges {
    seq: BadgeSeq,
    chats: BTreeMap<ChatId, ChatBadge>,
}

impl Badges {
    /// Starts tracking a retained dialog.
    pub fn track(&mut self, chat: ChatId) {
        self.chats.entry(chat).or_default();
    }

    /// Keeps only the chats still retained.
    pub fn retain(&mut self, mut keep: impl FnMut(ChatId) -> bool) {
        self.chats.retain(|c, _| keep(*c));
    }

    pub fn len(&self) -> usize {
        self.chats.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chats.is_empty()
    }

    /// Stamps a dialogs or `peer_dialogs` dispatch.
    pub fn dispatch(&mut self) -> BadgeSeq {
        self.seq = BadgeSeq(self.seq.0.saturating_add(1));
        self.seq
    }

    /// Something may have changed the chat's count: refresh it. A trigger
    /// that crosses an in-flight call is newer than its dispatch, so that
    /// result won't clear it.
    pub fn trigger(&mut self, chat: ChatId) {
        let now = self.seq;
        if let Some(badge) = self.chats.get_mut(&chat) {
            badge.dirty = Some(now);
        }
    }

    pub fn is_dirty(&self, chat: ChatId) -> bool {
        self.chats.get(&chat).is_some_and(|b| b.dirty.is_some())
    }

    /// Dirty chats, ascending.
    pub fn dirty(&self) -> impl Iterator<Item = ChatId> + '_ {
        self.chats
            .iter()
            .filter(|(_, b)| b.dirty.is_some())
            .map(|(c, _)| *c)
    }

    /// Offers a value for a tracked chat's badge. It is shown only if its
    /// stamp is newer than the shown value's. A fetched result dispatched
    /// after the dirty stamp also clears it.
    pub fn offer(&mut self, chat: ChatId, source: BadgeSource) -> Offer {
        let stamp = match source {
            BadgeSource::Fetched(stamp) => stamp,
            BadgeSource::ChatRead => self.dispatch(),
        };
        let Some(badge) = self.chats.get_mut(&chat) else {
            return Offer::Ignore;
        };
        if matches!(source, BadgeSource::Fetched(_)) && badge.dirty.is_some_and(|d| stamp > d) {
            badge.dirty = None;
        }
        if stamp > badge.shown {
            badge.shown = stamp;
            Offer::Apply
        } else {
            Offer::Ignore
        }
    }

    /// Whether `chat` may go out in a call now: dirty, and not sent within
    /// [`PEER_REFRESH_INTERVAL`].
    fn wants(&self, chat: ChatId, now: Instant) -> bool {
        self.chats.get(&chat).is_some_and(|b| {
            b.dirty.is_some()
                && b.dispatched
                    .is_none_or(|at| now.saturating_duration_since(at) >= PEER_REFRESH_INTERVAL)
        })
    }

    fn sent(&mut self, chat: ChatId, now: Instant) {
        if let Some(badge) = self.chats.get_mut(&chat) {
            badge.dispatched = Some(now);
        }
    }
}
