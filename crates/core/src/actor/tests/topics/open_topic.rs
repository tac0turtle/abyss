//! The open topic beyond page one (fetched by id, corrected by bootstrap
//! and the light refresh) and the open topic deleted while offline.

use std::time::Duration;

use super::*;
use crate::api::WindowState;
use crate::backend::BackendError;

#[tokio::test(start_paused = true)]
async fn a_full_command_queue_preserves_the_forum_and_one_slot_opens_both_views() {
    let a = forum(100);
    let b = forum(200);
    let mut script = FakeScript::demo(1, 1);
    add_big_forum(&mut script, a, 80);
    add_big_forum(&mut script, b, 80);
    let mut h = online(script).await;
    open_forum(&mut h, a).await;

    let thread = topic_key(b, 5);
    let epoch = h.events.observer().get();
    let open_b = || crate::actor::Stamped {
        epoch,
        cmd: Command::OpenForumTopic {
            chat: b,
            topic: tid(5),
        },
    };
    let slots = h.cmd.max_capacity();
    let full = h.cmd.try_reserve_many(slots).unwrap();
    assert!(matches!(
        h.cmd.try_send(open_b()),
        Err(tokio::sync::mpsc::error::TrySendError::Full(_))
    ));
    drop(full);
    h.send(Command::LoadTopics(a)).await;
    h.until(|e| matches!(e, Event::TopicsAppended { chat, .. } if *chat == a))
        .await;
    assert_eq!(page_calls(&h, a), 2, "A is still the shown forum");
    assert_eq!(page_calls(&h, b), 0);

    let remaining = h.cmd.try_reserve_many(slots - 1).unwrap();
    h.cmd.try_send(open_b()).unwrap();
    drop(remaining);
    let reset_b = |e: &Event| matches!(e, Event::TopicsReset { chat, .. } if *chat == b);
    if !reconciled(&mut h, thread).await.iter().any(reset_b) {
        h.until(reset_b).await;
    }
    assert_eq!(page_calls(&h, b), 1);
    h.send(Command::LoadTopics(a)).await;
    topics_ended(&mut h, a).await;
    assert_eq!(page_calls(&h, a), 2, "B replaced A as the shown forum");
    h.stop().await;
}

/// Opened beyond page one, the topic is fetched by id.
#[tokio::test(start_paused = true)]
async fn the_open_topic_beyond_page_one_is_fetched_by_id() {
    let (mut h, f) = opened_big(80).await;
    open_topic(&mut h, f, 5).await;
    let calls = by_id_calls(&h);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, vec![tid(5)]);
    h.stop().await;
}

/// The open topic is closed, then reopened while the client misses the
/// update. `reopen` corrects it: a rebuild's bootstrap, or a light refresh.
async fn missed_reopen(reopen: impl AsyncFnOnce(&mut Harness)) {
    let (mut h, f) = opened_big(80).await;
    let t = tid(5);
    open_topic(&mut h, f, 5).await;
    h.fake.close_topic(f, t).await.unwrap();
    h.until(|e| matches!(e, Event::TopicBumped { topic, .. } if topic.id == t && topic.closed))
        .await;
    events_for(&mut h, Duration::from_secs(3)).await; // the refetch confirms it
    h.fake.drop_next_update();
    h.fake.reopen_topic(f, t).await.unwrap();
    let seen = events_for(&mut h, Duration::from_secs(3)).await;
    assert!(
        states_of(&seen, t).iter().all(|s| s.closed),
        "the reopen is missed"
    );
    reopen(&mut h).await;
    let (ev, _) = h
        .until(|e| matches!(e, Event::TopicsAppended { topics, .. } if topics.iter().any(|x| x.id == t)))
        .await;
    let Event::TopicsAppended { topics, .. } = ev else {
        unreachable!()
    };
    assert!(topics.iter().any(|x| x.id == t && !x.closed), "unblocked");
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_missed_reopen_is_corrected_by_bootstraps_by_id_fetch() {
    missed_reopen(async |h| {
        h.fake.runner_exit();
        back_online(h).await;
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_light_refresh_corrects_a_stale_closed() {
    missed_reopen(async |h| {
        events_for(h, Duration::from_secs(16)).await;
        h.send(Command::Window(WindowState::Active)).await;
    })
    .await;
}

/// Round 3, finding 6: the open topic is deleted while the client is away
/// and no root deletion arrives. Bootstrap's by-id fetch says deleted, which
/// is fresh: the topic is removed and its thread invalidated.
#[tokio::test(start_paused = true)]
async fn the_open_topic_deleted_while_offline_is_removed() {
    let (mut h, f) = opened_big(10).await;
    let t = tid(5);
    h.send(Command::OpenThread(topic_key(f, 5))).await;
    events_for(&mut h, Duration::from_secs(2)).await;
    h.fake.drop_next_update();
    h.fake.delete_topic(f, t).await.unwrap();
    h.fake.runner_exit();
    back_online(&mut h).await;
    let (_, seen) = h
        .until(|e| matches!(e, Event::TopicRemoved { topic, .. } if *topic == t))
        .await;
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, Event::TopicRemoved { topic, .. } if *topic != t)),
        "{seen:?}"
    );
    h.until(|e| matches!(e, Event::ThreadInvalidated(k) if *k == topic_key(f, 5)))
        .await;
    let calls = by_id_calls(&h);
    assert!(calls.last().is_some_and(|(ids, _)| ids.contains(&t)));
    // Nothing keeps asking for the deleted topic.
    events_for(&mut h, Duration::from_secs(10)).await;
    assert_eq!(by_id_calls(&h).len(), calls.len());
    h.stop().await;
}

/// The same with the disk cache: removing the topic deletes its cached
/// rows, and only its own. The rebuild's tail of the open thread fails, so
/// only the removal can have deleted them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_topic_deleted_while_offline_loses_its_cached_rows() {
    use crate::backend::fake::{Fault, Op};
    use crate::cache::CacheKey;

    let dir = super::super::CacheDir::new();
    let key = CacheKey::generate().unwrap();
    let cache = super::super::cache_at(dir.path(), &key);
    let f = forum(100);
    let mut script = FakeScript::demo(1, 1);
    add_big_forum(&mut script, f, 10);
    for t in [5, 6] {
        let posts = script.history.get_mut(&f).unwrap();
        posts.push(message_in(topic_key(f, t), 100 + t, "post"));
    }
    let mut h = harness(script, Some(cache.clone())).await;
    h.online().await;
    open_forum(&mut h, f).await;
    for t in [6, 5] {
        h.send(Command::OpenThread(topic_key(f, t))).await;
        h.until(|e| matches!(e, Event::Reconciled { thread, .. } if *thread == topic_key(f, t)))
            .await;
    }
    cache.barrier().await.unwrap();
    let rows = async |t: i32| cache.tail(topic_key(f, t), 50).await.unwrap().len();
    assert_eq!((rows(5).await, rows(6).await), (2, 2), "both cached");

    h.fake.drop_next_update();
    h.fake.delete_topic(f, tid(5)).await.unwrap();
    h.fake
        .inject(Op::History, Fault::Error(BackendError::Transient));
    h.fake.runner_exit();
    h.until(|e| matches!(e, Event::TopicRemoved { topic, .. } if *topic == tid(5)))
        .await;
    // Queued before this event (and the event comes after `TopicRemoved`,
    // unlike the rebuild's own invalidation): the barrier orders after it.
    h.until(|e| matches!(e, Event::ThreadInvalidated(k) if *k == topic_key(f, 5)))
        .await;
    cache.barrier().await.unwrap();
    assert_eq!(rows(5).await, 0, "the deleted topic's rows are gone");
    assert_eq!(rows(6).await, 2, "a sibling topic's rows stay");
    h.stop().await;
}
