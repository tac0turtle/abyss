//! The main window through GPUI's test platform, against a same-thread core
//! double (GPUI's test scheduler rejects wakeups from core's real threads).

use std::cell::RefCell;
use std::rc::Rc;
use std::task::{self, Poll};

use abyss_core::api::{Command, Event};
use abyss_core::{SendError, ShutdownReport};
use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use gpui::{Entity, TestAppContext, VisualTestContext};

use super::bridge::{CoreLink, DRAIN_MAX, EventSource, ShutdownAck, drain};
use super::shutdown::Closing;
use super::{Failure, Root, State};
use crate::launch::LaunchError;

/// Commands the UI sent, and whether the queue reports full.
#[derive(Default)]
struct Sent {
    commands: Vec<Command>,
    full: bool,
}

/// The command half of the double.
struct TestLink {
    sent: Rc<RefCell<Sent>>,
    ack: Option<oneshot::Receiver<ShutdownReport>>,
}

/// The event half of the double, fed by [`Harness::events`].
struct TestEvents(mpsc::UnboundedReceiver<Event>);

impl CoreLink for TestLink {
    fn send(&self, cmd: Command) -> Result<(), SendError> {
        let mut sent = self.sent.borrow_mut();
        if sent.full {
            return Err(SendError::Full(cmd));
        }
        sent.commands.push(cmd);
        Ok(())
    }

    fn begin_shutdown(&mut self) -> Option<ShutdownAck> {
        let ack = self.ack.take()?;
        Some(Box::pin(async move { ack.await.ok() }))
    }
}

impl EventSource for TestEvents {
    fn poll_recv(&mut self, task: &mut task::Context<'_>) -> Poll<Option<Event>> {
        self.0.poll_next_unpin(task)
    }

    fn try_recv(&mut self) -> Option<Event> {
        self.0.try_recv().ok()
    }
}

#[cfg_attr(
    not(feature = "fake"),
    expect(
        dead_code,
        reason = "only the fake-backed tests send commands and acks"
    )
)]
struct Harness {
    events: mpsc::UnboundedSender<Event>,
    sent: Rc<RefCell<Sent>>,
    ack: Option<oneshot::Sender<ShutdownReport>>,
}

fn link() -> (Box<dyn CoreLink>, Box<dyn EventSource>, Harness) {
    let (events_tx, events) = mpsc::unbounded();
    let (ack_tx, ack) = oneshot::channel();
    let sent = Rc::new(RefCell::new(Sent::default()));
    let link = TestLink {
        sent: Rc::clone(&sent),
        ack: Some(ack),
    };
    let harness = Harness {
        events: events_tx,
        sent,
        ack: Some(ack_tx),
    };
    (Box::new(link), Box::new(TestEvents(events)), harness)
}

fn closing(root: &Entity<Root>, cx: &mut VisualTestContext) -> Option<Closing> {
    root.read_with(cx, |root, _| match &root.state {
        State::Running(s) => Some(s.closing),
        State::Failed(_) => None,
    })
}

#[gpui::test]
fn launch_errors_show_the_error_screen_and_close_freely(cx: &mut TestAppContext) {
    let error = LaunchError::FakeNotBuilt;
    let (root, cx) = cx.add_window_view(|window, cx| Root::new(Err(error), window, cx));
    cx.run_until_parked();
    root.read_with(cx, |root, _| {
        assert!(matches!(
            root.state,
            State::Failed(Failure::Launch(LaunchError::FakeNotBuilt))
        ));
    });
    assert_eq!(closing(&root, cx), None);
    assert!(
        cx.simulate_close(),
        "nothing to wait for: the window closes"
    );
}

#[test]
fn drain_takes_at_most_drain_max_per_wake() {
    let (_link, mut events, harness) = link();
    for _ in 0..DRAIN_MAX + 10 {
        harness
            .events
            .unbounded_send(Event::DialogsReset(Vec::new()))
            .unwrap();
    }
    let first = events.try_recv().unwrap();
    assert_eq!(drain(events.as_mut(), first).len(), DRAIN_MAX);
    let first = events.try_recv().unwrap();
    assert_eq!(drain(events.as_mut(), first).len(), 10);
}

/// The panel works in any state, here on the error screen. No store is set
/// up in tests, so the choice stays in memory.
#[gpui::test]
fn appearance_choice_rethemes_the_window_and_reset_restores_it(cx: &mut TestAppContext) {
    use crate::ui::theme::{Choice, Hue, Mode, Settings, Themes, Tint};

    let error = LaunchError::FakeNotBuilt;
    let (root, cx) = cx.add_window_view(|window, cx| Root::new(Err(error), window, cx));
    cx.run_until_parked();
    let showing = cx.update(|window, _| Mode::of(window));
    let before = cx.update(|window, cx| Themes::of(window, cx));
    assert_eq!(cx.update(|_, cx| Themes::settings(cx)), Settings::default());

    root.update(cx, Root::toggle_appearance);
    cx.run_until_parked();
    root.read_with(cx, |root, _| assert!(root.appearance_open));

    let choice = Choice {
        hue: Hue::all().next().unwrap(),
        tint: Tint::all().last().unwrap(),
    };
    cx.update(|_, cx| Themes::set(Settings::default().with(showing, choice), cx));
    cx.run_until_parked();
    assert_ne!(cx.update(|window, cx| Themes::of(window, cx)), before);
    assert_eq!(cx.update(|_, cx| Themes::settings(cx)).get(showing), choice);

    cx.update(|_, cx| Themes::set(Settings::default(), cx));
    cx.run_until_parked();
    assert_eq!(cx.update(|window, cx| Themes::of(window, cx)), before);

    root.update(cx, Root::toggle_appearance);
    root.read_with(cx, |root, _| assert!(!root.appearance_open));
}

/// The picker switches colourways by click; abyss's hues and Reset belong
/// to abyss alone, and Reset never leaves it.
#[gpui::test]
fn colourway_picker_switches_and_reset_stays_in_abyss(cx: &mut TestAppContext) {
    use gpui::Modifiers;

    use crate::ui::theme::colourway::Colourway;
    use crate::ui::theme::{Choice, Hue, Mode, Settings, Theme, Themes, Tint};

    let (root, cx) =
        cx.add_window_view(|window, cx| Root::new(Err(LaunchError::FakeNotBuilt), window, cx));
    root.update(cx, Root::toggle_appearance);
    cx.run_until_parked();
    let showing = cx.update(|window, _| Mode::of(window));
    let click = |cx: &mut VisualTestContext, id: &'static str| {
        let at = cx.debug_bounds(id).unwrap().center();
        cx.simulate_mouse_move(at, None, Modifiers::none());
        cx.simulate_click(at, Modifiers::none());
        cx.run_until_parked();
    };
    let shown = |cx: &mut VisualTestContext| cx.update(|window, cx| Themes::of(window, cx));
    assert_eq!(cx.update(|_, cx| Themes::colourway(cx)), Colourway::Abyss);
    assert!(
        cx.debug_bounds("appearance-reset").is_some(),
        "abyss resets"
    );

    let choice = Choice {
        hue: Hue::all().next().unwrap(),
        tint: Tint::all().last().unwrap(),
    };
    let hues = Settings::default().with(showing, choice);
    cx.update(|_, cx| Themes::set(hues, cx));
    cx.run_until_parked();

    for (colourway, id) in [
        (Colourway::Ultramarine, "colourway-ultramarine"),
        (Colourway::Sky, "colourway-sky"),
    ] {
        click(cx, id);
        assert_eq!(cx.update(|_, cx| Themes::colourway(cx)), colourway);
        let expected = Theme::new(showing, colourway, choice);
        assert_eq!(shown(cx), expected);
        assert!(cx.debug_bounds("appearance-reset").is_none(), "abyss only");
        assert_eq!(cx.update(|_, cx| Themes::settings(cx)), hues, "kept");
    }

    click(cx, "colourway-abyss");
    assert_eq!(shown(cx), Theme::new(showing, Colourway::Abyss, choice));
    click(cx, "appearance-reset");
    assert_eq!(cx.update(|_, cx| Themes::colourway(cx)), Colourway::Abyss);
    assert_eq!(cx.update(|_, cx| Themes::settings(cx)), Settings::default());
    assert_eq!(
        shown(cx),
        Theme::new(showing, Colourway::Abyss, Settings::default().get(showing))
    );
}

#[gpui::test]
fn settings_stays_open_inside_and_closes_on_a_backdrop_click(cx: &mut TestAppContext) {
    use gpui::{Modifiers, point, px};

    let (root, cx) =
        cx.add_window_view(|window, cx| Root::new(Err(LaunchError::FakeNotBuilt), window, cx));
    root.update(cx, Root::toggle_appearance);
    cx.run_until_parked();
    let panel = cx.debug_bounds("settings-panel").unwrap();
    // Blank padding inside the card must not bubble to the backdrop.
    let inside = panel.origin + point(px(8.), px(8.));
    cx.simulate_mouse_move(inside, None, Modifiers::none());
    cx.simulate_click(inside, Modifiers::none());
    cx.run_until_parked();
    root.read_with(cx, |root, _| assert!(root.appearance_open));

    let outside = point(px(2.), panel.center().y);
    assert!(!panel.contains(&outside));
    cx.simulate_mouse_move(outside, None, Modifiers::none());
    cx.simulate_click(outside, Modifiers::none());
    cx.run_until_parked();
    root.read_with(cx, |root, _| assert!(!root.appearance_open));
}

#[cfg(feature = "fake")]
mod fake {
    use abyss_core::api::{AuthState, PasswordBuf, UserError};
    use abyss_core::backend::fake::{FakeScript, summary};
    use abyss_core::model::{ChatId, PeerKind, ThreadKey};

    use super::*;
    use crate::shell::auth::AuthView;
    use crate::shell::session::Started;
    use crate::ui::text_input;

    mod folder_menu;
    mod forum;
    mod quit;
    mod tabs;

    fn open(cx: &mut TestAppContext) -> (Entity<Root>, &mut VisualTestContext, Harness) {
        cx.update(|cx| cx.bind_keys(text_input::key_bindings()));
        let (commands, events, harness) = link();
        let started = Started {
            commands,
            events,
            fake: None,
        };
        let (root, cx) =
            cx.add_window_view(|window, cx| Root::with_started(Ok(started), window, cx));
        cx.run_until_parked();
        (root, cx, harness)
    }

    fn read<R>(
        root: &Entity<Root>,
        cx: &mut VisualTestContext,
        f: impl FnOnce(&super::super::Session) -> R,
    ) -> R {
        root.read_with(cx, |root, _| match &root.state {
            State::Running(s) => f(s),
            State::Failed(failure) => panic!("failed: {failure:?}"),
        })
    }

    fn auth(root: &Entity<Root>, cx: &mut VisualTestContext) -> AuthView {
        read(root, cx, |s| s.auth.clone())
    }

    /// Core moves to `state`.
    fn core_says(harness: &Harness, cx: &mut VisualTestContext, state: AuthState) {
        harness.events.unbounded_send(Event::Auth(state)).unwrap();
        cx.run_until_parked();
    }

    fn last_command(harness: &Harness) -> Option<Command> {
        let sent = harness.sent.borrow();
        sent.commands.last().map(|c| match c {
            // `Command` is not `Clone` (it may carry a password).
            Command::SubmitPhone(p) => Command::SubmitPhone(p.clone()),
            Command::SubmitCode(c) => Command::SubmitCode(c.clone()),
            Command::SubmitPassword(p) => {
                Command::SubmitPassword(PasswordBuf::from_slice(p.as_bytes()).unwrap())
            }
            Command::RetryPasswordParams => Command::RetryPasswordParams,
            Command::SubmitCredentials { api_id, api_hash } => Command::SubmitCredentials {
                api_id: *api_id,
                api_hash: api_hash.clone(),
            },
            Command::Back => Command::Back,
            Command::ResendCode => Command::ResendCode,
            Command::ReloadCredentials => Command::ReloadCredentials,
            Command::ForgetStoredCredentials => Command::ForgetStoredCredentials,
            Command::Logout => Command::Logout,
            // A stand-in for commands these tests don't inspect.
            _ => Command::LoadDialogs,
        })
    }

    fn sent_count(harness: &Harness) -> usize {
        harness.sent.borrow().commands.len()
    }

    fn act(
        root: &Entity<Root>,
        cx: &mut VisualTestContext,
        f: impl FnOnce(&mut super::super::Session, &mut gpui::Window, &mut gpui::Context<'_, Root>),
    ) {
        root.update_in(cx, |root, window, cx| {
            root.with_session(|s, cx| f(s, window, cx), cx);
        });
        cx.run_until_parked();
    }

    /// The router follows core's states; each form sends its command.
    fn sign_in(root: &Entity<Root>, cx: &mut VisualTestContext, harness: &Harness) {
        assert_eq!(auth(root, cx), AuthView::Connecting);
        core_says(harness, cx, AuthState::Phone { error: None });
        assert_eq!(auth(root, cx), AuthView::Phone { error: None });
        cx.simulate_input("+15550100");
        cx.simulate_keystrokes("enter");
        assert!(matches!(last_command(harness), Some(Command::SubmitPhone(p)) if p == "+15550100"));
        let phone_hint = abyss_core::api::mask_phone("+15550100");
        core_says(
            harness,
            cx,
            AuthState::Code {
                phone_hint: phone_hint.clone(),
                sent: None,
                error: None,
            },
        );
        assert_eq!(
            auth(root, cx),
            AuthView::Code {
                phone_hint: phone_hint.into(),
                sent: None,
                error: None
            }
        );
        cx.simulate_input("12345");
        cx.simulate_keystrokes("enter");
        assert!(matches!(last_command(harness), Some(Command::SubmitCode(c)) if c == "12345"));
        core_says(
            harness,
            cx,
            AuthState::Password {
                hint: Some("demo".into()),
                error: None,
            },
        );
        assert!(matches!(auth(root, cx), AuthView::Password { .. }));
        cx.simulate_input("hunter2");
        cx.simulate_keystrokes("enter");
        assert!(matches!(
            last_command(harness),
            Some(Command::SubmitPassword(p)) if p.as_bytes() == b"hunter2"
        ));
        core_says(
            harness,
            cx,
            AuthState::SignedIn {
                display_name: "Demo User".into(),
                new_login: None,
            },
        );
        assert_eq!(auth(root, cx), AuthView::SignedIn);
    }

    /// Welcome → guide → entry (UI-only), Back locally, inline validation,
    /// then `SubmitCredentials` with the fields cleared.
    #[gpui::test]
    fn onboarding_entry_submits_credentials(cx: &mut TestAppContext) {
        use crate::shell::auth::{Onboarding, Stored};
        let (root, cx, harness) = open(cx);
        let need = AuthState::NeedCredentials {
            error: None,
            source: None,
        };
        core_says(&harness, cx, need);
        assert_eq!(auth(&root, cx), AuthView::Onboarding(Onboarding::Welcome));
        act(&root, cx, super::super::Session::connect);
        assert_eq!(auth(&root, cx), AuthView::Onboarding(Onboarding::Guide));
        act(&root, cx, super::super::Session::have_credentials);
        let entry = AuthView::Onboarding(Onboarding::Entry {
            error: None,
            stored: Stored::No,
        });
        assert_eq!(auth(&root, cx), entry);
        act(&root, cx, super::super::Session::back);
        assert_eq!(auth(&root, cx), AuthView::Onboarding(Onboarding::Guide));
        assert_eq!(sent_count(&harness), 0, "UI-only Back sends nothing");
        act(&root, cx, super::super::Session::have_credentials);

        // api_id is focused; Enter with no hash is refused inline.
        cx.simulate_input("12345");
        cx.simulate_keystrokes("enter");
        assert_eq!(sent_count(&harness), 0);
        assert_eq!(
            read(&root, cx, |s| s.entry_error.clone()),
            Some(crate::ui::copy::onboarding::HASH_INVALID.into())
        );
        act(&root, cx, |s, w, cx| {
            use gpui::Focusable;
            let handle = s.api_hash.focus_handle(cx);
            w.focus(&handle, cx);
        });
        let hash = "0123456789abcdef0123456789ABCDEF";
        cx.simulate_input(hash);
        // Masked: the field shows bullets, never the hash.
        let shown = read(&root, cx, |s| s.api_hash.clone());
        assert_eq!(
            shown.read_with(cx, |i, _| i.reveal()),
            crate::ui::password::Reveal::Hidden
        );
        cx.simulate_keystrokes("enter");
        let Some(Command::SubmitCredentials { api_id, api_hash }) = last_command(&harness) else {
            panic!("expected SubmitCredentials");
        };
        assert_eq!(api_id, 12345);
        assert_eq!(api_hash.expose(), hash.to_lowercase());
        assert_eq!(read(&root, cx, |s| s.entry_error.clone()), None);
        let api_id_left = read(&root, cx, |s| s.api_id.clone());
        assert_eq!(api_id_left.read_with(cx, |i, _| i.text().to_owned()), "");
        assert!(
            shown.read_with(cx, |i, _| i.secret().is_empty()),
            "wiped once admitted"
        );
    }

    /// Back on login screens goes to core; never away from an env pair.
    #[gpui::test]
    fn back_rules(cx: &mut TestAppContext) {
        let (root, cx, harness) = open(cx);
        core_says(&harness, cx, AuthState::Phone { error: None });
        act(&root, cx, super::super::Session::back);
        assert!(matches!(last_command(&harness), Some(Command::Back)));
        let sent = sent_count(&harness);
        let env = Event::CredentialSource(Some(abyss_core::api::CredentialSource::Env));
        harness.events.unbounded_send(env).unwrap();
        cx.run_until_parked();
        act(&root, cx, super::super::Session::back);
        assert_eq!(sent_count(&harness), sent, "no Back from an env pair");
        let code = AuthState::Code {
            phone_hint: "+1 •••• •••00".into(),
            sent: None,
            error: None,
        };
        core_says(&harness, cx, code);
        act(&root, cx, super::super::Session::back);
        assert!(matches!(last_command(&harness), Some(Command::Back)));
        core_says(&harness, cx, AuthState::Verifying { error: None });
        let sent = sent_count(&harness);
        act(&root, cx, super::super::Session::back);
        assert_eq!(sent_count(&harness), sent, "no Back while verifying");
    }

    /// A login in this run shows Done (with the 2FA recommendation), then
    /// the chats; Logout asks first and sends `Command::Logout` only once
    /// confirmed (task 6c).
    #[gpui::test]
    fn done_then_chats_then_logout(cx: &mut TestAppContext) {
        use crate::shell::auth::Saved;
        let (root, cx, harness) = open(cx);
        let signed_in = AuthState::SignedIn {
            display_name: String::new(),
            new_login: Some(abyss_core::api::NewLogin {
                has_password: Some(false),
            }),
        };
        core_says(&harness, cx, signed_in);
        assert_eq!(
            auth(&root, cx),
            AuthView::Done {
                has_password: Some(false),
                saved: Saved::Yes
            }
        );
        act(&root, cx, super::super::Session::open_chats);
        assert_eq!(auth(&root, cx), AuthView::SignedIn);
        let before = sent_count(&harness);
        act(&root, cx, super::super::Session::logout);
        assert_eq!(sent_count(&harness), before, "Logout asks first");
        assert!(read(&root, cx, super::super::Session::confirming_logout));
        act(&root, cx, super::super::Session::cancel_logout);
        assert!(!read(&root, cx, super::super::Session::confirming_logout));
        assert_eq!(sent_count(&harness), before, "cancel sends nothing");
        act(&root, cx, super::super::Session::logout);
        // A refused logout keeps the prompt up for another try.
        harness.sent.borrow_mut().full = true;
        act(&root, cx, super::super::Session::confirm_logout);
        assert!(read(&root, cx, super::super::Session::confirming_logout));
        harness.sent.borrow_mut().full = false;
        act(&root, cx, super::super::Session::confirm_logout);
        assert!(matches!(last_command(&harness), Some(Command::Logout)));
        assert!(!read(&root, cx, super::super::Session::confirming_logout));
        core_says(&harness, cx, AuthState::LoggingOut);
        assert_eq!(auth(&root, cx), AuthView::LoggingOut);
    }

    /// Task 6c: a failed local deletion shows "Logout incomplete" (never
    /// "Signed out"); Retry sends `Logout` again once admitted.
    #[gpui::test]
    fn logout_incomplete_offers_a_retry(cx: &mut TestAppContext) {
        use abyss_core::api::{LocalDeletion, RemoteLogout, StorageError};
        let (root, cx, harness) = open(cx);
        let failed = AuthState::LoggedOut {
            remote: RemoteLogout::Revoked,
            local: LocalDeletion::Failed(StorageError::Internal),
        };
        core_says(&harness, cx, failed);
        let incomplete = AuthView::LogoutIncomplete {
            remote: RemoteLogout::Revoked,
        };
        assert_eq!(auth(&root, cx), incomplete);
        harness.sent.borrow_mut().full = true;
        act(&root, cx, super::super::Session::retry_logout);
        assert_eq!(auth(&root, cx), incomplete, "refused: still incomplete");
        harness.sent.borrow_mut().full = false;
        act(&root, cx, super::super::Session::retry_logout);
        assert!(matches!(last_command(&harness), Some(Command::Logout)));
        assert_eq!(auth(&root, cx), AuthView::LoggingOut);
        let done = AuthState::LoggedOut {
            remote: RemoteLogout::Revoked,
            local: LocalDeletion::Deleted,
        };
        core_says(&harness, cx, done);
        assert_eq!(auth(&root, cx), AuthView::Connecting);
    }

    /// Task 6c: a refused auth command changes nothing; the input stays for
    /// another submit.
    #[gpui::test]
    fn refused_auth_commands_keep_the_input(cx: &mut TestAppContext) {
        let (root, cx, harness) = open(cx);
        let code = AuthState::Code {
            phone_hint: "+1 •••• •••00".into(),
            sent: None,
            error: Some(UserError::Rejected),
        };
        core_says(&harness, cx, code);
        harness.sent.borrow_mut().full = true;
        cx.simulate_input("12345");
        cx.simulate_keystrokes("enter");
        assert_eq!(sent_count(&harness), 0);
        let kept = read(&root, cx, |s| s.code.clone());
        assert_eq!(kept.read_with(cx, |i, _| i.text().to_owned()), "12345");
        assert!(
            matches!(auth(&root, cx), AuthView::Code { error: Some(_), .. }),
            "the error stays until core takes the code"
        );
        assert_eq!(
            read(&root, cx, |s| s.notice.clone()),
            Some(crate::ui::copy::user_error(UserError::Busy))
        );
        harness.sent.borrow_mut().full = false;
        cx.simulate_keystrokes("enter");
        assert!(matches!(last_command(&harness), Some(Command::SubmitCode(c)) if c == "12345"));
        assert_eq!(kept.read_with(cx, |i, _| i.text().to_owned()), "");

        core_says(
            &harness,
            cx,
            AuthState::Password {
                hint: None,
                error: None,
            },
        );
        harness.sent.borrow_mut().full = true;
        cx.simulate_input("pw");
        cx.simulate_keystrokes("enter");
        let field = read(&root, cx, |s| s.password.clone());
        assert_eq!(&*field.read_with(cx, |i, _| i.secret()), b"pw", "kept");
        harness.sent.borrow_mut().full = false;
        cx.simulate_keystrokes("enter");
        assert!(matches!(
            last_command(&harness),
            Some(Command::SubmitPassword(p)) if p.as_bytes() == b"pw"
        ));
        assert!(field.read_with(cx, |i, _| i.secret().is_empty()), "wiped");
    }

    #[gpui::test]
    fn resend_is_sent_from_the_code_screen(cx: &mut TestAppContext) {
        let (root, cx, harness) = open(cx);
        act(&root, cx, super::super::Session::resend_code);
        assert_eq!(sent_count(&harness), 0, "no resend outside the code screen");
        let code = AuthState::Code {
            phone_hint: "+1 •••• •••00".into(),
            sent: None,
            error: None,
        };
        core_says(&harness, cx, code);
        act(&root, cx, super::super::Session::resend_code);
        assert!(matches!(last_command(&harness), Some(Command::ResendCode)));
    }

    #[gpui::test]
    fn unfinished_logout_at_shutdown_says_so(cx: &mut TestAppContext) {
        let (root, cx, mut harness) = open(cx);
        assert!(!cx.simulate_close());
        let report = ShutdownReport {
            save: abyss_core::api::SaveStatus::Confirmed,
            logout: Some(abyss_core::api::LogoutStatus::Incomplete),
        };
        harness.ack.take().unwrap().send(report).unwrap();
        cx.run_until_parked();
        assert_eq!(closing(&root, cx), Some(Closing::LogoutUnfinished));
        assert!(cx.simulate_close());
    }

    #[gpui::test]
    fn used_up_password_token_asks_for_new_params_first(cx: &mut TestAppContext) {
        let (root, cx, harness) = open(cx);
        core_says(
            &harness,
            cx,
            AuthState::Password {
                hint: Some("h".into()),
                error: None,
            },
        );
        core_says(
            &harness,
            cx,
            AuthState::PasswordTokenUnavailable {
                error: UserError::Network,
            },
        );
        assert_eq!(
            auth(&root, cx),
            AuthView::Password {
                hint: Some("h".into()),
                error: Some(UserError::Network)
            }
        );
        cx.simulate_input("pw");
        cx.simulate_keystrokes("enter");
        assert!(matches!(
            last_command(&harness),
            Some(Command::RetryPasswordParams)
        ));
        core_says(
            &harness,
            cx,
            AuthState::Password {
                hint: Some("h".into()),
                error: None,
            },
        );
        cx.simulate_keystrokes("enter");
        assert!(matches!(
            last_command(&harness),
            Some(Command::SubmitPassword(p)) if p.as_bytes() == b"pw"
        ));
    }

    #[gpui::test]
    fn states_without_a_screen_show_a_notice(cx: &mut TestAppContext) {
        let (root, cx, harness) = open(cx);
        core_says(
            &harness,
            cx,
            AuthState::SignedInUnsaved {
                display_name: String::new(),
                new_login: None,
            },
        );
        assert_eq!(auth(&root, cx), AuthView::SignedIn);
        assert_eq!(
            read(&root, cx, |s| s.notice.clone()),
            Some(crate::ui::copy::SIGNED_IN_UNSAVED.into())
        );
        core_says(&harness, cx, AuthState::SignUpRequired);
        assert_eq!(auth(&root, cx), AuthView::SignUpRequired);
    }

    #[gpui::test]
    fn demo_login_then_chats_then_send(cx: &mut TestAppContext) {
        let (root, cx, harness) = open(cx);
        sign_in(&root, cx, &harness);
        let script = FakeScript::demo(3, 0);
        harness
            .events
            .unbounded_send(Event::DialogsReset(script.dialogs))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(read(&root, cx, |s| s.shell.list.as_slice().len()), 3);

        let chat = ChatId {
            kind: PeerKind::User,
            id: 1,
        };
        root.update_in(cx, |root, window, cx| {
            root.with_session(|s, cx| s.select_chat(chat, window, cx), cx);
        });
        cx.run_until_parked();
        cx.simulate_input("hello");
        cx.simulate_keystrokes("shift-enter");
        cx.simulate_input("there");
        cx.simulate_keystrokes("enter");
        let sent = harness.sent.borrow();
        // The short list also asks for the next dialogs page (LoadDialogs).
        let [
            ..,
            Command::OpenThread(_),
            Command::SendText {
                thread: to, text, ..
            },
        ] = sent.commands.as_slice()
        else {
            panic!("expected OpenThread, then one SendText");
        };
        assert_eq!(*to, ThreadKey::whole(chat));
        assert_eq!(text, "hello\nthere");
        drop(sent);
        // Live updates apply.
        let mut bumped = summary(
            ChatId {
                kind: PeerKind::User,
                id: 2,
            },
            "Bumped",
        );
        bumped.read.unread = 4;
        // A new message (a newer id) moves the chat up; read-state changes
        // alone do not.
        bumped.last_date = i64::from(i32::MAX);
        bumped.last_id = Some(abyss_core::model::MsgId(i32::MAX));
        harness
            .events
            .unbounded_send(Event::ChatBumped(bumped))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            read(&root, cx, |s| s.shell.list.as_slice()[0].title.clone()),
            "Bumped"
        );
    }

    #[gpui::test]
    fn full_queue_shows_busy(cx: &mut TestAppContext) {
        let (root, cx, harness) = open(cx);
        sign_in(&root, cx, &harness);
        harness.sent.borrow_mut().full = true;
        let script = FakeScript::demo(1, 0);
        harness
            .events
            .unbounded_send(Event::DialogsReset(script.dialogs))
            .unwrap();
        cx.run_until_parked();
        let chat = ChatId {
            kind: PeerKind::User,
            id: 1,
        };
        root.update_in(cx, |root, window, cx| {
            root.with_session(|s, cx| s.select_chat(chat, window, cx), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            read(&root, cx, |s| s.shell.selected()),
            None,
            "a refused OpenThread opens nothing"
        );
        harness.sent.borrow_mut().full = false;
        root.update_in(cx, |root, window, cx| {
            root.with_session(|s, cx| s.select_chat(chat, window, cx), cx);
        });
        cx.run_until_parked();
        harness.sent.borrow_mut().full = true;
        cx.simulate_input("hi");
        cx.simulate_keystrokes("enter");
        let notice = read(&root, cx, |s| s.notice.clone());
        assert_eq!(notice, Some(crate::ui::copy::user_error(UserError::Busy)));
        let draft = root.read_with(cx, |root, cx| match &root.state {
            State::Running(s) => s.composer.read(cx).text().to_owned(),
            State::Failed(_) => String::new(),
        });
        assert_eq!(draft, "hi", "the draft stays in the composer");
        assert_eq!(read(&root, cx, |s| s.shell.row_count()), 0, "no echo");
        harness.sent.borrow_mut().full = false;
        cx.simulate_keystrokes("enter");
        assert_eq!(read(&root, cx, |s| s.shell.row_count()), 1, "sent now");
        assert!(matches!(
            harness.sent.borrow().commands.last(),
            Some(Command::SendText { text, .. }) if text == "hi"
        ));
    }

    /// Task 6j: a send core refused (`SendRefused`) keeps its text. Put
    /// back never overwrites typed text; into an empty box it restores the
    /// text whole and the echo goes.
    #[gpui::test]
    fn a_refused_send_is_put_back_into_an_empty_box(cx: &mut TestAppContext) {
        let (root, cx, harness) = open(cx);
        sign_in(&root, cx, &harness);
        let script = FakeScript::demo(1, 0);
        harness
            .events
            .unbounded_send(Event::DialogsReset(script.dialogs))
            .unwrap();
        cx.run_until_parked();
        let chat = ChatId {
            kind: PeerKind::User,
            id: 1,
        };
        root.update_in(cx, |root, window, cx| {
            root.with_session(|s, cx| s.select_chat(chat, window, cx), cx);
        });
        cx.run_until_parked();
        cx.simulate_input("keep me");
        cx.simulate_keystrokes("enter");
        let local_id = match harness.sent.borrow().commands.last() {
            Some(Command::SendText { local_id, .. }) => *local_id,
            other => panic!("{other:?}"),
        };
        harness
            .events
            .unbounded_send(Event::SendRefused { local_id })
            .unwrap();
        cx.run_until_parked();
        let draft = |cx: &mut gpui::VisualTestContext| {
            root.read_with(cx, |root, cx| match &root.state {
                State::Running(s) => s.composer.read(cx).text().to_owned(),
                State::Failed(_) => String::new(),
            })
        };
        cx.simulate_input("typed");
        root.update_in(cx, |root, window, cx| {
            root.with_session(|s, cx| s.put_back(local_id, window, cx), cx);
        });
        assert_eq!(draft(cx), "typed", "typed text is never overwritten");
        assert_eq!(read(&root, cx, |s| s.shell.row_count()), 1, "still kept");
        root.update_in(cx, |root, window, cx| {
            root.with_session(
                |s, cx| s.composer.update(cx, |c, cx| c.clear(window, cx)),
                cx,
            );
            root.with_session(|s, cx| s.put_back(local_id, window, cx), cx);
        });
        assert_eq!(draft(cx), "keep me");
        assert_eq!(read(&root, cx, |s| s.shell.row_count()), 0);
    }

    /// Signed in with two chats listed and chat `id` open.
    fn chats_open(
        cx: &mut TestAppContext,
        id: i64,
    ) -> (Entity<Root>, &mut VisualTestContext, Harness) {
        let (root, cx, harness) = open(cx);
        sign_in(&root, cx, &harness);
        let script = FakeScript::demo(2, 0);
        harness
            .events
            .unbounded_send(Event::DialogsReset(script.dialogs))
            .unwrap();
        cx.run_until_parked();
        select(&root, cx, id);
        (root, cx, harness)
    }

    fn select(root: &Entity<Root>, cx: &mut VisualTestContext, id: i64) {
        let chat = ChatId {
            kind: PeerKind::User,
            id,
        };
        root.update_in(cx, |root, window, cx| {
            root.with_session(|s, cx| s.select_chat(chat, window, cx), cx);
        });
        cx.run_until_parked();
    }

    fn composer_text(root: &Entity<Root>, cx: &mut VisualTestContext) -> String {
        root.read_with(cx, |root, cx| match &root.state {
            State::Running(s) => s.composer.read(cx).text().to_owned(),
            State::Failed(_) => String::new(),
        })
    }

    /// Enter with `text` in the box; returns the send's id.
    fn submit_text(
        cx: &mut VisualTestContext,
        harness: &Harness,
        text: &str,
    ) -> abyss_core::model::LocalId {
        cx.simulate_input(text);
        cx.simulate_keystrokes("enter");
        match harness.sent.borrow().commands.last() {
            Some(Command::SendText { local_id, .. }) => *local_id,
            other => panic!("{other:?}"),
        }
    }

    fn user(id: i64) -> ChatId {
        ChatId {
            kind: PeerKind::User,
            id,
        }
    }

    fn selected(root: &Entity<Root>, cx: &mut VisualTestContext) -> Option<ChatId> {
        read(root, cx, |s| s.shell.selected().map(|t| t.chat))
    }

    fn screen(root: &Entity<Root>, cx: &mut VisualTestContext) -> crate::shell::view::Screen {
        read(root, cx, super::super::Session::screen)
    }

    /// Task 6m, finding 1: a pending send in chat 1, then a logout overlay
    /// (`state`), then core's stream closes. The overlay gives way to the
    /// local recovery view under `banner`; navigation and Copy reach the
    /// text; nothing is asked of core.
    fn recovery_behind_a_logout_overlay(cx: &mut TestAppContext, state: AuthState, banner: &str) {
        let (root, cx, harness) = chats_open(cx, 1);
        let id = submit_text(cx, &harness, "pending in 1");
        core_says(&harness, cx, state);
        assert_eq!(
            screen(&root, cx),
            crate::shell::view::Screen::Auth,
            "the overlay"
        );
        harness.events.close_channel();
        cx.run_until_parked();
        assert_eq!(screen(&root, cx), crate::shell::view::Screen::Recovery);
        assert_eq!(
            read(&root, cx, super::super::Session::stopped_banner),
            banner
        );
        let from = harness.sent.borrow().commands.len();
        select(&root, cx, 2);
        select(&root, cx, 1);
        assert_eq!(selected(&root, cx), Some(user(1)));
        let text = read(&root, cx, |s| s.shell.stranded_text(id).map(str::to_owned));
        assert_eq!(text.as_deref(), Some("pending in 1"), "Copy reaches it");
        cx.simulate_keystrokes("enter");
        assert!(
            commands_since(&harness, from).is_empty(),
            "nothing asked of core"
        );
    }

    #[gpui::test]
    fn a_core_stop_behind_logging_out_shows_the_recovery_view(cx: &mut TestAppContext) {
        recovery_behind_a_logout_overlay(
            cx,
            AuthState::LogoutStarting,
            crate::chat::copy::STOPPED_SIGNING_OUT,
        );
    }

    #[gpui::test]
    fn a_core_stop_behind_an_unstarted_logout_shows_the_recovery_view(cx: &mut TestAppContext) {
        recovery_behind_a_logout_overlay(
            cx,
            AuthState::LogoutNotStarted {
                error: abyss_core::api::StorageError::Keychain,
                trigger: abyss_core::api::LogoutTrigger::User,
            },
            crate::chat::copy::STOPPED_SIGN_OUT_NOT_STARTED,
        );
    }

    /// Task 6m audit: screens that keep no account state (a torn-down
    /// logout, a login step) show only "restart" once core stops; none of
    /// their actions, which need core, stays on screen.
    #[gpui::test]
    fn a_core_stop_without_account_state_asks_for_a_restart(cx: &mut TestAppContext) {
        let (root, cx, harness) = chats_open(cx, 1);
        core_says(&harness, cx, AuthState::LoggingOut);
        harness.events.close_channel();
        cx.run_until_parked();
        assert_eq!(screen(&root, cx), crate::shell::view::Screen::Stopped);
        assert_eq!(
            read(&root, cx, |s| s
                .shell
                .echoes()
                .count(ThreadKey::whole(user(1)))),
            0
        );
    }

    fn commands_since(harness: &Harness, from: usize) -> Vec<String> {
        harness
            .sent
            .borrow()
            .commands
            .iter()
            .skip(from)
            .map(|c| format!("{c:?}"))
            .collect()
    }

    fn message(chat: ChatId, id: i32) -> abyss_core::model::Message {
        abyss_core::backend::fake::text_message(chat, id, "m")
    }

    #[gpui::test]
    fn close_waits_for_core_then_reports_unconfirmed(cx: &mut TestAppContext) {
        let (root, cx, mut harness) = open(cx);
        assert_eq!(closing(&root, cx), Some(Closing::Open));
        assert!(
            !cx.simulate_close(),
            "the window stays open until core acks"
        );
        assert_eq!(closing(&root, cx), Some(Closing::Waiting));
        assert!(
            !cx.simulate_close(),
            "a second close while closing is refused"
        );
        harness
            .ack
            .take()
            .unwrap()
            .send(ShutdownReport::UNCONFIRMED)
            .unwrap();
        cx.run_until_parked();
        assert_eq!(closing(&root, cx), Some(Closing::Unconfirmed));
        assert!(cx.simulate_close(), "after the message, closing is allowed");
    }

    #[gpui::test]
    fn ui_timeout_ends_a_stalled_shutdown(cx: &mut TestAppContext) {
        let (root, cx, _harness) = open(cx);
        assert!(!cx.simulate_close());
        cx.executor()
            .advance_clock(crate::shell::shutdown::UI_TIMEOUT);
        cx.run_until_parked();
        // Timed out: the app quits (the test platform ignores the quit), and
        // the UI never claims the save was confirmed.
        assert_eq!(closing(&root, cx), Some(Closing::Waiting));
    }
}

#[cfg(feature = "snapshots")]
mod snapshots;
