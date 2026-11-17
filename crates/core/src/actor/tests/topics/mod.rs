//! Stage 2 tasks 6 and 7: forum topic lists and topic threads in the
//! actor, against the fake's forums (plan section 11, "Actor against the
//! fake").

mod admission;
mod bootstrap;
mod continuity;
mod crossing;
mod deletion;
mod drain;
mod flood;
mod kinds;
mod open_topic;
mod opening;
mod overflow;
mod threads;

use std::time::Duration;

use super::{Harness, harness};
use crate::api::{Command, ConnState, Event, HistoryDir, LoadTarget};
use crate::backend::fake::{FakeScript, TopicsCallRecord, TopicsRequest, summary};
use crate::model::{
    Body, ChatId, ChatKind, ChatSummary, Freshness, Message, MsgId, PeerKind, ThreadKey, Topic,
    TopicId, TopicOrder, TopicSnapshot, Visibility,
};
use crate::store::TopicsGen;

/// The demo forums' "now" (Unix seconds).
const NOW: i64 = 1_700_000_000;

fn forum(id: i64) -> ChatId {
    ChatId {
        kind: PeerKind::Channel,
        id,
    }
}

fn tid(n: i32) -> TopicId {
    TopicId::try_from(n).unwrap()
}

fn topic_key(chat: ChatId, topic: i32) -> ThreadKey {
    ThreadKey {
        chat,
        topic: Some(tid(topic)),
    }
}

fn forum_summary(chat: ChatId) -> ChatSummary {
    ChatSummary {
        kind: ChatKind::Supergroup {
            forum: true,
            gigagroup: false,
        },
        ..summary(chat, "Forum")
    }
}

/// One plain chat plus a demo forum (General and 60 topics: two pages) per
/// entry, served in its order.
fn demo_forums(forums: &[(ChatId, TopicOrder)]) -> FakeScript {
    let mut script = FakeScript::demo(1, 5);
    for &(chat, order) in forums {
        script.dialogs.push(forum_summary(chat));
        script.add_demo_forum(chat, NOW, order);
    }
    script
}

/// Adds a forum with General and `n` topics (ids 2..=n+1, created in id
/// order), each holding only its creation message: activity order is id
/// order, newest first.
fn add_big_forum(script: &mut FakeScript, chat: ChatId, n: i32) {
    script.dialogs.push(forum_summary(chat));
    let mut history = Vec::new();
    let mut topics = Vec::new();
    for k in 0..=n {
        let id = k + 1;
        let mut m = crate::backend::fake::text_message(chat, id, "");
        m.body = Body::Service(format!("created topic {id}"));
        m.thread.topic = (k > 0).then(|| tid(id));
        history.push(m);
        let topic = if k == 0 { TopicId::GENERAL } else { tid(id) };
        topics.push(TopicSnapshot {
            topic: Topic {
                id: topic,
                title: format!("topic {id}"),
                created: i64::from(id),
                closed: false,
                pinned: false,
                unread: 0,
                unread_mentions: 0,
                freshness: Freshness::Current,
                read_inbox_max: MsgId(id),
                last: None,
                last_date: 0,
                last_id: None,
            },
            visibility: Visibility::Shown,
            top_version: i64::MIN,
        });
    }
    script.history.insert(chat, history);
    script.topics.insert(chat, topics);
}

/// Waits until the server has seen `n` calls matching `pred`.
async fn calls_reached(h: &mut Harness, n: usize, pred: impl Fn(&TopicsRequest) -> bool) {
    for _ in 0..1000 {
        if h.fake
            .topics_calls()
            .iter()
            .filter(|r| pred(&r.request))
            .count()
            >= n
        {
            return;
        }
        events_for(h, Duration::from_millis(10)).await;
    }
    panic!("the server never saw call #{n}");
}

fn is_by_id(r: &TopicsRequest) -> bool {
    matches!(r, TopicsRequest::ById { .. })
}

fn is_page(r: &TopicsRequest) -> bool {
    matches!(r, TopicsRequest::Page { .. })
}

/// A forum of `n` topics (activity order is id order) whose first page is
/// open; topic 5 is beyond page one when `n` exceeds 54.
async fn opened_big(n: i32) -> (Harness, ChatId) {
    let f = forum(100);
    let mut script = FakeScript::demo(1, 1);
    add_big_forum(&mut script, f, n);
    let mut h = online(script).await;
    open_forum(&mut h, f).await;
    (h, f)
}

/// Opens topic `t` of `f` and waits until its list holds it (by id when
/// it is beyond the loaded pages).
async fn open_topic(h: &mut Harness, f: ChatId, t: i32) {
    h.send(Command::OpenThread(topic_key(f, t))).await;
    h.until(|e| matches!(e, Event::TopicsAppended { topics, .. } if topics.iter().any(|x| x.id == tid(t))))
        .await;
}

/// Waits until online.
async fn online(script: FakeScript) -> Harness {
    let mut h = harness(script, None).await;
    h.online().await;
    h
}

/// Waits for the next `TopicsReset` of `chat`.
async fn reset(h: &mut Harness, chat: ChatId) -> (TopicsGen, TopicOrder, Vec<Topic>) {
    let (ev, _) = h
        .until(|e| matches!(e, Event::TopicsReset { chat: c, .. } if *c == chat))
        .await;
    match ev {
        Event::TopicsReset {
            view,
            order,
            topics,
            ..
        } => (view, order, topics),
        _ => unreachable!(),
    }
}

/// Waits for the next `TopicsPaging` of `chat`; returns `has_more`.
async fn paging(h: &mut Harness, chat: ChatId) -> bool {
    let (ev, _) = h
        .until(|e| matches!(e, Event::TopicsPaging { chat: c, .. } if *c == chat))
        .await;
    matches!(ev, Event::TopicsPaging { has_more: true, .. })
}

/// Opens `chat`'s topic list (not retained) and waits for its first page.
async fn open_forum(h: &mut Harness, chat: ChatId) -> Vec<Topic> {
    h.send(Command::OpenForum(chat)).await;
    let (_, _, topics) = reset(h, chat).await;
    paging(h, chat).await;
    topics
}

fn page_calls(h: &Harness, chat: ChatId) -> usize {
    h.fake
        .topics_calls()
        .iter()
        .filter(|r| matches!(r.request, TopicsRequest::Page { chat: c, .. } if c == chat))
        .count()
}

/// The by-id calls the server saw, oldest first.
fn by_id_calls(h: &Harness) -> Vec<(Vec<TopicId>, tokio::time::Instant)> {
    h.fake
        .topics_calls()
        .into_iter()
        .filter_map(|TopicsCallRecord { request, at }| match request {
            TopicsRequest::ById { ids, .. } => Some((ids, at)),
            TopicsRequest::Page { .. } => None,
        })
        .collect()
}

/// Lets the actor and the fake run for `d` of the runtime's (paused) clock.
async fn run_for(d: Duration) {
    tokio::time::sleep(d).await;
}

/// Every later state of `topic` the events report (resets, appends, bumps).
fn states_of(events: &[Event], topic: TopicId) -> Vec<Topic> {
    let mut out = Vec::new();
    for e in events {
        match e {
            Event::TopicsReset { topics, .. } | Event::TopicsAppended { topics, .. } => {
                out.extend(topics.iter().filter(|t| t.id == topic).cloned());
            }
            Event::TopicBumped { topic: t, .. } if t.id == topic => out.push(t.clone()),
            _ => {}
        }
    }
    out
}

/// Collects events for `d` of the paused clock, taking them as they come
/// so the bounded event queue never holds the actor up.
async fn events_for(h: &mut Harness, d: Duration) -> Vec<Event> {
    const STEP: Duration = Duration::from_millis(100);
    let mut seen = Vec::new();
    let mut left = d;
    while !left.is_zero() {
        let step = left.min(STEP);
        run_for(step).await;
        left -= step;
        seen.extend(h.drain());
    }
    seen
}

/// Waits until the actor reports `LoadEnded` for `chat`'s topics.
async fn topics_ended(h: &mut Harness, chat: ChatId) -> Event {
    h.until(|e| matches!(e, Event::LoadEnded { target: LoadTarget::Topics(c), .. } if *c == chat))
        .await
        .0
}

/// Waits for `Online` after a rebuild.
async fn back_online(h: &mut Harness) {
    h.until(|e| matches!(e, Event::Connection(ConnState::Online)))
        .await;
}

/// A server-side message of `thread` (as the server states its topic).
fn message_in(thread: ThreadKey, id: i32, text: &str) -> Message {
    let mut m = crate::backend::fake::text_message(thread.chat, id, text);
    m.thread.topic = thread.topic.filter(|t| *t != TopicId::GENERAL);
    m
}

/// Forum 100 with General (1), topic A (2) and topic B (3), and a post in
/// each `(id, topic)` of `posts` (topic 1: General, without a header).
fn forum_with(posts: impl IntoIterator<Item = (i32, i32)>) -> (FakeScript, ChatId) {
    let f = forum(100);
    let mut script = FakeScript::demo(1, 1);
    add_big_forum(&mut script, f, 2);
    let history = script.history.entry(f).or_default();
    history.extend(
        posts
            .into_iter()
            .map(|(id, t)| message_in(topic_key(f, t), id, "post")),
    );
    (script, f)
}

/// Waits for the next `History` of `thread` in `dir`; returns its messages
/// (each checked to carry `thread`) and the events before it.
async fn history_of(
    h: &mut Harness,
    thread: ThreadKey,
    dir: HistoryDir,
) -> (Vec<Message>, Vec<Event>) {
    let (ev, skipped) = h
        .until(
            |e| matches!(e, Event::History { thread: t, dir: d, .. } if *t == thread && *d == dir),
        )
        .await;
    assert_keyed(&skipped);
    let Event::History { messages, .. } = ev else {
        unreachable!()
    };
    assert!(messages.iter().all(|m| m.thread == thread), "{messages:?}");
    (messages, skipped)
}

/// Waits until `thread`'s view is reconciled with the server's tail;
/// returns the events before it (each checked by [`assert_keyed`]).
async fn reconciled(h: &mut Harness, thread: ThreadKey) -> Vec<Event> {
    let (_, skipped) = h
        .until(|e| matches!(e, Event::Reconciled { thread: t, .. } if *t == thread))
        .await;
    assert_keyed(&skipped);
    skipped
}

/// The thread-equals-key invariant on events: every message a `History`
/// carries has the event's thread, and an `Edited` one is normalized (a
/// forum message has a topic).
fn assert_keyed(events: &[Event]) {
    for e in events {
        match e {
            Event::History {
                thread, messages, ..
            } => assert!(messages.iter().all(|m| m.thread == *thread), "{e:?}"),
            Event::Edited(m) if m.thread.chat.kind == PeerKind::Channel => {
                assert!(m.thread.topic.is_some(), "{e:?}");
            }
            _ => {}
        }
    }
}

fn ids(messages: &[Message]) -> Vec<i32> {
    messages.iter().map(|m| m.id.0).collect()
}
