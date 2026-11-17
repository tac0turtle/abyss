//! Crossing responses (plan section 11): a delayed snapshot sees the server
//! as it was at call time, and live updates cross it.

use std::time::Duration;

use super::*;
use crate::backend::Update;
use crate::backend::fake::{Fault, Op};
use crate::store::TopicEdit;

const DELAY: Duration = Duration::from_secs(5);

/// A forum of ten topics, opened.
async fn opened() -> (Harness, ChatId) {
    let f = forum(100);
    let mut script = FakeScript::demo(1, 1);
    add_big_forum(&mut script, f, 10);
    let mut h = online(script).await;
    open_forum(&mut h, f).await;
    (h, f)
}

/// Renames `t` on the server, which the client sees live: a meta change
/// that makes the topic due.
async fn rename(h: &Harness, f: ChatId, t: TopicId, title: &str) {
    let edit = TopicEdit {
        title: Some(title.to_owned()),
        ..TopicEdit::default()
    };
    h.fake.edit_topic(f, t, edit).await.unwrap();
}

/// Waits until the server has seen `n` by-id calls.
async fn by_id_reached(h: &Harness, n: usize) {
    for _ in 0..1000 {
        if by_id_calls(h).len() >= n {
            return;
        }
        run_for(Duration::from_millis(10)).await;
    }
    panic!("no by-id call #{n}");
}

/// A by-id snapshot taken after a close, crossing the reopen: the live
/// reopen is kept, and the refetch settles it reopened.
#[tokio::test(start_paused = true)]
async fn a_by_id_snapshot_crossing_a_close_then_reopen() {
    let (mut h, f) = opened().await;
    let t = tid(5);
    h.fake.inject(Op::TopicsById, Fault::Delay(DELAY));
    h.fake.close_topic(f, t).await.unwrap();
    by_id_reached(&h, 1).await; // its snapshot says closed
    h.fake.reopen_topic(f, t).await.unwrap();
    let (_, _) = h
        .until(|e| matches!(e, Event::TopicBumped { topic, .. } if topic.id == t && !topic.closed))
        .await;
    let seen = events_for(&mut h, 3 * DELAY).await;
    let states = states_of(&seen, t);
    assert!(states.iter().all(|s| !s.closed), "{states:?}");
    let calls = by_id_calls(&h);
    assert!(calls.len() >= 2 && calls[1].0.contains(&t), "refetched");
    h.stop().await;
}

/// A root deletion crossing a by-id snapshot that still finds the topic:
/// only a suspicion; a fresh by-id result that says deleted removes it.
#[tokio::test(start_paused = true)]
async fn a_root_deletion_crossing_a_snapshot_is_only_suspected() {
    let (mut h, f) = opened().await;
    let t = tid(5);
    h.fake.inject(Op::TopicsById, Fault::Delay(DELAY));
    rename(&h, f, t, "renamed").await;
    by_id_reached(&h, 1).await; // its snapshot finds the topic
    let start = tokio::time::Instant::now();
    h.fake.delete_topic(f, t).await.unwrap();
    let (_, seen) = h
        .until(|e| matches!(e, Event::TopicRemoved { topic, .. } if *topic == t))
        .await;
    assert!(
        tokio::time::Instant::now() >= start + DELAY,
        "not removed before the crossing snapshot completed"
    );
    assert!(
        states_of(&seen, t).iter().all(|s| s.title == "renamed"),
        "listed and usable while suspected"
    );
    assert!(
        by_id_calls(&h).len() >= 2,
        "removed by a later, fresh result"
    );
    h.stop().await;
}

/// Round 3, finding 1: a snapshot that already includes message 101
/// applies unread 1; the live 101 arriving after it is a trigger (`NewSince`),
/// never an increment, and the refetch says 1 again. A later snapshot with
/// an absolute read count is applied as is.
#[tokio::test(start_paused = true)]
async fn a_snapshot_with_101_then_the_live_101() {
    let (mut h, f) = opened().await;
    let t = tid(5);
    let m101 = message_in(topic_key(f, 5), 101, "hello");
    let mut history = h.fake.server_history(f);
    history.push(m101.clone());
    h.fake.set_history(f, history); // on the server; its update comes later
    rename(&h, f, t, "renamed").await;
    h.until(|e| matches!(e, Event::TopicsAppended { topics, .. } if topics.iter().any(|x| x.id == t && x.unread == 1)))
        .await;
    h.fake.push_update(Update::NewMessage(m101)).await;
    let (ev, _) = h
        .until(|e| matches!(e, Event::TopicBumped { topic, .. } if topic.id == t))
        .await;
    assert!(matches!(
        ev,
        Event::TopicBumped { topic, .. } if topic.unread == 1 && topic.freshness == Freshness::NewSince
    ));
    let seen = events_for(&mut h, Duration::from_secs(5)).await;
    let states = states_of(&seen, t);
    assert!(states.iter().all(|s| s.unread == 1), "{states:?}");
    let last = states.last().unwrap();
    assert_eq!(last.freshness, Freshness::Current, "refetched");

    // Read on the server, then refetched: the absolute count applies.
    h.send(Command::MarkRead {
        thread: topic_key(f, 5),
        max_id: MsgId(101),
    })
    .await;
    events_for(&mut h, Duration::from_secs(1)).await;
    rename(&h, f, t, "again").await;
    let seen = events_for(&mut h, Duration::from_secs(5)).await;
    let last = states_of(&seen, t).pop().unwrap();
    assert_eq!((last.unread, last.freshness), (0, Freshness::Current));
    h.stop().await;
}

/// A page dispatched before a by-id fetch but completing after it doesn't
/// regress what the by-id result set.
#[tokio::test(start_paused = true)]
async fn an_older_page_completing_after_a_by_id_result() {
    let (mut h, f) = opened().await;
    let t = tid(5);
    h.fake.inject(Op::Topics, Fault::Delay(DELAY));
    h.send(Command::OpenForum(f)).await; // its page sees `t` read
    reset(&mut h, f).await; // the retained list
    run_for(Duration::from_millis(100)).await;
    let mut history = h.fake.server_history(f);
    history.push(message_in(topic_key(f, 5), 200, "unseen"));
    h.fake.set_history(f, history); // no update arrives
    rename(&h, f, t, "renamed").await;
    h.until(|e| matches!(e, Event::TopicsAppended { topics, .. } if topics.iter().any(|x| x.id == t && x.unread == 1)))
        .await;
    let (_, _, topics) = reset(&mut h, f).await; // the delayed page
    let after = topics.iter().find(|x| x.id == t).unwrap();
    assert_eq!(after.unread, 1, "not regressed by the older page");
    assert_eq!(after.last_id, Some(MsgId(200)));
    h.stop().await;
}
