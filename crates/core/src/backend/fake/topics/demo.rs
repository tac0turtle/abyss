//! Demo forums for `cargo dev` and tests: General plus
//! [`DEMO_FORUM_TOPICS`] topics, more than one topics page, with pinned, closed
//! and unread topics, and activity in a different order from creation.

use super::super::{FakeScript, text_message};
use crate::model::{
    Body, ChatId, Freshness, Message, MsgId, PeerKind, Sender, ThreadKey, Topic, TopicId,
    TopicOrder, TopicSnapshot, Visibility,
};
use crate::sanitize::display_name;

/// Topics in a demo forum besides General.
pub const DEMO_FORUM_TOPICS: i32 = 60;
/// Posts spread over a demo forum's topics after they were created.
const POSTS: i32 = 240;
/// Seconds between two topic creations, and between two posts.
const CREATE_STEP: i64 = 3_600;
const POST_STEP: i64 = 600;
/// Coprime with `DEMO_FORUM_TOPICS + 1`: posts visit every topic in a
/// scrambled order, so activity order differs from creation order.
const SPREAD: i32 = 37;
/// The pinned topics (two) and the closed one.
const PINNED: [i32; 2] = [7, 23];
const CLOSED: i32 = 13;

const NAMES: [&str; 8] = [
    "Release planning",
    "Bug reports",
    "Design review",
    "Help wanted",
    "Announcements",
    "Off-topic",
    "Benchmarks",
    "Documentation",
];
const SENDERS: [&str; 3] = ["Ada", "Grace", "Linus"];

/// A demo forum's server state.
#[derive(Clone, Debug)]
pub struct DemoForum {
    /// Ascending by id, with topics as the server states them.
    pub history: Vec<Message>,
    pub topics: Vec<TopicSnapshot>,
}

/// The `k`-th topic of a demo forum (0 is General). Topic `k`'s creation
/// message has id `k + 1`, so topic ids run from 2 to 61.
fn topic_id(k: i32) -> Option<TopicId> {
    match k {
        0 => Some(TopicId::GENERAL),
        _ => TopicId::try_from(k.saturating_add(1)).ok(),
    }
}

fn title(k: i32) -> String {
    if k == 0 {
        return "General".to_owned();
    }
    let len = i32::try_from(NAMES.len()).unwrap_or(1);
    let name = usize::try_from(k.rem_euclid(len))
        .ok()
        .and_then(|i| NAMES.get(i))
        .copied()
        .unwrap_or("Topic");
    format!("{name} #{k}")
}

fn sender(j: i32) -> Option<Sender> {
    let i = usize::try_from(j).unwrap_or(0) % SENDERS.len();
    SENDERS.get(i).map(|name| Sender {
        id: ChatId {
            kind: PeerKind::User,
            id: 20_000 + i64::try_from(i).unwrap_or(0),
        },
        name: display_name(name),
    })
}

/// General plus [`DEMO_FORUM_TOPICS`] topics with their creation messages and
/// [`POSTS`] posts, the newest at `now` (Unix seconds). Topics 7 and 23 are
/// pinned, 13 is closed, and every third topic (General included) has
/// unread posts.
pub fn demo_forum(chat: ChatId, now: i64) -> DemoForum {
    let base = now - CREATE_STEP * i64::from(DEMO_FORUM_TOPICS + 1) - POST_STEP * i64::from(POSTS);
    let mut history = Vec::new();
    let mut created = Vec::new();
    // Message 1 opens General; message k + 1 creates topic k.
    for k in 0..=DEMO_FORUM_TOPICS {
        let Some(topic) = topic_id(k) else { continue };
        let date = base + CREATE_STEP * i64::from(k);
        let mut m = text_message(chat, k + 1, "");
        m.date = date;
        m.thread = ThreadKey {
            chat,
            topic: (k > 0).then_some(topic),
        };
        m.body = Body::Service(match k {
            0 => "Ada created the group".to_owned(),
            _ => format!("Ada created the topic «{}»", title(k)),
        });
        history.push(m);
        created.push((k, topic, date));
    }
    let first_post = DEMO_FORUM_TOPICS + 2;
    for j in 0..POSTS {
        let k = (j * SPREAD).rem_euclid(DEMO_FORUM_TOPICS + 1);
        let Some(topic) = topic_id(k) else { continue };
        let mut m = text_message(chat, first_post + j, &format!("Post {j} in {}", title(k)));
        m.date = base + CREATE_STEP * i64::from(DEMO_FORUM_TOPICS + 1) + POST_STEP * i64::from(j);
        m.thread = ThreadKey {
            chat,
            topic: (topic != TopicId::GENERAL).then_some(topic),
        };
        m.outgoing = j % 5 == 0;
        if !m.outgoing {
            m.sender = sender(j);
        }
        history.push(m);
    }
    let topics = created
        .into_iter()
        .map(|(k, id, date)| snapshot(&history, k, id, date))
        .collect();
    DemoForum { history, topics }
}

/// Every third topic is read up to its creation message only, the others up
/// to their newest message.
fn snapshot(history: &[Message], k: i32, id: TopicId, created: i64) -> TopicSnapshot {
    let root = MsgId(k + 1);
    let newest = history
        .iter()
        .filter(|m| m.thread.topic.unwrap_or(TopicId::GENERAL) == id)
        .map(|m| m.id)
        .max()
        .unwrap_or(root);
    TopicSnapshot {
        topic: Topic {
            id,
            title: display_name(&title(k)),
            created,
            closed: k == CLOSED,
            pinned: PINNED.contains(&k),
            unread: 0,
            unread_mentions: 0,
            freshness: Freshness::Current,
            read_inbox_max: if k % 3 == 0 { root } else { newest },
            last: None,
            last_date: 0,
            last_id: None,
        },
        visibility: Visibility::Shown,
        top_version: i64::MIN,
    }
}

impl FakeScript {
    /// Makes `chat` (already in `dialogs` with a forum kind) a demo forum
    /// served in `order`: replaces its history and topics with
    /// [`demo_forum`]'s.
    pub fn add_demo_forum(&mut self, chat: ChatId, now: i64, order: TopicOrder) {
        let DemoForum { history, topics } = demo_forum(chat, now);
        self.history.insert(chat, history);
        self.topics.insert(chat, topics);
        self.topic_order.insert(chat, order);
    }
}
