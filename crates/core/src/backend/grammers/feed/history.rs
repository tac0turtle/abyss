//! Observe history effects before and after grammers processes a batch.

use std::collections::{BTreeSet, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};

use grammers_client::tl;
use grammers_session::updates::{State, UpdatesLike};

use super::MAX_BATCH;
use super::map::{lists::MAX_DELETED_IDS, message::raw_key, peers};
use crate::model::{ChatId, PeerKind};

/// Only identities and ordering metadata are retained across replies. Overflow
/// loses the ability to prove coverage, so it falls back to an account purge.
#[derive(Default)]
pub(super) struct HistoryUpdates {
    pending: HashMap<Key, Pending>,
    overflow: bool,
}

#[derive(Clone, Copy)]
struct Pending {
    pts: i32,
    copies: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Effect {
    New(i32),
    Edit(i32),
    Delete(u64),
    ReadInbox(i32, Option<i32>),
    ReadOutbox(i32),
    WebPage,
    ReadContents,
    Folders,
    Pins,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    chat: Option<ChatId>,
    effect: Effect,
    pts: i32,
    count: i32,
}

struct Position {
    key: Key,
    pts: i32,
}

impl HistoryUpdates {
    pub(super) fn from_raw(raw: &[tl::enums::Update]) -> Self {
        Self::collect(raw.iter().filter_map(Position::history))
    }

    pub(super) fn offered_common(raw: &[tl::enums::Update], socket: &UpdatesLike) -> Self {
        Self::collect(
            raw.iter()
                .filter_map(Position::common)
                .chain(short_common(socket)),
        )
    }

    fn collect(positions: impl IntoIterator<Item = Position>) -> Self {
        let mut history = Self::default();
        for Position { key, pts } in positions {
            let pending = history
                .pending
                .entry(key)
                .or_insert(Pending { pts, copies: 0 });
            pending.pts = pending.pts.max(pts);
            pending.copies += 1;
        }
        history
    }

    pub(super) fn merge(&mut self, other: Self) {
        for (key, offered) in other.pending {
            if self.pending.len() >= MAX_BATCH && !self.pending.contains_key(&key) {
                self.overflow = true;
                break;
            }
            let pending = self.pending.entry(key).or_insert(offered);
            pending.pts = pending.pts.max(offered.pts);
            pending.copies = pending.copies.max(offered.copies);
        }
        self.overflow |= other.overflow;
    }

    pub(super) fn retain_above(&mut self, pts: i32) {
        self.pending.retain(|_, pending| pending.pts > pts);
    }

    pub(super) fn exclude_broadcasts(&mut self, broadcasts: &BTreeSet<i64>) {
        if !broadcasts.is_empty() {
            self.pending.retain(|key, _| {
                key.chat.is_none_or(|chat| {
                    chat.kind != PeerKind::Channel || !broadcasts.contains(&chat.id)
                })
            });
        }
    }

    pub(super) fn account_for(&mut self, applied: &[(tl::enums::Update, State)]) {
        for (update, _) in applied {
            if self.pending.is_empty() {
                break;
            }
            if let Some(Position { key, .. }) = Position::from_update(update) {
                self.remove_one(key);
                // A bare short send carries no peer. Its id is account-wide.
                if matches!(key.effect, Effect::New(_))
                    && key.chat.is_some_and(|chat| chat.kind != PeerKind::Channel)
                {
                    self.remove_one(Key { chat: None, ..key });
                }
            }
        }
    }

    fn remove_one(&mut self, key: Key) {
        if let std::collections::hash_map::Entry::Occupied(mut entry) = self.pending.entry(key) {
            if entry.get().copies == 1 {
                entry.remove();
            } else {
                entry.get_mut().copies -= 1;
            }
        }
    }

    pub(super) fn scopes(self) -> BTreeSet<Option<ChatId>> {
        if self.overflow {
            return BTreeSet::from([None]);
        }
        self.pending.into_keys().map(|key| key.chat).collect()
    }
}

impl Position {
    fn ordered(chat: Option<ChatId>, effect: Effect, pts: i32, count: i32) -> Self {
        Self {
            key: Key {
                chat,
                effect,
                pts,
                count,
            },
            pts,
        }
    }

    fn new_message(chat: Option<ChatId>, id: i32, pts: i32) -> Self {
        // A message returned in new_messages has the same identity but no pts.
        Self {
            pts,
            ..Self::ordered(chat, Effect::New(id), 0, 0)
        }
    }

    fn new_raw(raw: &tl::enums::Message, pts: i32) -> Option<Self> {
        let (chat, id) = if let tl::enums::Message::Empty(m) = raw {
            (m.peer_id.as_ref().and_then(peers::chat_id), m.id)
        } else {
            let (chat, id) = raw_key(raw)?;
            (Some(chat), id)
        };
        Some(Self::new_message(chat, id, pts))
    }

    fn edit(raw: &tl::enums::Message, pts: i32, count: i32) -> Option<Self> {
        let (chat, id) = raw_key(raw)?;
        Some(Self::ordered(Some(chat), Effect::Edit(id), pts, count))
    }

    fn history(update: &tl::enums::Update) -> Option<Self> {
        use tl::enums::Update as U;
        Some(match update {
            U::NewMessage(u) => {
                Self::new_raw(&u.message, u.pts).filter(|position| position.key.chat.is_some())?
            }
            U::NewChannelMessage(u) => {
                Self::new_raw(&u.message, u.pts).filter(|position| position.key.chat.is_some())?
            }
            U::EditMessage(u) => Self::edit(&u.message, u.pts, u.pts_count)?,
            U::EditChannelMessage(u) => Self::edit(&u.message, u.pts, u.pts_count)?,
            U::DeleteMessages(u) => Self::ordered(None, deletion(&u.messages), u.pts, u.pts_count),
            U::DeleteChannelMessages(u) => Self::ordered(
                Some(peers::channel_chat(u.channel_id)?),
                deletion(&u.messages),
                u.pts,
                u.pts_count,
            ),
            U::ReadHistoryInbox(u) => Self::ordered(
                Some(peers::chat_id(&u.peer)?),
                Effect::ReadInbox(u.max_id, u.top_msg_id),
                u.pts,
                u.pts_count,
            ),
            U::ReadHistoryOutbox(u) => Self::ordered(
                Some(peers::chat_id(&u.peer)?),
                Effect::ReadOutbox(u.max_id),
                u.pts,
                u.pts_count,
            ),
            U::ReadChannelInbox(u) => Self::ordered(
                Some(peers::channel_chat(u.channel_id)?),
                Effect::ReadInbox(u.max_id, None),
                u.pts,
                0,
            ),
            _ => return None,
        })
    }

    fn from_update(update: &tl::enums::Update) -> Option<Self> {
        Self::common(update).or_else(|| Self::history(update))
    }

    /// The Common constructors in the pinned grammers `PtsInfo::from_update`.
    fn common(update: &tl::enums::Update) -> Option<Self> {
        use tl::enums::Update as U;
        match update {
            U::NewMessage(u) => Self::new_raw(&u.message, u.pts),
            U::EditMessage(_)
            | U::DeleteMessages(_)
            | U::ReadHistoryInbox(_)
            | U::ReadHistoryOutbox(_) => Self::history(update),
            _ => Self::other_common(update),
        }
    }

    fn other_common(update: &tl::enums::Update) -> Option<Self> {
        use tl::enums::Update as U;
        let (chat, effect, pts, count) = match update {
            U::WebPage(u) => (None, Effect::WebPage, u.pts, u.pts_count),
            U::ReadMessagesContents(u) => (None, Effect::ReadContents, u.pts, u.pts_count),
            U::FolderPeers(u) => (None, Effect::Folders, u.pts, u.pts_count),
            U::PinnedMessages(u) => (peers::chat_id(&u.peer), Effect::Pins, u.pts, u.pts_count),
            _ => return None,
        };
        Some(Self::ordered(chat, effect, pts, count))
    }
}

fn deletion(ids: &[i32]) -> Effect {
    let mut hash = DefaultHasher::new();
    ids.get(..MAX_DELETED_IDS).unwrap_or(ids).hash(&mut hash);
    Effect::Delete(hash.finish())
}

/// grammers adapts these to Common updates before `PtsInfo::from_update`.
fn short_common(updates: &UpdatesLike) -> Option<Position> {
    use tl::enums::Updates;
    let raw = match updates {
        UpdatesLike::Updates(raw) => raw,
        UpdatesLike::InvitedUsers(reply) => &reply.updates,
        UpdatesLike::ChatInviteJoinResult(reply) => &reply.updates,
        UpdatesLike::ShortSentMessage { request, update } => {
            return Some(Position::new_message(
                input_chat(&request.peer),
                update.id,
                update.pts,
            ));
        }
        UpdatesLike::AffectedMessages(u) => {
            return Some(Position::ordered(None, deletion(&[]), u.pts, u.pts_count));
        }
        _ => return None,
    };
    let (chat, id, pts) = match raw {
        Updates::UpdateShortMessage(u) => (peers::user_chat(u.user_id), u.id, u.pts),
        Updates::UpdateShortChatMessage(u) => (peers::group_chat(u.chat_id), u.id, u.pts),
        Updates::UpdateShortSentMessage(u) => (None, u.id, u.pts),
        _ => return None,
    };
    Some(Position::new_message(chat, id, pts))
}

fn input_chat(peer: &tl::enums::InputPeer) -> Option<ChatId> {
    use tl::enums::InputPeer;
    match peer {
        InputPeer::User(u) => peers::user_chat(u.user_id),
        InputPeer::UserFromMessage(u) => peers::user_chat(u.user_id),
        InputPeer::Chat(c) => peers::group_chat(c.chat_id),
        InputPeer::Channel(c) => peers::channel_chat(c.channel_id),
        InputPeer::ChannelFromMessage(c) => peers::channel_chat(c.channel_id),
        InputPeer::Empty | InputPeer::PeerSelf => None,
    }
}
