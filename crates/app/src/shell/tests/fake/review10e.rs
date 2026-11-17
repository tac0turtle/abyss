//! Regression tests for the round 17 re-check of task 10d (plan section
//! 15), through the real window: the Retry pills. Each fails on the 10d
//! code.

use abyss_core::api::{HistoryDir, LoadTarget};
use abyss_core::model::ThreadKey;
use abyss_core::store::ViewGen;
use gpui::{Modifiers, px, size};

use super::*;
use crate::chat::retry::{MAX_DELAY, MAX_FAILURES};

/// Signs in and lists `chats` chats in an 800×300 window.
fn listed(root: &Entity<Root>, cx: &mut VisualTestContext, harness: &Harness, chats: u32) {
    sign_in(root, cx, harness);
    cx.simulate_resize(size(px(800.), px(300.)));
    let script = FakeScript::demo(chats, 0);
    harness
        .events
        .unbounded_send(Event::DialogsReset(script.dialogs))
        .unwrap();
    cx.run_until_parked();
}

/// Finding 3: the last refused attempt of the frame commands happens in
/// the after-paint check, and a quiet refusal redraws nothing, so the
/// Retry pill they now offer was never painted. With a full queue and
/// nothing else redrawing, the pill now shows once retries give up.
#[gpui::test]
fn exhausted_frame_retries_paint_the_retry_pill(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    listed(&root, cx, &harness, 1);
    root.update_in(cx, |root, window, cx| {
        root.with_session(|s, cx| s.select_chat(CHAT, window, cx), cx);
    });
    cx.run_until_parked();
    harness.sent.borrow_mut().full = true;
    harness
        .events
        .unbounded_send(Event::History {
            thread: ThreadKey::whole(CHAT),
            view: ViewGen(1),
            dir: HistoryDir::Tail,
            messages: (1..=30).map(|id| message(CHAT, id)).collect(),
            removed: Vec::new(),
            has_more: false,
        })
        .unwrap();
    cx.run_until_parked();
    for _ in 0..MAX_FAILURES {
        cx.executor().advance_clock(MAX_DELAY);
        cx.run_until_parked();
    }
    assert!(read(&root, cx, |s| s.shell.history_gave_up()));
    assert!(
        cx.debug_bounds("retry-history").is_some(),
        "the Retry pill is painted"
    );
}

/// Finding 4: the sidebar's Retry pill was painted under the chat rows,
/// and a click on it reached the row below (opening that chat). It is now
/// painted over them and takes the click.
#[gpui::test]
fn the_sidebar_retry_pill_takes_its_click(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    listed(&root, cx, &harness, 10);
    harness
        .events
        .unbounded_send(Event::DialogsPaging { has_more: true })
        .unwrap();
    cx.run_until_parked();
    let failed = || Event::LoadEnded {
        target: LoadTarget::Dialogs,
        error: Some(UserError::Network),
    };
    for _ in 0..MAX_FAILURES {
        harness.events.unbounded_send(failed()).unwrap();
        cx.run_until_parked();
        cx.executor().advance_clock(MAX_DELAY);
        cx.run_until_parked();
    }
    assert!(read(&root, cx, |s| s.shell.list.gave_up()));
    let pill = cx.debug_bounds("retry-chats").expect("the pill is painted");
    let from = sent_count(&harness);
    cx.simulate_click(pill.center(), Modifiers::none());
    cx.run_until_parked();
    let sent = commands_since(&harness, from);
    assert_eq!(count(&sent, "OpenThread"), 0, "no row under it: {sent:?}");
    assert_eq!(count(&sent, "LoadDialogs"), 1, "Retry: {sent:?}");
    assert_eq!(read(&root, cx, |s| s.shell.selected()), None);
}
