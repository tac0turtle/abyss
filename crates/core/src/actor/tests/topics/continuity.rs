//! Cache continuity per thread (plan sections 6.2, 10 and 11, "Cache
//! continuity"): live messages are written only through a contiguous
//! window of their own thread, a thread's tail reconcile decides overlap
//! against that thread's cached tail and replaces only its rows, and a gap
//! is never bridged. (The fence gap is task 8's.)

use super::super::{CacheDir, Harness, cache_at, harness};
use super::{forum_with, history_of, ids, reconciled, topic_key};
use crate::api::{Command, Event, HistoryDir};
use crate::backend::fake::{FakeScript, Op};
use crate::cache::{CacheHandle, CacheKey};
use crate::model::{ChatId, Message, MsgId, ThreadKey};

/// Posts 4..=199: topic A (2) the even ids, topic B (3) the odd ones, 99
/// each beside its root. Each thread's tail is 50 of them.
fn script() -> (FakeScript, ChatId) {
    forum_with((4..=199).map(|id| (id, if id % 2 == 0 { 2 } else { 3 })))
}

fn range(lo: i32, hi: i32, parity: i32) -> Vec<i32> {
    (lo..=hi).filter(|id| id % 2 == parity).collect()
}

async fn rows(cache: &CacheHandle, thread: ThreadKey) -> Vec<Message> {
    cache.barrier().await.unwrap();
    let rows = cache.tail(thread, 1000).await.unwrap();
    assert!(rows.iter().all(|m| m.thread == thread), "{rows:?}");
    rows
}

/// Opens `thread` and waits until its server tail is reconciled.
async fn open(h: &mut Harness, thread: ThreadKey) {
    h.send(Command::OpenThread(thread)).await;
    reconciled(h, thread).await;
}

struct Setup {
    h: Harness,
    cache: CacheHandle,
    _dir: CacheDir,
    a: ThreadKey,
    b: ThreadKey,
}

/// Online with topics B then A opened, so both tails are cached and A is
/// the open, contiguous thread.
async fn both_cached() -> Setup {
    let dir = CacheDir::new();
    let cache = cache_at(dir.path(), &CacheKey::generate().unwrap());
    let (script, f) = script();
    let (a, b) = (topic_key(f, 2), topic_key(f, 3));
    let mut h = harness(script, Some(cache.clone())).await;
    h.online().await;
    open(&mut h, b).await;
    open(&mut h, a).await;
    assert_eq!(ids(&rows(&cache, a).await), range(100, 198, 0));
    assert_eq!(ids(&rows(&cache, b).await), range(101, 199, 1));
    Setup {
        h,
        cache,
        _dir: dir,
        a,
        b,
    }
}

/// The closed-thread gap. A's cache ends at 198; A is closed, and 100 live
/// messages (200..=299) arrive. None is cached: A's window is no longer
/// contiguous. On reopen the server tail (250..=299) doesn't overlap A's
/// cached tail, so A's rows are replaced (B's kept), and the older page
/// below it comes from the server, not from the cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_topic_thread_leaves_no_bridged_gap() {
    let Setup {
        mut h,
        cache,
        _dir,
        a,
        b,
    } = both_cached().await;
    let before_b = rows(&cache, b).await;
    h.send(Command::CloseThread(a)).await;
    // Commands and updates take separate channels: B's reconciled tail
    // proves the close was handled before the live messages below.
    open(&mut h, b).await;
    for _ in 0..100 {
        h.fake.incoming(a, "while closed").await.unwrap();
    }
    h.until(|e| matches!(e, Event::ChatBumped(s) if s.last_id == Some(MsgId(299))))
        .await;
    assert_eq!(
        ids(&rows(&cache, a).await),
        range(100, 198, 0),
        "nothing live was cached"
    );

    open(&mut h, a).await;
    assert_eq!(
        ids(&rows(&cache, a).await),
        (250..=299).collect::<Vec<_>>(),
        "A's rows replaced by the tail"
    );
    assert_eq!(rows(&cache, b).await, before_b, "B's rows kept");
    let calls = h.fake.calls(Op::History);
    h.send(Command::LoadOlder(a)).await;
    let (older, _) = history_of(&mut h, a, HistoryDir::Older).await;
    assert_eq!(ids(&older), (200..=249).collect::<Vec<_>>());
    assert!(h.fake.calls(Op::History) > calls, "from the server");
    h.stop().await;
    cache.close().await.unwrap();
}

/// A tail that overlaps A's cached tail reconciles only A's ids in its
/// range: a message deleted on the server leaves A's cache, and B's rows,
/// whose ids interleave with A's, are untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconciling_a_topic_keeps_its_siblings_rows() {
    let Setup {
        mut h,
        cache,
        _dir,
        a,
        b,
    } = both_cached().await;
    let before_b = rows(&cache, b).await;
    let f = a.chat;
    let mut history = h.fake.server_history(f);
    history.retain(|m| m.id != MsgId(150));
    h.fake.set_history(f, history);
    open(&mut h, a).await;
    let expected: Vec<i32> = range(98, 198, 0)
        .into_iter()
        .filter(|id| *id != 150)
        .collect();
    assert_eq!(ids(&rows(&cache, a).await), expected, "150 reconciled away");
    assert_eq!(rows(&cache, b).await, before_b, "B's rows kept");
    h.stop().await;
    cache.close().await.unwrap();
}

/// A live message for a thread with no window is not cached; one for the
/// open, contiguous thread is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_messages_into_a_thread_without_a_window_are_not_cached() {
    let dir = CacheDir::new();
    let cache = cache_at(dir.path(), &CacheKey::generate().unwrap());
    let (script, f) = script();
    let (a, b) = (topic_key(f, 2), topic_key(f, 3));
    let mut h = harness(script, Some(cache.clone())).await;
    h.online().await;
    open(&mut h, a).await;
    let to_b = h.fake.incoming(b, "no window").await.unwrap();
    let to_a = h.fake.incoming(a, "open").await.unwrap();
    history_of(&mut h, a, HistoryDir::Newer).await;
    assert!(rows(&cache, b).await.is_empty(), "{to_b:?} not cached");
    assert_eq!(rows(&cache, a).await.last(), Some(&to_a));
    h.stop().await;
    cache.close().await.unwrap();
}
