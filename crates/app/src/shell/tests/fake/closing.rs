//! Quit starts the closing overlay from any state of the window (review N43):
//! Settings, the logout prompt and menus close first, so nothing is left
//! beside the overlay to take focus or to be read behind it.

use abyss_core::api::AuthState;
use abyss_core::backend::fake::FakeScript;
use gpui::{point, px};

use super::*;
use crate::shell::shutdown::Next;

fn signed_in(cx: &mut TestAppContext) -> (Entity<Root>, &mut VisualTestContext, Harness) {
    let (root, cx, harness) = open(cx);
    core_says(
        &harness,
        cx,
        AuthState::SignedIn {
            display_name: String::new(),
            new_login: None,
        },
    );
    let script = FakeScript::demo(3, 0);
    harness
        .events
        .unbounded_send(Event::DialogsReset(script.dialogs))
        .unwrap();
    cx.run_until_parked();
    (root, cx, harness)
}

fn quit(root: &Entity<Root>, cx: &mut VisualTestContext) {
    root.update_in(cx, |root, window, cx| {
        assert!(!root.request_quit(window, cx), "shutdown owns the quit");
    });
    cx.run_until_parked();
}

/// Whether the closing overlay is the one thing taking Tab.
fn overlay_is_closing(root: &Entity<Root>, cx: &mut VisualTestContext) -> bool {
    root.read_with(cx, |root, _| {
        root.overlay_focus()
            .is_some_and(|focus| std::ptr::eq(focus, &raw const root.closing_focus))
    })
}

#[gpui::test]
fn quit_closes_settings_and_the_overlay_takes_focus(cx: &mut TestAppContext) {
    let (root, cx, _harness) = signed_in(cx);
    root.update_in(cx, Root::toggle_appearance);
    cx.run_until_parked();
    root.read_with(cx, |root, _| assert!(root.appearance_open));
    assert!(cx.debug_bounds("settings-panel").is_some());
    assert!(!overlay_is_closing(&root, cx));

    quit(&root, cx);
    root.read_with(cx, |root, _| assert!(!root.appearance_open));
    assert_eq!(closing(&root, cx), Some(Closing::Waiting));
    assert!(cx.debug_bounds("settings-panel").is_none());
    assert!(overlay_is_closing(&root, cx));
}

#[gpui::test]
fn quit_closes_the_logout_prompt_and_menus(cx: &mut TestAppContext) {
    let (root, cx, _harness) = signed_in(cx);
    act(&root, cx, super::super::super::Session::logout);
    read(&root, cx, |s| assert!(s.confirming_logout()));
    quit(&root, cx);
    read(&root, cx, |s| assert!(!s.confirming_logout()));
    assert!(overlay_is_closing(&root, cx));

    let (root, cx, _harness) = signed_in(cx);
    let chat = FakeScript::demo(3, 0).dialogs.first().unwrap().id;
    act(&root, cx, |s, window, cx| {
        s.open_chat_menu(chat, point(px(10.), px(10.)), window, cx);
    });
    read(&root, cx, |s| assert!(s.menu.is_some()));
    quit(&root, cx);
    read(&root, cx, |s| assert!(s.menu.is_none()));
    assert!(overlay_is_closing(&root, cx));
}

#[gpui::test]
fn the_quit_cards_keep_the_overlay_focus(cx: &mut TestAppContext) {
    let (root, cx, _harness) = signed_in(cx);
    quit(&root, cx);
    for (next, shown) in [
        (Next::ShowUnconfirmed, Closing::Unconfirmed),
        (Next::ShowLogoutUnfinished, Closing::LogoutUnfinished),
    ] {
        root.update(cx, |root, cx| root.finish_shutdown(next, cx));
        cx.run_until_parked();
        assert_eq!(closing(&root, cx), Some(shown));
        assert!(overlay_is_closing(&root, cx));
    }
}
