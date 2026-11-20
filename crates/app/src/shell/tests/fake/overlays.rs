//! What the modal overlays do to the pane and the screens behind them:
//! a covered chat is not read and not watched (review A64, N57), focus lands
//! on what the pane renders (A68, N56), an overlay never outlives the screen
//! it belongs to (N55), and a secret never outlives its screen (N59).

use std::ops::RangeInclusive;

use abyss_core::api::{HistoryDir, LogoutTrigger, StorageError};
use abyss_core::model::MsgId;
use abyss_core::store::ViewGen;
use gpui::{Focusable, point, px};

use super::*;
use crate::shell::auth::Onboarding;
use crate::shell::session::Session;
use crate::ui::password::{PasswordInput, Reveal};

fn chat1() -> ThreadKey {
    ThreadKey::whole(user(1))
}

fn core_sends(harness: &Harness, cx: &mut VisualTestContext, event: Event) {
    harness.events.unbounded_send(event).unwrap();
    cx.run_until_parked();
}

/// The tail of chat 1: messages `ids`, all there is.
fn tail(ids: RangeInclusive<i32>) -> Event {
    Event::History {
        thread: chat1(),
        view: ViewGen(1),
        dir: HistoryDir::Tail,
        messages: ids.map(|id| message(user(1), id)).collect(),
        removed: Vec::new(),
        has_more: false,
    }
}

fn sent_by(harness: &Harness, f: impl FnOnce()) -> Vec<String> {
    let from = sent_count(harness);
    f();
    commands_since(harness, from)
}

fn marks_read(sent: &[String]) -> bool {
    sent.iter().any(|c| c.starts_with("MarkRead"))
}

fn watches_tail(sent: &[String]) -> bool {
    let watched = format!("{:?}", Command::ViewingTail(Some(chat1())));
    sent.contains(&watched)
}

/// The covered chat's tail arrives while an overlay shows: nothing is read
/// and nothing is watched. The test then lifts the overlay.
fn covered_tail(
    cx: &mut TestAppContext,
    cover: impl FnOnce(&Entity<Root>, &mut VisualTestContext),
    uncover: Option<fn(&Entity<Root>, &mut VisualTestContext)>,
) {
    let (root, cx, harness) = chats_open(cx, 1);
    cx.simulate_resize(gpui::size(gpui::px(800.), gpui::px(400.)));
    cover(&root, cx);
    let covered = sent_by(&harness, || core_sends(&harness, cx, tail(1..=3)));
    assert!(
        !marks_read(&covered),
        "read behind the overlay: {covered:?}"
    );
    assert!(!watches_tail(&covered), "watched behind it: {covered:?}");
    let Some(uncover) = uncover else { return };
    let shown = sent_by(&harness, || uncover(&root, cx));
    assert!(marks_read(&shown), "read once it is seen: {shown:?}");
    assert!(watches_tail(&shown), "watched once it is seen: {shown:?}");
}

#[gpui::test]
fn settings_over_a_chat_neither_reads_nor_watches_it(cx: &mut TestAppContext) {
    covered_tail(
        cx,
        |root, cx| {
            root.update_in(cx, Root::toggle_appearance);
            cx.run_until_parked();
        },
        Some(|root, cx| {
            root.update_in(cx, Root::toggle_appearance);
            cx.run_until_parked();
        }),
    );
}

#[gpui::test]
fn the_logout_prompt_over_a_chat_neither_reads_nor_watches_it(cx: &mut TestAppContext) {
    covered_tail(
        cx,
        |root, cx| act(root, cx, Session::logout),
        Some(|root, cx| act(root, cx, Session::cancel_logout)),
    );
}

#[gpui::test]
fn the_closing_overlay_over_a_chat_neither_reads_nor_watches_it(cx: &mut TestAppContext) {
    covered_tail(
        cx,
        |root, cx| {
            root.update_in(cx, |root, window, cx| {
                assert!(!root.request_quit(window, cx));
            });
            cx.run_until_parked();
        },
        None,
    );
}

/// A watched tail stops being watched when an overlay covers it, and is
/// reported again, with the read, when it is uncovered.
#[gpui::test]
fn an_overlay_opened_over_a_watched_tail_stops_watching_it(cx: &mut TestAppContext) {
    let (root, cx, harness) = chats_open(cx, 1);
    cx.simulate_resize(gpui::size(gpui::px(800.), gpui::px(400.)));
    let first = sent_by(&harness, || core_sends(&harness, cx, tail(1..=3)));
    assert!(watches_tail(&first), "{first:?}");

    let covered = sent_by(&harness, || act(&root, cx, Session::logout));
    let none = format!("{:?}", Command::ViewingTail(None));
    assert!(covered.contains(&none), "told it is covered: {covered:?}");

    core_sends(&harness, cx, tail(1..=4));
    let after = sent_by(&harness, || act(&root, cx, Session::cancel_logout));
    assert!(watches_tail(&after), "{after:?}");
    let read = format!(
        "{:?}",
        Command::MarkRead {
            thread: chat1(),
            max_id: MsgId(4),
        }
    );
    assert!(after.contains(&read), "{after:?}");
}

/// A context menu covers a few rows, not the transcript: it does not count.
#[gpui::test]
fn a_context_menu_does_not_cover_the_pane(cx: &mut TestAppContext) {
    let (root, cx, harness) = chats_open(cx, 1);
    cx.simulate_resize(gpui::size(gpui::px(800.), gpui::px(400.)));
    act(&root, cx, |s, window, cx| {
        s.open_chat_menu(user(2), point(px(100.), px(100.)), window, cx);
    });
    assert!(read(&root, cx, |s| s.menu.is_some()));
    let sent = sent_by(&harness, || core_sends(&harness, cx, tail(1..=3)));
    assert!(marks_read(&sent), "{sent:?}");
    assert!(watches_tail(&sent), "{sent:?}");
}

fn focused(root: &Entity<Root>, cx: &mut VisualTestContext) -> (bool, bool) {
    let (composer, pane) = root.read_with(cx, |root, cx| match &root.state {
        State::Running(s) => (s.composer.focus_handle(cx), s.pane_focus.clone()),
        State::Failed(failure) => panic!("failed: {failure:?}"),
    });
    cx.update(|window, _| (composer.is_focused(window), pane.is_focused(window)))
}

/// A chat core says can no longer be written to.
fn read_only_dialogs() -> Event {
    let mut script = FakeScript::demo(2, 0).dialogs;
    for chat in &mut script {
        chat.can_send = false;
    }
    Event::DialogsReset(script)
}

fn signed_in_state() -> AuthState {
    AuthState::SignedIn {
        display_name: "Demo User".into(),
        new_login: None,
    }
}

fn not_started(trigger: LogoutTrigger) -> AuthState {
    AuthState::LogoutNotStarted {
        error: StorageError::Keychain,
        trigger,
    }
}

/// "Stay signed in" over a read-only chat: the pane has no message box, so
/// focus goes to the pane (review A68, N56).
#[gpui::test]
fn stay_signed_in_over_a_read_only_chat_focuses_the_pane(cx: &mut TestAppContext) {
    let (root, cx, harness) = chats_open(cx, 1);
    core_sends(&harness, cx, read_only_dialogs());
    assert_eq!(focused(&root, cx), (false, true), "the pane holds focus");

    core_says(&harness, cx, AuthState::LogoutStarting);
    core_says(&harness, cx, not_started(LogoutTrigger::User));
    core_says(&harness, cx, signed_in_state());
    assert_eq!(
        focused(&root, cx),
        (false, true),
        "the unrendered message box takes no focus"
    );
}

// Overlays belong to the account's screen.

fn menu_is_open(root: &Entity<Root>, cx: &mut VisualTestContext) -> bool {
    read(root, cx, |s| s.menu.is_some())
}

fn overlay_is_open(root: &Entity<Root>, cx: &mut VisualTestContext) -> bool {
    read(root, cx, Session::overlay_open)
}

fn root_owns_focus(root: &Entity<Root>, cx: &mut VisualTestContext) -> bool {
    root.read_with(cx, |root, _| root.overlay_focus().is_some())
}

fn open_menu(root: &Entity<Root>, cx: &mut VisualTestContext) {
    act(root, cx, |s, window, cx| {
        s.open_chat_menu(user(2), point(px(100.), px(100.)), window, cx);
    });
    assert!(menu_is_open(root, cx));
}

/// Server revocation with a menu open: the account's screens go, and the
/// menu goes with them, so Tab reaches the logout screen's Retry.
#[gpui::test]
fn a_revocation_closes_an_open_menu(cx: &mut TestAppContext) {
    let (root, cx, harness) = chats_open(cx, 1);
    open_menu(&root, cx);
    assert!(root_owns_focus(&root, cx));

    core_says(&harness, cx, AuthState::LogoutStarting);
    core_says(&harness, cx, not_started(LogoutTrigger::Server));
    assert!(!menu_is_open(&root, cx), "the menu is gone");
    assert!(!overlay_is_open(&root, cx));
    assert!(!root_owns_focus(&root, cx), "Tab steps the screen");

    core_says(&harness, cx, signed_in_state());
    assert!(!overlay_is_open(&root, cx));
    assert_eq!(focused(&root, cx), (true, false), "back in the message box");
}

/// The same with the logout prompt open: it closes through the normal path,
/// so its focus is released and nothing is owed.
#[gpui::test]
fn a_revocation_closes_the_logout_prompt_and_pays_the_focus_debt(cx: &mut TestAppContext) {
    let (root, cx, harness) = chats_open(cx, 1);
    act(&root, cx, Session::logout);
    assert!(read(&root, cx, Session::confirming_logout));

    core_says(&harness, cx, AuthState::LogoutStarting);
    assert!(!read(&root, cx, Session::confirming_logout));
    assert!(!overlay_is_open(&root, cx));
    assert!(!root_owns_focus(&root, cx));
    core_says(&harness, cx, not_started(LogoutTrigger::Server));
    assert!(!overlay_is_open(&root, cx));
    assert!(!root_owns_focus(&root, cx), "Tab reaches Retry");

    // Back on the chats, the box has focus; nothing was left owed.
    core_says(&harness, cx, signed_in_state());
    assert!(!overlay_is_open(&root, cx));
    assert_eq!(focused(&root, cx), (true, false));
    cx.simulate_input("hello");
    assert_eq!(composer_text(&root, cx), "hello");
}

/// Defensive: should a confirmation outlive its screen (the flag is raised
/// by hand here; the screen change itself closes it, as above), confirming
/// it closes it the normal way. It sends nothing, and it does not come back
/// when the signed-in screen does.
#[gpui::test]
fn confirming_a_stale_logout_prompt_closes_it_for_good(cx: &mut TestAppContext) {
    let (root, cx, harness) = chats_open(cx, 1);
    core_says(&harness, cx, AuthState::LogoutStarting);
    act(&root, cx, |s, _, _| s.open_logout_prompt());
    assert!(!read(&root, cx, Session::confirming_logout), "not shown");
    assert!(!overlay_is_open(&root, cx));

    let from = sent_count(&harness);
    act(&root, cx, Session::confirm_logout);
    assert!(commands_since(&harness, from).is_empty(), "nothing sent");
    core_says(&harness, cx, not_started(LogoutTrigger::User));
    core_says(&harness, cx, signed_in_state());
    assert!(
        !read(&root, cx, Session::confirming_logout),
        "gone for good"
    );
    assert!(!overlay_is_open(&root, cx));
    assert!(!root_owns_focus(&root, cx));
}

// Secrets (N59).

fn hash_field(root: &Entity<Root>, cx: &mut VisualTestContext) -> Entity<PasswordInput> {
    read(root, cx, |s| s.api_hash.clone())
}

const HASH: &str = "0123456789abcdef0123456789abcdef";

/// Types a hash into the entry's field and shows it.
fn type_and_reveal_hash(root: &Entity<Root>, cx: &mut VisualTestContext) {
    act(root, cx, |s, window, cx| {
        let handle = s.api_hash.focus_handle(cx);
        window.focus(&handle, cx);
    });
    cx.simulate_input(HASH);
    act(root, cx, |s, _, cx| s.toggle_hash_mask(cx));
    let field = hash_field(root, cx);
    assert_eq!(field.read_with(cx, |f, _| f.reveal()), Reveal::Shown);
    assert!(!field.read_with(cx, |f, _| f.secret().is_empty()));
}

fn assert_hash_gone(root: &Entity<Root>, cx: &mut VisualTestContext) {
    let field = hash_field(root, cx);
    assert!(field.read_with(cx, |f, _| f.secret().is_empty()), "wiped");
    assert_eq!(
        field.read_with(cx, |f, _| f.reveal()),
        Reveal::Hidden,
        "and hidden again"
    );
}

fn at_entry(cx: &mut TestAppContext) -> (Entity<Root>, &mut VisualTestContext, Harness) {
    let (root, cx, harness) = open(cx);
    let need = AuthState::NeedCredentials {
        error: None,
        source: None,
    };
    core_says(&harness, cx, need);
    act(&root, cx, Session::connect);
    act(&root, cx, Session::have_credentials);
    assert!(matches!(
        auth(&root, cx),
        AuthView::Onboarding(Onboarding::Entry { .. })
    ));
    (root, cx, harness)
}

#[gpui::test]
fn back_from_the_entry_wipes_the_hash_and_hides_it(cx: &mut TestAppContext) {
    let (root, cx, _harness) = at_entry(cx);
    type_and_reveal_hash(&root, cx);
    act(&root, cx, Session::back);
    assert_eq!(auth(&root, cx), AuthView::Onboarding(Onboarding::Guide));
    assert_hash_gone(&root, cx);
    act(&root, cx, Session::have_credentials);
    assert_hash_gone(&root, cx);
}

/// Core moves on from the entry (the saved pair is used): the hash goes.
#[gpui::test]
fn leaving_the_entry_for_a_saved_pair_wipes_the_hash(cx: &mut TestAppContext) {
    let (root, cx, harness) = at_entry(cx);
    type_and_reveal_hash(&root, cx);
    core_says(&harness, cx, AuthState::Phone { error: None });
    assert_hash_gone(&root, cx);
}

/// A logout wipes what a login left in the credential and code fields, even
/// ones no screen shows now.
#[gpui::test]
fn a_logout_wipes_the_hash_and_the_code(cx: &mut TestAppContext) {
    let (root, cx, harness) = chats_open(cx, 1);
    act(&root, cx, |s, window, cx| {
        s.api_hash.update(cx, |hash, cx| {
            hash.fill(HASH, cx);
            hash.set_reveal(Reveal::Shown, cx);
        });
        assert!(
            s.code
                .update(cx, |code, cx| code.put_text("123", window, cx))
        );
    });
    assert_eq!(code_text(&root, cx), "123");

    core_says(&harness, cx, AuthState::LoggingOut);
    assert_hash_gone(&root, cx);
    assert_eq!(code_text(&root, cx), "");
}

fn code_text(root: &Entity<Root>, cx: &mut VisualTestContext) -> String {
    root.read_with(cx, |root, cx| match &root.state {
        State::Running(s) => s.code.read(cx).text().to_owned(),
        State::Failed(failure) => panic!("failed: {failure:?}"),
    })
}

fn at_code(cx: &mut TestAppContext) -> (Entity<Root>, &mut VisualTestContext, Harness) {
    let (root, cx, harness) = open(cx);
    core_says(&harness, cx, AuthState::Phone { error: None });
    cx.simulate_input("+15550100");
    cx.simulate_keystrokes("enter");
    core_says(
        &harness,
        cx,
        AuthState::Code {
            phone_hint: abyss_core::api::mask_phone("+15550100"),
            sent: None,
            error: None,
        },
    );
    cx.simulate_input("123");
    assert_eq!(code_text(&root, cx), "123");
    (root, cx, harness)
}

#[gpui::test]
fn back_from_the_code_wipes_it(cx: &mut TestAppContext) {
    let (root, cx, harness) = at_code(cx);
    core_says(&harness, cx, AuthState::Phone { error: None });
    assert_eq!(code_text(&root, cx), "");
}

#[gpui::test]
fn shutdown_wipes_the_code(cx: &mut TestAppContext) {
    let (root, cx, _harness) = at_code(cx);
    root.update_in(cx, |root, window, cx| {
        assert!(!root.request_quit(window, cx));
    });
    cx.run_until_parked();
    assert_eq!(code_text(&root, cx), "");
}
