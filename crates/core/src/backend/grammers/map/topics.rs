//! Forum responses (plan section 5): `messages.forumTopics` pages and by-id
//! results, and `messages.peerDialogs`.
//!
//! Every topic id goes through [`TopicId::try_from`]; a malformed item is
//! skipped on its own and counted, never filed under another id. Titles are
//! sanitized and never logged.

use std::collections::HashMap;

use grammers_client::tl;

use super::lists::{
    DialogsParts, MAX_RESPONSE_PEERS, MappedDialogs, Oversized, RESPONSE_SLACK, at_most,
    checked_dialogs, count, dialogs, is_broadcast,
};
use super::message::{message, raw_date, raw_key};
use super::peers::{Directory, channel_chat, chat_id};
use crate::backend::{TopicsById, TopicsCursor, TopicsPage, Update};
use crate::model::{
    ChatId, Freshness, Message, MsgId, Preview, Topic, TopicId, TopicOrder, TopicSnapshot,
    Visibility,
};
use crate::sanitize::display_name;
use crate::store::PostClosed;

/// The parts of a `messages.ForumTopics` response, checked against the
/// request. No `Debug` of the payload: it carries titles and message text.
pub(crate) struct TopicsParts<'a> {
    /// `order_by_create_date`.
    pub(crate) order: TopicOrder,
    /// The forum's total number of topics.
    pub(crate) count: i32,
    pub(crate) topics: &'a [tl::enums::ForumTopic],
    /// Each topic's top message.
    pub(crate) messages: &'a [tl::enums::Message],
    pub(crate) users: &'a [tl::enums::User],
    pub(crate) chats: &'a [tl::enums::Chat],
}

impl std::fmt::Debug for TopicsParts<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TopicsParts")
            .field("order", &self.order)
            .field("count", &self.count)
            .field("topics", &self.topics.len())
            .field("messages", &self.messages.len())
            .field("users", &self.users.len())
            .field("chats", &self.chats.len())
            .finish()
    }
}

/// The parts of a topics response requested with `limit` (a page's limit, or
/// the number of ids asked for), or [`Oversized`] when it carries more topics,
/// messages or peers than that request allows. Checked before anything is
/// mapped or hashed.
pub(crate) fn topics_parts(
    resp: &tl::enums::messages::ForumTopics,
    limit: usize,
) -> Result<TopicsParts<'_>, Oversized> {
    let tl::enums::messages::ForumTopics::Topics(r) = resp;
    let items = limit.saturating_add(RESPONSE_SLACK);
    at_most(r.topics.len(), items)?;
    // One top message per topic; the slack covers a root sent alongside.
    at_most(r.messages.len(), items.saturating_mul(2))?;
    at_most(
        r.users.len().saturating_add(r.chats.len()),
        MAX_RESPONSE_PEERS,
    )?;
    Ok(TopicsParts {
        order: if r.order_by_create_date {
            TopicOrder::Created
        } else {
            TopicOrder::LastActivity
        },
        count: r.count,
        topics: &r.topics,
        messages: &r.messages,
        users: &r.users,
        chats: &r.chats,
    })
}

/// Top messages of `chat` in a response, by id.
type Tops<'a> = HashMap<i32, &'a tl::enums::Message>;

fn tops<'a>(parts: &TopicsParts<'a>, chat: ChatId) -> Tops<'a> {
    parts
        .messages
        .iter()
        .filter_map(|m| match raw_key(m)? {
            (c, id) if c == chat => Some((id, m)),
            _ => None,
        })
        .collect()
}

/// Topics mapped from one response. `Debug` shows counts only.
#[derive(Default)]
pub(crate) struct MappedTopics {
    pub(crate) found: Vec<TopicSnapshot>,
    /// `forumTopicDeleted` ids.
    pub(crate) deleted: Vec<TopicId>,
    /// Items dropped for a non-positive id, or a topic that failed
    /// validation, for a debug count.
    pub(crate) skipped: usize,
}

impl std::fmt::Debug for MappedTopics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedTopics")
            .field("found", &self.found.len())
            .field("deleted", &self.deleted.len())
            .field("skipped", &self.skipped)
            .finish()
    }
}

/// Maps every topic of a response of `chat`, in response order.
pub(crate) fn topics(parts: &TopicsParts<'_>, dir: &Directory, chat: ChatId) -> MappedTopics {
    let tops = tops(parts, chat);
    let mut out = MappedTopics {
        found: Vec::with_capacity(parts.topics.len()),
        ..MappedTopics::default()
    };
    for raw in parts.topics {
        match raw {
            tl::enums::ForumTopic::Topic(t) => match snapshot(t, &tops, dir) {
                Some(s) => out.found.push(s),
                None => out.skipped += 1,
            },
            tl::enums::ForumTopic::Deleted(d) => match TopicId::try_from(d.id) {
                Ok(id) => out.deleted.push(id),
                Err(_) => out.skipped += 1,
            },
        }
    }
    out
}

/// One `forumTopic`, with its top message from `tops` for the preview and
/// the snapshot's `top_version`. `None` for a non-positive id.
///
/// The title is sanitized. A `title_missing` topic, or one whose title
/// sanitizes to nothing, has an empty title: the app shows its fixed
/// fallback ("General" for id 1, "Topic" otherwise; plan section 3).
/// `icon_color` and `icon_emoji_id` are not mapped (decision D3).
pub(crate) fn snapshot(
    t: &tl::types::ForumTopic,
    tops: &Tops<'_>,
    dir: &Directory,
) -> Option<TopicSnapshot> {
    let id = TopicId::try_from(t.id).ok()?;
    let raw_top = tops.get(&t.top_message).copied();
    let top = raw_top
        .and_then(|m| message(m, dir))
        .filter(Message::is_valid);
    let topic = Topic {
        id,
        title: if t.title_missing {
            String::new()
        } else {
            display_name(&t.title)
        },
        created: i64::from(t.date),
        closed: t.closed,
        pinned: t.pinned,
        unread: count(t.unread_count),
        unread_mentions: count(t.unread_mentions_count),
        freshness: Freshness::Current,
        read_inbox_max: MsgId(t.read_inbox_max_id.max(0)),
        last: top.as_ref().map(Preview::of),
        // The raw date: the topic's activity is known even when its top
        // message does not map (an excluded or invalid message).
        last_date: raw_top.map_or(0, |m| i64::from(raw_date(m))),
        // As for dialogs, the top message id is authoritative on its own.
        last_id: (t.top_message > 0).then_some(MsgId(t.top_message)),
    };
    if !topic.is_valid() {
        return None;
    }
    Some(TopicSnapshot {
        topic,
        // `hidden` applies to General only.
        visibility: if t.hidden {
            Visibility::Hidden
        } else {
            Visibility::Shown
        },
        top_version: top.as_ref().map_or(i64::MIN, Message::version),
    })
}

/// Whether the user may post in `chat`'s closed topics, from the channel in
/// the response. Refused when the response does not carry it.
pub(crate) fn post_closed(dir: &Directory, chat: ChatId) -> PostClosed {
    dir.get(chat).map_or(PostClosed::Refused, |e| e.post_closed)
}

/// Where the next page starts, or `None` when this page is the last: it was
/// empty, or the topics listed so far reach the server's `count`. A short
/// page is not the end: `getForumTopics` may return fewer topics than asked
/// for while more remain. Built from the last raw topic of the page (also one that did not
/// map, so a malformed item is not fetched again), by the order the page
/// reported (plan section 5):
/// - [`TopicOrder::LastActivity`]: the top message's date and id;
/// - [`TopicOrder::Created`]: `forumTopic.date` and the top message's id.
///
/// The cursor's `index` counts the topics listed before it.
pub(crate) fn next_cursor(
    parts: &TopicsParts<'_>,
    chat: ChatId,
    previous: Option<&TopicsCursor>,
) -> Option<TopicsCursor> {
    let listed = previous
        .map_or(0, |c| c.index)
        .saturating_add(u32::try_from(parts.topics.len()).unwrap_or(u32::MAX));
    if parts.topics.is_empty() || i64::from(listed) >= i64::from(parts.count) {
        return None;
    }
    let last = parts.topics.iter().rev().find_map(|t| match t {
        tl::enums::ForumTopic::Topic(t) => Some(t),
        tl::enums::ForumTopic::Deleted(_) => None,
    })?;
    let offset_date = match parts.order {
        TopicOrder::LastActivity => tops(parts, chat)
            .get(&last.top_message)
            .map_or(0, |m| raw_date(m)),
        TopicOrder::Created => last.date,
    };
    Some(TopicsCursor {
        order: parts.order,
        offset_date,
        offset_id: last.top_message,
        offset_topic: last.id,
        index: listed,
    })
}

/// A topics page of `chat` after `previous`.
/// `forumTopicDeleted` entries are skipped (a page never removes a topic).
pub(crate) fn page(
    parts: &TopicsParts<'_>,
    dir: &Directory,
    chat: ChatId,
    previous: Option<&TopicsCursor>,
) -> (TopicsPage, usize) {
    let mapped = topics(parts, dir, chat);
    let skipped = mapped.skipped.saturating_add(mapped.deleted.len());
    let page = TopicsPage {
        order: parts.order,
        topics: mapped.found,
        next: next_cursor(parts, chat, previous),
        post_closed: post_closed(dir, chat),
    };
    (page, skipped)
}

/// A by-id result of `chat`. Ids the server leaves out are neither found
/// nor deleted: core keeps them due (plan section 6.4).
pub(crate) fn by_id(parts: &TopicsParts<'_>, dir: &Directory, chat: ChatId) -> (TopicsById, usize) {
    let mapped = topics(parts, dir, chat);
    let result = TopicsById {
        found: mapped.found,
        deleted: mapped.deleted,
        post_closed: post_closed(dir, chat),
    };
    (result, mapped.skipped)
}

/// The parts of a `messages.PeerDialogs` response for `limit` requested
/// chats, size-checked like a dialogs page.
pub(crate) fn peer_dialogs_parts(
    resp: &tl::enums::messages::PeerDialogs,
    limit: usize,
) -> Result<DialogsParts<'_>, Oversized> {
    let tl::enums::messages::PeerDialogs::Dialogs(r) = resp;
    checked_dialogs(
        DialogsParts {
            dialogs: &r.dialogs,
            messages: &r.messages,
            users: &r.users,
            chats: &r.chats,
            complete: true,
        },
        limit,
    )
}

/// The summaries of the `requested` chats, mapped like a dialogs page (kind
/// from `chats`, unread, top message; broadcasts excluded). Dialogs for
/// chats nobody asked about are dropped: they are not this call's answer.
pub(crate) fn peer_dialogs(
    parts: &DialogsParts<'_>,
    dir: &Directory,
    requested: &[ChatId],
) -> MappedDialogs {
    let mut mapped = dialogs(parts, dir, requested.len());
    mapped.chats.retain(|c| requested.contains(&c.id));
    mapped.next = None;
    mapped
}

/// A topic id from an update; `None` (the update is dropped) when it is
/// not positive. The id is not logged.
pub(super) fn topic_of(id: i32) -> Option<TopicId> {
    let topic = TopicId::try_from(id).ok();
    if topic.is_none() {
        tracing::debug!("topic update with a non-positive topic id dropped");
    }
    topic
}

/// The forum's live updates (plan section 5): topic reads, topic edits,
/// pins and channel changes. `None` for every other update, for broadcast
/// channels, and for a non-positive topic id. None of these has a `pts`,
/// so a lost one is never replayed; only a later snapshot corrects it.
pub(super) fn forum_update(raw: &tl::enums::Update, dir: &Directory) -> Option<Update> {
    use tl::enums::Update as U;
    match raw {
        U::ReadChannelDiscussionInbox(u) => {
            let chat = channel_chat(u.channel_id)?;
            // A discussion of a channel post (comments are out of v1).
            if u.broadcast_id.is_some() || is_broadcast(chat, dir) {
                return None;
            }
            Some(Update::ReadTopicMax {
                chat,
                topic: topic_of(u.top_msg_id)?,
                max_id: MsgId(u.read_max_id.max(0)),
            })
        }
        U::EditForumTopic(u) => Some(Update::TopicEdited {
            chat: chat_id(&u.peer)?,
            topic: topic_of(u.topic_id)?,
            title: u.title.as_deref().map(display_name),
            closed: u.closed,
            visibility: u.hidden.map(|hidden| {
                if hidden {
                    Visibility::Hidden
                } else {
                    Visibility::Shown
                }
            }),
        }),
        U::PinnedForumTopic(u) => {
            topic_of(u.topic_id)?;
            Some(Update::TopicsPinned {
                chat: chat_id(&u.peer)?,
            })
        }
        U::PinnedForumTopics(u) => Some(Update::TopicsPinned {
            chat: chat_id(&u.peer)?,
        }),
        U::Channel(u) => {
            let chat = channel_chat(u.channel_id)?;
            (!is_broadcast(chat, dir)).then_some(Update::ChatChanged { chat })
        }
        _ => None,
    }
}
