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
    serde::Serialize,
    serde::Deserialize,
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
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
    /// `contact`: the user is in the account's contacts (chat folders' rules
    /// tell contacts from other people).
    Private {
        contact: bool,
    },
    Bot,
    Group,
    Supergroup {
        forum: bool,
        gigagroup: bool,
    },
}

/// Telegram's peer folders (not chat folders): every chat is in exactly one.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash,
)]
pub enum PeerFolder {
    /// Folder 0, the main chat list.
    #[default]
    Main,
    /// Folder 1, the archive.
    Archive,
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
    /// The confirmed unread count and read pointer: server evidence only
    /// (stage 2 plan, section 17.6.1). Shown through
    /// [`ChatSummary::displayed_unread`].
    pub read: ReadState,
    pub unread_mentions: u32,
    pub last: Option<Preview>,
    /// Unix seconds.
    pub last_date: i64,
    /// Id of the chat's newest message, when known (the dialog's top message,
    /// then every newer message that bumped it). Orders bumps and replays
    /// without relying on timestamps, which collide within a second.
    pub last_id: Option<MsgId>,
    pub can_send: bool,
    /// Notifications are muted until this time (Unix seconds; `0`: not
    /// muted, `i32::MAX`: for good). Checked against the clock when used
    /// ([`ChatSummary::muted_at`]), so an expired mute stops counting.
    /// Chat folders can leave muted chats out.
    pub muted_until: i64,
    /// The main list or the archive. The list holds both; the UI shows one
    /// at a time.
    pub folder: PeerFolder,
    /// The id an outstanding read of the whole chat goes up to, if one
    /// does: a display overlay that core projects from its read intents
    /// whenever it installs or emits the row. Never stored, cached or part
    /// of a snapshot.
    #[serde(skip)]
    pub reading: Option<MsgId>,
}

/// A chat's confirmed read state, one value under the unread stamp: the
/// server's unread count (plus Stage 1's counted messages) and the server's
/// read pointer, which only ever rises.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash,
)]
pub struct ReadState {
    pub unread: u32,
    /// The highest incoming message id read (`read_inbox_max_id`).
    pub read_inbox_max: MsgId,
}

/// How the server ordered a forum's topic list
/// (`messages.forumTopics.order_by_create_date`).
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TopicOrder {
    /// By last activity: a topic's newest message ranks it.
    LastActivity,
    /// By creation date, newest first. Live messages never move a topic.
    Created,
}

/// How a topic's unread count relates to what happened since the server sent
/// it. A display hint only: the count itself is always the server's (plan
/// section 6.4).
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    /// The count came from the newest snapshot, and nothing happened since.
    Current,
    /// An incoming message arrived after the count was taken.
    NewSince,
    /// The user read the topic after the count was taken.
    ReadSince,
    /// Something else changed the count (a remote read, a deletion).
    Stale,
}

/// Whether a topic is listed (`forumTopic.hidden`, General only). Hidden
/// topics are kept for routing but never emitted.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Shown,
    Hidden,
}

/// Why a topic left the shown list (`Event::TopicRemoved`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopicGone {
    /// A fresh by-id result confirmed the deletion: nothing can be posted
    /// to it any more.
    Deleted,
    /// Its visibility became hidden (General only); it can still be
    /// posted to.
    Hidden,
    /// Cut by the list's cap: it still exists, only no longer listed.
    Evicted,
}

/// One forum topic, as the UI shows it.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Topic {
    pub id: TopicId,
    /// Sanitized via [`crate::sanitize::display_name`].
    pub title: String,
    /// The topic's creation date (`forumTopic.date`), Unix seconds.
    pub created: i64,
    pub closed: bool,
    pub pinned: bool,
    /// The server's count only; never changed locally (plan section 6.4).
    pub unread: u32,
    pub unread_mentions: u32,
    pub freshness: Freshness,
    pub read_inbox_max: MsgId,
    pub last: Option<Preview>,
    /// Unix seconds.
    pub last_date: i64,
    /// The topic's top (newest) message.
    pub last_id: Option<MsgId>,
}

impl Topic {
    pub fn is_valid(&self) -> bool {
        is_clean(&self.title, MAX_NAME_BYTES) && self.last.as_ref().is_none_or(Preview::is_valid)
    }

    /// Heap bytes retained beyond the inline struct (title and preview), for byte budgets.
    pub fn retained_bytes(&self) -> usize {
        self.title.capacity() + self.last.as_ref().map_or(0, Preview::retained_bytes)
    }
}

/// A topic as the backend reports it: the public [`Topic`] plus what core
/// applies but never emits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicSnapshot {
    pub topic: Topic,
    pub visibility: Visibility,
    /// [`Message::version`] of the top message (`i64::MIN` when it was never
    /// edited, or there is none). Orders a live edit of the top message
    /// against this snapshot's preview.
    pub top_version: i64,
}

/// The rules of a chat folder (`dialogFilter` flags): which chats it takes
/// in beyond those it names, and which of those it leaves out.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FolderRules(u8);

impl FolderRules {
    pub const CONTACTS: Self = Self(1);
    pub const NON_CONTACTS: Self = Self(1 << 1);
    pub const GROUPS: Self = Self(1 << 2);
    pub const BROADCASTS: Self = Self(1 << 3);
    pub const BOTS: Self = Self(1 << 4);
    pub const EXCLUDE_MUTED: Self = Self(1 << 5);
    pub const EXCLUDE_READ: Self = Self(1 << 6);
    pub const EXCLUDE_ARCHIVED: Self = Self(1 << 7);

    #[must_use]
    pub fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn has(self, rule: Self) -> bool {
        self.0 & rule.0 == rule.0
    }
}

impl ChatSummary {
    /// The unread count to show: 0 while an outstanding read covers every
    /// known message (Stage 1's overlay), the confirmed count otherwise. A
    /// forum's count is its badge, which no read overlays.
    pub fn displayed_unread(&self) -> u32 {
        let forum = matches!(self.kind, ChatKind::Supergroup { forum: true, .. });
        let covered = self
            .reading
            .is_some_and(|r| self.last_id.is_none_or(|last| r >= last));
        if covered && !forum {
            0
        } else {
            self.read.unread
        }
    }

    /// Whether notifications are muted at `now` (Unix seconds).
    pub fn muted_at(&self, now: i64) -> bool {
        self.muted_until > now
    }
}

/// Whether [`ChatFolder::contains`] applies a folder's "exclude read" rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadRule {
    /// As Telegram applies it: folder editing, and every chat the unread
    /// filter doesn't keep.
    Applies,
    /// Waived: a chat the unread filter keeps stays when it is read.
    Waived,
}

/// Which chat folder an edit was made for: its id, title, rules and shared
/// flag as the UI last saw them, compared field by field (no hash, so no
/// collisions). Best-effort detection of an observable identity change: a
/// folder replaced (on another device) by one with the same id and another
/// title, rules or shared flag has another tag, so a queued edit is
/// refused instead of changing the wrong folder. A replacement identical in
/// those fields (a "Work" deleted and recreated as it was) has the same tag
/// and is not detected. Changes to its chats keep the tag. Bounded by the
/// title's bound; opaque, and never logged.
#[derive(Clone, PartialEq, Eq)]
pub struct FolderTag {
    id: i32,
    title: String,
    rules: FolderRules,
    shared: bool,
}

impl std::fmt::Debug for FolderTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FolderTag(..)")
    }
}

/// A chat folder (`dialogFilter`): a user-defined, synced view of the chat
/// list. `pinned` and `include` are always in it, `exclude` never; other
/// chats are in it when they match `rules`. Ids are the server's.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ChatFolder {
    pub id: i32,
    /// Sanitized via [`crate::sanitize::display_name`].
    pub title: String,
    pub rules: FolderRules,
    /// Shown first, in this order.
    pub pinned: Vec<ChatId>,
    pub include: Vec<ChatId>,
    pub exclude: Vec<ChatId>,
    /// A shared folder (chat list invite): read-only here.
    pub shared: bool,
}

impl ChatFolder {
    /// Whether `chat` is in this folder at `now` (Unix seconds, for mutes),
    /// by Telegram's rule as Telegram Desktop applies it
    /// (`ChatFilter::contains`): excluded chats never are, named ones always
    /// are, and any other is when its kind matches and no exclusion applies.
    /// "Exclude muted" keeps a muted chat with an unread mention unless it
    /// is archived. `read` waives "exclude read" alone, for a chat the
    /// unread filter keeps (stage 2 plan, section 17.4): every other step
    /// applies as ever.
    pub fn contains(&self, chat: &ChatSummary, now: i64, read: ReadRule) -> bool {
        if self.exclude.contains(&chat.id) {
            return false;
        }
        if self.pinned.contains(&chat.id) || self.include.contains(&chat.id) {
            return true;
        }
        let r = self.rules;
        let kind = match chat.kind {
            ChatKind::Private { contact: true } => r.has(FolderRules::CONTACTS),
            ChatKind::Private { contact: false } => r.has(FolderRules::NON_CONTACTS),
            ChatKind::Bot => r.has(FolderRules::BOTS),
            ChatKind::Group | ChatKind::Supergroup { .. } => r.has(FolderRules::GROUPS),
        };
        let mentioned = chat.unread_mentions > 0 && chat.folder == PeerFolder::Main;
        kind && !(r.has(FolderRules::EXCLUDE_MUTED) && chat.muted_at(now) && !mentioned)
            && !(r.has(FolderRules::EXCLUDE_READ)
                && read == ReadRule::Applies
                && chat.displayed_unread() == 0
                && chat.unread_mentions == 0)
            && !(r.has(FolderRules::EXCLUDE_ARCHIVED) && chat.folder == PeerFolder::Archive)
    }

    /// Heap bytes retained, for byte budgets.
    pub fn retained_bytes(&self) -> usize {
        let ids = self.pinned.capacity() + self.include.capacity() + self.exclude.capacity();
        self.title.capacity() + ids * std::mem::size_of::<ChatId>()
    }

    pub fn is_valid(&self) -> bool {
        is_clean(&self.title, MAX_NAME_BYTES)
    }

    /// This folder's [`FolderTag`].
    pub fn tag(&self) -> FolderTag {
        FolderTag {
            id: self.id,
            title: self.title.clone(),
            rules: self.rules,
            shared: self.shared,
        }
    }
}

/// The longest chat folder title Telegram accepts, in characters.
pub const MAX_FOLDER_TITLE: usize = 12;

/// A change to the account's chat folders (folders plan, part C). Only
/// regular folders can be changed; shared ones are read-only. An edit of an
/// existing folder carries the [`FolderTag`] the UI saw, checked against
/// the folder as fetched right before the write (best-effort: see
/// [`FolderTag`] for what it cannot tell apart).
#[derive(Clone, PartialEq, Eq)]
pub enum FolderEdit {
    /// Puts `chat` in the folder by name (and out of its exclusions).
    AddChat {
        folder: i32,
        tag: FolderTag,
        chat: ChatId,
    },
    /// Takes `chat` out of the folder: off its named chats, and onto its
    /// exclusions when its rules would still take it in.
    RemoveChat {
        folder: i32,
        tag: FolderTag,
        chat: ChatId,
    },
    /// A new folder holding `chat`. `title` is sanitized and at most
    /// [`MAX_FOLDER_TITLE`] characters.
    Create {
        title: String,
        chat: ChatId,
    },
    Delete {
        folder: i32,
        tag: FolderTag,
    },
}

impl FolderEdit {
    /// The existing folder the edit is for, and its expected tag.
    pub fn target(&self) -> Option<(i32, &FolderTag)> {
        match self {
            Self::AddChat { folder, tag, .. }
            | Self::RemoveChat { folder, tag, .. }
            | Self::Delete { folder, tag } => Some((*folder, tag)),
            Self::Create { .. } => None,
        }
    }
}

/// Titles and tags stay out of logs.
impl std::fmt::Debug for FolderEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AddChat { folder, chat, .. } => f
                .debug_struct("AddChat")
                .field("folder", folder)
                .field("chat", chat)
                .finish_non_exhaustive(),
            Self::RemoveChat { folder, chat, .. } => f
                .debug_struct("RemoveChat")
                .field("folder", folder)
                .field("chat", chat)
                .finish_non_exhaustive(),
            Self::Create { chat, .. } => f
                .debug_struct("Create")
                .field("chat", chat)
                .finish_non_exhaustive(),
            Self::Delete { folder, .. } => f
                .debug_struct("Delete")
                .field("folder", folder)
                .finish_non_exhaustive(),
        }
    }
}

/// A folder title as Telegram takes it: sanitized, trimmed, non-empty and
/// at most [`MAX_FOLDER_TITLE`] characters. `None` otherwise.
pub fn folder_title(raw: &str) -> Option<String> {
    let title = crate::sanitize::display_name(raw.trim());
    let title = title.trim();
    let n = title.chars().count();
    (n > 0 && n <= MAX_FOLDER_TITLE).then(|| title.to_owned())
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
        self.title.capacity() + self.last.as_ref().map_or(0, Preview::retained_bytes)
    }
}

impl Preview {
    /// Heap bytes retained beyond the inline value, for byte budgets.
    pub fn retained_bytes(&self) -> usize {
        match self {
            Preview::Text(s) | Preview::Media(MediaKind::Document { name: s, .. }) => s.capacity(),
            _ => 0,
        }
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
