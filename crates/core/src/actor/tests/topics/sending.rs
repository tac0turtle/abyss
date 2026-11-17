//! Sends into forum topics (plan section 6.7): the thread a send carries
//! across fences and rebuilds, and closed or deleted topics, refused by
//! core when its list knows and by the server when it doesn't.

use std::time::Duration;

use super::*;
use crate::actor::{Internal, MAX_CONCURRENT};
use crate::api::{SendState, UserError};
use crate::backend::Update;
use crate::backend::fake::{Fault, Op};
use crate::model::LocalId;
use crate::store::PostClosed;

async fn send(h: &Harness, thread: ThreadKey, local_id: u64) {
    h.send(Command::SendText {
        thread,
        text: "into the topic".into(),
        local_id: LocalId(local_id),
    })
    .await;
}

/// The send's next state other than `Sending`, and the events before it.
async fn settled(h: &mut Harness, local_id: u64) -> (SendState, Vec<Event>) {
    let id = LocalId(local_id);
    let (ev, seen) = h
        .until(|e| matches!(e, Event::SendState { local_id, state } if *local_id == id && *state != SendState::Sending))
        .await;
    let Event::SendState { state, .. } = ev else {
        unreachable!()
    };
    (state, seen)
}

/// The server's send attempts into `thread`, oldest first: the thread
/// (`reply_to`) and the `random_id` of each.
fn attempts_into(h: &Harness, thread: ThreadKey) -> Vec<(ThreadKey, i64)> {
    h.fake
        .send_attempts()
        .into_iter()
        .filter(|(t, _)| *t == thread)
        .collect()
}

/// A send into a topic is queued behind busy permits when its chat is
/// fenced: it goes back to the pending table and waits out the kind check.
/// After the fence lifts its attempt hangs, and once a rebuild drops it the
/// send is replayed into the same topic with the same `random_id`.
#[tokio::test(start_paused = true)]
async fn a_topic_send_waits_for_the_fence_and_survives_a_rebuild() {
    let (script, f) = forum_with([]);
    let mut h = online(script).await;
    let thread = topic_key(f, 2);
    let busy = u64::try_from(MAX_CONCURRENT).unwrap();
    for i in 0..busy {
        h.fake
            .inject(Op::Send, Fault::Delay(Duration::from_secs(5)));
        h.send(Command::SendText {
            thread: ThreadKey::whole(super::super::chat(1)),
            text: format!("holds a permit {i}"),
            local_id: LocalId(100 + i),
        })
        .await;
    }
    run_for(Duration::from_millis(100)).await;
    h.fake
        .inject(Op::PeerDialogs, Fault::Delay(Duration::from_secs(10)));
    h.fake.inject(Op::Send, Fault::Hang);
    send(&h, thread, 1).await;
    h.fake.push_update(Update::ChatChanged { chat: f }).await;
    // The permits free at 5 s; the check then runs until 15 s.
    events_for(&mut h, Duration::from_secs(12)).await;
    assert_eq!(peer_calls_with(&h, f).len(), 1, "the check is out");
    assert!(attempts_into(&h, thread).is_empty(), "held by the fence");
    while attempts_into(&h, thread).is_empty() {
        events_for(&mut h, Duration::from_millis(100)).await;
    }
    h.internal.send(Internal::Wake).await.unwrap();
    let (state, _) = settled(&mut h, 1).await;
    assert!(matches!(state, SendState::Sent { .. }), "{state:?}");
    let attempts = attempts_into(&h, thread);
    assert_eq!(attempts.len(), 2, "{attempts:?}");
    assert_eq!(attempts[0], attempts[1], "same topic and random_id");
    assert!(h.fake.sent().contains(&attempts[0]));
    assert_eq!(h.fake.builds(), 2);
    h.stop().await;
}

/// Closed and deleted topics. A topic the shown list holds closed refuses
/// a send before admission. One closed or deleted while the list missed
/// it is refused by the server (`Failed`) and refetched by id, which shows
/// it closed, or removes it. Once a result says the user may post in closed
/// topics, the closed topic takes a send.
#[tokio::test(start_paused = true)]
async fn closed_and_deleted_topics_refuse_sends() {
    let f = forum(100);
    let mut script = FakeScript::demo(1, 1);
    add_big_forum(&mut script, f, 4);
    let mut h = online(script).await;
    open_forum(&mut h, f).await;
    let closed = |t: i32| {
        move |e: &Event| {
            states_of(std::slice::from_ref(e), tid(t))
                .iter()
                .any(|s| s.closed)
        }
    };
    let refused_with = |seen: &[Event], err: UserError| {
        seen.iter()
            .any(|e| matches!(e, Event::Error(x) if *x == err))
    };

    h.fake.close_topic(f, tid(2)).await.unwrap();
    h.until(closed(2)).await;
    send(&h, topic_key(f, 2), 1).await;
    let (_, seen) = h
        .until(|e| matches!(e, Event::SendRefused { local_id } if *local_id == LocalId(1)))
        .await;
    assert!(refused_with(&seen, UserError::TopicClosed), "{seen:?}");
    assert!(attempts_into(&h, topic_key(f, 2)).is_empty());

    h.fake.drop_next_update();
    h.fake.close_topic(f, tid(3)).await.unwrap();
    send(&h, topic_key(f, 3), 2).await;
    let (state, seen) = settled(&mut h, 2).await;
    assert_eq!(state, SendState::Failed);
    assert!(refused_with(&seen, UserError::TopicClosed), "{seen:?}");
    h.until(closed(3)).await;

    h.fake.set_post_closed(f, PostClosed::Allowed);
    h.fake.drop_next_update();
    h.fake.delete_topic(f, tid(4)).await.unwrap();
    send(&h, topic_key(f, 4), 3).await;
    let (state, seen) = settled(&mut h, 3).await;
    assert_eq!(state, SendState::Failed);
    assert!(refused_with(&seen, UserError::TopicDeleted), "{seen:?}");
    h.until(|e| matches!(e, Event::TopicRemoved { topic, .. } if *topic == tid(4)))
        .await;

    send(&h, topic_key(f, 2), 4).await;
    let (state, _) = settled(&mut h, 4).await;
    assert!(matches!(state, SendState::Sent { .. }), "{state:?}");
    h.stop().await;
}
