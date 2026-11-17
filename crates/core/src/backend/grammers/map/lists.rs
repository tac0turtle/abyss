//! Responses to pages and updates: dialogs, history, live updates and the
//! `messages.sendMessage` result.

use std::collections::HashMap;

use grammers_client::tl;

use super::message::{message, raw_date, raw_key};
use super::peers::{Directory, MAX_DIRECTORY, channel_chat, chat_id};
use super::topics::{forum_update, topic_of};
use crate::backend::{DialogsCursor, HistoryPage, HistoryQuery, Update};
use crate::model::{
    ChatId, ChatSummary, Message, MsgId, PeerFolder, Preview, ReadState, ThreadKey,
};
use crate::store::{ForumStatus, Route, route};

/// Ids accepted from one deletion update. Telegram batches at most 100 per
/// request; the bound only guards against a malformed update.
pub(crate) const MAX_DELETED_IDS: usize = 10_000;

/// Telegram's peer folder id of the archive (`dialog.folder_id`,
/// `folderPeer.folder_id`); `0`, or no id, is the main list.
pub(crate) const ARCHIVE_FOLDER: i32 = 1;
/// The peer folder a raw folder id names. Only the archive has an id other
/// than the main list's; an unknown one is treated as the main list.
fn peer_folder(id: i32) -> PeerFolder {
    if id == ARCHIVE_FOLDER {
        PeerFolder::Archive
    } else {
        PeerFolder::Main
    }
}

/// Folder moves accepted from one `updateFolderPeers`. The bound only guards
/// against a malformed update.
pub(crate) const MAX_FOLDER_MOVES: usize = 1000;

/// Items a response may carry beyond the requested limit (folder entries,
/// a message sent while the page was being built) before it is rejected.
pub(crate) const RESPONSE_SLACK: usize = 16;
/// Users plus chats one response may carry. Auxiliary peers are not
/// proportional to the requested items (one service message can add hundreds
/// of users; mentions, forwards and replies name more), so they are bounded
/// on their own, by what a [`Directory`] holds.
pub(crate) const MAX_RESPONSE_PEERS: usize = MAX_DIRECTORY;

/// A response carrying more than its request allows. It is rejected as a
/// whole: mapping a truncated prefix would present partial data as the
/// server's answer (a cursor past chats never shown, a `has_more` that lies).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Oversized;

pub(super) fn at_most(n: usize, max: usize) -> Result<(), Oversized> {
    if n <= max { Ok(()) } else { Err(Oversized) }
}

/// The parts of a `messages.Dialogs` response, checked against the request.
/// No `Debug` of the payload: it carries names, phones and message text.
pub(crate) struct DialogsParts<'a> {
    pub(crate) dialogs: &'a [tl::enums::Dialog],
    pub(crate) messages: &'a [tl::enums::Message],
    pub(crate) users: &'a [tl::enums::User],
    pub(crate) chats: &'a [tl::enums::Chat],
    /// The server sent the whole list.
    pub(crate) complete: bool,
}

impl std::fmt::Debug for DialogsParts<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DialogsParts")
            .field("dialogs", &self.dialogs.len())
            .field("messages", &self.messages.len())
            .field("users", &self.users.len())
            .field("chats", &self.chats.len())
            .field("complete", &self.complete)
            .finish()
    }
}

/// The parts of a dialogs page requested with `limit`, or [`Oversized`] when
/// the response carries more dialogs, messages or peers than that request
/// allows. Checked before anything is mapped, hashed or reserved.
pub(crate) fn dialogs_parts(
    resp: &tl::enums::messages::Dialogs,
    limit: usize,
) -> Result<DialogsParts<'_>, Oversized> {
    checked_dialogs(raw_dialogs_parts(resp), limit)
}

/// `parts` if they fit a request for `limit` dialogs.
pub(super) fn checked_dialogs(
    parts: DialogsParts<'_>,
    limit: usize,
) -> Result<DialogsParts<'_>, Oversized> {
    let items = limit.saturating_add(RESPONSE_SLACK);
    at_most(parts.dialogs.len(), items)?;
    // One top message per dialog; forum dialogs may add a topic's top.
    at_most(parts.messages.len(), items.saturating_mul(2))?;
    at_most(
        parts.users.len().saturating_add(parts.chats.len()),
        MAX_RESPONSE_PEERS,
    )?;
    Ok(parts)
}

fn raw_dialogs_parts(resp: &tl::enums::messages::Dialogs) -> DialogsParts<'_> {
    use tl::enums::messages::Dialogs as D;
    match resp {
        D::Dialogs(d) => DialogsParts {
            dialogs: &d.dialogs,
            messages: &d.messages,
            users: &d.users,
            chats: &d.chats,
            complete: true,
        },
        D::Slice(d) => DialogsParts {
            dialogs: &d.dialogs,
            messages: &d.messages,
            users: &d.users,
            chats: &d.chats,
            complete: false,
        },
        D::NotModified(_) => DialogsParts {
            dialogs: &[],
            messages: &[],
            users: &[],
            chats: &[],
            complete: true,
        },
    }
}

/// A mapped dialogs page. `Debug` shows counts only.
#[derive(Default)]
pub(crate) struct MappedDialogs {
    pub(crate) chats: Vec<ChatSummary>,
    pub(crate) next: Option<DialogsCursor>,
    /// Broadcast channels left out (plan section 7), for a debug count.
    pub(crate) excluded: usize,
}

impl std::fmt::Debug for MappedDialogs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedDialogs")
            .field("chats", &self.chats.len())
            .field("more", &self.next.is_some())
            .field("excluded", &self.excluded)
            .finish()
    }
}

type Tops<'a> = HashMap<(ChatId, i32), &'a tl::enums::Message>;

/// Where a raw dialog entry would continue the list: its peer and top
/// message. Folders count (they hold a place in the list, and `iter_dialogs`
/// continues after them too); communities carry no peer.
fn cursor_point(d: &tl::enums::Dialog) -> Option<(ChatId, i32)> {
    match d {
        tl::enums::Dialog::Dialog(d) => Some((chat_id(&d.peer)?, d.top_message)),
        tl::enums::Dialog::Folder(f) => Some((chat_id(&f.peer)?, f.top_message)),
        tl::enums::Dialog::Community(_) => None,
    }
}

/// Maps a dialogs page requested with `limit`. Folders and communities are
/// skipped, broadcast channels excluded. Whether more pages follow is decided
/// from the raw response, as `iter_dialogs` does: a slice with `limit` raw
/// entries, whatever display filtering left. The cursor continues after the
/// last raw entry that names a peer, shown or not.
pub(crate) fn dialogs(parts: &DialogsParts<'_>, dir: &Directory, limit: usize) -> MappedDialogs {
    let tops: Tops<'_> = parts
        .messages
        .iter()
        .filter_map(|m| Some((raw_key(m)?, m)))
        .collect();
    let mut out = MappedDialogs {
        chats: Vec::with_capacity(parts.dialogs.len().min(limit)),
        ..MappedDialogs::default()
    };
    for d in parts.dialogs {
        if let tl::enums::Dialog::Dialog(d) = d {
            summarize(d, &tops, dir, &mut out);
        }
    }
    let more = !parts.complete && parts.dialogs.len() >= limit;
    out.next = parts
        .dialogs
        .iter()
        .rev()
        .find_map(cursor_point)
        .filter(|_| more)
        .map(|(chat, offset_id)| DialogsCursor {
            offset_date: tops.get(&(chat, offset_id)).map_or(0, |m| raw_date(m)),
            offset_id,
            offset_peer: Some(chat),
            index: 0,
        });
    out
}

/// Adds one raw dialog to the page, unless it is excluded or invalid.
fn summarize(d: &tl::types::Dialog, tops: &Tops<'_>, dir: &Directory, out: &mut MappedDialogs) {
    let Some(chat) = chat_id(&d.peer) else {
        return;
    };
    let Some(entry) = dir.get(chat) else {
        return;
    };
    if entry.broadcast {
        out.excluded += 1;
        return;
    }
    let top = tops.get(&(chat, d.top_message)).copied();
    let last_message = top.and_then(|m| message(m, dir));
    let summary = ChatSummary {
        id: chat,
        title: entry.title.clone(),
        kind: entry.kind,
        pinned: d.pinned,
        read: ReadState {
            unread: count(d.unread_count),
            read_inbox_max: MsgId(d.read_inbox_max_id.max(0)),
        },
        unread_mentions: count(d.unread_mentions_count),
        last: last_message.as_ref().map(Preview::of),
        last_date: last_message.as_ref().map_or(0, |m| m.date),
        // The top message id is authoritative even when the message itself
        // did not map (excluded or invalid).
        last_id: (d.top_message > 0).then_some(MsgId(d.top_message)),
        can_send: entry.can_send,
        muted_until: muted_until(&d.notify_settings),
        folder: peer_folder(d.folder_id.unwrap_or(0)),
        reading: None,
    };
    if summary.is_valid() {
        out.chats.push(summary);
    }
}

/// Until when notifications are muted (Unix seconds; `0`: not muted).
/// Telegram mutes until a date (`i32::MAX` for good); the date is kept, so
/// an expired mute stops counting without a new page.
fn muted_until(settings: &tl::enums::PeerNotifySettings) -> i64 {
    let tl::enums::PeerNotifySettings::Settings(s) = settings;
    s.mute_until.map_or(0, |until| i64::from(until.max(0)))
}

pub(super) fn count(n: i32) -> u32 {
    u32::try_from(n).unwrap_or(0)
}

/// The parts of a `messages.Messages` response, checked against the request.
/// No `Debug` of the payload: it carries names, phones and message text.
pub(crate) struct MessagesParts<'a> {
    pub(crate) messages: &'a [tl::enums::Message],
    pub(crate) users: &'a [tl::enums::User],
    pub(crate) chats: &'a [tl::enums::Chat],
    pub(crate) complete: bool,
}

impl std::fmt::Debug for MessagesParts<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessagesParts")
            .field("messages", &self.messages.len())
            .field("users", &self.users.len())
            .field("chats", &self.chats.len())
            .field("complete", &self.complete)
            .finish()
    }
}

/// The parts of a history page requested with `limit`, or [`Oversized`] when
/// the response carries more messages or peers than that request allows.
/// Checked before anything is mapped or sorted.
pub(crate) fn messages_parts(
    resp: &tl::enums::messages::Messages,
    limit: usize,
) -> Result<MessagesParts<'_>, Oversized> {
    let parts = raw_messages_parts(resp);
    at_most(parts.messages.len(), limit.saturating_add(RESPONSE_SLACK))?;
    at_most(
        parts.users.len().saturating_add(parts.chats.len()),
        MAX_RESPONSE_PEERS,
    )?;
    Ok(parts)
}

fn raw_messages_parts(resp: &tl::enums::messages::Messages) -> MessagesParts<'_> {
    use tl::enums::messages::Messages as M;
    match resp {
        M::Messages(m) => MessagesParts {
            messages: &m.messages,
            users: &m.users,
            chats: &m.chats,
            complete: true,
        },
        M::Slice(m) => MessagesParts {
            messages: &m.messages,
            users: &m.users,
            chats: &m.chats,
            complete: false,
        },
        M::ChannelMessages(m) => MessagesParts {
            messages: &m.messages,
            users: &m.users,
            chats: &m.chats,
            complete: false,
        },
        M::NotModified(_) => MessagesParts {
            messages: &[],
            users: &[],
            chats: &[],
            complete: true,
        },
    }
}

/// Whether `m` belongs to `thread`. A whole-chat key takes every message of
/// the chat. A topic key takes the messages that normalize to it: in a forum
/// a message without a topic header is General's (plan section 3). Messages
/// keep the topic the server stated; core normalizes them.
fn in_thread(m: &Message, thread: ThreadKey) -> bool {
    m.thread.chat == thread.chat
        && (thread.topic.is_none()
            || route(m.thread.chat, m.thread.topic, ForumStatus::Forum) == Route::Thread(thread))
}

/// Maps a history page of `thread` requested with `limit`: only messages of
/// that thread on the requested side of the anchor, ascending, without
/// duplicates.
pub(crate) fn history(
    parts: &MessagesParts<'_>,
    dir: &Directory,
    thread: ThreadKey,
    query: HistoryQuery,
    limit: usize,
) -> HistoryPage {
    let mut messages: Vec<Message> = parts
        .messages
        .iter()
        .filter_map(|m| message(m, dir))
        .filter(|m| in_thread(m, thread) && m.is_valid())
        .filter(|m| match query {
            // `Around` keeps both sides of its anchor; core checks what
            // lies above it.
            HistoryQuery::Tail | HistoryQuery::Around(_) => true,
            HistoryQuery::Before(id) => m.id < id,
            HistoryQuery::After(id) => m.id > id,
        })
        .collect();
    messages.sort_unstable_by_key(|m| m.id);
    messages.dedup_by_key(|m| m.id);
    let has_more = !parts.complete && parts.messages.len() >= limit;
    HistoryPage { messages, has_more }
}

pub(super) fn is_broadcast(chat: ChatId, dir: &Directory) -> bool {
    dir.get(chat).is_some_and(|e| e.broadcast)
}

/// Maps a live update. `None` for everything the client ignores (counted by
/// the caller at debug level), for broadcast-channel traffic, and for topic
/// updates naming a non-positive topic.
pub(crate) fn update(raw: &tl::enums::Update, dir: &Directory) -> Option<Update> {
    use tl::enums::Update as U;
    let live = |m: &tl::enums::Message| message(m, dir).filter(Message::is_valid);
    match raw {
        U::NewMessage(u) => live(&u.message).map(Update::NewMessage),
        U::NewChannelMessage(u) => live(&u.message).map(Update::NewMessage),
        U::EditMessage(u) => live(&u.message).map(Update::Edited),
        U::EditChannelMessage(u) => live(&u.message).map(Update::Edited),
        U::DeleteMessages(u) => Some(Update::Deleted {
            chat: None,
            ids: deleted_ids(&u.messages),
        }),
        U::DeleteChannelMessages(u) => {
            let chat = channel_chat(u.channel_id)?;
            if is_broadcast(chat, dir) {
                return None;
            }
            Some(Update::Deleted {
                chat: Some(chat),
                ids: deleted_ids(&u.messages),
            })
        }
        // grammers asserts that this update never names a channel
        // (`message_box/adaptor.rs:520`, plan risk 2), so today only
        // non-channel peers reach here. Channel peers are mapped the same
        // way, for a grammers that stops asserting.
        U::ReadHistoryInbox(u) => Some(Update::ReadInbox {
            chat: chat_id(&u.peer)?,
            topic: match u.top_msg_id {
                Some(top) => Some(topic_of(top)?),
                None => None,
            },
            still_unread: count(u.still_unread_count),
            max_id: MsgId(u.max_id.max(0)),
        }),
        U::ReadChannelInbox(u) => Some(Update::ReadInbox {
            chat: channel_chat(u.channel_id)?,
            topic: None,
            still_unread: count(u.still_unread_count),
            max_id: MsgId(u.max_id.max(0)),
        }),
        // A chat folder was created, edited, deleted or reordered: core
        // refetches the list (folders plan, part B).
        U::DialogFilter(_) | U::DialogFilterOrder(_) | U::DialogFilters => {
            Some(Update::ChatFoldersChanged)
        }
        // Archived or unarchived, here or on another device.
        U::FolderPeers(u) => {
            let moves: Vec<(ChatId, PeerFolder)> = u
                .folder_peers
                .iter()
                .take(MAX_FOLDER_MOVES)
                .filter_map(|tl::enums::FolderPeer::Peer(p)| {
                    let chat = chat_id(&p.peer)?;
                    Some((chat, peer_folder(p.folder_id)))
                })
                .collect();
            (!moves.is_empty()).then_some(Update::Folders(moves))
        }
        // Correlated with the pending-send table by core (plan section 9).
        U::MessageId(u) if u.id > 0 && u.random_id != 0 => Some(Update::MessageId {
            random_id: u.random_id,
            id: MsgId(u.id),
        }),
        _ => forum_update(raw, dir),
    }
}

fn deleted_ids(ids: &[i32]) -> Vec<MsgId> {
    ids.iter()
        .take(MAX_DELETED_IDS)
        .copied()
        .map(MsgId)
        .collect()
}

/// The server id `messages.sendMessage` assigned to `random_id`, if the
/// response carries the mapping (plan section 9).
pub(crate) fn sent_id(resp: &tl::enums::Updates, random_id: i64) -> Option<MsgId> {
    use tl::enums::Updates as U;
    let from_list = |updates: &[tl::enums::Update]| {
        updates.iter().find_map(|u| match u {
            tl::enums::Update::MessageId(m) if m.random_id == random_id => Some(MsgId(m.id)),
            _ => None,
        })
    };
    match resp {
        U::UpdateShortSentMessage(u) => Some(MsgId(u.id)),
        U::Updates(u) => from_list(&u.updates),
        U::Combined(u) => from_list(&u.updates),
        U::UpdateShort(u) => from_list(std::slice::from_ref(&u.update)),
        U::TooLong | U::UpdateShortMessage(_) | U::UpdateShortChatMessage(_) => None,
    }
}
