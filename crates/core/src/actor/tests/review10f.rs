//! Regression tests for the round 18 re-check of task 10e (plan section 15),
//! the core half: an older load that keeps being cut off from the window
//! starts over a bounded number of times, and only the load the UI waits on
//! (of the view it shows) acknowledges its request. Each fails on the 10e
//! code (or with the fix disabled).

use std::time::Duration;

use super::{CacheDir, Harness, cache_at, chat, harness, history_ids, open_settled, session};
use crate::actor::loads::MAX_OLDER_RESTARTS;
use crate::api::{Command, Event, HistoryDir, LoadTarget, SendState, UserError, WindowState};
use crate::backend::fake::{FakeScript, Fault, Op, text_message};
use crate::backend::{BackendError, Update};
use crate::cache::CacheKey;
use crate::model::{ChatId, LocalId, Message, MsgId, ThreadKey};

/// Older-page requests one user load makes before it gives up.
fn requests() -> i32 {
    i32::from(MAX_OLDER_RESTARTS) + 1
}

fn reconciled_in(c: ChatId) -> impl Fn(&Event) -> bool {
    move |e| matches!(e, Event::Reconciled { thread, .. } if *thread == ThreadKey::whole(c))
}

fn older(e: &Event) -> bool {
    matches!(e, Event::History { thread: c, dir: HistoryDir::Older, .. } if *c == ThreadKey::whole(chat(1)))
}

fn ended(e: &Event) -> bool {
    let target = LoadTarget::History {
        thread: ThreadKey::whole(chat(1)),
        dir: HistoryDir::Older,
    };
    matches!(e, Event::LoadEnded { target: t, .. } if *t == target)
}

fn tail_of(c: ChatId) -> impl Fn(&Event) -> bool {
    move |e| matches!(e, Event::History { thread, dir: HistoryDir::Tail, .. } if *thread == ThreadKey::whole(c))
}

async fn open(h: &mut Harness) {
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h.until(reconciled_in(chat(1))).await;
}

/// Deletes message `id` of chat 1 (the window's oldest), waiting until
/// core applied it.
async fn delete(h: &mut Harness, id: i32) {
    h.fake
        .push_update(Update::Deleted {
            chat: Some(chat(1)),
            ids: vec![MsgId(id)],
        })
        .await;
    h.until(|e| matches!(e, Event::Deleted { .. })).await;
}

/// Switches to chat 2 and back to chat 1: a new view of chat 1, with its
/// tail applied.
async fn reopen(h: &mut Harness) {
    h.send(Command::OpenThread(ThreadKey::whole(chat(2)))).await;
    h.until(tail_of(chat(2))).await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h.until(reconciled_in(chat(1))).await;
}

/// Finding 1: the window's oldest end moves during every request (here by
/// deleting it; live messages trimming a full window do the same), so
/// every page is dropped. The load used to start over forever, with
/// nothing reaching the UI. Now it starts over `MAX_OLDER_RESTARTS` times
/// and then ends with `Busy`, so the UI's bounded backoff takes over.
#[tokio::test(start_paused = true)]
async fn an_older_load_cut_off_every_time_ends_with_an_error() {
    let mut h = harness(FakeScript::demo(1, 100), None).await;
    open(&mut h).await;
    let before = h.fake.calls(Op::History);
    for _ in 0..=requests() {
        h.fake
            .inject(Op::History, Fault::Delay(Duration::from_secs(25)));
    }
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    // Request k runs from about 25k s; its edge (51 + k) goes at 25k + 1.
    for front in 51..51 + requests() {
        tokio::time::sleep(Duration::from_secs(1)).await;
        delete(&mut h, front).await;
        tokio::time::sleep(Duration::from_secs(25)).await;
    }
    let (ev, skipped) = h.until(|e| ended(e) || older(e)).await;
    assert!(
        matches!(
            ev,
            Event::LoadEnded {
                error: Some(UserError::Busy),
                ..
            }
        ),
        "{ev:?}"
    );
    assert!(!skipped.iter().any(older), "every page was cut off");
    let made = h.fake.calls(Op::History) - before;
    assert_eq!(made, usize::try_from(requests()).unwrap(), "bounded");
    // The next request of the UI has a budget of its own.
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    let (ev, skipped) = h.until(|e| ended(e) || older(e)).await;
    assert!(older(&ev), "{ev:?}");
    assert!(!skipped.iter().any(ended));
    h.stop().await;
}

/// Finding 1, the cache-served branch: an older page read from the cache
/// is checked against the window's edge too, and its restarts share the
/// same budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cached_older_page_cut_off_every_time_ends_with_an_error() {
    let dir = CacheDir::new();
    let key = CacheKey::generate().unwrap();
    session(FakeScript::demo(1, 200), cache_at(dir.path(), &key), 2).await;
    let mut cache = cache_at(dir.path(), &key);
    let gate = cache.gate_older_reads();
    let mut h = harness(FakeScript::demo(1, 200), Some(cache.clone())).await;
    open_settled(&mut h, &cache).await;
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    for (k, front) in (151..151 + requests()).enumerate() {
        assert!(
            wait_for(|| gate.entered() > k).await,
            "read {k} reached the gate"
        );
        delete(&mut h, front).await;
        gate.release();
    }
    let answer =
        tokio::time::timeout(Duration::from_secs(10), h.until(|e| ended(e) || older(e))).await;
    let (ev, skipped) = answer.expect("the load ends instead of reading again");
    assert!(
        matches!(
            ev,
            Event::LoadEnded {
                error: Some(UserError::Busy),
                ..
            }
        ),
        "{ev:?}"
    );
    assert!(!skipped.iter().any(older));
    assert_eq!(gate.entered(), usize::try_from(requests()).unwrap());
    h.stop().await;
}

/// Polls `done` in real time for up to 10 s; whether it came true.
async fn wait_for(done: impl Fn() -> bool) -> bool {
    for _ in 0..1_000 {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// Finding 2: chat 1 has a slow `LoadOlder` that will fail. The user
/// switches away and back, the new tail arrives, and they ask for older
/// history again. The old load's failure used to end the new view's
/// pending load (`LoadEnded` names no view), so the UI asked once more.
#[tokio::test(start_paused = true)]
async fn an_old_views_failure_does_not_end_the_new_views_load() {
    let mut h = harness(FakeScript::demo(2, 200), None).await;
    open(&mut h).await;
    h.fake.lead(Op::History, Duration::from_secs(10));
    h.fake
        .inject(Op::History, Fault::Error(BackendError::Transient));
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    old_view_ends_quietly(h).await;
}

/// Finding 2, the same with a panicking old load.
#[tokio::test(start_paused = true)]
async fn an_old_views_panic_does_not_end_the_new_views_load() {
    let mut h = harness(FakeScript::demo(2, 200), None).await;
    open(&mut h).await;
    h.fake.lead(Op::History, Duration::from_secs(10));
    h.fake.inject(Op::History, Fault::Panic);
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    old_view_ends_quietly(h).await;
}

/// With chat 1's older load out (ending at 10 s), reopens chat 1, asks
/// for older history in the new view (answered at 20 s) and checks that
/// the old load's end acknowledges nothing.
async fn old_view_ends_quietly(mut h: Harness) {
    // Both tails at once; the new view's older page slowly.
    h.fake.inject(Op::History, Fault::Delay(Duration::ZERO));
    h.fake.inject(Op::History, Fault::Delay(Duration::ZERO));
    h.fake
        .inject(Op::History, Fault::Delay(Duration::from_secs(20)));
    reopen(&mut h).await;
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    let (ev, skipped) = h.until(|e| ended(e) || older(e)).await;
    assert!(older(&ev), "{ev:?}");
    assert_eq!(history_ids(&ev), (101..=150).collect::<Vec<_>>());
    assert!(!skipped.iter().any(ended), "{skipped:?}");
    tokio::time::sleep(Duration::from_secs(60)).await;
    let rest = h.drain();
    assert!(!rest.iter().any(|e| ended(e) || older(e)), "{rest:?}");
    h.stop().await;
}

fn history(ids: std::ops::RangeInclusive<i32>) -> Vec<Message> {
    ids.map(|i| text_message(chat(1), i, "m")).collect()
}

/// Finding 2, a stale success: a covering fetch absorbed the user's
/// `LoadOlder`, and the user reopened the chat before its page came. The
/// page is stale and dropped, and what the fetch owed the old view goes
/// with that view (checked by the actor's invariant on every turn, which
/// fails with the cleanup disabled). The new view's load is answered once.
#[tokio::test(start_paused = true)]
async fn a_stale_absorbed_fetch_leaves_nothing_behind() {
    let mut h = harness(FakeScript::demo(2, 10), None).await;
    open(&mut h).await;
    h.fake
        .inject(Op::Send, Fault::Error(BackendError::SentUnconfirmed));
    h.send(Command::SendText {
        thread: ThreadKey::whole(chat(1)),
        text: "hi".into(),
        local_id: LocalId(1),
    })
    .await;
    h.until(|e| {
        matches!(
            e,
            Event::SendState {
                state: SendState::SentUnconfirmed,
                ..
            }
        )
    })
    .await;
    // The refresh tail (61..=110) at once, the covering fetch slowly.
    h.fake.set_history(chat(1), history(1..=110));
    h.fake.inject(Op::History, Fault::Delay(Duration::ZERO));
    h.fake
        .inject(Op::History, Fault::Delay(Duration::from_secs(25)));
    tokio::time::sleep(Duration::from_secs(16)).await;
    h.send(Command::Window(WindowState::Active)).await;
    h.until(reconciled_in(chat(1))).await;
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await; // absorbed by the fetch
    reopen(&mut h).await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    h.drain();
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    let (ev, _) = h.until(|e| ended(e) || older(e)).await;
    assert!(older(&ev) || ended(&ev));
    tokio::time::sleep(Duration::from_secs(60)).await;
    let rest = h.drain();
    assert!(!rest.iter().any(|e| ended(e) || older(e)), "{rest:?}");
    h.stop().await;
}

/// Round 20 (re-check of task 10f), finding 1: a `LoadOlder` in flight when
/// the user logs out. Logout drops the open chat and its windows; the load
/// it owed the UI must go with them, or the actor's invariant check fails
/// and core stops in the middle of the logout.
#[tokio::test(start_paused = true)]
async fn logout_with_an_older_load_in_flight_completes() {
    let mut h = harness(FakeScript::demo(2, 200), None).await;
    open(&mut h).await;
    h.fake.lead(Op::History, Duration::from_secs(10));
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    h.send(Command::Logout).await;
    h.auth(|s| matches!(s, crate::api::AuthState::LoggedOut { .. }))
        .await;
    tokio::time::sleep(Duration::from_secs(30)).await;
    // Core still runs: it answers a command after the old load ended.
    h.send(Command::LoadDialogs).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    h.stop().await;
}

/// Round 20, finding 2: a `LoadOlder` queued behind sixteen hanging calls
/// when the instance is rebuilt. The purge used to drop it silently, so the
/// UI, which keeps its window across a rebuild, stayed pending until some
/// later bootstrap invalidated the chat. Now it gets `LoadEnded(Busy)`.
#[tokio::test(start_paused = true)]
async fn a_queued_older_load_purged_by_a_rebuild_is_answered() {
    let mut h = harness(FakeScript::demo(2, 200), None).await;
    open(&mut h).await;
    let max = crate::actor::MAX_CONCURRENT;
    for _ in 0..max {
        h.fake.inject(Op::Read, Fault::Hang);
    }
    for i in 0..max {
        let id = i64::try_from(i).unwrap() + 10;
        h.send(Command::MarkRead {
            thread: ThreadKey::whole(chat(id)),
            max_id: MsgId(1),
        })
        .await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(h.fake.calls(Op::Read), max);
    let before = h.fake.calls(Op::History);
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.fake.calls(Op::History), before, "queued behind the reads");
    h.fake
        .stream_error(crate::backend::StreamError::Fatal)
        .await;
    let (ev, _) = h.until(ended).await;
    assert!(
        matches!(
            ev,
            Event::LoadEnded {
                error: Some(UserError::Busy),
                ..
            }
        ),
        "{ev:?}"
    );
    h.stop().await;
}
