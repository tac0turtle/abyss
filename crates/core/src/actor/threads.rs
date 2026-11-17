//! Thread normalization in the actor (plan stage 2, sections 3 and 6.2):
//! every message that enters a window, the cache or an event goes through
//! [`route`] first, so its `thread` is its window's key. In a forum a
//! message's topic is always `Some` (General for no header); outside one it
//! is always `None`.
//!
//! - **Forum knowledge** comes from the chat list's kind. The retained views
//!   (windows and topic lists) remember the last kind a summary stated for
//!   their chat ([`KnownKinds`]), so a chat that leaves the list keeps it.
//!   Only a view's shape is never taken as "not a forum".
//! - **History pages** (server and cache) are normalized under that
//!   knowledge, and only the messages routed to the page's own thread are
//!   kept. A topic key is a forum view, so its pages route as a forum's.
//! - **Live messages and edits** route under [`Actor::forum_status`]. A
//!   message routed elsewhere than a thread ([`Route::ChatOnly`],
//!   [`Route::ForumSuspected`]) reaches no window and no cache, and every
//!   window of its chat loses cache continuity, since that window's cache
//!   may now miss it (section 6.6, general rule). Fencing and the
//!   forum-change path for a suspected forum are task 8's.

use std::collections::HashMap;

use super::Actor;
use crate::api::UserError;
use crate::backend::Backend;
use crate::model::{ChatId, ChatKind, Message, PeerKind, ThreadKey};
use crate::store::{ForumStatus, MAX_TOPIC_LISTS, MAX_WINDOWS, Route, normalize};

/// The forum status a chat's kind states.
fn kind_status(kind: ChatKind) -> ForumStatus {
    match kind {
        ChatKind::Supergroup { forum: true, .. } => ForumStatus::Forum,
        ChatKind::Private { .. }
        | ChatKind::Bot
        | ChatKind::Group
        | ChatKind::Supergroup { .. } => ForumStatus::NotForum,
    }
}

/// The last forum status a chat summary stated, for each chat with a
/// retained view (a window or a topic list). Updated whenever summaries
/// apply or views change ([`Actor::note_kinds`]), and pruned to the chats
/// with views then: at most `MAX_WINDOWS + MAX_TOPIC_LISTS` entries.
#[derive(Debug, Default)]
pub(super) struct KnownKinds(HashMap<ChatId, ForumStatus>);

impl KnownKinds {
    fn get(&self, chat: ChatId) -> Option<ForumStatus> {
        self.0.get(&chat).copied()
    }
}

impl<B: Backend> Actor<B> {
    /// What core knows about whether `chat` is a forum. Only channels can
    /// be forums. Otherwise: the chat list's kind, else the kind last stated
    /// for the chat's retained views, else a retained topic list or topic
    /// window (core opened them for a forum). A chat none of these
    /// describes is [`ForumStatus::Unknown`].
    pub(super) fn forum_status(&self, chat: ChatId) -> ForumStatus {
        if chat.kind != PeerKind::Channel {
            return ForumStatus::NotForum;
        }
        if let Some(summary) = self.dialogs.get(chat) {
            return kind_status(summary.kind);
        }
        if let Some(known) = self.forum_kinds.get(chat) {
            return known;
        }
        let topic_view = self.topics.lists.get(chat).is_some()
            || self
                .windows
                .threads()
                .any(|t| t.chat == chat && t.topic.is_some());
        if topic_view {
            ForumStatus::Forum
        } else {
            ForumStatus::Unknown
        }
    }

    /// Records, for every chat with a retained view, the kind its summary
    /// states now, and forgets the chats without views (see
    /// [`KnownKinds`]). Run when summaries apply and when views change.
    pub(super) fn note_kinds(&mut self) {
        let mut views: Vec<ChatId> = self
            .windows
            .threads()
            .map(|t| t.chat)
            .chain(self.topics.lists.chats().iter().copied())
            .collect();
        views.sort_unstable();
        views.dedup();
        let known = &mut self.forum_kinds.0;
        known.retain(|chat, _| views.binary_search(chat).is_ok());
        for chat in views {
            if let Some(summary) = self.dialogs.get(chat) {
                known.insert(chat, kind_status(summary.kind));
            }
        }
        debug_assert!(known.len() <= MAX_WINDOWS + MAX_TOPIC_LISTS);
    }

    /// Whether `OpenThread(thread)` is refused: a whole-chat key of a
    /// known forum (plan stage 2, section 4), which has no whole-chat
    /// history. Answered `Error(Rejected)`; nothing else changes.
    pub(super) async fn refuse_open(&self, thread: ThreadKey) -> bool {
        let refused =
            thread.topic.is_none() && self.forum_status(thread.chat) == ForumStatus::Forum;
        if refused {
            tracing::debug!("a forum's whole-chat key refused");
            self.error(UserError::Rejected).await;
        }
        refused
    }

    /// Normalizes a history page of `thread` and keeps only the messages
    /// routed to it. A page answers for its own thread, so a topic key's
    /// page routes as a forum's; a whole-chat key's under what core knows
    /// about the chat (a forum's messages then all leave it).
    pub(super) fn keep_thread(&self, thread: ThreadKey, page: Vec<Message>) -> Vec<Message> {
        let status = match thread.topic {
            Some(_) => ForumStatus::Forum,
            None => self.forum_status(thread.chat),
        };
        let before = page.len();
        let kept: Vec<Message> = page
            .into_iter()
            .filter_map(|m| match normalize(m, status) {
                (Route::Thread(key), m) if key == thread => Some(m),
                _ => None,
            })
            .collect();
        if kept.len() < before {
            let dropped = before - kept.len();
            tracing::debug!(dropped, "history of other threads left out of a page");
        }
        kept
    }

    /// Routes a live message or edit. For [`Route::Thread`] the message
    /// comes back normalized; otherwise unchanged, and the chat's windows
    /// lose cache continuity (see the module docs).
    pub(super) fn route_live(&mut self, m: Message) -> (Route, Message) {
        let status = self.forum_status(m.thread.chat);
        let (route, m) = normalize(m, status);
        if let Route::ChatOnly(chat) | Route::ForumSuspected(chat) = route {
            self.windows.lose_continuity(chat);
        }
        (route, m)
    }
}
