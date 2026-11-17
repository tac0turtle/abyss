//! UI-facing domain model. Contains no access hashes or other credentials; those
//! stay inside the backend.

use crate::rich_text::RichText;
use crate::sanitize::{MAX_NAME_BYTES, MAX_PREVIEW_BYTES, MAX_SERVICE_BYTES, clean, is_clean};

#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
pub enum PeerKind {
    User,
    /// Basic (small) group.
    Chat,
    /// Supergroup. Broadcast channels are filtered out before reaching the model.
    Channel,
}

impl PeerKind {
    /// Whether message ids are unique per account (private chats, basic groups)
    /// rather than per chat (supergroups), so a deletion may arrive without a chat.
    pub fn has_account_unique_ids(self) -> bool {
        !matches!(self, PeerKind::Channel)
    }
}

#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
pub struct ChatId {
    pub kind: PeerKind,
    pub id: i64,
}

/// Server message id, unique per chat (supergroups) or per account (others).
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
pub struct MsgId(pub i32);

/// Client-side id of a message the user is sending.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
pub struct LocalId(pub u64);

/// Groups `(chat, id)` pairs by chat, keeping first-seen chat order and id order.
pub(crate) fn group_by_chat(
    pairs: impl IntoIterator<Item = (ChatId, MsgId)>,
) -> Vec<(ChatId, Vec<MsgId>)> {
    let mut groups: Vec<(ChatId, Vec<MsgId>)> = Vec::new();
    for (chat, id) in pairs {
        match groups.iter_mut().find(|(c, _)| *c == chat) {
            Some((_, ids)) => ids.push(id),
            None => groups.push((chat, vec![id])),
        }
    }
    groups
}

/// A forum topic's id: the id of the service message that created it. Always
/// at least 1: the only constructors are [`TryFrom<i32>`] and
/// [`TopicId::GENERAL`], and deserialization goes through `TryFrom`, so a
/// stored or received non-positive id fails to decode.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
#[serde(try_from = "i32", into = "i32")]
pub struct TopicId(i32);

impl TopicId {
    /// The General topic every forum has.
    pub const GENERAL: TopicId = TopicId(1);

    pub const fn get(self) -> i32 {
        self.0
    }
}

/// A topic id that is zero or negative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidTopicId(pub i32);

impl std::fmt::Display for InvalidTopicId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid topic id {} (must be at least 1)", self.0)
    }
}

impl std::error::Error for InvalidTopicId {}

impl TryFrom<i32> for TopicId {
    type Error = InvalidTopicId;

    fn try_from(id: i32) -> Result<Self, InvalidTopicId> {
        if id > 0 {
            Ok(Self(id))
        } else {
            Err(InvalidTopicId(id))
        }
    }
}

impl From<TopicId> for i32 {
    fn from(id: TopicId) -> i32 {
        id.0
    }
}

/// A conversation: a whole chat (`topic: None`) or one topic of a forum.
/// `topic` is the forum topic a message belongs to when the server says so;
/// forum chats are still shown as one stream, keyed with `topic: None`
/// (forum topics are Stage 2).
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ThreadKey {
    pub chat: ChatId,
    pub topic: Option<TopicId>,
}

impl ThreadKey {
    /// The whole of `chat`, not one of its topics.
    pub const fn whole(chat: ChatId) -> Self {
        Self { chat, topic: None }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatKind {
    Private,
    Bot,
    Group,
    Supergroup { forum: bool, gigagroup: bool },
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum Preview {
    Text(String),
    Media(MediaKind),
    Service,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ChatSummary {
    pub id: ChatId,
    /// Sanitized via [`crate::sanitize::display_name`].
    pub title: String,
    pub kind: ChatKind,
    pub pinned: bool,
    pub unread: u32,
    pub unread_mentions: u32,
    pub last: Option<Preview>,
    /// Unix seconds.
    pub last_date: i64,
    /// Id of the chat's newest message, when known (the dialog's top message,
    /// then every newer message that bumped it). Orders bumps and replays
    /// without relying on timestamps, which collide within a second.
    pub last_id: Option<MsgId>,
    pub can_send: bool,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Sender {
    pub id: ChatId,
    /// Sanitized via [`crate::sanitize::display_name`].
    pub name: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Photo,
    Video,
    Voice,
    Audio,
    /// `name` is sanitized; `size` in bytes.
    Document {
        name: String,
        size: u64,
    },
    Sticker,
    Gif,
    Poll,
    Location,
    Contact,
    Other,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum Body {
    Text(RichText),
    /// Inert placeholder: media is never downloaded or decoded in Stage 1.
    Media {
        kind: MediaKind,
        caption: RichText,
    },
    Service(String),
    Unsupported,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub id: MsgId,
    pub thread: ThreadKey,
    pub sender: Option<Sender>,
    /// Unix seconds.
    pub date: i64,
    /// Unix seconds of the last edit; `None` if never edited. Orders a live edit
    /// against a snapshot of the same message (plan section 2, bootstrap).
    pub edit_date: Option<i64>,
    pub outgoing: bool,
    pub reply_to: Option<MsgId>,
    pub body: Body,
}

impl Message {
    pub fn edited(&self) -> bool {
        self.edit_date.is_some()
    }

    /// Ordering key between two copies of the same message: the later edit wins,
    /// and an unedited copy never beats an edited one.
    pub fn version(&self) -> i64 {
        self.edit_date.unwrap_or(i64::MIN)
    }
}

impl ChatSummary {
    /// Heap bytes retained beyond the inline struct (title and preview), for byte budgets.
    pub fn retained_bytes(&self) -> usize {
        let preview = match &self.last {
            Some(Preview::Text(s) | Preview::Media(MediaKind::Document { name: s, .. })) => {
                s.capacity()
            }
            _ => 0,
        };
        self.title.capacity() + preview
    }
}

/// Inline size of a message slot, charged once per message.
const MESSAGE_OVERHEAD: usize = std::mem::size_of::<Message>();

impl Message {
    /// Bytes retained by this message (inline slot plus heap capacities), for byte budgets.
    pub fn retained_bytes(&self) -> usize {
        let sender = self.sender.as_ref().map_or(0, |s| s.name.capacity());
        let body = match &self.body {
            Body::Text(rt) => rt.retained_bytes(),
            Body::Media { kind, caption } => {
                let name = match kind {
                    MediaKind::Document { name, .. } => name.capacity(),
                    _ => 0,
                };
                name + caption.retained_bytes()
            }
            Body::Service(s) => s.capacity(),
            Body::Unsupported => 0,
        };
        MESSAGE_OVERHEAD + sender + body
    }
}

// Validation of whole domain values. Everything the backend produces must satisfy
// these, and the disk cache rejects stored values that do not. `RichText` checks
// its own invariants on construction and deserialization.

impl MediaKind {
    pub fn is_valid(&self) -> bool {
        match self {
            MediaKind::Document { name, .. } => is_clean(name, MAX_NAME_BYTES),
            _ => true,
        }
    }
}

impl Preview {
    /// The chat-list preview of a message: a one-line, bounded, clean text, or
    /// the media kind.
    pub fn of(m: &Message) -> Self {
        match &m.body {
            Body::Text(rt) => Preview::Text(clean(rt.text(), MAX_PREVIEW_BYTES)),
            Body::Media { kind, .. } => Preview::Media(kind.clone()),
            Body::Service(_) | Body::Unsupported => Preview::Service,
        }
    }

    pub fn is_valid(&self) -> bool {
        match self {
            Preview::Text(s) => is_clean(s, MAX_PREVIEW_BYTES),
            Preview::Media(kind) => kind.is_valid(),
            Preview::Service => true,
        }
    }
}

impl ChatSummary {
    pub fn is_valid(&self) -> bool {
        is_clean(&self.title, MAX_NAME_BYTES) && self.last.as_ref().is_none_or(Preview::is_valid)
    }
}

impl Message {
    pub fn is_valid(&self) -> bool {
        let sender_ok = self
            .sender
            .as_ref()
            .is_none_or(|s| is_clean(&s.name, MAX_NAME_BYTES));
        let body_ok = match &self.body {
            Body::Text(_) | Body::Unsupported => true,
            Body::Media { kind, .. } => kind.is_valid(),
            Body::Service(s) => is_clean(s, MAX_SERVICE_BYTES),
        };
        sender_ok && body_ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(topic: Option<TopicId>) -> Message {
        Message {
            id: MsgId(5),
            thread: ThreadKey {
                chat: ChatId {
                    kind: PeerKind::Channel,
                    id: 10,
                },
                topic,
            },
            sender: None,
            date: 0,
            edit_date: None,
            outgoing: false,
            reply_to: None,
            body: Body::Unsupported,
        }
    }

    #[test]
    fn topic_ids_below_one_are_rejected() {
        for bad in [0, -1, i32::MIN] {
            assert_eq!(TopicId::try_from(bad), Err(InvalidTopicId(bad)));
        }
        assert_eq!(TopicId::try_from(1), Ok(TopicId::GENERAL));
        assert_eq!(TopicId::GENERAL.get(), 1);
        assert_eq!(TopicId::try_from(i32::MAX).map(TopicId::get), Ok(i32::MAX));
    }

    /// Decoding goes through `TryFrom`: a serialized 0 or -1 is no topic id,
    /// alone or inside a stored message.
    #[test]
    fn serialized_topic_ids_below_one_fail_to_decode() {
        for bad in [0i32, -1] {
            let bytes = postcard::to_allocvec(&bad).unwrap();
            assert!(postcard::from_bytes::<TopicId>(&bytes).is_err());
        }
        let bytes = postcard::to_allocvec(&7i32).unwrap();
        assert_eq!(
            postcard::from_bytes::<TopicId>(&bytes).ok(),
            TopicId::try_from(7).ok()
        );

        let stored = message(Some(TopicId::GENERAL));
        let bytes = postcard::to_allocvec(&stored).unwrap();
        assert_eq!(postcard::from_bytes::<Message>(&bytes).ok(), Some(stored));
        // The same message with its topic overwritten by 0 or -1 on the wire.
        let encoded_one = postcard::to_allocvec(&1i32).unwrap();
        for bad in [0i32, -1] {
            let encoded_bad = postcard::to_allocvec(&bad).unwrap();
            let tampered = splice(&bytes, &encoded_one, &encoded_bad);
            assert!(postcard::from_bytes::<Message>(&tampered).is_err());
        }
    }

    /// `bytes` with the single occurrence of `old` after the `Some` tag of
    /// the topic replaced by `new`.
    fn splice(bytes: &[u8], old: &[u8], new: &[u8]) -> Vec<u8> {
        let none = postcard::to_allocvec(&message(None)).unwrap();
        // The encodings differ from the topic on: `None` is tag 0, `Some` is
        // tag 1 followed by the id.
        let at = bytes
            .iter()
            .zip(&none)
            .position(|(a, b)| a != b)
            .expect("the topic differs");
        let start = at + 1;
        assert_eq!(bytes.get(start..start + old.len()), Some(old));
        [&bytes[..start], new, &bytes[start + old.len()..]].concat()
    }
}
