//! The fake server's forums (plan section 5, "Fake").
//!
//! A chat is a forum when its dialog kind says so. Its topics live in
//! `State::topics`; their messages live in the chat's history with the topic
//! the server would state (`None` for General). Each topic's activity and
//! unread count are served from that history and the topic's
//! `read_inbox_max`, so sends, reads and deletions show up in later snapshots
//! the way they would on the server.

mod controls;
mod demo;
#[cfg(test)]
mod tests;

use std::cmp::Reverse;
use std::collections::BTreeSet;

pub use demo::{DEMO_FORUM_TOPICS, DemoForum, demo_forum};

use super::{State, to_usize};
use crate::backend::{BackendError, RejectReason, TopicsById, TopicsCursor, TopicsPage};
use crate::model::{
    ChatId, ChatKind, ChatSummary, Freshness, Message, MsgId, Preview, ThreadKey, TopicId,
    TopicOrder, TopicSnapshot,
};
use crate::store::{ForumStatus, MAX_PEER_DIALOGS, MAX_TOPICS_BY_ID, PostClosed, Route, route};

fn rejected(reason: RejectReason) -> BackendError {
    BackendError::Rejected(reason)
}

fn forum_status(s: &State, chat: ChatId) -> ForumStatus {
    let forum = s
        .dialogs
        .iter()
        .any(|c| c.id == chat && matches!(c.kind, ChatKind::Supergroup { forum: true, .. }));
    if forum {
        ForumStatus::Forum
    } else {
        ForumStatus::NotForum
    }
}

fn require_forum(s: &State, chat: ChatId) -> Result<(), BackendError> {
    match forum_status(s, chat) {
        ForumStatus::Forum => Ok(()),
        ForumStatus::NotForum | ForumStatus::Unknown => Err(rejected(RejectReason::NotForum)),
    }
}

/// Whether `m` belongs to `thread` once normalized. A whole-chat key
/// (`topic: None`) takes every message of the chat, as the server's
/// `getHistory` does for a forum; core keeps only the messages `route`
/// files under the key it asked for.
fn belongs(m: &Message, thread: ThreadKey, status: ForumStatus) -> bool {
    m.thread.chat == thread.chat
        && (thread.topic.is_none()
            || route(m.thread.chat, m.thread.topic, status) == Route::Thread(thread))
}

/// The messages of `thread` in the chat's history, ascending by id.
pub(super) fn thread_history(s: &State, thread: ThreadKey) -> Vec<&Message> {
    let status = forum_status(s, thread.chat);
    s.history
        .get(&thread.chat)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|m| belongs(m, thread, status))
        .collect()
}

/// The thread a message posted into `thread` states: General's messages carry
/// no topic header.
pub(super) fn raw_thread(thread: ThreadKey) -> ThreadKey {
    ThreadKey {
        chat: thread.chat,
        topic: thread.topic.filter(|t| *t != TopicId::GENERAL),
    }
}

/// The newest message id and date a chat has ever held. It never goes down
/// when messages are deleted: Telegram never reuses a message id, and a topic
/// id is a message id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct HighWater {
    id: i32,
    date: Option<i64>,
}

impl HighWater {
    /// Raises the mark to cover `messages`.
    pub(super) fn cover(&mut self, messages: &[Message]) {
        for m in messages {
            self.id = self.id.max(m.id.0);
            self.date = Some(self.date.map_or(m.date, |d| d.max(m.date)));
        }
    }
}

/// A new incoming text message at the end of `thread`'s chat (`thread` as
/// stated by the server, see [`raw_thread`]): an id above every id the chat
/// ever held, a later date. Ids are never reused; running out is a rejection.
pub(super) fn next_message(
    s: &mut State,
    thread: ThreadKey,
    text: &str,
) -> Result<Message, BackendError> {
    let mark = s.high_water.entry(thread.chat).or_default();
    let id = mark
        .id
        .checked_add(1)
        .ok_or(rejected(RejectReason::Other))?;
    let date = mark.date.map_or(i64::from(id), |d| d.saturating_add(1));
    mark.id = id;
    mark.date = Some(date);
    let mut msg = super::text_message(thread.chat, id, text);
    msg.thread = thread;
    msg.date = date;
    Ok(msg)
}

fn stored(s: &State, chat: ChatId, topic: TopicId) -> Option<&TopicSnapshot> {
    s.topics.get(&chat)?.iter().find(|t| t.topic.id == topic)
}

fn stored_mut(s: &mut State, chat: ChatId, topic: TopicId) -> Option<&mut TopicSnapshot> {
    s.topics
        .get_mut(&chat)?
        .iter_mut()
        .find(|t| t.topic.id == topic)
}

fn post_closed(s: &State, chat: ChatId) -> PostClosed {
    s.post_closed
        .get(&chat)
        .copied()
        .unwrap_or(PostClosed::Refused)
}

/// Whether the server takes a send into `thread`: the topic must exist, and
/// a closed one needs [`PostClosed::Allowed`].
pub(super) fn check_send(s: &State, thread: ThreadKey) -> Result<(), BackendError> {
    let Some(topic) = thread.topic else {
        return Ok(());
    };
    if forum_status(s, thread.chat) != ForumStatus::Forum {
        return Err(rejected(RejectReason::Other));
    }
    let snap = stored(s, thread.chat, topic).ok_or(rejected(RejectReason::TopicDeleted))?;
    match (snap.topic.closed, post_closed(s, thread.chat)) {
        (true, PostClosed::Refused) => Err(rejected(RejectReason::TopicClosed)),
        _ => Ok(()),
    }
}

/// A topic read (`readDiscussion`) moves the topic's `read_inbox_max`
/// forward. A whole-chat read moves the chat's pointer
/// ([`move_pointer`]) and changes no topic: core never marks a forum read
/// as a whole (plan section 2, U2).
pub(super) fn mark_read(
    s: &mut State,
    thread: ThreadKey,
    max_id: MsgId,
) -> Result<(), BackendError> {
    let Some(topic) = thread.topic else {
        if forum_status(s, thread.chat) != ForumStatus::Forum {
            move_pointer(s, thread.chat, max_id);
        }
        return Ok(());
    };
    if forum_status(s, thread.chat) != ForumStatus::Forum {
        return Err(rejected(RejectReason::Other));
    }
    let snap = stored_mut(s, thread.chat, topic).ok_or(rejected(RejectReason::TopicDeleted))?;
    snap.topic.read_inbox_max = snap.topic.read_inbox_max.max(max_id);
    Ok(())
}

/// A stored topic as the server serves it now: activity and unread from the
/// chat's history.
fn served(history: &[Message], chat: ChatId, snap: &TopicSnapshot) -> TopicSnapshot {
    let thread = ThreadKey {
        chat,
        topic: Some(snap.topic.id),
    };
    let mut out = snap.clone();
    let read = snap.topic.read_inbox_max;
    let mut unread: u32 = 0;
    let mut top: Option<&Message> = None;
    for m in history
        .iter()
        .filter(|m| belongs(m, thread, ForumStatus::Forum))
    {
        if !m.outgoing && m.id > read {
            unread = unread.saturating_add(1);
        }
        top = Some(m);
    }
    out.topic.unread = unread;
    out.topic.freshness = Freshness::Current;
    out.topic.last = top.map(Preview::of);
    out.topic.last_date = top.map_or(0, |m| m.date);
    out.topic.last_id = top.map(|m| m.id);
    out.top_version = top.map_or(i64::MIN, Message::version);
    out
}

/// Every topic of `chat`, served, in `order`: pinned topics first in their
/// stored order, then the rest by activity (top message id) or by creation
/// date, newest first, ties by id.
fn server_order(s: &State, chat: ChatId, order: TopicOrder) -> Vec<TopicSnapshot> {
    let history = s.history.get(&chat).map(Vec::as_slice).unwrap_or_default();
    let (mut pinned, mut rest): (Vec<TopicSnapshot>, Vec<TopicSnapshot>) = s
        .topics
        .get(&chat)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|t| served(history, chat, t))
        .partition(|t| t.topic.pinned);
    match order {
        TopicOrder::LastActivity => {
            rest.sort_by_key(|t| Reverse((t.topic.last_id, t.topic.id)));
        }
        TopicOrder::Created => {
            rest.sort_by_key(|t| Reverse((t.topic.created, t.topic.id)));
        }
    }
    pinned.append(&mut rest);
    pinned
}

fn order_of(s: &State, chat: ChatId) -> TopicOrder {
    s.topic_order
        .get(&chat)
        .copied()
        .unwrap_or(TopicOrder::LastActivity)
}

/// `messages.getForumTopics`: pages by index in the forum's current order.
/// The page reports that order whatever the cursor was built for, as a
/// server whose order changed between pages would.
pub(super) fn page(
    s: &State,
    chat: ChatId,
    cursor: Option<TopicsCursor>,
    limit: u32,
) -> Result<TopicsPage, BackendError> {
    require_forum(s, chat)?;
    let order = order_of(s, chat);
    let all = server_order(s, chat, order);
    let start = match cursor {
        Some(_) if s.repeat_pages.contains(&chat) => 0,
        cursor => cursor.map_or(0, |c| to_usize(c.index)).min(all.len()),
    };
    let topics: Vec<TopicSnapshot> = all
        .iter()
        .skip(start)
        .take(to_usize(limit.max(1)))
        .cloned()
        .collect();
    let end = start + topics.len();
    let next = topics.last().filter(|_| end < all.len()).map(|last| {
        TopicsCursor::after(order, &last.topic, u32::try_from(end).unwrap_or(u32::MAX))
    });
    Ok(TopicsPage {
        order,
        topics,
        next,
        post_closed: post_closed(s, chat),
    })
}

/// `messages.getForumTopicsByID`: every distinct requested id is found or
/// deleted, in request order.
pub(super) fn by_id(s: &State, chat: ChatId, ids: &[TopicId]) -> Result<TopicsById, BackendError> {
    if ids.len() > MAX_TOPICS_BY_ID {
        return Err(rejected(RejectReason::Other));
    }
    require_forum(s, chat)?;
    let history = s.history.get(&chat).map(Vec::as_slice).unwrap_or_default();
    let mut seen = BTreeSet::new();
    let mut found = Vec::new();
    let mut deleted = Vec::new();
    for &id in ids.iter().filter(|id| seen.insert(**id)) {
        match stored(s, chat, id) {
            Some(snap) => found.push(served(history, chat, snap)),
            None => deleted.push(id),
        }
    }
    Ok(TopicsById {
        found,
        deleted,
        post_closed: post_closed(s, chat),
    })
}

/// Raises a chat's read pointer (`read_inbox_max_id`) to `max_id`, by a
/// read or a pushed read update. From then on the chat's count is served
/// from its history ([`current_summary`]).
pub(super) fn move_pointer(s: &mut State, chat: ChatId, max_id: MsgId) {
    if let Some(dialog) = s.dialogs.iter_mut().find(|d| d.id == chat) {
        dialog.read.read_inbox_max = dialog.read.read_inbox_max.max(max_id);
        s.pointed.insert(chat);
    }
}

/// A chat's summary as the server reports it now. A forum's `unread` is its
/// number of unread topics (plan section 7, D1). Once a read moved another
/// chat's pointer, its `unread` is the number of incoming messages above
/// the pointer, so the pair is consistent; before that the scripted pair
/// is served.
pub(super) fn current_summary(s: &State, summary: &ChatSummary) -> ChatSummary {
    let mut out = summary.clone();
    let unread = if forum_status(s, summary.id) == ForumStatus::Forum {
        if !s.topics.contains_key(&summary.id) {
            return out;
        }
        server_order(s, summary.id, TopicOrder::LastActivity)
            .iter()
            .filter(|t| t.topic.unread > 0)
            .count()
    } else if s.pointed.contains(&summary.id) {
        let pointer = summary.read.read_inbox_max;
        let thread = ThreadKey {
            chat: summary.id,
            topic: None,
        };
        thread_history(s, thread)
            .iter()
            .filter(|m| !m.outgoing && m.id > pointer)
            .count()
    } else {
        return out;
    };
    out.read.unread = u32::try_from(unread).unwrap_or(u32::MAX);
    out
}

/// `messages.getPeerDialogs`: the current summaries of the known chats among
/// `chats`, in request order, without duplicates.
pub(super) fn peer_dialogs(s: &State, chats: &[ChatId]) -> Result<Vec<ChatSummary>, BackendError> {
    if chats.len() > MAX_PEER_DIALOGS {
        return Err(rejected(RejectReason::Other));
    }
    let mut seen = BTreeSet::new();
    Ok(chats
        .iter()
        .filter(|c| seen.insert(**c))
        .filter_map(|c| s.dialogs.iter().find(|d| d.id == *c))
        .map(|d| current_summary(s, d))
        .collect())
}
