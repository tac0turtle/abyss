//! Regression tests for review round 19 (re-check of task 6d), task 6e.
//! Each one fails on the code before its fix.

use std::time::Duration;

use super::review6d::{restart_over, stored_signed_in};
use super::{Harness, Setup, start};
use crate::api::{
    AuthState, Command, Event, LocalDeletion, LogoutTrigger, SendState, StorageError, UserError,
};
use crate::backend::BackendError;
use crate::backend::fake::{FAKE_SELF_HASH, FAKE_SELF_ID, FakeBackend, FakeScript, Fault, Op};
use crate::model::{LocalId, ThreadKey};
use crate::secret::{MemoryStore, SecretItem, SecretStore};

fn is_not_started(s: &AuthState) -> bool {
    matches!(s, AuthState::LogoutNotStarted { .. })
}

fn is_blocked(s: &AuthState) -> bool {
    matches!(s, AuthState::StartupBlocked { .. })
}

/// A signed-in demo account, online.
async fn online() -> Harness {
    let (backend, fake) = FakeBackend::new(super::review6c::demo());
    let mut h = start(backend, fake, Setup::default()).await;
    h.online().await;
    h
}

/// A store holding a signed-in session, plus the marker if `marker`.
fn store_with(marker: Option<&[u8]>) -> MemoryStore {
    let store = MemoryStore::new();
    let session = crate::session::signed_in_blob(FAKE_SELF_ID, FAKE_SELF_HASH);
    store.set(SecretItem::Session, &session).unwrap();
    if let Some(marker) = marker {
        store.set(SecretItem::LogoutPending, marker).unwrap();
    }
    store
}

/// Finding 1 (high): a failed marker write performs no side effect at all:
/// no terminate, no `auth.logOut`, no retirement, no teardown, no delete.
/// The account stays signed in; "Stay signed in" goes back to it, and
/// Retry starts the logout once the Keychain takes the marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_marker_write_performs_no_side_effect() {
    let mut h = online().await;
    let retired = h.fake.retired();
    h.faults.fail_set_of(SecretItem::LogoutPending);
    h.send(Command::Logout).await;
    let (ev, seen) = h
        .until(|e| matches!(e, Event::Auth(s) if is_not_started(s) || matches!(s, AuthState::LoggedOut { .. })))
        .await;
    assert!(
        matches!(
            ev,
            Event::Auth(AuthState::LogoutNotStarted {
                error: StorageError::Keychain,
                trigger: LogoutTrigger::User,
            })
        ),
        "{ev:?}"
    );
    let torn_down = seen.iter().any(|e| {
        matches!(e, Event::Auth(AuthState::LoggingOut))
            || matches!(e, Event::DialogsReset(v) if v.is_empty())
    });
    assert!(!torn_down, "teardown without a marker: {seen:?}");
    assert_eq!(h.fake.calls(Op::SignOut), 0, "auth.logOut without a marker");
    assert_eq!(
        h.fake.retired(),
        retired,
        "instance retired without a marker"
    );
    assert!(stored_signed_in(&h.store).await, "session deleted");
    // Stay signed in.
    h.send(Command::Back).await;
    h.auth(AuthState::is_signed_in).await;
    // Retry once the Keychain works: the logout runs to the end.
    h.faults.heal_item(SecretItem::LogoutPending);
    h.send(Command::Logout).await;
    h.auth(|s| {
        matches!(
            s,
            AuthState::LoggedOut {
                local: LocalDeletion::Deleted,
                ..
            }
        )
    })
    .await;
    assert_eq!(h.fake.calls(Op::SignOut), 1);
    assert!(!stored_signed_in(&h.store).await);
    h.stop().await;
}

/// The hang under load in `failed_marker_write_performs_no_side_effect`:
/// the startup check (`fetch_self`) answers only after a logout's marker
/// write failed. It must still sign the account in behind "Couldn't start
/// logout", so "Stay signed in" shows the account, not `Connecting`.
/// Before the fix the overlay replaced the real login state, the check
/// (which applies only to `Connecting`) was dropped, and Back restored
/// `Connecting` for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_check_answering_behind_an_unstarted_logout_still_signs_in() {
    let (backend, fake) = FakeBackend::new(super::review6c::demo());
    fake.inject(Op::FetchSelf, Fault::Delay(Duration::from_millis(1500)));
    let mut h = start(backend, fake, Setup::default()).await;
    h.online().await;
    h.faults.fail_set_of(SecretItem::LogoutPending);
    h.send(Command::Logout).await;
    let (_, seen) = h
        .until(|e| matches!(e, Event::Auth(s) if is_not_started(s)))
        .await;
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, Event::Auth(s) if s.is_signed_in())),
        "the check answered before the marker failed: {seen:?}"
    );
    // The check answers behind the overlay: still "Couldn't start logout".
    h.auth(is_not_started).await;
    h.send(Command::Back).await;
    let (ev, _) = h.until(|e| matches!(e, Event::Auth(_))).await;
    assert!(
        matches!(&ev, Event::Auth(s) if s.is_signed_in()),
        "Stay signed in showed {ev:?}"
    );
    h.stop().await;
}

/// Finding 1 (high), the invariant across a restart: the marker write, the
/// remote logout and the session delete all fail, then the process
/// restarts. The restart may load the session only if no logout side
/// effect ever ran for it; after a Retry that did start the logout, the
/// restart never signs the account in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_marker_write_then_restart_never_loads_a_session_a_logout_started_for() {
    let mut h = online().await;
    h.faults.fail_set_of(SecretItem::LogoutPending);
    h.faults.fail_delete_of(SecretItem::Session);
    h.fake
        .inject(Op::SignOut, Fault::Error(BackendError::Transient));
    h.send(Command::Logout).await;
    h.auth(|s| is_not_started(s) || matches!(s, AuthState::LoggedOut { .. }))
        .await;
    let started = h.fake.calls(Op::SignOut) > 0 || h.fake.retired() > 0;
    let store = h.store.clone();
    h.stop().await;
    let mut h = restart_over(&store, Setup::default()).await;
    let state = h
        .auth(|s| s.is_signed_in() || matches!(s, AuthState::LoggedOut { .. }))
        .await;
    assert!(
        !(state.is_signed_in() && started),
        "a session a logout was started for was loaded"
    );
    assert!(state.is_signed_in(), "nothing started, so nothing ends");

    // Retry with the marker writable: remote and delete still fail.
    h.faults.fail_delete_of(SecretItem::Session);
    h.fake
        .inject(Op::SignOut, Fault::Error(BackendError::Transient));
    h.send(Command::Logout).await;
    h.auth(|s| matches!(s, AuthState::LoggedOut { .. })).await;
    let store = h.store.clone();
    h.stop().await;
    assert!(stored_signed_in(&store).await, "the delete failed");
    let mut h = restart_over(&store, Setup::default()).await;
    let (_, seen) = h
        .until(|e| matches!(e, Event::Auth(AuthState::LoggedOut { .. })))
        .await;
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, Event::Auth(s) if s.is_signed_in())),
        "the logged-out account was signed in: {seen:?}"
    );
    h.stop().await;
}

/// Finding 2 (high): an unreadable marker is "unknown", never a pending
/// logout. Startup is blocked with its own state, Retry (and Logout)
/// re-read instead of deleting, and a valid session survives every retry
/// that still cannot read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreadable_marker_blocks_startup() {
    let store = store_with(None);
    let setup = Setup {
        marker_unreadable: true,
        ..Setup::default()
    };
    let mut h = restart_over(&store, setup).await;
    let (_, seen) = h
        .until(|e| matches!(e, Event::Auth(s) if is_blocked(s)))
        .await;
    assert!(
        !seen.iter().any(|e| matches!(
            e,
            Event::Auth(AuthState::LoggedOut { .. } | AuthState::LoggingOut)
        )),
        "shown as a logout: {seen:?}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.fake.builds(), 0, "no client while blocked");
    for retry in [Command::RetryStartup, Command::Logout] {
        h.send(retry).await;
        let state = h
            .auth(|s| {
                is_blocked(s) || matches!(s, AuthState::LoggingOut | AuthState::LoggedOut { .. })
            })
            .await;
        assert!(
            is_blocked(&state),
            "retry deleted on an unread marker: {state:?}"
        );
    }
    assert!(stored_signed_in(&h.store).await, "a valid account deleted");
    assert_eq!(h.fake.builds(), 0);
    h.stop().await;
}

/// Finding 2: a transient read failure, then the marker reads as absent:
/// Retry resumes the normal startup and signs the stored account in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_marker_read_failure_with_no_marker_starts_normally() {
    let store = store_with(None);
    let setup = Setup {
        marker_unreadable: true,
        ..Setup::default()
    };
    let mut h = restart_over(&store, setup).await;
    h.auth(is_blocked).await;
    h.faults.heal_item(SecretItem::LogoutPending);
    h.send(Command::RetryStartup).await;
    let (_, seen) = h
        .until(|e| matches!(e, Event::Auth(s) if s.is_signed_in()))
        .await;
    // The account's chats load (in either order with `SignedIn`).
    let chats = |e: &Event| matches!(e, Event::DialogsReset(v) if !v.is_empty());
    if !seen.iter().any(chats) {
        h.until(chats).await;
    }
    assert!(stored_signed_in(&h.store).await);
    h.stop().await;
}

/// Finding 2: a transient read failure, then the marker reads as present:
/// Retry runs the interrupted logout's deletion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_marker_read_failure_with_a_marker_resumes_the_logout() {
    let store = store_with(Some(&[0]));
    let setup = Setup {
        marker_unreadable: true,
        ..Setup::default()
    };
    let mut h = restart_over(&store, setup).await;
    h.auth(is_blocked).await;
    h.faults.heal_item(SecretItem::LogoutPending);
    h.send(Command::RetryStartup).await;
    let (_, seen) = h
        .until(|e| {
            matches!(
                e,
                Event::Auth(AuthState::LoggedOut {
                    local: LocalDeletion::Deleted,
                    ..
                })
            )
        })
        .await;
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, Event::Auth(s) if s.is_signed_in()))
    );
    assert!(!stored_signed_in(&h.store).await);
    assert!(h.store.get(SecretItem::LogoutPending).unwrap().is_none());
    h.stop().await;
}

/// Finding 5: two attempts share one pair. The first proves it (its code
/// request succeeds) but the save fails; the second's first code request
/// hits `FloodWait`. Retry must not save on the first attempt's proof.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_never_saves_on_an_earlier_attempts_proof() {
    use crate::api::{ApiHash, CredentialItem};
    use crate::backend::fake::FAKE_API_ID;
    use crate::credentials::EnvResolution;

    let mut script = FakeScript::demo(2, 5);
    script.login.password = None;
    script.env = EnvResolution::Absent;
    let (backend, fake) = FakeBackend::new(script);
    let setup = Setup {
        signed_out: true,
        ..Setup::default()
    };
    let mut h = start(backend, fake, setup).await;
    h.auth(|s| matches!(s, AuthState::NeedCredentials { .. }))
        .await;
    h.send(Command::SubmitCredentials {
        api_id: FAKE_API_ID,
        api_hash: ApiHash::parse("00112233445566778899aabbccddeeff").unwrap(),
    })
    .await;
    let phone = |s: &AuthState| matches!(s, AuthState::Phone { error: None });
    h.auth(phone).await;
    // Attempt 1: proven, but the save fails.
    h.faults.fail_set_of(SecretItem::ApiCredentials);
    h.send(Command::SubmitPhone("+44 7700 900123".into())).await;
    let code = |e: &Event| matches!(e, Event::Auth(AuthState::Code { .. }));
    let (_, seen) = h
        .until(|e| matches!(e, Event::CredentialItem(CredentialItem::SaveFailed(_))))
        .await;
    if !seen.iter().any(code) {
        h.until(code).await;
    }
    h.faults.heal_item(SecretItem::ApiCredentials);
    // Attempt 2, same pair: its first code request is flood-waited.
    h.send(Command::Back).await;
    h.auth(phone).await;
    h.fake.inject(
        Op::RequestCode,
        Fault::Error(BackendError::FloodWait { seconds: 600 }),
    );
    h.send(Command::SubmitPhone("+44 7700 900123".into())).await;
    h.auth(|s| matches!(s, AuthState::FloodWait { .. })).await;
    h.send(Command::RetryCredentialSave).await;
    h.until(|e| matches!(e, Event::CredentialItem(_))).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !h.store.contains(SecretItem::ApiCredentials),
        "saved on an earlier attempt's proof"
    );
    h.stop().await;
}

/// Optional regression: Back → Forget → Back while a sign-in is still
/// queued. The later Back must not weaken the recorded Forget: once the
/// sign-in fails, the stored pair is deleted and entry is shown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn back_forget_back_during_a_sign_in_keeps_the_forget() {
    use super::review6d::{entered, signed_out_with};
    use crate::credentials::EnvResolution;

    let setup = Setup {
        stored_creds: Some(entered()),
        ..Setup::default()
    };
    let mut h = signed_out_with(EnvResolution::Absent, setup).await;
    h.reach_code().await;
    h.fake
        .inject(Op::SignIn, Fault::Queued(Duration::from_millis(300)));
    h.send(Command::SubmitCode("99999".into())).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    h.send(Command::Back).await;
    h.send(Command::ForgetStoredCredentials).await;
    h.send(Command::Back).await;
    let state = h
        .auth(|s| {
            matches!(
                s,
                AuthState::NeedCredentials { .. } | AuthState::Phone { .. }
            )
        })
        .await;
    assert!(
        matches!(state, AuthState::NeedCredentials { .. }),
        "the Forget was lost: {state:?}"
    );
    assert!(h.item_absent().await, "the stored pair was kept");
    h.stop().await;
}

/// Sends `MAX_CONCURRENT` texts from `LocalId(first)` on, each held by
/// `fault`, so they take every admission permit.
async fn occupy_permits(h: &Harness, first: u64, fault: Fault) {
    let count = u64::try_from(crate::actor::MAX_CONCURRENT).unwrap();
    for id in first..first + count {
        h.fake.inject(Op::Send, fault);
        h.send(Command::SendText {
            thread: ThreadKey::whole(super::chat(1)),
            text: "filler".into(),
            local_id: LocalId(id),
        })
        .await;
    }
}

/// The final state of `LocalId(1)`, and whether `Error(FloodWait(599))`
/// came before it.
async fn flooded_send_ends(h: &mut Harness) -> (SendState, bool) {
    let (ev, seen) = h
        .until(|e| {
            matches!(
                e,
                Event::SendState {
                    local_id: LocalId(1),
                    state: SendState::Failed | SendState::Sent { .. },
                }
            )
        })
        .await;
    let warned = seen
        .iter()
        .any(|e| matches!(e, Event::Error(UserError::FloodWait { seconds: 599 })));
    match ev {
        Event::SendState { state, .. } => (state, warned),
        _ => unreachable!(),
    }
}

/// Backend attempts of `LocalId(1)`, the first send made (its `random_id`
/// is the first one the fake saw).
fn flooded_attempts(h: &Harness) -> usize {
    let attempts = h.fake.send_attempts();
    let first = attempts.first().unwrap().1;
    attempts.iter().filter(|(_, id)| *id == first).count()
}

/// Online, with `LocalId(1)` flood-waited for 599 s (its automatic window
/// is 600 s).
async fn flooded_at_zero() -> Harness {
    let mut h = super::harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    h.fake.inject(
        Op::Send,
        Fault::Error(BackendError::FloodWait { seconds: 599 }),
    );
    h.send(Command::SendText {
        thread: ThreadKey::whole(super::chat(1)),
        text: "hi".into(),
        local_id: LocalId(1),
    })
    .await;
    h.until(|e| {
        matches!(
            e,
            Event::SendState {
                local_id: LocalId(1),
                state: SendState::FloodWait { .. },
            }
        )
    })
    .await;
    h
}

/// Finding 4: the flood wait ends at 599 s and the resend is queued behind
/// busy permits; it is admitted after 600 s. Admission enforces the window:
/// `Error(FloodWait)`, then `Failed`, and the send is never made.
#[tokio::test(start_paused = true)]
async fn queued_flood_retry_admitted_past_the_window_fails() {
    let mut h = flooded_at_zero().await;
    tokio::time::sleep(Duration::from_secs(590)).await;
    // Held 20 s: busy from 590 s to 610 s, across the resend at 599 s.
    occupy_permits(&h, 10, Fault::Delay(Duration::from_secs(20))).await;
    let (state, warned) = flooded_send_ends(&mut h).await;
    assert_eq!(state, SendState::Failed, "sent past the window");
    assert!(warned, "no FloodWait error before Failed");
    assert_eq!(flooded_attempts(&h), 1, "the backend saw the resend");
    h.stop().await;
}

/// Finding 4: the queued resend survives a rebuild (the busy calls stall
/// and the instance is replaced) and is admitted on the new instance after
/// the window: it still fails without a call.
#[tokio::test(start_paused = true)]
async fn queued_flood_retry_surviving_a_rebuild_fails_past_the_window() {
    let mut h = flooded_at_zero().await;
    tokio::time::sleep(Duration::from_secs(590)).await;
    // Hung until their deadline (620 s): every permit parks, a stall rebuild.
    occupy_permits(&h, 10, Fault::Hang).await;
    let (state, warned) = flooded_send_ends(&mut h).await;
    assert_eq!(state, SendState::Failed, "sent past the window");
    assert!(warned, "no FloodWait error before Failed");
    assert_eq!(flooded_attempts(&h), 1, "the backend saw the resend");
    assert!(h.fake.builds() >= 2, "no rebuild happened");
    h.stop().await;
}
