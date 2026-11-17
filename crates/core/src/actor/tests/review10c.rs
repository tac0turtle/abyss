//! Regression tests for the round 12 re-check of task 10b (plan section 15),
//! the core half: every UI load is answered, and unconfirmed sends are
//! reported covered only on evidence. Each fails on the code before task 10c.

use std::time::Duration;

use super::{Harness, chat, harness, history_ids};
use crate::actor::covered::MAX_COVER_FETCHES;
use crate::api::{Command, Event, HistoryDir, LoadTarget, SendState, UserError, WindowState};
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

/// Opens chat 1 and waits for its server tail.
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

/// Finding 4: a `LoadOlder` asked while an unchanged light refresh loads the
/// tail used to be dropped (only `Reconciled` followed). It now runs after.
#[tokio::test(start_paused = true)]
async fn a_load_asked_during_a_refresh_runs_after_it() {
    let mut h = harness(FakeScript::demo(2, 100), None).await;
    open(&mut h).await;
    h.fake
        .inject(Op::History, Fault::Delay(Duration::from_secs(3)));
    refresh(&h).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    let (_, before) = h.until(reconciled).await;
    assert!(!before.iter().any(older), "the refresh was still running");
    let (ev, _) = h.until(older).await;
    assert_eq!(history_ids(&ev), (1..=50).collect::<Vec<_>>());
    // A load core cannot run is answered too.
    h.send(Command::LoadOlder(ThreadKey::whole(chat(2)))).await;
    let (ev, _) = h.until(|e| matches!(e, Event::LoadEnded { .. })).await;
    let target = LoadTarget::History {
        thread: ThreadKey::whole(chat(2)),
        dir: HistoryDir::Older,
    };
    assert!(matches!(ev, Event::LoadEnded { target: t, error: None } if t == target));
    h.stop().await;
}

/// Finding 2: a failed or exhausted dialogs load is reported as such,
/// not only as an `Error` the UI cannot attribute.
#[tokio::test(start_paused = true)]
async fn dialogs_loads_end_explicitly() {
    let mut h = harness(FakeScript::demo(150, 1), None).await;
    h.online().await;
    h.fake
        .inject(Op::Dialogs, Fault::Error(BackendError::Rejected));
    h.send(Command::LoadDialogs).await;
    let (ev, _) = h.until(|e| matches!(e, Event::LoadEnded { .. })).await;
    assert!(matches!(
        ev,
        Event::LoadEnded {
            target: LoadTarget::Dialogs,
            error: Some(UserError::Rejected)
        }
    ));
    h.send(Command::LoadDialogs).await;
    h.until(|e| matches!(e, Event::DialogsPaging { has_more: false }))
        .await;
    h.send(Command::LoadDialogs).await;
    let (ev, _) = h.until(|e| matches!(e, Event::LoadEnded { .. })).await;
    assert!(matches!(
        ev,
        Event::LoadEnded {
            target: LoadTarget::Dialogs,
            error: None
        }
    ));
    h.stop().await;
}

/// Sends local id 1 to chat 1 (holding 1..=10); the server accepts it as
/// message 11 without telling its id. The server then holds `1..=last`.
async fn unconfirmed_send(h: &mut Harness, last: i32) {
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
    let server: Vec<Message> = (1..=last).map(|i| text_message(chat(1), i, "m")).collect();
    h.fake.set_history(chat(1), server);
}

/// Finding 5: more than a page of newer messages puts the send below the
/// refreshed tail, so no tail holds it. Core fetches older pages until the
/// window reaches it, and only then reports it covered.
#[tokio::test(start_paused = true)]
async fn a_send_below_the_tail_is_covered_only_once_fetched() {
    let mut h = harness(FakeScript::demo(1, 10), None).await;
    open(&mut h).await;
    unconfirmed_send(&mut h, 80).await;
    refresh(&h).await;
    let (ev, before) = h.until(covered).await;
    assert!(
        matches!(&ev, Event::SendsCovered { thread: c, sends } if *c == ThreadKey::whole(chat(1)) && *sends == [LocalId(1)])
    );
    let tail = before.iter().position(reconciled).expect("the refresh");
    let page = before.iter().rposition(older).expect("an older page");
    assert!(tail < page, "the tail alone did not cover it");
    assert_eq!(history_ids(&before[page]), (1..=30).collect::<Vec<_>>());
    h.stop().await;
}

/// Finding 5: the fetches are bounded; past them the send stays unconfirmed
/// (its echo stays) rather than being guessed covered.
#[tokio::test(start_paused = true)]
async fn covering_fetches_are_bounded() {
    let mut h = harness(FakeScript::demo(1, 10), None).await;
    open(&mut h).await;
    unconfirmed_send(&mut h, 400).await;
    refresh(&h).await;
    h.until(reconciled).await;
    tokio::time::sleep(Duration::from_secs(30)).await;
    let rest = h.drain();
    assert!(!rest.iter().any(covered), "not reached: not covered");
    let pages = rest.iter().filter(|e| older(e)).count();
    assert_eq!(pages, usize::from(MAX_COVER_FETCHES));
    h.stop().await;
}
