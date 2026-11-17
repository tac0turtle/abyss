//! The fence gap (plan sections 6.6 and 11, "Cache continuity"), and task
//! 7's dependency: nothing but a tail reload restores continuity after it
//! is lost, so the fence's lift must reload the open thread and continuity
//! must be back afterwards.

use std::time::Duration;

use super::super::{CacheDir, cache_at, harness};
use super::{forum_with, history_of, ids, reconciled, topic_key};
use crate::api::{Command, Event, HistoryDir};
use crate::backend::Update;
use crate::backend::fake::{Fault, Op};
use crate::cache::{CacheHandle, CacheKey};
use crate::model::{Message, MsgId, ThreadKey};

async fn rows(cache: &CacheHandle, thread: ThreadKey) -> Vec<Message> {
    cache.barrier().await.unwrap();
    let rows = cache.tail(thread, 1000).await.unwrap();
    assert!(rows.iter().all(|m| m.thread == thread), "{rows:?}");
    rows
}

/// Topic A's cache holds its even posts 100..=198 and its window is
/// contiguous. The forum is fenced; live 200..=259 are routed `ChatOnly`
/// and not cached; the check returns the *unchanged* kind; live 260 arrives
/// after the lift, before the reloaded tail. None of it is written as
/// contiguous with 198: the lift invalidated A and reloaded its tail, and
/// only that tail's reconcile (no overlap with 198, so a replace) restored
/// continuity, after which live messages are cached again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fence_gap_is_not_bridged_and_the_lift_restores_continuity() {
    let dir = CacheDir::new();
    let cache = cache_at(dir.path(), &CacheKey::generate().unwrap());
    let (script, f) = forum_with((4..=199).map(|id| (id, if id % 2 == 0 { 2 } else { 3 })));
    let a = topic_key(f, 2);
    let mut h = harness(script, Some(cache.clone())).await;
    h.online().await;
    h.send(Command::OpenThread(a)).await;
    reconciled(&mut h, a).await;
    let cached: Vec<i32> = (100..=198).filter(|id| id % 2 == 0).collect();
    assert_eq!(ids(&rows(&cache, a).await), cached);

    h.fake
        .inject(Op::PeerDialogs, Fault::Delay(Duration::from_secs(2)));
    h.fake.push_update(Update::ChatChanged { chat: f }).await;
    h.until(|e| matches!(e, Event::ThreadInvalidated(t) if *t == a))
        .await;
    for _ in 0..60 {
        h.fake.incoming(a, "while fenced").await.unwrap();
    }
    h.until(|e| matches!(e, Event::ChatBumped(s) if s.last_id == Some(MsgId(259))))
        .await;
    assert_eq!(
        ids(&rows(&cache, a).await),
        cached,
        "nothing cached while fenced"
    );

    // The lift: the tail reload is held back so a live message lands first.
    h.fake.lead(Op::History, Duration::from_secs(1));
    h.until(|e| matches!(e, Event::ThreadInvalidated(t) if *t == a))
        .await;
    h.fake.incoming(a, "after the lift").await.unwrap();
    reconciled(&mut h, a).await;
    let after = ids(&rows(&cache, a).await);
    assert!(
        after.iter().all(|id| *id >= 200),
        "the gap is never bridged: {after:?}"
    );
    let first = *after.first().unwrap();
    assert_eq!(
        after,
        (first..=260).collect::<Vec<_>>(),
        "a replaced, gap-free run"
    );

    // Continuity is back: a live message is written through again.
    let next = h.fake.incoming(a, "contiguous again").await.unwrap();
    history_of(&mut h, a, HistoryDir::Newer).await;
    assert_eq!(rows(&cache, a).await.last(), Some(&next));
    h.stop().await;
    cache.close().await.unwrap();
}
