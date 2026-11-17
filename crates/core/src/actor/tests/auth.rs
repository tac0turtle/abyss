//! Login state machine (plan section 4). Paused time unless a test needs the
//! login barrier's real answer (the persistence worker is an OS thread, and
//! paused time may jump past `LOGIN_BARRIER_WAIT` while it answers).

use std::time::Duration;

use super::{Harness, Setup, chat, harness_with, start};
use crate::api::{AuthState, Command, ConnState, Event, PasswordBuf, UserError};
use crate::backend::fake::{DEMO_PASSWORD, FAKE_CODE, FakeBackend, FakeScript, Fault, Op};
use crate::backend::{BackendError, RejectReason};
use crate::model::ThreadKey;
use crate::secret::{SecretItem, StoreOp};

const PHONE: &str = "+44 7700 900123";

/// A demo account (two chats) without 2FA.
fn no_password() -> FakeScript {
    let mut script = FakeScript::demo(2, 5);
    script.login.password = None;
    script
}

/// Starts signed out and waits for the phone prompt.
async fn signed_out(script: FakeScript) -> Harness {
    let (backend, fake) = FakeBackend::new(script);
    let setup = Setup {
        signed_out: true,
        ..Setup::default()
    };
    let mut h = start(backend, fake, setup).await;
    h.auth(|s| matches!(s, AuthState::Phone { error: None }))
        .await;
    h
}

fn password(p: &str) -> Command {
    Command::SubmitPassword(PasswordBuf::from_slice(p.as_bytes()).unwrap())
}

fn is_code(s: &AuthState) -> bool {
    matches!(s, AuthState::Code { .. })
}

impl Harness {
    async fn phone_to_code(&mut self) -> AuthState {
        self.send(Command::SubmitPhone(PHONE.into())).await;
        self.auth(is_code).await
    }

    async fn signed_in(&mut self) -> AuthState {
        self.auth(AuthState::is_signed_in).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_is_confirmed_and_goes_online() {
    let mut h = signed_out(no_password()).await;
    let code = h.phone_to_code().await;
    let AuthState::Code {
        phone_hint,
        error,
        sent,
    } = code
    else {
        unreachable!()
    };
    assert_eq!(error, None);
    assert!(!phone_hint.contains("900123"), "the hint is masked");
    let sent = sent.expect("the sent-code info");
    assert_eq!(sent.delivery, crate::api::CodeDelivery::App);
    assert!(sent.resend_at.is_some());
    h.send(Command::SubmitCode(FAKE_CODE.into())).await;
    assert_eq!(
        h.signed_in().await,
        AuthState::SignedIn {
            display_name: "Demo User".into(),
            new_login: Some(crate::api::NewLogin {
                has_password: Some(false)
            }),
        }
    );
    let (ev, _) = h
        .until(|e| matches!(e, Event::DialogsReset(v) if !v.is_empty()))
        .await;
    assert!(matches!(ev, Event::DialogsReset(v) if v.len() == 2));
    h.online().await;
    assert!(h.store.contains(SecretItem::Session));
    assert_eq!(h.stop().await.save, crate::api::SaveStatus::Confirmed);
}

#[tokio::test(start_paused = true)]
async fn invalid_phone_and_code_are_rejected_locally() {
    let mut h = signed_out(no_password()).await;
    h.send(Command::SubmitPhone("12ab".into())).await;
    h.auth(|s| {
        matches!(
            s,
            AuthState::Phone {
                error: Some(UserError::PhoneInvalid)
            }
        )
    })
    .await;
    h.phone_to_code().await;
    h.send(Command::SubmitCode("12x45".into())).await;
    h.auth(|s| {
        matches!(
            s,
            AuthState::Code {
                error: Some(UserError::InvalidCode),
                ..
            }
        )
    })
    .await;
    assert_eq!(h.fake.calls(Op::SignIn), 0);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn two_factor_wrong_then_right_password() {
    let mut h = signed_out(FakeScript::demo(2, 5)).await;
    h.phone_to_code().await;
    h.send(Command::SubmitCode(FAKE_CODE.into())).await;
    let state = h.auth(|s| matches!(s, AuthState::Password { .. })).await;
    assert!(matches!(
        state,
        AuthState::Password {
            hint: Some(_),
            error: None
        }
    ));
    h.send(password("wrong")).await;
    h.auth(|s| {
        matches!(
            s,
            AuthState::Password {
                error: Some(UserError::InvalidPassword),
                ..
            }
        )
    })
    .await;
    h.send(password(DEMO_PASSWORD)).await;
    h.signed_in().await;
    h.online().await;
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn flood_wait_on_code_request_holds_then_resumes() {
    let mut h = signed_out(no_password()).await;
    h.fake.inject(
        Op::RequestCode,
        Fault::Error(BackendError::FloodWait { seconds: 30 }),
    );
    h.send(Command::SubmitPhone(PHONE.into())).await;
    let state = h.auth(|s| matches!(s, AuthState::FloodWait { .. })).await;
    let AuthState::FloodWait { resume, .. } = state else {
        unreachable!()
    };
    assert_eq!(*resume, AuthState::Phone { error: None });
    // During the wait a submission only repeats the state.
    h.send(Command::SubmitPhone(PHONE.into())).await;
    h.auth(|s| matches!(s, AuthState::FloodWait { .. })).await;
    assert_eq!(h.fake.calls(Op::RequestCode), 1);
    tokio::time::sleep(Duration::from_secs(31)).await;
    h.auth(|s| matches!(s, AuthState::Phone { error: None }))
        .await;
    h.phone_to_code().await;
    assert_eq!(h.fake.calls(Op::RequestCode), 2);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn expired_code_then_resend() {
    let mut h = signed_out(no_password()).await;
    h.phone_to_code().await;
    h.fake.expire_code();
    h.send(Command::SubmitCode(FAKE_CODE.into())).await;
    h.auth(|s| {
        matches!(
            s,
            AuthState::Code {
                error: Some(UserError::InvalidCode),
                ..
            }
        )
    })
    .await;
    h.send(Command::ResendCode).await;
    h.auth(|s| matches!(s, AuthState::Code { error: None, .. }))
        .await;
    // The expired request cannot be resent: a fresh code is requested.
    assert_eq!(h.fake.calls(Op::ResendCode), 1);
    assert_eq!(h.fake.calls(Op::RequestCode), 2);
    h.send(Command::SubmitCode(FAKE_CODE.into())).await;
    h.signed_in().await;
    h.stop().await;
}

/// grammers' `SignInError::Other` on `check_password`: the token is gone, so
/// the user must refetch the password parameters before trying again.
#[tokio::test(start_paused = true)]
async fn other_error_on_check_password_needs_new_params() {
    let mut h = signed_out(FakeScript::demo(2, 5)).await;
    h.phone_to_code().await;
    h.send(Command::SubmitCode(FAKE_CODE.into())).await;
    h.auth(|s| matches!(s, AuthState::Password { .. })).await;
    h.fake.inject(
        Op::CheckPassword,
        Fault::Error(BackendError::Rejected(RejectReason::Other)),
    );
    h.send(password(DEMO_PASSWORD)).await;
    h.auth(|s| matches!(s, AuthState::PasswordTokenUnavailable { .. }))
        .await;
    // Without new parameters, a submission is not sent.
    h.send(password(DEMO_PASSWORD)).await;
    h.auth(|s| matches!(s, AuthState::PasswordTokenUnavailable { .. }))
        .await;
    assert_eq!(h.fake.calls(Op::CheckPassword), 1);
    h.send(Command::RetryPasswordParams).await;
    h.auth(|s| matches!(s, AuthState::Password { error: None, .. }))
        .await;
    h.send(password(DEMO_PASSWORD)).await;
    h.signed_in().await;
    h.stop().await;
}

/// The sign-in went through but its answer was lost: the probe finds the
/// session authorized and completes the login.
#[tokio::test(start_paused = true)]
async fn sign_in_timeout_probe_finds_it_authorized() {
    let mut h = signed_out(no_password()).await;
    h.phone_to_code().await;
    h.fake.inject(Op::SignIn, Fault::CompleteThenHang);
    h.send(Command::SubmitCode(FAKE_CODE.into())).await;
    // A second submission while the first is pending is refused.
    h.send(Command::SubmitCode(FAKE_CODE.into())).await;
    h.until(|e| matches!(e, Event::Error(UserError::Busy)))
        .await;
    h.signed_in().await;
    assert_eq!(h.fake.calls(Op::IsAuthorized), 1);
    assert_eq!(h.fake.calls(Op::SignIn), 1);
    h.online().await;
    h.stop().await;
}

/// The sign-in never reached the server: the probe finds no authorization and
/// returns to the code step with the same (still valid) code.
#[tokio::test(start_paused = true)]
async fn sign_in_timeout_probe_finds_it_unauthorized() {
    let mut h = signed_out(no_password()).await;
    h.phone_to_code().await;
    h.fake.inject(Op::SignIn, Fault::Hang);
    h.send(Command::SubmitCode(FAKE_CODE.into())).await;
    h.auth(|s| {
        matches!(
            s,
            AuthState::Code {
                error: Some(UserError::Network),
                ..
            }
        )
    })
    .await;
    assert_eq!(h.fake.calls(Op::IsAuthorized), 1);
    h.send(Command::SubmitCode(FAKE_CODE.into())).await;
    h.signed_in().await;
    h.stop().await;
}

/// A panic in a login call (a grammers bug) clears the pending flag: the user
/// can retry right away, with the same code.
#[tokio::test(start_paused = true)]
async fn panic_in_a_login_call_clears_the_pending_flag() {
    let mut h = signed_out(no_password()).await;
    h.phone_to_code().await;
    h.fake.inject(Op::SignIn, Fault::Panic);
    h.send(Command::SubmitCode(FAKE_CODE.into())).await;
    h.auth(|s| {
        matches!(
            s,
            AuthState::Code {
                error: Some(UserError::Internal),
                ..
            }
        )
    })
    .await;
    h.send(Command::SubmitCode(FAKE_CODE.into())).await;
    h.signed_in().await;
    assert!(
        !h.drain()
            .iter()
            .any(|e| matches!(e, Event::Error(UserError::Busy)))
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn sign_up_required_is_reported() {
    let mut script = no_password();
    script.login.sign_up_required = true;
    let mut h = signed_out(script).await;
    h.phone_to_code().await;
    h.send(Command::SubmitCode(FAKE_CODE.into())).await;
    h.auth(|s| *s == AuthState::SignUpRequired).await;
    h.stop().await;
}

/// The login's self-user write cannot reach the Keychain: signed in, but the
/// user is told the session was not saved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_barrier_failure_is_signed_in_unsaved() {
    let mut h = signed_out(no_password()).await;
    h.faults.fail(StoreOp::Set);
    h.phone_to_code().await;
    h.send(Command::SubmitCode(FAKE_CODE.into())).await;
    assert_eq!(
        h.signed_in().await,
        AuthState::SignedInUnsaved {
            display_name: "Demo User".into(),
            new_login: Some(crate::api::NewLogin {
                has_password: Some(false)
            }),
        }
    );
    h.online().await;
    assert_eq!(h.stop().await.save, crate::api::SaveStatus::Unconfirmed);
}

/// A second migration halts the instance and asks for a new sign-in; signing
/// in clears the halt.
#[tokio::test(start_paused = true)]
async fn second_migration_routes_to_sign_in_and_login_clears_the_halt() {
    let (backend, fake) = FakeBackend::new(no_password());
    let mut h = harness_with(backend, fake, None).await;
    for _ in 0..2 {
        h.online().await;
        h.fake
            .inject(Op::History, Fault::Error(BackendError::Migrated));
        h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
        h.until(|e| matches!(e, Event::Error(UserError::AccountMigrated)))
            .await;
    }
    h.until(|e| matches!(e, Event::Connection(ConnState::Offline)))
        .await;
    h.auth(|s| {
        matches!(
            s,
            AuthState::Phone {
                error: Some(UserError::AccountMigrated)
            }
        )
    })
    .await;
    h.phone_to_code().await;
    h.send(Command::SubmitCode(FAKE_CODE.into())).await;
    h.signed_in().await;
    h.online().await;
    assert_eq!(h.fake.builds(), 3);
    h.stop().await;
}

// Task 6 review findings (task 6b). Each test fails on the task 6 code.
