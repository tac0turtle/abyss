//! Top-message deletion (plan section 11), each time with an older
//! snapshot in flight: the deletion is a live activity change, the older
//! snapshot conflicts, and the refetch sets the server's (lower) state.

use std::time::Duration;

use super::*;
use crate::backend::Update;
use crate::backend::fake::{Fault, Op};
use crate::store::TopicEdit;

/// A forum of ten topics whose topic 5 holds posts `posts` after its root.
async fn with_posts(posts: &[i32]) -> (Harness, ChatId) {
    let f = forum(100);
    let mut script = FakeScript::demo(1, 1);
    add_big_forum(&mut script, f, 10);
    let history = script.history.get_mut(&f).unwrap();
    for &id in posts {
        let mut m = message_in(topic_key(f, 5), id, "post");
        m.outgoing = true;
        history.push(m);
    }
    let mut h = online(script).await;
    open_forum(&mut h, f).await;
    (h, f)
}

/// Deletes `ids` on the server and delivers the deletion.
async fn delete(h: &Harness, f: ChatId, ids: &[i32]) {
    let history = h
        .fake
        .server_history(f)
        .into_iter()
        .filter(|m| !ids.contains(&m.id.0))
        .collect();
    h.fake.set_history(f, history);
    h.fake
        .push_update(Update::Deleted {
            chat: Some(f),
            ids: ids.iter().map(|&id| MsgId(id)).collect(),
        })
        .await;
}

/// Starts a by-id snapshot of topic 5 that completes after `delay` (a
/// rename makes it due) and waits until the server took it.
async fn older_snapshot_in_flight(h: &Harness, f: ChatId, delay: Duration) {
    let before = by_id_calls(h).len();
    h.fake.inject(Op::TopicsById, Fault::Delay(delay));
    let edit = TopicEdit {
        title: Some("renamed".to_owned()),
        ..TopicEdit::default()
    };
    h.fake.edit_topic(f, tid(5), edit).await.unwrap();
    for _ in 0..1000 {
        if by_id_calls(h).len() > before {
            return;
        }
        run_for(Duration::from_millis(10)).await;
    }
    panic!("no by-id call");
}

/// Deleting the latest message 200 while an older snapshot (still showing
/// 200) is in flight: that snapshot conflicts, the refetch sets 199.
#[tokio::test(start_paused = true)]
async fn deleting_the_latest_message() {
    let (mut h, f) = with_posts(&[199, 200]).await;
    let t = tid(5);
    older_snapshot_in_flight(&h, f, Duration::from_secs(5)).await;
    h.drain(); // the rename
    delete(&h, f, &[200]).await;
    let seen = events_for(&mut h, Duration::from_secs(15)).await;
    let states = states_of(&seen, t);
    assert!(
        !states
            .iter()
            .any(|s| s.last_id == Some(MsgId(200)) && s.freshness == Freshness::Current),
        "the older snapshot is never applied: {states:?}"
    );
    let last = states.last().unwrap();
    assert_eq!(last.last_id, Some(MsgId(199)), "{states:?}");
    assert!(by_id_calls(&h).len() >= 2, "refetched after the conflict");
    h.stop().await;
}

/// Deleting the last remaining post: the refetch sets the server's state
/// (the root), a lower `last_id` is accepted.
#[tokio::test(start_paused = true)]
async fn deleting_the_last_remaining_message() {
    let (mut h, f) = with_posts(&[150]).await;
    let t = tid(5);
    older_snapshot_in_flight(&h, f, Duration::from_secs(5)).await;
    delete(&h, f, &[150]).await;
    let seen = events_for(&mut h, Duration::from_secs(15)).await;
    let states = states_of(&seen, t);
    let last = states.last().unwrap();
    assert_eq!(last.last_id, Some(MsgId(5)), "the root: {states:?}");
    h.stop().await;
}
