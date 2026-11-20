//! Stage 2 task 17: opening a thread at its read position and returning to
//! the live tail (plan sections 17.6.4–17.6.6). Chat 1 holds 1..=200, so
//! its tail is 151..=200 and `Around(60)` is 51..=100.

use std::time::Duration;

use super::{CacheDir, Harness, cache_at, chat, harness, history_ids, open_settled};
use crate::api::{Command, ConnState, Event, HistoryDir, LoadTarget, UserError, WindowState};
use crate::backend::fake::{BuildFault, FakeBackend, FakeScript, Fault, Op, text_message};
use crate::backend::{BackendError, HistoryQuery, Update};
use crate::cache::{CacheHandle, CacheKey};
use crate::model::{Message, MsgId, ThreadKey, TopicId};
use crate::store::ViewGen;

fn whole() -> ThreadKey {
    ThreadKey::whole(chat(1))
}

fn open_at(after: i32) -> Command {
    Command::OpenThreadAt {
        thread: whole(),
        after: MsgId(after),
    }
}

/// The queries of chat 1's history calls since the first `skip`.
fn queries(h: &Harness, skip: usize) -> Vec<HistoryQuery> {
    h.fake
        .history_calls()
        .into_iter()
        .skip(skip)
        .filter(|(t, _)| *t == whole())
        .map(|(_, q)| q)
        .collect()
}

/// Waits for chat 1's next `History` in `dir`: its view, ids and `has_more`,
/// and the events before it.
async fn next_history(h: &mut Harness, dir: HistoryDir) -> (ViewGen, Vec<i32>, bool, Vec<Event>) {
    let (ev, skipped) = h
        .until(|e| matches!(e, Event::History { thread, dir: d, .. } if *thread == whole() && *d == dir))
        .await;
    let ids = history_ids(&ev);
    let Event::History { view, has_more, .. } = ev else {
        unreachable!()
    };
    (view, ids, has_more, skipped)
}

/// Waits for chat 1's next `Reconciled`: its view, and the events before it.
async fn next_reconciled(h: &mut Harness) -> (ViewGen, Vec<Event>) {
    let (ev, skipped) = h
        .until(|e| matches!(e, Event::Reconciled { thread, .. } if *thread == whole()))
        .await;
    let Event::Reconciled { view, .. } = ev else {
        unreachable!()
    };
    (view, skipped)
}

async fn rows(cache: &CacheHandle) -> Vec<Message> {
    cache.barrier().await.unwrap();
    cache.tail(whole(), 1000).await.unwrap()
}

/// Waits until chat 1 has had `n` history calls.
async fn calls_reached(h: &Harness, n: usize) {
    while queries(h, 0).len() < n {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A live message of chat 1 tagged with a forum topic it cannot have: it
/// bypasses the window, which loses cache continuity (task 7).
fn tagged(id: i32) -> Message {
    let mut m = text_message(chat(1), id, "tagged");
    m.thread.topic = TopicId::try_from(7).ok();
    m
}

/// Opens chat 1 at 60 (`Around`: 51..=100) and pages down to the tail
/// (101..=150, then 151..=200, which attaches the window). Returns the
/// view; the restoring tail follows.
async fn anchored_to_tail(h: &mut Harness) -> ViewGen {
    h.send(open_at(60)).await;
    let (view, ids, _, _) = next_history(h, HistoryDir::Around).await;
    assert_eq!(ids, (51..=100).collect::<Vec<_>>());
    for more in [true, false] {
        h.send(Command::LoadNewer(whole())).await;
        let (newer, _, has_more, _) = next_history(h, HistoryDir::Newer).await;
        assert_eq!((newer, has_more), (view, more));
    }
    view
}

/// A cached harness over chat 1, online.
async fn cached() -> (Harness, CacheHandle, CacheDir) {
    let dir = CacheDir::new();
    let cache = cache_at(dir.path(), &CacheKey::generate().unwrap());
    let mut h = harness(FakeScript::demo(1, 200), Some(cache.clone())).await;
    h.online().await;
    (h, cache, dir)
}

/// The retained, attached and contiguous window is never shown: only the
/// server tail fetched for the open is, and since it reaches the read
/// position it is applied as an open's tail, continuity included.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_tail_reaching_the_read_position_opens_the_thread() {
    let dir = CacheDir::new();
    let cache = cache_at(dir.path(), &CacheKey::generate().unwrap());
    let mut h = harness(FakeScript::demo(1, 200), Some(cache.clone())).await;
    open_settled(&mut h, &cache).await;
    next_reconciled(&mut h).await;
    let calls = queries(&h, 0).len();

    h.send(open_at(170)).await;
    let (view, skipped) = next_reconciled(&mut h).await;
    let shown: Vec<Vec<i32>> = skipped
        .iter()
        .filter(|e| matches!(e, Event::History { .. }))
        .map(history_ids)
        .collect();
    assert_eq!(shown, [(151..=200).collect::<Vec<_>>()], "no seed");
    assert_eq!(queries(&h, calls), [HistoryQuery::Tail]);

    let live = h.fake.incoming(whole(), "live").await.unwrap();
    let (newer, ids, _, _) = next_history(&mut h, HistoryDir::Newer).await;
    assert_eq!((newer, ids), (view, vec![live.id.0]));
    assert_eq!(rows(&cache).await.last(), Some(&live), "written through");
    h.stop().await;
    cache.close().await.unwrap();
}

/// A tail that doesn't reach the read position is dropped unapplied, and
/// the same open loads `Around` instead: a detached window. A light
/// refresh then leaves it alone.
#[tokio::test(start_paused = true)]
async fn a_tail_short_of_the_read_position_gives_way_to_around() {
    let mut h = harness(FakeScript::demo(1, 200), None).await;
    h.online().await;
    h.send(open_at(100)).await;
    let (_, ids, has_more, skipped) = next_history(&mut h, HistoryDir::Around).await;
    assert_eq!((ids, has_more), ((91..=140).collect(), true));
    assert!(
        !skipped
            .iter()
            .any(|e| matches!(e, Event::History { .. } | Event::Reconciled { .. })),
        "the tail was dropped unapplied: {skipped:?}"
    );
    assert_eq!(
        queries(&h, 0),
        [HistoryQuery::Tail, HistoryQuery::Around(MsgId(100))]
    );

    tokio::time::sleep(Duration::from_secs(16)).await;
    h.send(Command::Window(WindowState::Active)).await;
    h.until(|e| matches!(e, Event::DialogsReset(_))).await;
    calls_reached(&h, 3).await;
    h.send(Command::LoadNewer(whole())).await;
    let (_, ids, _, skipped) = next_history(&mut h, HistoryDir::Newer).await;
    assert_eq!(ids, (141..=190).collect::<Vec<_>>(), "still at 140");
    assert!(
        !skipped.iter().any(|e| matches!(
            e,
            Event::History { .. } | Event::Reconciled { .. } | Event::ThreadInvalidated(_)
        )),
        "the refresh left the window alone: {skipped:?}"
    );
    h.stop().await;
}

/// An `Around` page with nothing above the read position (the unread
/// messages were deleted meanwhile) is dropped for the tail, applied
/// whatever it covers.
#[tokio::test(start_paused = true)]
async fn around_with_nothing_above_falls_back_to_the_tail() {
    let mut h = harness(FakeScript::demo(1, 200), None).await;
    h.online().await;
    h.fake.inject(Op::History, Fault::Delay(Duration::ZERO));
    h.fake
        .inject(Op::History, Fault::Delay(Duration::from_secs(1)));
    h.send(open_at(100)).await;
    calls_reached(&h, 2).await;
    let mut history = h.fake.server_history(chat(1));
    history.retain(|m| m.id <= MsgId(100));
    h.fake.set_history(chat(1), history);

    let (_, ids, _, skipped) = next_history(&mut h, HistoryDir::Tail).await;
    assert_eq!(ids, (51..=100).collect::<Vec<_>>());
    assert!(
        !skipped.iter().any(|e| matches!(e, Event::History { .. })),
        "no Around: {skipped:?}"
    );
    assert_eq!(
        queries(&h, 0),
        [
            HistoryQuery::Tail,
            HistoryQuery::Around(MsgId(100)),
            HistoryQuery::Tail
        ]
    );
    h.stop().await;
}

/// The anchored open is best effort: a failed load ends in `LoadEnded` in
/// its direction, and offline it ends at once, so the UI can fall back to
/// `OpenThread`. A negative read position is refused.
#[tokio::test(start_paused = true)]
async fn an_anchored_open_that_fails_ends_its_load() {
    let ended = |dir| {
        move |e: &Event| {
            matches!(e, Event::LoadEnded {
                target: LoadTarget::History { thread, dir: d },
                error: Some(_),
            } if *thread == whole() && *d == dir)
        }
    };
    let (backend, fake) = FakeBackend::new(FakeScript::demo(1, 200));
    fake.on_next_build(BuildFault::Fail(BackendError::Transient));
    let mut h = super::harness_with(backend, fake, None).await;
    h.until(|e| matches!(e, Event::Connection(ConnState::Offline)))
        .await;
    h.send(open_at(60)).await;
    let (ev, _) = h.until(ended(HistoryDir::Tail)).await;
    assert!(matches!(
        ev,
        Event::LoadEnded {
            error: Some(UserError::Network),
            ..
        }
    ));
    assert!(queries(&h, 0).is_empty(), "offline: nothing asked");

    h.online().await;
    h.fake
        .inject(Op::History, Fault::Error(BackendError::Transient));
    h.send(open_at(60)).await;
    h.until(ended(HistoryDir::Tail)).await;
    h.fake.inject(Op::History, Fault::Delay(Duration::ZERO));
    h.fake
        .inject(Op::History, Fault::Error(BackendError::Transient));
    h.send(open_at(60)).await;
    h.until(ended(HistoryDir::Around)).await;

    h.send(open_at(-1)).await;
    h.until(|e| matches!(e, Event::Error(UserError::Rejected)))
        .await;
    h.stop().await;
}

/// An anchored window's pages (`Around`, older, newer) and the live
/// messages it misses leave the disk cache as it was. Reaching the tail
/// asks for one restoring tail, which keeps the view, writes the tail
/// through and restores continuity: the next live message is cached.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_anchored_window_reaches_the_cache_only_through_its_restoring_tail() {
    let (mut h, cache, _dir) = cached().await;
    h.send(Command::OpenThread(whole())).await;
    next_reconciled(&mut h).await;
    let before = rows(&cache).await;
    assert_eq!(before.first().map(|m| m.id), Some(MsgId(151)));

    h.send(open_at(60)).await;
    let (view, _, _, _) = next_history(&mut h, HistoryDir::Around).await;
    h.send(Command::LoadOlder(whole())).await;
    next_history(&mut h, HistoryDir::Older).await;
    let missed = h.fake.incoming(whole(), "missed").await.unwrap();
    h.until(|e| matches!(e, Event::ChatBumped(s) if s.last_id == Some(missed.id)))
        .await;
    for _ in 0..2 {
        h.send(Command::LoadNewer(whole())).await;
        let (_, _, has_more, _) = next_history(&mut h, HistoryDir::Newer).await;
        assert!(has_more);
    }
    assert_eq!(rows(&cache).await, before, "the cache is untouched");

    let calls = queries(&h, 0).len();
    h.send(Command::LoadNewer(whole())).await;
    let (_, ids, has_more, _) = next_history(&mut h, HistoryDir::Newer).await;
    assert_eq!((ids, has_more), (vec![missed.id.0], false));
    let (reconciled, skipped) = next_reconciled(&mut h).await;
    assert_eq!(reconciled, view, "the view is kept");
    assert!(!skipped.iter().any(|e| matches!(e, Event::History { .. })));
    assert_eq!(
        queries(&h, calls),
        [HistoryQuery::After(MsgId(200)), HistoryQuery::Tail],
        "one restoring tail"
    );
    let live = h.fake.incoming(whole(), "live").await.unwrap();
    next_history(&mut h, HistoryDir::Newer).await;
    let after = rows(&cache).await;
    assert_eq!(after.first().map(|m| m.id), Some(MsgId(151)));
    assert_eq!(after.last(), Some(&live), "continuity restored");
    h.stop().await;
    cache.close().await.unwrap();
}

/// How the restoring tail fails to restore continuity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Spoiled {
    /// A live message bypassed the window after it was requested.
    Bypassed,
    /// The call failed.
    Failed,
}

/// The restoring tail doesn't restore continuity (it was requested before
/// the window lost it, or it failed): live messages stay uncached. A later
/// light refresh's tail, requested with its own mark, restores it in the
/// same view, and live messages are written through again.
async fn a_spoiled_restore_is_recovered_by_a_light_refresh(spoiled: Spoiled) {
    let (mut h, cache, _dir) = cached().await;
    // The restoring tail is the fifth history call.
    let release = h.fake.hold(Op::History, 5);
    if spoiled == Spoiled::Failed {
        for _ in 0..4 {
            h.fake.inject(Op::History, Fault::Delay(Duration::ZERO));
        }
        h.fake
            .inject(Op::History, Fault::Error(BackendError::Transient));
    }
    let view = anchored_to_tail(&mut h).await;
    calls_reached(&h, 5).await;
    if spoiled == Spoiled::Bypassed {
        h.fake
            .push_update(Update::NewMessage(tagged(300).into()))
            .await;
        h.until(|e| matches!(e, Event::ChatBumped(s) if s.last_id == Some(MsgId(300))))
            .await;
    }
    release.notify_one();
    match spoiled {
        Spoiled::Bypassed => assert_eq!(next_reconciled(&mut h).await.0, view),
        Spoiled::Failed => {
            h.until(|e| matches!(e, Event::Error(UserError::Network)))
                .await;
        }
    }
    h.fake.incoming(whole(), "uncached").await.unwrap();
    next_history(&mut h, HistoryDir::Newer).await;
    assert!(rows(&cache).await.is_empty(), "continuity stayed lost");

    // Activation past the minimum gap (real time: the cache is involved).
    tokio::time::sleep(Duration::from_secs(16)).await;
    h.send(Command::Window(WindowState::Active)).await;
    assert_eq!(next_reconciled(&mut h).await.0, view, "the view is kept");
    let live = h.fake.incoming(whole(), "live").await.unwrap();
    next_history(&mut h, HistoryDir::Newer).await;
    assert_eq!(
        rows(&cache).await.last(),
        Some(&live),
        "continuity restored"
    );
    h.stop().await;
    cache.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restoring_tail_from_before_a_loss_of_continuity_is_recovered() {
    a_spoiled_restore_is_recovered_by_a_light_refresh(Spoiled::Bypassed).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_restoring_tail_is_recovered() {
    a_spoiled_restore_is_recovered_by_a_light_refresh(Spoiled::Failed).await;
}

/// A restoring tail that finds a message missing inside the window (a
/// gap) replaces the view, as a tail does, and restores continuity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restoring_tail_with_a_gap_replaces_the_view() {
    let (mut h, cache, _dir) = cached().await;
    let mut history = h.fake.server_history(chat(1));
    let hidden = history.remove(179);
    assert_eq!(hidden.id, MsgId(180));
    h.fake.set_history(chat(1), history.clone());
    let release = h.fake.hold(Op::History, 5);
    let view = anchored_to_tail(&mut h).await;
    calls_reached(&h, 5).await;
    history.insert(179, hidden);
    h.fake.set_history(chat(1), history);
    release.notify_one();

    let (tail, ids, _, _) = next_history(&mut h, HistoryDir::Tail).await;
    assert_ne!(tail, view, "a new view");
    assert!(ids.contains(&180));
    assert_eq!(next_reconciled(&mut h).await.0, tail);
    assert_eq!(rows(&cache).await.last().map(|m| m.id), Some(MsgId(200)));
    h.stop().await;
    cache.close().await.unwrap();
}

/// A user load asked while the restoring tail runs waits for it (the
/// thread's single-flight slot) and then runs against the same view.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_load_waits_for_the_restoring_tail() {
    let (mut h, cache, _dir) = cached().await;
    let release = h.fake.hold(Op::History, 5);
    let view = anchored_to_tail(&mut h).await;
    calls_reached(&h, 5).await;
    h.send(Command::LoadOlder(whole())).await;
    // Answered in order: the load above was taken while the tail is held.
    h.send(open_at(-1)).await;
    h.until(|e| matches!(e, Event::Error(UserError::Rejected)))
        .await;
    release.notify_one();

    let (older, ids, _, skipped) = next_history(&mut h, HistoryDir::Older).await;
    assert_eq!(older, view);
    assert_eq!(ids, (1..=50).collect::<Vec<_>>());
    assert!(
        skipped
            .iter()
            .any(|e| matches!(e, Event::Reconciled { view: v, .. } if *v == view)),
        "the restoring tail landed first: {skipped:?}"
    );
    assert_eq!(
        queries(&h, 4),
        [HistoryQuery::Tail, HistoryQuery::Before(MsgId(51))]
    );
    h.stop().await;
    cache.close().await.unwrap();
}
