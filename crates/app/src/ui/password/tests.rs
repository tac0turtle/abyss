//! The password field through GPUI's test platform: typed text, the platform
//! text APIs, secure-input pairing, submit and cancel.

use std::sync::atomic::Ordering;

use gpui::{
    Context, Entity, EntityInputHandler, Focusable, TestAppContext, VisualTestContext, Window, div,
    prelude::*,
};

use super::secret::BULLET;
use super::secure_input::sys::ACTIVE;
use super::{PasswordEvent, PasswordInput};
use crate::ui::text_input::key_bindings;

// The secure-input counter is process-global; these tests touch it, so they
// run under one lock.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn active() -> isize {
    ACTIVE.load(Ordering::SeqCst)
}

fn setup(cx: &mut TestAppContext) -> (Entity<PasswordInput>, &mut VisualTestContext) {
    cx.update(|cx| cx.bind_keys(key_bindings()));
    let (input, cx) = cx.add_window_view(PasswordInput::new);
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    cx.update(|window, cx| window.focus(&input.focus_handle(cx), cx));
    cx.run_until_parked();
    (input, cx)
}

#[gpui::test]
fn platform_apis_only_see_bullets(cx: &mut TestAppContext) {
    let _serial = SERIAL.lock();
    let (input, cx) = setup(cx);
    cx.simulate_input("pä🙂");
    input.update_in(cx, |input, window, cx| {
        assert_eq!(input.text_length_utf16(window, cx), Some(3));
        let mut actual = None;
        let text = input.text_for_range(0..10, &mut actual, window, cx);
        assert_eq!(text.as_deref(), Some("•••"));
        assert_eq!(actual, Some(0..3));
        let sel = input.selected_text_range(false, window, cx).unwrap();
        assert_eq!(sel.range, 3..3);
        // Copy and cut are not handled: nothing reaches the clipboard.
    });
    cx.simulate_keystrokes("cmd-a cmd-c cmd-x");
    assert!(cx.read_from_clipboard().is_none());
    input.update_in(cx, |input, window, cx| {
        let mut actual = None;
        let text = input.text_for_range(0..3, &mut actual, window, cx).unwrap();
        assert!(text.chars().all(|c| c == BULLET));
        assert_eq!(input.edit.len(), 3);
    });
}

/// Task 6c: the `api_hash` field keeps hex only, and the platform text APIs
/// and the clipboard never see the hash, shown or hidden.
#[gpui::test]
fn api_hash_never_reaches_the_platform(cx: &mut TestAppContext) {
    let _serial = SERIAL.lock();
    cx.update(|cx| cx.bind_keys(key_bindings()));
    let (input, cx) = cx.add_window_view(PasswordInput::api_hash);
    cx.update(|window, cx| {
        window.activate_window();
        window.focus(&input.focus_handle(cx), cx);
    });
    cx.run_until_parked();
    cx.simulate_input("ab cd\n12");
    for reveal in [super::Reveal::Hidden, super::Reveal::Shown] {
        input.update(cx, |input, cx| input.set_reveal(reveal, cx));
        input.update_in(cx, |input, window, cx| {
            assert_eq!(&*input.secret(), b"abcd12");
            let mut actual = None;
            let text = input.text_for_range(0..99, &mut actual, window, cx);
            assert_eq!(text.as_deref(), Some("••••••"), "{reveal:?}");
            assert!(!format!("{:?}", input.edit).contains("abcd"));
        });
        cx.simulate_keystrokes("cmd-a cmd-c");
        assert!(cx.read_from_clipboard().is_none());
    }
    input.update(cx, PasswordInput::clear);
    input.update(cx, |input, _| assert!(input.secret().is_empty()));
}

/// Review N59: a wiped `api_hash` field is hidden again, so the next visit
/// never shows the old text, nor an empty field already revealed.
#[gpui::test]
fn clearing_the_api_hash_hides_it_again(cx: &mut TestAppContext) {
    let _serial = SERIAL.lock();
    cx.update(|cx| cx.bind_keys(key_bindings()));
    let (input, cx) = cx.add_window_view(PasswordInput::api_hash);
    input.update(cx, |input, cx| input.set_reveal(super::Reveal::Shown, cx));
    input.update(cx, PasswordInput::clear);
    input.update(cx, |input, _| {
        assert_eq!(input.reveal(), super::Reveal::Hidden);
    });
}

#[gpui::test]
fn dead_key_through_the_input_handler(cx: &mut TestAppContext) {
    let _serial = SERIAL.lock();
    let (input, cx) = setup(cx);
    input.update_in(cx, |input, window, cx| {
        input.replace_and_mark_text_in_range(None, "´", Some(1..1), window, cx);
        assert_eq!(input.marked_text_range(window, cx), Some(0..1));
        input.replace_text_in_range(None, "é", window, cx);
        assert_eq!(input.marked_text_range(window, cx), None);
        assert_eq!(&input.take_secret(cx)[..], "é".as_bytes());
    });
}

#[gpui::test]
fn submit_hands_over_and_wipes_cancel_wipes(cx: &mut TestAppContext) {
    let _serial = SERIAL.lock();
    let (input, cx) = setup(cx);
    let submitted = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = std::rc::Rc::clone(&submitted);
    cx.update(|_, cx| {
        cx.subscribe(&input, move |input, event: &PasswordEvent, cx| {
            if *event == PasswordEvent::Submit {
                let secret = input.update(cx, PasswordInput::take_secret);
                sink.borrow_mut().extend_from_slice(&secret);
            }
        })
        .detach();
    });
    cx.simulate_input("hunter2");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(&submitted.borrow()[..], b"hunter2");
    assert!(input.read_with(cx, |i, _| i.is_empty()));
    assert!(input.read_with(cx, |i, _| i.edit.secret().raw().iter().all(|b| *b == 0)));
    cx.simulate_input("again");
    cx.simulate_keystrokes("escape");
    assert!(input.read_with(cx, |i, _| i.is_empty()));
}

#[gpui::test]
fn secure_input_follows_focus_activation_and_drop(cx: &mut TestAppContext) {
    let _serial = SERIAL.lock();
    let base = active();
    let (input, cx) = setup(cx);
    assert_eq!(active(), base + 1, "focused field enables secure input");
    cx.update(Window::blur);
    cx.run_until_parked();
    assert_eq!(active(), base, "blur disables it");
    cx.update(|window, cx| window.focus(&input.focus_handle(cx), cx));
    cx.run_until_parked();
    assert_eq!(active(), base + 1);
    cx.deactivate_window();
    cx.run_until_parked();
    assert_eq!(active(), base, "an inactive window disables it");
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    assert_eq!(active(), base + 1);
}

/// Owns the field so the test can release it while it is focused.
struct Host {
    field: Option<Entity<PasswordInput>>,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        div().children(self.field.clone())
    }
}

#[gpui::test]
fn dropping_a_focused_field_disables_secure_input(cx: &mut TestAppContext) {
    let _serial = SERIAL.lock();
    let base = active();
    let (host, cx) = cx.add_window_view(|window, cx| Host {
        field: Some(cx.new(|cx| PasswordInput::new(window, cx))),
    });
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    let field = host.read_with(cx, |h, _| h.field.clone()).unwrap();
    cx.update(|window, cx| window.focus(&field.focus_handle(cx), cx));
    cx.run_until_parked();
    assert_eq!(active(), base + 1);
    let weak = field.downgrade();
    drop(field);
    host.update(cx, |h, cx| {
        h.field = None;
        cx.notify();
    });
    cx.run_until_parked();
    assert!(weak.upgrade().is_none());
    // Released entities are dropped on the next effect flush.
    cx.update(|_, _| {});
    assert_eq!(active(), base, "drop disables it");
}
