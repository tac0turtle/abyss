//! TL messages to the domain [`Message`]: text and entities through
//! [`RichText::from_entities`], media as inert placeholders, service messages
//! as sanitized one-line descriptions.

use grammers_client::tl;

use super::peers::{Directory, chat_id};
use crate::model::{
    Body, ChatId, InvalidTopicId, MediaKind, Message, MsgId, Sender, ThreadKey, TopicId,
};
use crate::rich_text::{
    EntityKind, MAX_ENTITIES, MAX_TEXT_BYTES, MAX_URL_BYTES, RawEntity, RichText,
};
use crate::sanitize::{MAX_SERVICE_BYTES, clean, display_name};

const UNKNOWN_SENDER: &str = "Unknown";
const SOMEONE: &str = "Someone";
const FILE: &str = "file";
/// Names listed in a service message before "and N others".
const MAX_LISTED_NAMES: usize = 3;

/// Maps a TL message. `None` for empty messages, out-of-range peers,
/// broadcast-channel posts (excluded in Stage 1) and forum headers naming a
/// non-positive topic (filing those under another topic would misfile them).
pub(crate) fn message(raw: &tl::enums::Message, dir: &Directory) -> Option<Message> {
    match raw {
        tl::enums::Message::Empty(_) => None,
        tl::enums::Message::Message(m) => regular(m, dir),
        tl::enums::Message::Service(m) => service(m, dir),
    }
}

/// `(chat, id)` of a raw message, for lookups.
pub(crate) fn raw_key(raw: &tl::enums::Message) -> Option<(ChatId, i32)> {
    match raw {
        tl::enums::Message::Empty(m) => Some((chat_id(m.peer_id.as_ref()?)?, m.id)),
        tl::enums::Message::Message(m) => Some((chat_id(&m.peer_id)?, m.id)),
        tl::enums::Message::Service(m) => Some((chat_id(&m.peer_id)?, m.id)),
    }
}

pub(crate) fn raw_date(raw: &tl::enums::Message) -> i32 {
    match raw {
        tl::enums::Message::Empty(_) => 0,
        tl::enums::Message::Message(m) => m.date,
        tl::enums::Message::Service(m) => m.date,
    }
}

fn in_broadcast(chat: ChatId, dir: &Directory) -> bool {
    dir.get(chat).is_some_and(|e| e.broadcast)
}

fn regular(m: &tl::types::Message, dir: &Directory) -> Option<Message> {
    let chat = chat_id(&m.peer_id)?;
    if m.post || in_broadcast(chat, dir) {
        return None;
    }
    let text = rich(&m.message, m.entities.as_deref());
    let body = match m.media.as_ref().and_then(media_kind) {
        Some(kind) => Body::Media {
            kind,
            caption: text,
        },
        None => Body::Text(text),
    };
    let (reply_to, topic) = kept(reply_and_topic(m.reply_to.as_ref()))?;
    Some(Message {
        id: MsgId(m.id),
        thread: ThreadKey { chat, topic },
        sender: sender(m.from_id.as_ref(), (!m.out).then_some(&m.peer_id), dir),
        date: i64::from(m.date),
        edit_date: m.edit_date.map(i64::from),
        outgoing: m.out,
        reply_to,
        body,
    })
}

fn service(m: &tl::types::MessageService, dir: &Directory) -> Option<Message> {
    let chat = chat_id(&m.peer_id)?;
    if m.post || in_broadcast(chat, dir) {
        return None;
    }
    let sender = sender(m.from_id.as_ref(), (!m.out).then_some(&m.peer_id), dir);
    let actor = m.from_id.as_ref().and_then(chat_id);
    let who = name_of(dir, actor);
    let text = describe(&m.action, &who, actor, dir);
    let (reply_to, mut topic) = kept(reply_and_topic(m.reply_to.as_ref()))?;
    if matches!(m.action, tl::enums::MessageAction::TopicCreate(_)) {
        topic = Some(kept(TopicId::try_from(m.id))?);
    }
    Some(Message {
        id: MsgId(m.id),
        thread: ThreadKey { chat, topic },
        sender,
        date: i64::from(m.date),
        edit_date: None,
        outgoing: m.out,
        reply_to,
        body: Body::Service(clean(&text, MAX_SERVICE_BYTES)),
    })
}

/// The sender: `from_id` when present, otherwise `fallback`, the chat itself
/// for incoming messages (private chats and anonymous posts carry no
/// `from_id`).
fn sender(
    from: Option<&tl::enums::Peer>,
    fallback: Option<&tl::enums::Peer>,
    dir: &Directory,
) -> Option<Sender> {
    let id = chat_id(from.or(fallback)?)?;
    let name = dir
        .get(id)
        .map_or_else(|| UNKNOWN_SENDER.to_owned(), |e| e.name.clone());
    Some(Sender { id, name })
}

/// `None` for a malformed topic id, which drops the message; counted at
/// debug. The id is not logged: it is server data with no use in a report.
fn kept<T>(checked: Result<T, InvalidTopicId>) -> Option<T> {
    if checked.is_err() {
        tracing::debug!("message with a non-positive topic id dropped");
    }
    checked.ok()
}

/// `(reply_to, topic)`. In a forum every message of a topic replies to the
/// topic root: that is its topic, not a reply. Replies to other chats are
/// dropped (Stage 1 only quotes messages of the same chat). A forum header
/// naming a non-positive topic is an error: the caller drops the message.
pub(crate) fn reply_and_topic(
    header: Option<&tl::enums::MessageReplyHeader>,
) -> Result<(Option<MsgId>, Option<TopicId>), InvalidTopicId> {
    let Some(tl::enums::MessageReplyHeader::Header(h)) = header else {
        return Ok((None, None));
    };
    let reply = h
        .reply_to_msg_id
        .filter(|_| h.reply_to_peer_id.is_none())
        .map(MsgId);
    if !h.forum_topic {
        return Ok((reply, None));
    }
    match h.reply_to_top_id {
        Some(top) => Ok((reply, Some(TopicId::try_from(top)?))),
        None => Ok((None, h.reply_to_msg_id.map(TopicId::try_from).transpose()?)),
    }
}

/// Text plus entities, within the formatting budget (plan section 6).
/// Over-budget input is rendered without its formatting, but never without its
/// spoilers: only those entities are converted (at most one more than the
/// budget, which hides the whole text), so nothing unbounded is allocated and
/// nothing hidden is revealed.
pub(crate) fn rich(text: &str, entities: Option<&[tl::enums::MessageEntity]>) -> RichText {
    let entities = entities.unwrap_or_default();
    if entities.is_empty() {
        return RichText::plain(text);
    }
    if text.len() > MAX_TEXT_BYTES || entities.len() > MAX_ENTITIES {
        let spoilers: Vec<RawEntity> = entities
            .iter()
            .filter(|e| matches!(e, tl::enums::MessageEntity::Spoiler(_)))
            .take(MAX_ENTITIES + 1)
            .map(raw_entity)
            .collect();
        // The resolver skips empty and out-of-range spoilers before it counts
        // against its limit, so a kept prefix holding some could pass while a
        // valid spoiler beyond it was discarded here and its text shown. The
        // raw count is the only count that sees every entity: past the limit
        // the spoilers cannot be established (review A33).
        if spoilers.len() > MAX_ENTITIES {
            return RichText::all_hidden(text);
        }
        return RichText::from_entities(text, &spoilers);
    }
    let raw: Vec<RawEntity> = entities.iter().map(raw_entity).collect();
    RichText::from_entities(text, &raw)
}

/// A hidden link target, cut at the first char boundary past the URL budget:
/// a longer target fails the budget either way, so nothing is copied beyond
/// that, and a target is never shortened into one that would pass.
fn bounded_url(url: &str) -> String {
    let end = url
        .char_indices()
        .map(|(i, _)| i)
        .find(|&i| i > MAX_URL_BYTES)
        .unwrap_or(url.len());
    url.get(..end).unwrap_or(url).to_owned()
}

fn raw_entity(e: &tl::enums::MessageEntity) -> RawEntity {
    use tl::enums::MessageEntity as E;
    let (offset, length, kind) = match e {
        E::Bold(x) => (x.offset, x.length, EntityKind::Bold),
        E::Italic(x) => (x.offset, x.length, EntityKind::Italic),
        E::Underline(x) => (x.offset, x.length, EntityKind::Underline),
        E::Strike(x) => (x.offset, x.length, EntityKind::Strike),
        E::Code(x) => (x.offset, x.length, EntityKind::Code),
        E::Pre(x) => (x.offset, x.length, EntityKind::Pre),
        E::Spoiler(x) => (x.offset, x.length, EntityKind::Spoiler),
        E::Mention(x) => (x.offset, x.length, EntityKind::Mention),
        E::MentionName(x) => (x.offset, x.length, EntityKind::Mention),
        E::InputMessageEntityMentionName(x) => (x.offset, x.length, EntityKind::Mention),
        E::Url(x) => (x.offset, x.length, EntityKind::Url),
        E::TextUrl(x) => (x.offset, x.length, EntityKind::TextUrl(bounded_url(&x.url))),
        E::Unknown(x) => (x.offset, x.length, EntityKind::Other),
        E::Hashtag(x) => (x.offset, x.length, EntityKind::Other),
        E::BotCommand(x) => (x.offset, x.length, EntityKind::Other),
        E::Email(x) => (x.offset, x.length, EntityKind::Other),
        E::Phone(x) => (x.offset, x.length, EntityKind::Other),
        E::Cashtag(x) => (x.offset, x.length, EntityKind::Other),
        E::BankCard(x) => (x.offset, x.length, EntityKind::Other),
        E::CustomEmoji(x) => (x.offset, x.length, EntityKind::Other),
        E::Blockquote(x) => (x.offset, x.length, EntityKind::Other),
        E::FormattedDate(x) => (x.offset, x.length, EntityKind::Other),
        E::DiffInsert(x) => (x.offset, x.length, EntityKind::Other),
        E::DiffReplace(x) => (x.offset, x.length, EntityKind::Other),
        E::DiffDelete(x) => (x.offset, x.length, EntityKind::Other),
        E::TonAddress(x) => (x.offset, x.length, EntityKind::Other),
    };
    RawEntity {
        offset,
        length,
        kind,
    }
}

/// The inert placeholder for a message's media. `None` when the media is not
/// shown as an attachment (empty, link previews).
pub(crate) fn media_kind(media: &tl::enums::MessageMedia) -> Option<MediaKind> {
    use tl::enums::MessageMedia as M;
    Some(match media {
        M::Empty | M::WebPage(_) => return None,
        M::Photo(_) => MediaKind::Photo,
        M::Document(d) => document_kind(d),
        M::Geo(_) | M::GeoLive(_) | M::Venue(_) => MediaKind::Location,
        M::Contact(_) => MediaKind::Contact,
        M::Poll(_) => MediaKind::Poll,
        M::Unsupported
        | M::Game(_)
        | M::Invoice(_)
        | M::Dice(_)
        | M::Story(_)
        | M::Giveaway(_)
        | M::GiveawayResults(_)
        | M::PaidMedia(_)
        | M::ToDo(_)
        | M::VideoStream(_)
        | M::Widget(_) => MediaKind::Other,
    })
}

fn document_kind(m: &tl::types::MessageMediaDocument) -> MediaKind {
    if m.voice {
        return MediaKind::Voice;
    }
    if m.round || m.video {
        return MediaKind::Video;
    }
    let Some(tl::enums::Document::Document(doc)) = &m.document else {
        return MediaKind::Other;
    };
    let mut kind = None;
    let mut name = None;
    for attr in &doc.attributes {
        use tl::enums::DocumentAttribute as A;
        match attr {
            A::Sticker(_) | A::CustomEmoji(_) => kind = Some(MediaKind::Sticker),
            A::Animated => kind = kind.or(Some(MediaKind::Gif)),
            A::Video(_) => kind = kind.or(Some(MediaKind::Video)),
            A::Audio(a) if a.voice => kind = kind.or(Some(MediaKind::Voice)),
            A::Audio(_) => kind = kind.or(Some(MediaKind::Audio)),
            A::Filename(f) => name = Some(f.file_name.as_str()),
            A::ImageSize(_) | A::HasStickers => {}
        }
    }
    kind.unwrap_or_else(|| {
        let name = display_name(name.unwrap_or_default());
        MediaKind::Document {
            name: if name.is_empty() {
                FILE.to_owned()
            } else {
                name
            },
            size: u64::try_from(doc.size).unwrap_or(0),
        }
    })
}

fn name_of(dir: &Directory, chat: Option<ChatId>) -> String {
    chat.and_then(|c| dir.get(c))
        .map_or_else(|| SOMEONE.to_owned(), |e| e.name.clone())
}

fn names(dir: &Directory, users: &[i64]) -> String {
    let mut listed: Vec<String> = users
        .iter()
        .take(MAX_LISTED_NAMES)
        .map(|&id| name_of(dir, super::peers::user_chat(id)))
        .collect();
    let rest = users.len().saturating_sub(MAX_LISTED_NAMES);
    if rest > 0 {
        listed.push(format!("{rest} others"));
    }
    listed.join(", ")
}

/// A one-line description of a service action. Names and titles are
/// sanitized; the caller bounds the result.
fn describe(
    action: &tl::enums::MessageAction,
    who: &str,
    actor: Option<ChatId>,
    dir: &Directory,
) -> String {
    use tl::enums::MessageAction as A;
    let is_actor = |id: i64| actor.is_some_and(|a| a.id == id);
    match action {
        A::ChatCreate(a) => format!("{who} created the group «{}»", display_name(&a.title)),
        A::ChannelCreate(a) => format!("{who} created the group «{}»", display_name(&a.title)),
        A::ChatEditTitle(a) => format!("{who} renamed the group to «{}»", display_name(&a.title)),
        A::ChatEditPhoto(_) => format!("{who} changed the group photo"),
        A::ChatDeletePhoto => format!("{who} removed the group photo"),
        A::ChatAddUser(a) if a.users.len() == 1 && a.users.iter().all(|&u| is_actor(u)) => {
            format!("{who} joined the group")
        }
        A::ChatAddUser(a) => format!("{who} added {}", names(dir, &a.users)),
        A::ChatDeleteUser(a) if is_actor(a.user_id) => format!("{who} left the group"),
        A::ChatDeleteUser(a) => format!("{who} removed {}", names(dir, &[a.user_id])),
        A::ChatJoinedByLink(_) => format!("{who} joined via an invite link"),
        A::ChatJoinedByRequest => format!("{who} was accepted into the group"),
        A::ChatMigrateTo(_) | A::ChannelMigrateFrom(_) => {
            "The group was upgraded to a supergroup".to_owned()
        }
        A::PinMessage => format!("{who} pinned a message"),
        A::HistoryClear => "History was cleared".to_owned(),
        A::PhoneCall(_) => "Call".to_owned(),
        A::GroupCall(_) | A::GroupCallScheduled(_) | A::ConferenceCall(_) => {
            "Video chat".to_owned()
        }
        A::ScreenshotTaken => format!("{who} took a screenshot"),
        A::ContactSignUp => format!("{who} joined Telegram"),
        A::SetMessagesTtl(_) => format!("{who} changed the auto-delete timer"),
        A::TopicCreate(a) => format!("{who} created the topic «{}»", display_name(&a.title)),
        A::TopicEdit(_) => format!("{who} edited a topic"),
        A::CustomAction(a) => clean(&a.message, MAX_SERVICE_BYTES),
        _ => "Service message".to_owned(),
    }
}
