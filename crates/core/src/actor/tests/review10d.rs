//! Regression tests for the round 16 re-check of task 10c (plan section 15),
//! the core half: coverage evidence counts only history the window shows,
//! and covering fetches stay background work. Each fails on the 10c code.

use std::time::Duration;

use super::{Harness, chat, harness, history_ids};
use crate::actor::covered::MAX_COVER_FETCHES;
use crate::api::{Command, Event, HistoryDir, SendState, WindowState};
use crate::backend::BackendError;
use crate::backend::fake::{FakeScript, Fault, Op, text_message};
use crate::model::{LocalId, Message, ThreadKey};

fn reconciled(e: &Event) -> bool {
    matches!(e, Event::Reconciled { thread: c, .. } if *c == ThreadKey::whole(chat(1)))
}

fn older(e: &Event) -> bool {
    matches!(e, Event::History { thread: c, dir: HistoryDir::Older, .. } if *c == ThreadKey::whole(chat(1)))
}

fn covered(e: &Event) -> bool {
    matches!(e, Event::SendsCovered { .. })
}

fn unconfirmed(e: &Event) -> bool {
    matches!(
        e,
        Event::SendState {
            state: SendState::SentUnconfirmed,
            ..
        }
    )
}

fn history(ids: std::ops::RangeInclusive<i32>) -> Vec<Message> {
    ids.map(|i| text_message(chat(1), i, "m")).collect()
}

async fn open(h: &mut Harness) {
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h.until(reconciled).await;
}

/// Runs a light refresh (activation past the minimum gap).
async fn refresh(h: &Harness) {
    tokio::time::sleep(Duration::from_secs(16)).await;
    h.send(Command::Window(WindowState::Active)).await;
}

/// Finding 1 (HIGH): send 11 (floor 10) is accepted late, after a reopen
/// loaded 51..=100. Then 31..=100 are deleted unseen and a refresh returns
/// the whole history, 1..=30. The merge used to ignore all of it (below
/// the window), delete the window and report the send covered from the
/// start of history: the echo went, and message 11 never reached the UI.
/// Now the tail replaces the window, and the cover follows it.
#[tokio::test(start_paused = true)]
async fn coverage_counts_only_history_the_window_shows() {
    let mut h = harness(FakeScript::demo(1, 10), None).await;
    accepted_after_reopen(&mut h).await;
    h.fake.set_history(chat(1), history(1..=30));
    refresh(&h).await;
    let (_, before) = h.until(reconciled).await;
    assert_eq!(
        last_history(&before),
        (Some(HistoryDir::Tail), (1..=30).collect::<Vec<_>>()),
        "the refresh shows what is left of the tail, message 11 included"
    );
    let (_, between) = h.until(covered).await;
    assert!(
        !between.iter().any(|e| matches!(e, Event::Deleted { .. })),
        "and the echo goes while 11 is shown"
    );
    h.stop().await;
}

/// Finding 1: as above, but 61..=100 were deleted. The merge keeps 51..=60
/// and ignores 1..=50 of the server's whole history. The cover starts at
/// what the window holds (51), not at the start of history: core fetches
/// the older page, and only once 11 is shown is the send covered.
#[tokio::test(start_paused = true)]
async fn ignored_older_messages_are_no_evidence() {
    let mut h = harness(FakeScript::demo(1, 10), None).await;
    accepted_after_reopen(&mut h).await;
    h.fake.set_history(chat(1), history(1..=60));
    refresh(&h).await;
    let (_, before) = h.until(covered).await;
    assert_eq!(
        last_history(&before),
        (Some(HistoryDir::Older), (1..=50).collect::<Vec<_>>()),
        "covered by the older page that shows 11"
    );
    h.stop().await;
}

/// Opens chat 1 (holding 1..=10) and sends to it. The answer is lost; the
/// resend after the call deadline is told it is a duplicate:
/// `SentUnconfirmed`, floor 10, message 11 on the server. Before that, the
/// chat is reopened with 1..=100 on the server (the window: 51..=100).
async fn accepted_after_reopen(h: &mut Harness) {
    open(h).await;
    h.fake.inject(Op::Send, Fault::CompleteThenHang);
    h.send(Command::SendText {
        thread: ThreadKey::whole(chat(1)),
        text: "hi".into(),
        local_id: LocalId(1),
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    h.fake.set_history(chat(1), history(1..=100));
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    let tail = |e: &Event| matches!(e, Event::History { dir: HistoryDir::Tail, messages, .. } if messages.len() == 50);
    let (ev, _) = h.until(tail).await;
    assert_eq!(history_ids(&ev), (51..=100).collect::<Vec<_>>());
    h.until(unconfirmed).await;
}

/// The direction and ids of the last `History` event in `events`.
fn last_history(events: &[Event]) -> (Option<HistoryDir>, Vec<i32>) {
    let last = events.iter().rfind(|e| matches!(e, Event::History { .. }));
    let dir = last.and_then(|e| match e {
        Event::History { dir, .. } => Some(*dir),
        _ => None,
    });
    (dir, last.map(history_ids).unwrap_or_default())
}

/// Finding 6: a `LoadOlder` asked while a covering fetch runs is answered
/// by that fetch. It used to wait behind every remaining covering fetch,
/// then fetch a page more.
#[tokio::test(start_paused = true)]
async fn a_load_asked_during_a_covering_fetch_is_answered_by_it() {
    let mut h = harness(FakeScript::demo(1, 10), None).await;
    open(&mut h).await;
    h.fake
        .inject(Op::Send, Fault::Error(BackendError::SentUnconfirmed));
    h.send(Command::SendText {
        thread: ThreadKey::whole(chat(1)),
        text: "hi".into(),
        local_id: LocalId(1),
    })
    .await;
    h.until(unconfirmed).await;
    h.fake.set_history(chat(1), history(1..=400));
    // The refresh tail at once, the first covering fetch slowly.
    h.fake.inject(Op::History, Fault::Delay(Duration::ZERO));
    h.fake
        .inject(Op::History, Fault::Delay(Duration::from_secs(5)));
    refresh(&h).await;
    h.until(reconciled).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    let (first, _) = h.until(older).await;
    assert_eq!(history_ids(&first), (301..=350).collect::<Vec<_>>());
    tokio::time::sleep(Duration::from_secs(60)).await;
    let rest = h.drain();
    let pages = 1 + rest.iter().filter(|e| older(e)).count();
    assert_eq!(pages, usize::from(MAX_COVER_FETCHES), "no page twice");
    assert!(!rest.iter().any(|e| matches!(e, Event::LoadEnded { .. })));
    assert!(!rest.iter().any(covered), "400 is out of reach");
    h.stop().await;
}
