//! What a person hears when abyss closes, stops or discards a draft: the
//! closing overlay and its two quit cards, the recovery banner, the restart
//! screen and the discarded-draft notice, and where Tab goes while the
//! closing overlay covers the window (review N43). Each is asserted in the
//! tree GPUI sends and, for the text, in what `AppKit` hands a client.

use abyss_core::api::{AuthState, Event};
use abyss_core::store::ForumStatus;
use gpui::{App, AppContext, WindowHandle, px, size};
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSString};

use super::super::{NSAccessibilityElement, appkit_window, assert_native_role};
use super::{
    ALICE, AnyWindowHandle, EventSink, NSAccessibility, OLD, Root, Value, apply, assert_focus,
    find_node, has_value, key, labels_of, nodes_of, open_sized, send, signed_in, tab_to, tree,
    type_text, when,
};
use crate::chat::copy as chat_copy;
use crate::shell::shutdown::Next;
use crate::ui::copy;

pub(super) fn run(cx: &mut App) {
    keys_reach_the_window(cx);
    closing_overlay_and_quit_cards(cx);
    quit_with_settings_open(cx);
    quit_with_the_logout_prompt_or_a_menu_open(cx);
    stopped_and_recovery(cx);
    discarded_notice(cx);
}

/// A signed-in window over the five fixture chats.
fn signed_in_window(cx: &mut App) -> (WindowHandle<Root>, crate::shell::tests::Harness) {
    let (started, harness) = crate::shell::tests::started_fixture();
    let handle = open_sized(cx, size(px(960.), px(640.)), |window, cx| {
        cx.new(|cx| Root::with_started(Ok(started), window, cx))
    });
    signed_in(handle, cx);
    (handle, harness)
}

/// Settles focus: an overlay moves it after the draw that shows it.
fn settled(handle: WindowHandle<Root>, cx: &mut App) -> Value {
    let _ = tree(handle, cx);
    tree(handle, cx)
}

/// Quit as the Quit action does: shutdown begins, and the window stays.
fn quit(handle: WindowHandle<Root>, cx: &mut App) {
    handle
        .update(cx, |root, window, cx| {
            assert!(!root.request_quit(window, cx), "shutdown owns the quit");
        })
        .unwrap();
}

fn has_dialog(tree: &Value, label: &str) -> bool {
    nodes_of(tree, "Dialog")
        .iter()
        .any(|node| node["aria"]["label"] == label)
}

/// Tab and Shift+Tab have nowhere else to go: focus stays on `label`.
fn assert_trapped_on(handle: WindowHandle<Root>, cx: &mut App, label: &str) {
    for step in ["tab", "shift-tab", "tab"] {
        key(handle, cx, step);
        assert_focus(&tree(handle, cx), label);
    }
}

/// Some native element under the window has `text` as its value or title.
fn native_has_text(handle: WindowHandle<Root>, cx: &mut App, text: &str) -> bool {
    AnyWindowHandle::from(handle)
        .update(cx, |_, window, _| {
            contains_text(appkit_window(window).accessibilityChildren(), text, 0)
        })
        .unwrap()
}

fn contains_text(children: Option<Retained<NSArray>>, text: &str, depth: usize) -> bool {
    assert!(depth < 16, "native tree is deeper than expected");
    children.is_some_and(|children| {
        children.iter().any(|child| {
            let Ok(element) = child.downcast::<NSAccessibilityElement>() else {
                return false;
            };
            let value = element
                .accessibilityValue()
                .and_then(|value| value.downcast::<NSString>().ok())
                .map(|value| value.to_string());
            value.as_deref() == Some(text)
                || element
                    .accessibilityTitle()
                    .is_some_and(|title| title.to_string() == text)
                || contains_text(element.accessibilityChildren(), text, depth + 1)
        })
    })
}

/// Cmd+Q over the signed-in shell: `Closing…` is a modal dialog `VoiceOver`
/// can find, focus moves into it from the hidden body, and the cards that
/// follow explain themselves (N43.1, N43.2).
fn closing_overlay_and_quit_cards(cx: &mut App) {
    for (next, title, detail) in [
        (
            Next::ShowUnconfirmed,
            copy::UNCONFIRMED_TITLE,
            copy::UNCONFIRMED_DETAIL,
        ),
        (
            Next::ShowLogoutUnfinished,
            copy::LOGOUT_UNFINISHED_TITLE,
            copy::LOGOUT_UNFINISHED_DETAIL,
        ),
    ] {
        let (handle, _harness) = signed_in_window(cx);
        // Focus is on a control of the body the overlay will hide.
        tab_to(handle, cx, "Settings");
        waiting(handle, cx);
        quit_card(handle, cx, next, title, detail);
    }
}

fn waiting(handle: WindowHandle<Root>, cx: &mut App) {
    quit(handle, cx);
    let waiting = settled(handle, cx);
    find_node(&waiting, "Dialog", copy::CLOSING);
    assert!(has_value(&waiting, "Status", copy::CLOSING));
    assert!(
        native_has_text(handle, cx, copy::CLOSING),
        "VoiceOver sees Closing… on every Cmd+Q"
    );
    assert_native_role(handle, cx, copy::CLOSING, "AXWindow");
    assert_focus(&waiting, copy::CLOSING);
    assert_trapped_on(handle, cx, copy::CLOSING);
}

fn quit_card(handle: WindowHandle<Root>, cx: &mut App, next: Next, title: &str, detail: &str) {
    handle
        .update(cx, |root, _, cx| root.finish_shutdown(next, cx))
        .unwrap();
    let card = settled(handle, cx);
    find_node(&card, "Dialog", title);
    find_node(&card, "Heading", title);
    assert!(has_value(&card, "Alert", detail), "the detail is an alert");
    find_node(&card, "Button", copy::QUIT);
    for text in [title, detail] {
        assert!(native_has_text(handle, cx, text), "{text} is not native");
    }
    assert_native_role(handle, cx, copy::QUIT, "AXButton");
    assert_focus(&card, copy::QUIT);
    assert_trapped_on(handle, cx, copy::QUIT);
}

/// Cmd+Q with Settings open closes it: the panel is not drawn over the
/// overlay, and Tab is not trapped in an occluded dialog (N43.2).
fn quit_with_settings_open(cx: &mut App) {
    let (handle, _harness) = signed_in_window(cx);
    crate::shell::route_actions(handle, cx);
    tab_to(handle, cx, "Settings");
    key(handle, cx, "enter");
    assert!(has_dialog(&settled(handle, cx), "Settings"));

    key(handle, cx, "cmd-q");
    let closing = settled(handle, cx);
    assert!(!has_dialog(&closing, "Settings"), "settings is closed");
    find_node(&closing, "Dialog", copy::CLOSING);
    assert_focus(&closing, copy::CLOSING);
    assert_trapped_on(handle, cx, copy::CLOSING);

    handle
        .update(cx, |root, _, cx| {
            root.finish_shutdown(Next::ShowUnconfirmed, cx);
        })
        .unwrap();
    assert_focus(&settled(handle, cx), copy::QUIT);
    assert_trapped_on(handle, cx, copy::QUIT);
}

/// The logout prompt and an open menu close too: neither dialog stays
/// behind the overlay, and focus is in the overlay.
fn quit_with_the_logout_prompt_or_a_menu_open(cx: &mut App) {
    let (handle, _harness) = signed_in_window(cx);
    tab_to(handle, cx, copy::onboarding::LOGOUT);
    key(handle, cx, "enter");
    assert!(has_dialog(&settled(handle, cx), copy::LOGOUT_CONFIRM_TITLE));
    quit(handle, cx);
    let closing = settled(handle, cx);
    assert!(!has_dialog(&closing, copy::LOGOUT_CONFIRM_TITLE));
    assert_focus(&closing, copy::CLOSING);
    assert_trapped_on(handle, cx, copy::CLOSING);

    let (handle, _harness) = signed_in_window(cx);
    let alice = format!(
        "Alice, 3 unread, muted, pinned, last: see you at five, {}",
        when(OLD)
    );
    tab_to(handle, cx, &alice);
    key(handle, cx, "shift-f10");
    find_node(&settled(handle, cx), "Menu", "Actions for Alice");
    quit(handle, cx);
    let closing = settled(handle, cx);
    assert!(nodes_of(&closing, "Menu").is_empty(), "the menu is closed");
    assert_focus(&closing, copy::CLOSING);
    assert_trapped_on(handle, cx, copy::CLOSING);
}

/// A stopped core says so to `VoiceOver`, with or without an account to keep
/// (N43.3).
fn stopped_and_recovery(cx: &mut App) {
    let (handle, _harness) = signed_in_window(cx);
    core_stopped(handle, cx);
    let recovery = tree(handle, cx);
    assert!(has_value(&recovery, "Status", chat_copy::CORE_STOPPED_COPY));
    assert!(native_has_text(handle, cx, chat_copy::CORE_STOPPED_COPY));

    let (handle, _harness) = signed_in_window(cx);
    apply(handle, cx, AuthState::LoggingOut);
    core_stopped(handle, cx);
    let stopped = tree(handle, cx);
    assert!(has_value(&stopped, "Status", chat_copy::STOPPED_RESTART));
    assert!(native_has_text(handle, cx, chat_copy::STOPPED_RESTART));
}

fn core_stopped(handle: WindowHandle<Root>, cx: &mut App) {
    handle.update(cx, EventSink::core_stopped).unwrap();
}

/// A draft discarded by the chat becoming a forum is announced, and its
/// Copy and Dismiss are controls (N43.3).
fn discarded_notice(cx: &mut App) {
    let (handle, _harness) = signed_in_window(cx);
    let alice = format!(
        "Alice, 3 unread, muted, pinned, last: see you at five, {}",
        when(OLD)
    );
    tab_to(handle, cx, &alice);
    key(handle, cx, "enter");
    assert_focus(&tree(handle, cx), "Message to Alice");
    type_text(handle, cx, "a draft");
    send(
        handle,
        cx,
        vec![Event::KindChanged {
            chat: ALICE,
            forum: ForumStatus::Forum,
        }],
    );
    let notice = tree(handle, cx);
    let line = nodes_of(&notice, "Status")
        .iter()
        .filter_map(|node| node["aria"]["value"].as_str())
        .find(|value| value.starts_with("Your draft in "))
        .expect("the discarded draft is announced")
        .to_owned();
    assert!(native_has_text(handle, cx, &line));
    let buttons = labels_of(&notice, "Button");
    for label in [chat_copy::COPY_TEXT, chat_copy::DISMISS] {
        assert!(buttons.contains(&label.to_owned()), "{label} is a button");
    }
}

/// The app-level keys reach the window. GPUI runs a global action listener
/// inside the window's own update, where updating that window fails, so each
/// route waits for the dispatch to end (`route_actions`). Before, Cmd+Q took
/// the window-gone branch and quit at once, Cmd+W quit instead of closing,
/// and Cmd+comma did nothing.
fn keys_reach_the_window(cx: &mut App) {
    let (started, _harness) = crate::shell::tests::started_fixture();
    let handle = open_sized(cx, size(px(960.), px(640.)), |window, cx| {
        cx.new(|cx| Root::with_started(Ok(started), window, cx))
    });
    crate::shell::route_actions(handle, cx);
    assert!(!has_dialog(&settled(handle, cx), copy::SETTINGS));
    key(handle, cx, "cmd-,");
    assert!(has_dialog(&settled(handle, cx), copy::SETTINGS));
    key(handle, cx, "cmd-,");
    assert!(!has_dialog(&settled(handle, cx), copy::SETTINGS));
    // Not signed in: closing the window cannot hide it, so it shuts down.
    key(handle, cx, "cmd-w");
    let closing = settled(handle, cx);
    find_node(&closing, "Dialog", copy::CLOSING);
    assert_focus(&closing, copy::CLOSING);
}
