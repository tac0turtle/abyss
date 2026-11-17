//! A topic thread (plan section 11, "A topic thread"): its tail from the
//! cache then the server, older and newer paging, `Reconciled`, and
//! invalidation on rebuild. Every message it shows carries its key: the
//! server's raw topic (`None` for General) is normalized by `route`.

use super::super::{CacheDir, Internal, cache_at};
use super::{
    add_big_forum, assert_keyed, forum, forum_with, history_of, ids, message_in, online,
    reconciled, topic_key,
};
use crate::api::{Command, Event, HistoryDir, UserError};
use crate::backend::Update;
use crate::backend::fake::{FakeScript, Op};
use crate::cache::CacheKey;
use crate::model::{Body, ThreadKey};
use crate::rich_text::RichText;

/// Posts 4..=203, topic A (2) the even ids and topic B (3) the odd ones.
fn alternating() -> impl Iterator<Item = (i32, i32)> {
    (4..=203).map(|id| (id, if id % 2 == 0 { 2 } else { 3 }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_topic_thread_shows_its_cached_tail_then_the_servers() {
    let dir = CacheDir::new();
    let key = CacheKey::generate().unwrap();
    let (script, f) = forum_with(alternating());
    let a = topic_key(f, 2);
    let evens = |lo: i32, hi: i32| (lo..=hi).filter(|id| id % 2 == 0).collect::<Vec<_>>();

    // First run: the server tail and one older page are cached.
    let cache = cache_at(dir.path(), &key);
    let mut h = super::super::harness(script.clone(), Some(cache.clone())).await;
    h.online().await;
    h.send(Command::OpenThread(a)).await;
    let (tail, _) = history_of(&mut h, a, HistoryDir::Tail).await;
    assert_eq!(ids(&tail), evens(104, 202));
    reconciled(&mut h, a).await;
    h.send(Command::LoadOlder(a)).await;
    let (older, _) = history_of(&mut h, a, HistoryDir::Older).await;
    assert_eq!(ids(&older), evens(4, 102));
    h.stop().await;
    cache.barrier().await.unwrap();
    let rows = cache.tail(a, 500).await.unwrap();
    assert_eq!(ids(&rows), evens(4, 202), "only topic A's rows, under A");
    assert!(rows.iter().all(|m| m.thread == a));
    assert!(cache.tail(topic_key(f, 3), 500).await.unwrap().is_empty());
    assert!(
        cache
            .tail(ThreadKey::whole(f), 500)
            .await
            .unwrap()
            .is_empty()
    );
    cache.close().await.unwrap();

    // Second run: the cached tail first, then the server's, reconciled; an
    // older page comes from the (now contiguous) cache.
    let cache = cache_at(dir.path(), &key);
    let mut h = super::super::harness(script, Some(cache.clone())).await;
    h.online().await;
    h.send(Command::OpenThread(a)).await;
    let (first, _) = history_of(&mut h, a, HistoryDir::Tail).await;
    assert_eq!(ids(&first), evens(104, 202), "from the cache");
    let skipped = reconciled(&mut h, a).await;
    assert!(
        skipped.iter().any(
            |e| matches!(e, Event::History { thread, dir: HistoryDir::Tail, .. } if *thread == a)
        ),
        "then the server's tail"
    );
    let calls = h.fake.calls(Op::History);
    h.send(Command::LoadOlder(a)).await;
    let (older, _) = history_of(&mut h, a, HistoryDir::Older).await;
    assert_eq!(ids(&older), evens(4, 102));
    assert_eq!(h.fake.calls(Op::History), calls, "served by the cache");
    h.stop().await;
    cache.close().await.unwrap();
}

/// Older pages past the window's cap detach it from the live tail; newer
/// pages then come back down. Topic B's interleaved ids never show.
#[tokio::test(start_paused = true)]
async fn a_topic_thread_pages_older_and_newer() {
    let (script, f) = forum_with((4..=2400).map(|id| (id, if id % 10 == 0 { 3 } else { 2 })));
    let a = topic_key(f, 2);
    let mut h = online(script).await;
    h.send(Command::OpenThread(a)).await;
    let (tail, _) = history_of(&mut h, a, HistoryDir::Tail).await;
    assert!(tail.iter().all(|m| m.id.0 % 10 != 0));
    let mut detached = false;
    for _ in 0..60 {
        h.send(Command::LoadOlder(a)).await;
        let (ev, _) = h
            .until(|e| matches!(e, Event::History { thread, dir: HistoryDir::Older, .. } if *thread == a))
            .await;
        assert_keyed(std::slice::from_ref(&ev));
        if matches!(&ev, Event::History { removed, .. } if !removed.is_empty()) {
            detached = true;
            break;
        }
    }
    assert!(detached, "the window slid off the live tail");
    h.send(Command::LoadNewer(a)).await;
    let (newer, _) = history_of(&mut h, a, HistoryDir::Newer).await;
    assert!(!newer.is_empty());
    assert!(newer.iter().all(|m| m.id.0 % 10 != 0), "only topic A");
    h.stop().await;
}

/// General's messages have no topic header; they open, load and arrive
/// live under `TopicId::GENERAL`, and an edit is normalized too.
#[tokio::test(start_paused = true)]
async fn general_is_a_thread_of_its_own() {
    let (script, f) = forum_with([(4, 1), (5, 2), (6, 1)]);
    let general = topic_key(f, 1);
    let mut h = online(script).await;
    h.send(Command::OpenThread(general)).await;
    let (tail, _) = history_of(&mut h, general, HistoryDir::Tail).await;
    assert_eq!(ids(&tail), [1, 4, 6], "General's root and posts");
    reconciled(&mut h, general).await;

    let posted = h.fake.incoming(general, "live").await.unwrap();
    assert_eq!(posted.thread.topic, None, "the server states no topic");
    let (live, _) = history_of(&mut h, general, HistoryDir::Newer).await;
    assert_eq!(ids(&live), [posted.id.0]);

    let mut edit = message_in(general, 6, "edited");
    edit.edit_date = Some(edit.date + 1);
    edit.body = Body::Text(RichText::plain("edited"));
    h.fake.push_update(Update::Edited(edit)).await;
    let (ev, _) = h.until(|e| matches!(e, Event::Edited(_))).await;
    assert!(matches!(&ev, Event::Edited(m) if m.thread == general && m.id.0 == 6));
    h.stop().await;
}

/// Live messages reach only their own topic's window.
#[tokio::test(start_paused = true)]
async fn live_messages_reach_their_topic_only() {
    let (script, f) = forum_with(alternating());
    let (a, b) = (topic_key(f, 2), topic_key(f, 3));
    let mut h = online(script).await;
    h.send(Command::OpenThread(a)).await;
    reconciled(&mut h, a).await;
    let into_b = h.fake.incoming(b, "to B").await.unwrap();
    let into_a = h.fake.incoming(a, "to A").await.unwrap();
    let (live, skipped) = history_of(&mut h, a, HistoryDir::Newer).await;
    assert_eq!(ids(&live), [into_a.id.0]);
    assert!(
        !skipped.iter().any(|e| matches!(e, Event::History { .. })),
        "{into_b:?} went nowhere: {skipped:?}"
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_rebuild_invalidates_a_topic_thread() {
    let (script, f) = forum_with(alternating());
    let a = topic_key(f, 2);
    let mut h = online(script).await;
    h.send(Command::OpenThread(a)).await;
    reconciled(&mut h, a).await;
    h.internal.send(Internal::Wake).await.unwrap();
    let skipped = reconciled(&mut h, a).await;
    let invalidated = skipped
        .iter()
        .position(|e| matches!(e, Event::ThreadInvalidated(t) if *t == a))
        .expect("invalidated");
    assert!(
        skipped.iter().skip(invalidated).any(
            |e| matches!(e, Event::History { thread, dir: HistoryDir::Tail, messages, .. } if *thread == a && !messages.is_empty())
        ),
        "a new tail after the invalidation: {skipped:?}"
    );
    h.stop().await;
}

/// A forum has no whole-chat thread: `OpenThread` with its whole-chat key
/// is refused (`Error(Rejected)`, plan section 4) and changes nothing, so
/// the open topic stays open and keeps its live messages.
#[tokio::test(start_paused = true)]
async fn a_forums_whole_chat_key_is_refused() {
    let f = forum(100);
    let mut script = FakeScript::demo(1, 1);
    add_big_forum(&mut script, f, 3);
    let (whole, a) = (ThreadKey::whole(f), topic_key(f, 2));
    let mut h = online(script).await;
    h.send(Command::OpenThread(a)).await;
    reconciled(&mut h, a).await;
    h.send(Command::OpenThread(whole)).await;
    let (_, skipped) = h
        .until(|e| matches!(e, Event::Error(UserError::Rejected)))
        .await;
    assert!(
        !skipped
            .iter()
            .any(|e| matches!(e, Event::ThreadInvalidated(_))),
        "{skipped:?}"
    );
    let posted = h.fake.incoming(a, "still open").await.unwrap();
    let (live, skipped) = history_of(&mut h, a, HistoryDir::Newer).await;
    assert_eq!(ids(&live), [posted.id.0]);
    assert!(
        !skipped
            .iter()
            .any(|e| matches!(e, Event::History { thread, .. } if *thread == whole)),
        "{skipped:?}"
    );
    h.stop().await;
}
