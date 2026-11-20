//! An overlay owns keyboard focus until it closes (review A56). Core's
//! replies and timers move the auth screen on by themselves; they must not
//! carry focus into a field the overlay covers, and the field takes it back
//! when the last overlay closes.

use gpui::Focusable;

use super::*;

fn code_is_focused(root: &Entity<Root>, cx: &mut VisualTestContext) -> bool {
    let handle = root.read_with(cx, |root, cx| match &root.state {
        State::Running(session) => session.code.focus_handle(cx),
        State::Failed(failure) => panic!("failed: {failure:?}"),
    });
    cx.update(|window, _| handle.is_focused(window))
}

fn code_text(root: &Entity<Root>, cx: &mut VisualTestContext) -> String {
    root.read_with(cx, |root, cx| match &root.state {
        State::Running(session) => session.code.read(cx).text().to_owned(),
        State::Failed(failure) => panic!("failed: {failure:?}"),
    })
}

/// The code prompt core replies with after a phone number.
fn code_prompt() -> AuthState {
    AuthState::Code {
        phone_hint: abyss_core::api::mask_phone("+15550100"),
        sent: None,
        error: None,
    }
}

fn submit_phone(root: &Entity<Root>, cx: &mut VisualTestContext, harness: &Harness) {
    core_says(harness, cx, AuthState::Phone { error: None });
    cx.simulate_input("+15550100");
    cx.simulate_keystrokes("enter");
    assert!(matches!(auth(root, cx), AuthView::Phone { .. }));
}

#[gpui::test]
fn a_code_reply_behind_settings_leaves_focus_in_settings(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    submit_phone(&root, cx, &harness);
    root.update_in(cx, Root::toggle_appearance);
    cx.run_until_parked();

    core_says(&harness, cx, code_prompt());
    assert!(matches!(auth(&root, cx), AuthView::Code { .. }));
    assert!(!code_is_focused(&root, cx), "the field is covered");
    cx.simulate_input("1234");
    assert_eq!(code_text(&root, cx), "", "typing never reaches it");

    root.update_in(cx, Root::toggle_appearance);
    cx.run_until_parked();
    assert!(code_is_focused(&root, cx), "closing Settings enters it");
    cx.simulate_input("1234");
    assert_eq!(code_text(&root, cx), "1234");
}

#[gpui::test]
fn a_code_reply_while_closing_leaves_focus_on_the_overlay(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    submit_phone(&root, cx, &harness);
    root.update_in(cx, |root, window, cx| {
        assert!(!root.request_quit(window, cx));
    });
    cx.run_until_parked();
    assert_eq!(closing(&root, cx), Some(Closing::Waiting));

    core_says(&harness, cx, code_prompt());
    assert!(!code_is_focused(&root, cx));
    cx.simulate_input("1234");
    assert_eq!(code_text(&root, cx), "");
    assert!(root.read_with(cx, |root, _| {
        root.overlay_focus()
            .is_some_and(|focus| std::ptr::eq(focus, &raw const root.closing_focus))
    }));
}
