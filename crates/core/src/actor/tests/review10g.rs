//! Regression tests for the round 21 re-check of the round 20 fix (plan
//! section 15), fixed in task 10g: the byte budget never evicts the open
//! chat's window, and a rebuild answers the user load it abandons, queued
//! or running, exactly once. Each fails with its fix disabled.

use std::time::Duration;

use super::{Harness, chat, harness};
use crate::api::{Command, ConnState, Event, HistoryDir, LoadTarget, UserError};
use crate::backend::fake::{BuildFault, FakeScript, Op, text_message};
use crate::backend::{BackendError, StreamError, Update};
use crate::model::{ChatId, Message, Sender, ThreadKey};
use crate::store::MAX_STORE_BYTES;

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

fn invalidated(e: &Event) -> bool {
    matches!(e, Event::ThreadInvalidated(c) if *c == ThreadKey::whole(chat(1)))
}

async fn open(h: &mut Harness, c: ChatId) {
    h.send(Command::OpenThread(ThreadKey::whole(c))).await;
    h.until(reconciled_in(c)).await;
}

/// A live message retaining more than the whole byte budget: its sender
/// name's capacity is charged, though it holds one byte.
fn huge(c: ChatId, id: i32) -> Message {
    let mut name = String::with_capacity(MAX_STORE_BYTES + 1);
    name.push('x');
    Message {
        sender: Some(Sender { id: c, name }),
        ..text_message(c, id, "big")
    }
}

/// Finding 1: chat 2's window is retained in the background, chat 1 is
/// open with a `LoadOlder` in flight, and a live message to chat 2 pushes
/// the store over its byte budget. The budget used to evict the least
/// recently used window other than chat 2, which was the open chat 1: the
/// owed load then belonged to no window (the actor's invariant fired), and
/// in release builds the next full bootstrap skipped chat 1. Now chat 2,
/// the background chat that grew, trims itself, and the load is answered.
#[tokio::test(start_paused = true)]
async fn a_background_message_over_budget_keeps_the_open_window() {
    let mut h = harness(FakeScript::demo(2, 200), None).await;
    h.online().await;
    open(&mut h, chat(2)).await;
    open(&mut h, chat(1)).await;
    h.fake.lead(Op::History, Duration::from_secs(10));
    let before = h.fake.calls(Op::History);
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(h.fake.calls(Op::History), before + 1, "the load is running");
    h.fake
        .push_update(Update::NewMessage(huge(chat(2), 201)))
        .await;
    let (ev, skipped) = h.until(|e| older(e) || ended(e)).await;
    assert!(older(&ev), "{ev:?}");
    assert!(!skipped.iter().any(invalidated), "{skipped:?}");
    // The open window still takes live messages and reports them.
    h.fake
        .push_update(Update::NewMessage(text_message(chat(1), 201, "live")))
        .await;
    h.until(
        |e| matches!(e, Event::History { thread: c, dir: HistoryDir::Newer, .. } if *c == ThreadKey::whole(chat(1))),
    )
    .await;
    h.stop().await;
}

/// Finding 2: a `LoadOlder` already running (it holds a permit) when the
/// stream fails. Retirement cancels it and its late result is stale, so
/// the UI, which keeps its window across a rebuild, used to wait on it
/// while the rebuild failed. Now retirement answers it once with `Busy`,
/// and nothing answers it again once the rebuild completes.
#[tokio::test(start_paused = true)]
async fn a_running_older_load_abandoned_by_a_rebuild_is_answered_once() {
    let mut h = harness(FakeScript::demo(2, 200), None).await;
    h.online().await;
    open(&mut h, chat(1)).await;
    h.fake.lead(Op::History, Duration::from_secs(10));
    let before = h.fake.calls(Op::History);
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(h.fake.calls(Op::History), before + 1, "the load is running");
    // The first rebuild fails: recovery stalls behind its backoff.
    h.fake
        .on_next_build(BuildFault::Fail(BackendError::Transient));
    h.fake.stream_error(StreamError::Fatal).await;
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
    let (_, rest) = h
        .until(|e| matches!(e, Event::Connection(ConnState::Online)))
        .await;
    tokio::time::sleep(Duration::from_secs(30)).await;
    let rest: Vec<Event> = rest.into_iter().chain(h.drain()).collect();
    assert!(!rest.iter().any(|e| ended(e) || older(e)), "{rest:?}");
    // The UI's retry is a fresh load, answered by its page.
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    let (ev, _) = h.until(|e| older(e) || ended(e)).await;
    assert!(older(&ev), "{ev:?}");
    h.stop().await;
}

/// Round 22 (re-check of task 10g): a `LoadOlder` deferred behind a running
/// tail when the stream fails. Retirement used to answer only the owed
/// load; the deferred one was promoted right after, queued for an instance
/// that failing builds never deliver, and the UI stayed pending until a
/// rebuild finally succeeded. Now retirement answers it with `Busy` before
/// recovery.
#[tokio::test(start_paused = true)]
async fn a_deferred_older_load_is_answered_by_a_stalled_rebuild() {
    let mut h = harness(FakeScript::demo(2, 200), None).await;
    h.online().await;
    open(&mut h, chat(1)).await;
    open(&mut h, chat(2)).await;
    // Chat 1's retained window refreshes its tail slowly on reopening.
    h.fake.lead(Op::History, Duration::from_secs(10));
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let before = h.fake.calls(Op::History);
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        h.fake.calls(Op::History),
        before,
        "deferred behind the tail"
    );
    for _ in 0..3 {
        h.fake
            .on_next_build(BuildFault::Fail(BackendError::Transient));
    }
    h.fake.stream_error(StreamError::Fatal).await;
    let (ev, skipped) = h
        .until(|e| ended(e) || matches!(e, Event::Connection(ConnState::Online)))
        .await;
    assert!(
        matches!(
            ev,
            Event::LoadEnded {
                error: Some(UserError::Busy),
                ..
            }
        ),
        "answered before recovery: {ev:?} after {skipped:?}"
    );
    h.stop().await;
}
