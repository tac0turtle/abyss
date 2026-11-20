//! The sidebar's rows by keyboard: Option+Cmd+Down and Up bring the selected
//! row into view (the Stage 2 follow-up of plan section 4.3), and Escape
//! returns focus to the row that was opened, even after the list scrolled
//! away from it.

use abyss_core::model::{ChatKind, ChatSummary, MsgId, PeerFolder, PeerKind, ReadState};
use gpui::px;

use super::*;

const CHATS: i64 = 120;

fn chat(n: i64) -> ChatSummary {
    ChatSummary {
        id: ChatId {
            kind: PeerKind::User,
            id: n,
        },
        title: format!("Chat {n}"),
        kind: ChatKind::Private { contact: true },
        pinned: false,
        read: ReadState {
            unread: 0,
            read_inbox_max: MsgId(0),
        },
        unread_mentions: 0,
        last: None,
        last_date: 1_700_000_000 - n,
        last_id: None,
        can_send: true,
        muted_until: None,
        scope_muted_until: 0,
        folder: PeerFolder::Main,
        reading: None,
        topic_unread: None,
    }
}

fn listed(cx: &mut TestAppContext) -> (Entity<Root>, &mut VisualTestContext, Harness) {
    cx.update(|cx| cx.bind_keys(crate::shell::key_bindings()));
    let (root, cx, harness) = open(cx);
    sign_in(&root, cx, &harness);
    let chats = (1..=CHATS).map(chat).collect();
    harness
        .events
        .unbounded_send(Event::DialogsReset(chats))
        .unwrap();
    cx.run_until_parked();
    (root, cx, harness)
}

/// Whether the selected row lies inside the sidebar list's viewport.
fn selected_in_view(cx: &mut VisualTestContext) -> bool {
    let Some(row) = cx.debug_bounds("selected-row") else {
        return false;
    };
    let window = cx.update(|window, _| window.viewport_size());
    row.top() >= px(0.) && row.bottom() <= window.height
}

/// Stepping down past the screen's end scrolls the list, row by row, and
/// stepping back up scrolls it back: the selection is never out of sight.
#[gpui::test]
fn stepping_keeps_the_selected_row_in_view(cx: &mut TestAppContext) {
    let (root, cx, _harness) = listed(cx);
    select(&root, cx, 1);
    assert!(selected_in_view(cx));
    let steps = 60;
    for step in 1..=steps {
        cx.simulate_keystrokes("alt-cmd-down");
        cx.run_until_parked();
        assert_eq!(selected(&root, cx), Some(user(1 + step)));
        assert!(selected_in_view(cx), "chat {} is off screen", 1 + step);
    }
    let offset = read(&root, cx, |s| {
        s.nav
            .scroll(s.shell.list.view())
            .0
            .borrow()
            .base_handle
            .offset()
            .y
    });
    assert!(offset < px(0.), "the list scrolled");
    for step in (1..=steps).rev() {
        cx.simulate_keystrokes("alt-cmd-up");
        cx.run_until_parked();
        assert_eq!(selected(&root, cx), Some(user(step)));
        assert!(selected_in_view(cx), "chat {step} is off screen");
    }
}

/// Escape from the composer goes back to the opened row; with the list
/// scrolled far from it, the row is scrolled back into view and focused.
#[gpui::test]
fn escape_returns_to_the_opened_row_even_when_it_scrolled_away(cx: &mut TestAppContext) {
    let (root, cx, _harness) = listed(cx);
    select(&root, cx, 5);
    assert!(selected_in_view(cx));

    // Scroll the list away from the row by hand.
    read(&root, cx, |s| {
        s.nav
            .scroll(s.shell.list.view())
            .0
            .borrow()
            .base_handle
            .set_offset(gpui::point(px(0.), px(-4_000.)));
    });
    act(&root, cx, |_, _, cx| cx.notify());
    assert!(!selected_in_view(cx), "the row is off screen");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(selected_in_view(cx), "Escape brought the row back");
    // Focus is on the row, so Return opens it again, which focuses the box.
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(selected(&root, cx), Some(user(5)));
}
