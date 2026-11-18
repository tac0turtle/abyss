//! The `--fake` demo account: enough chats and messages to exercise the chat
//! list and message view by hand (task 9 and 10 acceptance): 1,000 dialogs of
//! every kind, and one 2,000-message chat with emoji, RTL, 4,096-character
//! text, every entity kind, every media kind, service and unsupported
//! messages, edits, outgoing messages, senders and over-budget formatting.
//! Every forum (`n % 10 == 2`) serves General plus 60 topics, more than one
//! topics page, with pinned, closed and unread topics; chat 12's list is in
//! creation order, the others by activity (Stage 2 task 3; the app shows
//! topics from task 10).
//!
//! Each chat's read pointer leaves its unread count of incoming messages
//! above it, so unread chats open at their first unread message (Stage 2
//! task 18): the showcase has 120 unread among its own messages, chat 6
//! only its own messages above the pointer, and the first forum's General
//! topic over a page of unread posts.

use abyss_core::backend::fake::FakeScript;
use abyss_core::model::{
    Body, ChatFolder, ChatId, ChatKind, ChatSummary, FolderRules, MediaKind, Message, MsgId,
    PeerFolder, PeerKind, Preview, ReadState, Sender, ThreadKey, TopicOrder,
};
use abyss_core::rich_text::{EntityKind, MAX_ENTITIES, RawEntity, RichText};
use abyss_core::sanitize::display_name;

const CHATS: i64 = 1000;
const SHOWCASE_MESSAGES: i32 = 2000;
const MESSAGES: i32 = 30;
/// The demo forum whose topics are listed by creation date.
const CREATION_ORDERED_FORUM: i64 = 12;
/// Unread messages in the showcase: more than two history pages.
const SHOWCASE_UNREAD: u32 = 120;
/// The chat whose only messages above its read pointer are the user's own.
const OWN_ABOVE_POINTER: i64 = 6;
/// The demo forum whose General topic gains a page of unread posts.
const UNREAD_FORUM: i64 = 2;
/// Posts added to that General topic.
const GENERAL_POSTS: i32 = 120;

fn chat_id(n: i64) -> ChatId {
    let kind = match n % 5 {
        2 | 3 => PeerKind::Channel,
        4 => PeerKind::Chat,
        _ => PeerKind::User,
    };
    ChatId { kind, id: n }
}

fn kind_of(n: i64) -> ChatKind {
    match n % 5 {
        2 if n % 10 == 2 => ChatKind::Supergroup {
            forum: true,
            gigagroup: false,
        },
        2 => ChatKind::Supergroup {
            forum: false,
            gigagroup: n % 20 == 7,
        },
        3 => ChatKind::Supergroup {
            forum: false,
            gigagroup: n % 15 == 3,
        },
        4 => ChatKind::Group,
        _ if n % 7 == 0 => ChatKind::Bot,
        _ => ChatKind::Private {
            contact: n % 3 == 0,
        },
    }
}

fn summary(n: i64, now: i64) -> ChatSummary {
    let kind = kind_of(n);
    let title = match n {
        1 => "Formatting showcase".to_owned(),
        2 => "Forum: abyss contributors".to_owned(),
        _ => format!("Chat {n}"),
    };
    let last = match n % 4 {
        0 => Preview::Media(MediaKind::Photo),
        1 => Preview::Text(format!(
            "Last message in chat {n}, long enough to be cut off in the row"
        )),
        2 => Preview::Text("مرحبا بالعالم".into()),
        _ => Preview::Service,
    };
    ChatSummary {
        id: chat_id(n),
        title: display_name(&title),
        kind,
        pinned: n <= 3,
        read: ReadState {
            unread: u32::try_from(n % 9).unwrap_or(0) * u32::try_from(n % 3).unwrap_or(0),
            read_inbox_max: MsgId(0),
        },
        unread_mentions: 0,
        last: Some(last),
        last_date: now - n * 3_600,
        last_id: None,
        can_send: !matches!(
            kind,
            ChatKind::Supergroup {
                gigagroup: true,
                ..
            }
        ),
        muted_until: if n % 4 == 3 { i64::from(i32::MAX) } else { 0 },
        // Every sixth chat is archived, so the demo has an archive to open.
        folder: if n > 3 && n % 6 == 5 {
            PeerFolder::Archive
        } else {
            PeerFolder::Main
        },
        reading: None,
        topic_unread: None,
    }
}

fn message(chat: ChatId, id: i32, date: i64, body: Body) -> Message {
    Message {
        id: MsgId(id),
        thread: ThreadKey { chat, topic: None },
        sender: None,
        date,
        edit_date: None,
        outgoing: false,
        reply_to: None,
        body,
    }
}

fn entity(offset: i32, length: i32, kind: EntityKind) -> RawEntity {
    RawEntity {
        offset,
        length,
        kind,
    }
}

/// Every entity kind, overlapping, around multi-byte text.
fn formatted() -> RichText {
    let text =
        "bold italic under strike `code` pre spoiler @mention https://abyss.example click 😀 done";
    RichText::from_entities(
        text,
        &[
            entity(0, 11, EntityKind::Bold),
            entity(5, 12, EntityKind::Italic),
            entity(12, 5, EntityKind::Underline),
            entity(18, 6, EntityKind::Strike),
            entity(25, 6, EntityKind::Code),
            entity(32, 3, EntityKind::Pre),
            entity(36, 7, EntityKind::Spoiler),
            entity(44, 8, EntityKind::Mention),
            entity(53, 21, EntityKind::Url),
            entity(
                75,
                5,
                EntityKind::TextUrl("https://example.org/real-target".into()),
            ),
        ],
    )
}

fn showcase_body(i: i32) -> Body {
    let text = |t: &str| Body::Text(RichText::plain(t));
    match i % 12 {
        0 => Body::Text(formatted()),
        1 => text("Emoji: 👩‍💻🚀🇺🇦 and ZWJ 👨‍👩‍👧‍👦"),
        2 => text("עברית ועוד: مرحبا بالعالم mixed with English"),
        3 if i % 120 == 3 => text(&"long text ".repeat(410)),
        4 => Body::Media {
            kind: media(i),
            caption: RichText::plain("A caption under the media"),
        },
        5 => Body::Service("Ada joined the group".into()),
        6 => Body::Unsupported,
        7 => {
            let entities: Vec<RawEntity> = (0..=i32::try_from(MAX_ENTITIES).unwrap_or(256))
                .map(|k| entity(k % 40, 1, EntityKind::Bold))
                .collect();
            Body::Text(RichText::from_entities(
                &"over budget ".repeat(4),
                &entities,
            ))
        }
        _ => Body::Text(RichText::plain(&format!("Message {i}"))),
    }
}

fn media(i: i32) -> MediaKind {
    match (i / 12) % 11 {
        0 => MediaKind::Photo,
        1 => MediaKind::Video,
        2 => MediaKind::Voice,
        3 => MediaKind::Audio,
        4 => MediaKind::Document {
            name: display_name("report-2026.pdf"),
            size: 1_572_864,
        },
        5 => MediaKind::Sticker,
        6 => MediaKind::Gif,
        7 => MediaKind::Poll,
        8 => MediaKind::Location,
        9 => MediaKind::Contact,
        _ => MediaKind::Other,
    }
}

fn showcase(chat: ChatId, now: i64) -> Vec<Message> {
    (1..=SHOWCASE_MESSAGES)
        .map(|i| {
            let date = now - i64::from(SHOWCASE_MESSAGES - i) * 600;
            let mut m = message(chat, i, date, showcase_body(i));
            m.outgoing = i % 5 == 0;
            m.edit_date = (i % 9 == 0).then_some(date + 60);
            m
        })
        .collect()
}

/// Sets `summary`'s newest message id, and places its read pointer so its
/// unread count of incoming messages is above it. The showcase gets
/// [`SHOWCASE_UNREAD`]. In chat [`OWN_ABOVE_POINTER`] only the user's own
/// last three messages are above it, under a scripted count of 1, which the
/// fake serves until the first read.
fn read_up_to(summary: &mut ChatSummary, history: &mut [Message]) {
    summary.last_id = history.last().map(|m| m.id);
    if summary.id == chat_id(1) {
        summary.read.unread = SHOWCASE_UNREAD;
    }
    if summary.id == chat_id(OWN_ABOVE_POINTER) {
        let own = history.len().saturating_sub(3);
        for m in history.iter_mut().skip(own) {
            m.outgoing = true;
        }
        summary.read = ReadState {
            unread: 1,
            read_inbox_max: history.get(own).map_or(MsgId(0), |m| MsgId(m.id.0 - 1)),
        };
        return;
    }
    let unread = usize::try_from(summary.read.unread).unwrap_or(usize::MAX);
    summary.read.read_inbox_max = match unread.checked_sub(1) {
        None => history.last().map_or(MsgId(0), |m| m.id),
        Some(k) => history
            .iter()
            .rev()
            .filter(|m| !m.outgoing)
            .nth(k)
            .map_or(MsgId(0), |m| MsgId(m.id.0 - 1)),
    };
}

/// [`GENERAL_POSTS`] posts into the forum's General topic, newest last,
/// every fifth the user's own: General is read only up to its first
/// message, so it opens around its read position.
fn general_posts(history: &mut Vec<Message>, now: i64) {
    let chat = chat_id(UNREAD_FORUM);
    let first = history.last().map_or(1, |m| m.id.0 + 1);
    for i in 0..GENERAL_POSTS {
        let date = now - i64::from(GENERAL_POSTS - i) * 4;
        let body = Body::Text(RichText::plain(&format!("Catching up {i} in General")));
        let mut m = message(chat, first + i, date, body);
        m.outgoing = i % 5 == 0;
        if !m.outgoing {
            m.sender = Some(Sender {
                id: chat_id(10_001),
                name: display_name("Grace"),
            });
        }
        history.push(m);
    }
}

fn ordinary(chat: ChatId, n: i64, now: i64) -> Vec<Message> {
    let senders = ["Ada", "Grace", "Linus"];
    (1..=MESSAGES)
        .map(|i| {
            let date = now - n * 3_600 - i64::from(MESSAGES - i) * 120;
            let body = Body::Text(RichText::plain(&format!("Message {i} in chat {n}")));
            let mut m = message(chat, i, date, body);
            m.outgoing = i % 4 == 0;
            if chat.kind != PeerKind::User && !m.outgoing {
                let k = usize::try_from(i).unwrap_or(0) % senders.len();
                m.sender = senders.get(k).map(|name| Sender {
                    id: chat_id(i64::from(i) + 10_000),
                    name: display_name(name),
                });
            }
            m
        })
        .collect()
}

/// The demo account at time `now` (Unix seconds).
pub(crate) fn script(now: i64) -> FakeScript {
    let mut script = FakeScript::default();
    for n in 1..=CHATS {
        let summary = summary(n, now);
        let id = summary.id;
        let forum = matches!(summary.kind, ChatKind::Supergroup { forum: true, .. });
        script.dialogs.push(summary);
        if forum {
            let order = if n == CREATION_ORDERED_FORUM {
                TopicOrder::Created
            } else {
                TopicOrder::LastActivity
            };
            script.add_demo_forum(id, now, order);
            continue;
        }
        let mut history = if n == 1 {
            showcase(id, now)
        } else {
            ordinary(id, n, now)
        };
        if let Some(summary) = script.dialogs.last_mut() {
            read_up_to(summary, &mut history);
        }
        script.history.insert(id, history);
    }
    if let Some(general) = script.history.get_mut(&chat_id(UNREAD_FORUM)) {
        general_posts(general, now);
    }
    script.folders = folders();
    script
}

/// Three chat folders: one by rules, one by rules with exclusions, and one
/// of named chats with a pinned one.
fn folders() -> Vec<ChatFolder> {
    let folder = |id, title: &str, rules| ChatFolder {
        id,
        title: display_name(title),
        rules,
        pinned: Vec::new(),
        include: Vec::new(),
        exclude: Vec::new(),
        shared: false,
    };
    let people = FolderRules::CONTACTS.with(FolderRules::NON_CONTACTS);
    let unread = people
        .with(FolderRules::GROUPS)
        .with(FolderRules::EXCLUDE_READ)
        .with(FolderRules::EXCLUDE_MUTED);
    let mut work = folder(4, "Work", FolderRules::default());
    work.pinned = vec![chat_id(2)];
    work.include = [1, 4, 9, 14, 24].map(chat_id).to_vec();
    vec![
        folder(2, "People", people),
        folder(3, "Unread", unread),
        work,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_data_is_valid_domain_data() {
        let script = script(1_794_837_900);
        assert_eq!(script.dialogs.len(), 1000);
        assert!(script.dialogs.iter().all(ChatSummary::is_valid));
        assert!(script.folders.iter().all(ChatFolder::is_valid));
        let showcase = &script.history[&chat_id(1)];
        assert_eq!(showcase.len(), 2000);
        assert!(showcase.iter().all(Message::is_valid));
        assert!(showcase.windows(2).all(|w| w[0].id < w[1].id));
        let long = showcase.iter().filter_map(|m| match &m.body {
            Body::Text(rt) => Some(rt.text().len()),
            _ => None,
        });
        assert!(long.max().unwrap() >= 4096);
    }

    #[test]
    fn demo_forums_have_paging_sized_topic_lists() {
        let script = script(1_794_837_900);
        let forums: Vec<ChatId> = script
            .dialogs
            .iter()
            .filter(|c| matches!(c.kind, ChatKind::Supergroup { forum: true, .. }))
            .map(|c| c.id)
            .collect();
        assert_eq!(forums.len(), 100);
        for chat in &forums {
            let topics = &script.topics[chat];
            assert_eq!(topics.len(), 61, "General plus 60 topics");
            assert!(topics.len() > usize::try_from(abyss_core::store::TOPICS_PAGE).unwrap());
            assert_eq!(topics.iter().filter(|t| t.topic.pinned).count(), 2);
            assert_eq!(topics.iter().filter(|t| t.topic.closed).count(), 1);
            assert!(topics.iter().all(|t| t.topic.is_valid()));
            assert!(script.history[chat].iter().all(Message::is_valid));
        }
        assert_eq!(
            script.topic_order[&chat_id(CREATION_ORDERED_FORUM)],
            TopicOrder::Created
        );
        assert_eq!(script.topic_order[&chat_id(2)], TopicOrder::LastActivity);
    }
}
