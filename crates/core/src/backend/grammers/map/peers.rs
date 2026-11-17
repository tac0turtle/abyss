//! Peers: ids, display data and access hashes from TL users and chats.
//!
//! Every id from the server is range-checked here before it reaches grammers'
//! `PeerId`: the unchecked constructors only `debug_assert`, `PeerId::kind` is
//! `unreachable!()` outside the ranges, and even the checked
//! `PeerId::channel` overflows (a debug-build panic) on ids near `i64::MAX`.

use std::collections::HashMap;
use std::ops::RangeInclusive;

use grammers_client::session::types::{PeerAuth, PeerId, PeerInfo};
use grammers_client::tl;

use crate::model::{ChatId, ChatKind, PeerKind};
use crate::sanitize::{MAX_NAME_BYTES, clean, display_name};
use crate::store::PostClosed;

/// Entries kept from one response. Telegram sends at most a few hundred users
/// and chats per response; anything beyond this is ignored.
pub(crate) const MAX_DIRECTORY: usize = 4096;

const DELETED_ACCOUNT: &str = "Deleted Account";
const UNKNOWN: &str = "Unknown";
const SAVED_MESSAGES: &str = "Saved Messages";

// Bare id ranges, as in `grammers-session/src/peer.rs` (`USER_ID_RANGE` etc.).
const USER_IDS: RangeInclusive<i64> = 1..=0xff_ffff_ffff;
const CHAT_IDS: RangeInclusive<i64> = 1..=999_999_999_999;
const CHANNEL_IDS: RangeInclusive<i64> = 1..=997_852_516_352;
const MONOFORUM_IDS: RangeInclusive<i64> = 1_002_147_483_649..=3_000_000_000_000;

fn valid(kind: PeerKind, id: i64) -> bool {
    match kind {
        PeerKind::User => USER_IDS.contains(&id),
        PeerKind::Chat => CHAT_IDS.contains(&id),
        PeerKind::Channel => CHANNEL_IDS.contains(&id) || MONOFORUM_IDS.contains(&id),
    }
}

fn checked(kind: PeerKind, id: i64) -> Option<ChatId> {
    valid(kind, id).then_some(ChatId { kind, id })
}

pub(crate) fn user_chat(id: i64) -> Option<ChatId> {
    checked(PeerKind::User, id)
}

pub(crate) fn group_chat(id: i64) -> Option<ChatId> {
    checked(PeerKind::Chat, id)
}

pub(crate) fn channel_chat(id: i64) -> Option<ChatId> {
    checked(PeerKind::Channel, id)
}

/// The chat a TL peer names, if its id is in range.
pub(crate) fn chat_id(peer: &tl::enums::Peer) -> Option<ChatId> {
    match peer {
        tl::enums::Peer::User(p) => user_chat(p.user_id),
        tl::enums::Peer::Chat(p) => group_chat(p.chat_id),
        tl::enums::Peer::Channel(p) => channel_chat(p.channel_id),
    }
}

/// grammers' id for one of our chats (only built from an in-range id).
pub(crate) fn peer_id(chat: ChatId) -> Option<PeerId> {
    if !valid(chat.kind, chat.id) {
        return None;
    }
    match chat.kind {
        PeerKind::User => PeerId::user(chat.id),
        PeerKind::Chat => PeerId::chat(chat.id),
        PeerKind::Channel => PeerId::channel(chat.id),
    }
}

/// The `InputPeer` addressing `chat`, given what the session knows about it.
/// Users and channels need an access hash; basic groups do not.
pub(crate) fn input_peer(chat: ChatId, info: Option<&PeerInfo>) -> Option<tl::enums::InputPeer> {
    match (chat.kind, info) {
        (PeerKind::Chat, _) => {
            group_chat(chat.id)?;
            Some(tl::enums::InputPeer::Chat(tl::types::InputPeerChat {
                chat_id: chat.id,
            }))
        }
        (PeerKind::User, Some(PeerInfo::User { id, auth, .. })) if *id == chat.id => {
            Some(tl::enums::InputPeer::User(tl::types::InputPeerUser {
                user_id: chat.id,
                access_hash: auth.as_ref().map(|a| a.hash())?,
            }))
        }
        (PeerKind::Channel, Some(PeerInfo::Channel { id, auth, .. })) if *id == chat.id => {
            Some(tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
                channel_id: chat.id,
                access_hash: auth.as_ref().map(|a| a.hash())?,
            }))
        }
        _ => None,
    }
}

/// The `InputChannel` for `channels.*` calls, from an `InputPeer`.
pub(crate) fn input_channel(peer: &tl::enums::InputPeer) -> Option<tl::enums::InputChannel> {
    match peer {
        tl::enums::InputPeer::Channel(c) => {
            Some(tl::enums::InputChannel::Channel(tl::types::InputChannel {
                channel_id: c.channel_id,
                access_hash: c.access_hash,
            }))
        }
        _ => None,
    }
}

/// What the client shows and needs about one peer of a response. `Debug`
/// shows the chat kind only: names and titles are private.
#[derive(Clone)]
pub(crate) struct PeerEntry {
    /// Sender name (sanitized).
    pub(crate) name: String,
    /// Chat-list title (sanitized): the name, or "Saved Messages" for self.
    pub(crate) title: String,
    pub(crate) kind: ChatKind,
    /// A broadcast channel: excluded from Stage 1 (plan section 7).
    pub(crate) broadcast: bool,
    pub(crate) can_send: bool,
    /// Whether the user may post in a forum's closed topics: the channel's
    /// creator, or an admin with `manage_topics` (plan section 4). Refused
    /// for every other peer.
    pub(crate) post_closed: PostClosed,
    /// For the session's peer cache. "Min" constructors carry `auth: None`,
    /// so they never overwrite a known hash (`extend_info`).
    pub(crate) info: PeerInfo,
}

impl std::fmt::Debug for PeerEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerEntry")
            .field("kind", &self.kind)
            .field("broadcast", &self.broadcast)
            .field("can_send", &self.can_send)
            .finish_non_exhaustive()
    }
}

/// Peers of one response, keyed by chat. Bounded by [`MAX_DIRECTORY`].
/// `Debug` shows the count only.
#[derive(Default)]
pub(crate) struct Directory {
    peers: HashMap<ChatId, PeerEntry>,
}

impl std::fmt::Debug for Directory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Directory")
            .field("peers", &self.peers.len())
            .finish()
    }
}

impl Directory {
    /// Reads at most [`MAX_DIRECTORY`] raw entries, users first: nothing past
    /// the bound is even sanitized. Callers reject responses whose peer count
    /// exceeds their own, smaller, budget before this runs.
    pub(crate) fn from_lists(users: &[tl::enums::User], chats: &[tl::enums::Chat]) -> Self {
        let users = users.get(..MAX_DIRECTORY).unwrap_or(users);
        let room = MAX_DIRECTORY.saturating_sub(users.len());
        let chats = chats.get(..room).unwrap_or(chats);
        let mut dir = Directory {
            peers: HashMap::with_capacity(users.len().saturating_add(chats.len())),
        };
        for user in users {
            dir.insert_user(user);
        }
        for chat in chats {
            dir.insert_chat(chat);
        }
        dir
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.peers.len()
    }

    pub(crate) fn insert_user(&mut self, user: &tl::enums::User) {
        if let tl::enums::User::User(u) = user
            && let Some((chat, entry)) = user_entry(u)
        {
            self.insert(chat, entry);
        }
    }

    pub(crate) fn insert_chat(&mut self, chat: &tl::enums::Chat) {
        if let Some((id, entry)) = chat_entry(chat) {
            self.insert(id, entry);
        }
    }

    fn insert(&mut self, chat: ChatId, entry: PeerEntry) {
        if self.peers.len() < MAX_DIRECTORY || self.peers.contains_key(&chat) {
            self.peers.insert(chat, entry);
        }
    }

    pub(crate) fn get(&self, chat: ChatId) -> Option<&PeerEntry> {
        self.peers.get(&chat)
    }

    /// Peer infos worth caching in the session: those that can address a chat.
    pub(crate) fn infos(&self) -> impl Iterator<Item = PeerInfo> + '_ {
        self.peers
            .values()
            .filter(|e| e.info.auth().is_some())
            .map(|e| e.info.clone())
    }
}

fn user_name(u: &tl::types::User) -> String {
    let first = clean(u.first_name.as_deref().unwrap_or_default(), MAX_NAME_BYTES);
    let last = clean(u.last_name.as_deref().unwrap_or_default(), MAX_NAME_BYTES);
    let joined = match (first.is_empty(), last.is_empty()) {
        (_, true) => first,
        (true, false) => last,
        (false, false) => format!("{first} {last}"),
    };
    let name = display_name(&joined);
    match (name.is_empty(), u.deleted) {
        (false, _) => name,
        (true, true) => DELETED_ACCOUNT.to_owned(),
        (true, false) => UNKNOWN.to_owned(),
    }
}

fn user_entry(u: &tl::types::User) -> Option<(ChatId, PeerEntry)> {
    let chat = user_chat(u.id)?;
    let name = user_name(u);
    let title = if u.is_self {
        SAVED_MESSAGES.to_owned()
    } else {
        name.clone()
    };
    let entry = PeerEntry {
        name,
        title,
        kind: if u.bot {
            ChatKind::Bot
        } else {
            ChatKind::Private
        },
        broadcast: false,
        can_send: !u.deleted,
        post_closed: PostClosed::Refused,
        info: PeerInfo::User {
            id: u.id,
            auth: u.access_hash.filter(|_| !u.min).map(PeerAuth::from_hash),
            bot: Some(u.bot),
            is_self: Some(u.is_self),
        },
    };
    Some((chat, entry))
}

fn title(raw: &str) -> String {
    let t = display_name(raw);
    if t.is_empty() { UNKNOWN.to_owned() } else { t }
}

fn sends_banned(rights: Option<&tl::enums::ChatBannedRights>) -> bool {
    matches!(rights, Some(tl::enums::ChatBannedRights::Rights(r)) if r.send_messages || r.send_plain)
}

fn manages_topics(rights: Option<&tl::enums::ChatAdminRights>) -> bool {
    matches!(rights, Some(tl::enums::ChatAdminRights::Rights(r)) if r.manage_topics)
}

fn chat_entry(chat: &tl::enums::Chat) -> Option<(ChatId, PeerEntry)> {
    use tl::enums::Chat as C;
    match chat {
        C::Chat(c) => {
            let id = group_chat(c.id)?;
            let admin = c.creator || c.admin_rights.is_some();
            let can_send = !c.left
                && !c.deactivated
                && c.migrated_to.is_none()
                && (admin || !sends_banned(c.default_banned_rights.as_ref()));
            let mut entry = group_entry(c.id, &c.title);
            entry.can_send = can_send;
            Some((id, entry))
        }
        C::Forbidden(c) => Some((group_chat(c.id)?, group_entry(c.id, &c.title))),
        C::Channel(c) => channel_entry(c),
        C::ChannelForbidden(c) => {
            let id = channel_chat(c.id)?;
            let t = title(&c.title);
            Some((
                id,
                PeerEntry {
                    name: t.clone(),
                    title: t,
                    kind: ChatKind::Supergroup {
                        forum: false,
                        gigagroup: false,
                    },
                    broadcast: c.broadcast,
                    can_send: false,
                    post_closed: PostClosed::Refused,
                    info: PeerInfo::Channel {
                        id: c.id,
                        auth: Some(PeerAuth::from_hash(c.access_hash)),
                        kind: None,
                    },
                },
            ))
        }
        // Communities are not chats; empty chats carry nothing to show.
        C::Empty(_) | C::Community(_) | C::CommunityForbidden(_) => None,
    }
}

/// A basic group that cannot be sent to until the caller says otherwise.
fn group_entry(id: i64, raw_title: &str) -> PeerEntry {
    let t = title(raw_title);
    PeerEntry {
        name: t.clone(),
        title: t,
        kind: ChatKind::Group,
        broadcast: false,
        can_send: false,
        post_closed: PostClosed::Refused,
        info: PeerInfo::Chat { id },
    }
}

fn channel_entry(c: &tl::types::Channel) -> Option<(ChatId, PeerEntry)> {
    let id = channel_chat(c.id)?;
    let admin = c.creator || c.admin_rights.is_some();
    let banned = sends_banned(c.banned_rights.as_ref())
        || (!admin && sends_banned(c.default_banned_rights.as_ref()));
    let t = title(&c.title);
    let kind = if c.gigagroup {
        Some(grammers_client::session::types::ChannelKind::Gigagroup)
    } else if c.broadcast {
        Some(grammers_client::session::types::ChannelKind::Broadcast)
    } else if c.megagroup {
        Some(grammers_client::session::types::ChannelKind::Megagroup)
    } else {
        None
    };
    let entry = PeerEntry {
        name: t.clone(),
        title: t,
        kind: ChatKind::Supergroup {
            forum: c.forum,
            gigagroup: c.gigagroup,
        },
        broadcast: c.broadcast,
        // Gigagroups are read-only for members (plan section 7).
        can_send: !c.left && !c.gigagroup && !c.broadcast && !banned,
        post_closed: if c.creator || manages_topics(c.admin_rights.as_ref()) {
            PostClosed::Allowed
        } else {
            PostClosed::Refused
        },
        info: PeerInfo::Channel {
            id: c.id,
            auth: c.access_hash.filter(|_| !c.min).map(PeerAuth::from_hash),
            kind,
        },
    };
    Some((id, entry))
}
