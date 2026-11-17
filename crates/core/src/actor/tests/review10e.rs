//! Regression tests for the round 17 re-check of task 10d (plan section 15),
//! the core half: an older page applies only while it continues the window,
//! and a panicked history task ends only its own load. Each fails on the
//! 10d code.

use std::time::Duration;

use super::{Harness, chat, harness, history_ids};
use crate::api::{Command, Event, HistoryDir, LoadTarget, SendState, WindowState};
use crate::backend::fake::{FakeScript, Fault, Op, text_message};
use crate::backend::{BackendError, Update};
use crate::model::{LocalId, Message, MsgId, ThreadKey};

fn reconciled(e: &Event) -> bool {
    matches!(e, Event::Reconciled { thread: c, .. } if *c == ThreadKey::whole(chat(1)))
}

fn older(e: &Event) -> bool {
    matches!(e, Event::History { thread: c, dir: HistoryDir::Older, .. } if *c == ThreadKey::whole(chat(1)))
}

fn covered(e: &Event) -> bool {
    matches!(e, Event::SendsCovered { .. })
}

fn ended(e: &Event) -> bool {
    matches!(e, Event::LoadEnded { .. })
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

/// Delivers live messages `ids` to chat 1, waiting until each batch is
/// shown. Returns every event seen meanwhile.
async fn live(h: &mut Harness, ids: std::ops::RangeInclusive<i32>) -> Vec<Event> {
    let mut seen = Vec::new();
    let last = *ids.end();
    for id in ids {
        h.fake
            .push_update(Update::NewMessage(text_message(chat(1), id, "live")))
            .await;
        if id % 100 == 0 || id == last {
            let shown = |e: &Event| {
                matches!(
                    e,
                    Event::History {
                        dir: HistoryDir::Newer,
                        ..
                    }
                ) && history_ids(e).contains(&id)
            };
            let (ev, skipped) = h.until(shown).await;
            seen.extend(skipped);
            seen.push(ev);
        }
    }
    seen
}

/// Live messages trim the window's oldest end, then deletions make room:
/// 2,000 messages from `first` up, then the newest 50 of them deleted.
async fn trim_then_free(h: &mut Harness, first: i32) -> Vec<Event> {
    let last = first + 1_999;
    let mut seen = live(h, first..=last).await;
    let ids: Vec<MsgId> = (last - 49..=last).map(MsgId).collect();
    h.fake
        .push_update(Update::Deleted {
            chat: Some(chat(1)),
            ids,
        })
        .await;
    let (ev, skipped) = h.until(|e| matches!(e, Event::Deleted { .. })).await;
    seen.extend(skipped);
    seen.push(ev);
    seen
}

/// Finding 1 (HIGH): the window holds 61..=110 and a covering fetch asks
/// for the page older than 61, which a `LoadOlder` of the user joins.
/// While it runs, 2,000 live messages trim 61..=110 away and deletions
/// free 50 slots. The page (11..=60) fit and was prepended, leaving a hole
/// at 61..=110 that the cover spanned: the send (floor 10, message 11) was
/// certified. Now the page is dropped and the user's load starts over from
/// the window's current edge.
#[tokio::test(start_paused = true)]
async fn a_covering_page_cut_off_from_the_window_is_dropped() {
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
    // The refresh tail (61..=110 of the server's 1..=110 then) at once,
    // the first covering fetch slowly.
    h.fake.set_history(chat(1), history(1..=110));
    h.fake.inject(Op::History, Fault::Delay(Duration::ZERO));
    h.fake
        .inject(Op::History, Fault::Delay(Duration::from_secs(25)));
    refresh(&h).await;
    h.until(reconciled).await;
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    h.fake.set_history(chat(1), history(1..=2_110));
    let during = trim_then_free(&mut h, 111).await;
    assert!(!during.iter().any(older), "the fetch was still running");
    tokio::time::sleep(Duration::from_secs(120)).await;
    let rest = h.drain();
    assert!(!rest.iter().any(covered), "no hole is certified");
    let first = rest.iter().find(|e| older(e)).expect("an answer");
    assert_eq!(
        history_ids(first),
        (61..=110).collect::<Vec<_>>(),
        "the user's load, from the window's current edge"
    );
    assert!(
        !rest
            .iter()
            .any(|e| older(e) && history_ids(e).contains(&11)),
        "11..=60 never fits: the window is full"
    );
    assert!(!rest.iter().any(ended), "the user's load was answered once");
    h.stop().await;
}

/// Finding 1, the same race for the user's own `LoadOlder`: its page no
/// longer continues the window once the window's oldest end was trimmed.
/// It used to be prepended below a hole; now the load starts over.
#[tokio::test(start_paused = true)]
async fn a_users_older_page_cut_off_from_the_window_starts_over() {
    let mut h = harness(FakeScript::demo(1, 100), None).await;
    open(&mut h).await;
    h.fake.set_history(chat(1), history(1..=2_100));
    h.fake
        .inject(Op::History, Fault::Delay(Duration::from_secs(25)));
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    let during = trim_then_free(&mut h, 101).await;
    assert!(!during.iter().any(older), "the load was still running");
    let (first, before) = h.until(older).await;
    assert_eq!(
        history_ids(&first),
        (51..=100).collect::<Vec<_>>(),
        "not 1..=50 below a hole at 51..=100"
    );
    assert!(!before.iter().any(ended));
    tokio::time::sleep(Duration::from_secs(60)).await;
    let rest = h.drain();
    assert!(!rest.iter().any(|e| older(e) || ended(e)), "answered once");
    h.stop().await;
}

/// Finding 2: a refresh tail panics while a `LoadOlder` waits behind it.
/// Core used to end both directions (`LoadEnded`) and then run the
/// deferred load as well, so the UI was answered twice and asked again.
#[tokio::test(start_paused = true)]
async fn a_panicked_tail_does_not_end_a_deferred_load() {
    let mut h = harness(FakeScript::demo(1, 200), None).await;
    open(&mut h).await;
    h.fake.inject(Op::History, Fault::Panic);
    refresh(&h).await;
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    let (ev, before) = h.until(older).await;
    assert_eq!(history_ids(&ev), (101..=150).collect::<Vec<_>>());
    assert!(!before.iter().any(ended), "{before:?}");
    tokio::time::sleep(Duration::from_secs(60)).await;
    let rest = h.drain();
    assert!(!rest.iter().any(|e| older(e) || ended(e)), "{rest:?}");
    // A panicked load the UI asked for still ends, in its own direction.
    h.fake.inject(Op::History, Fault::Panic);
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    let (ev, _) = h.until(ended).await;
    let target = LoadTarget::History {
        thread: ThreadKey::whole(chat(1)),
        dir: HistoryDir::Older,
    };
    assert!(matches!(ev, Event::LoadEnded { target: t, .. } if t == target));
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert!(!h.drain().iter().any(ended), "one end per load");
    h.stop().await;
}
