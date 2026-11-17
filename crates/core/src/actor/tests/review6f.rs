//! Regression tests for review round 25 (re-check of task 6e), task 6f.
//! Each one fails on the code before its fix.

use std::time::Duration;

use super::{Harness, Setup, chat, start};
use crate::api::{
    AuthState, Command, CredentialError, CredentialSource, EnvVar, Event, HistoryDir, LoadTarget,
    LogoutTrigger, SendState,
};
use crate::backend::BackendError;
use crate::backend::fake::{FAKE_SELF_HASH, FAKE_SELF_ID, FakeBackend, FakeScript, Fault, Op};
use crate::credentials::EnvResolution;
use crate::model::{LocalId, ThreadKey};
use crate::secret::{MemoryStore, SecretItem, SecretStore};

/// Online with chat 1 open (older history exists), and `LocalId(1)`
/// flood-waited for `seconds` (its automatic window is 600 s).
pub(super) async fn flooded(seconds: u32) -> Harness {
    let mut h = super::harness(FakeScript::demo(1, 120), None).await;
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h.until(|e| {
        matches!(
            e,
            Event::History {
                dir: HistoryDir::Tail,
                ..
            }
        )
    })
    .await;
    h.fake
        .inject(Op::Send, Fault::Error(BackendError::FloodWait { seconds }));
    h.send(Command::SendText {
        thread: ThreadKey::whole(chat(1)),
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

/// An answer to a `LoadOlder` of chat 1: its page, or its end.
fn older_answer(e: &Event) -> bool {
    matches!(
        e,
        Event::History {
            dir: HistoryDir::Older,
            ..
        } | Event::LoadEnded {
            target: LoadTarget::History {
                dir: HistoryDir::Older,
                ..
            },
            ..
        }
    )
}

/// Finding 1 (high): while the logout's marker is written, the UI is shown
/// `LogoutStarting`, and every command it queued is answered or honoured:
/// a refused retry goes back to its send's state, two overlapping older
/// loads of the open chat are both answered, and a discard cancels the
/// flood-waited send. After the marker fails and "Stay signed in", the
/// flood wait ends with nothing to resend.
#[tokio::test(start_paused = true)]
async fn discard_while_the_marker_is_written_stops_the_automatic_retry() {
    let mut h = flooded(599).await;
    assert_eq!(h.fake.calls(Op::Send), 1);
    // The marker write stalls (a Keychain prompt nobody answers), then fails.
    h.faults.fail_set_of(SecretItem::LogoutPending);
    h.faults.hold();
    h.send(Command::Logout).await;
    h.auth(|s| *s == AuthState::LogoutStarting).await;
    h.send(Command::RetrySend(LocalId(1))).await;
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
    // The first load is in flight when the second arrives (deferred).
    h.fake
        .inject(Op::History, Fault::Delay(Duration::from_secs(2)));
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    h.until(older_answer).await;
    h.until(older_answer).await;
    h.send(Command::DiscardSend(LocalId(1))).await;
    h.faults.release();
    h.auth(|s| matches!(s, AuthState::LogoutNotStarted { .. }))
        .await;
    h.send(Command::Back).await;
    h.auth(AuthState::is_signed_in).await;
    // Past the flood wait (599 s) and the automatic window (600 s).
    tokio::time::sleep(Duration::from_secs(700)).await;
    let seen = h.drain();
    assert_eq!(h.fake.calls(Op::Send), 1, "the discarded send went again");
    assert!(
        !seen.iter().any(|e| matches!(
            e,
            Event::SendState {
                local_id: LocalId(1),
                state: SendState::Sending | SendState::Sent { .. },
            }
        )),
        "{seen:?}"
    );
    h.stop().await;
}

/// Finding 2: startup was blocked on an unreadable marker; Retry reads it
/// as absent, and the env override is partial or malformed. The credential
/// error is shown, not swallowed as if a logout were running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_retry_shows_an_env_credential_error() {
    let cases = [
        (
            EnvResolution::Partial {
                missing: EnvVar::ApiHash,
            },
            CredentialError::EnvPartial {
                missing: EnvVar::ApiHash,
            },
        ),
        (
            EnvResolution::Malformed { var: EnvVar::ApiId },
            CredentialError::EnvMalformed { var: EnvVar::ApiId },
        ),
    ];
    for (env, error) in cases {
        let store = MemoryStore::new();
        let session = crate::session::signed_in_blob(FAKE_SELF_ID, FAKE_SELF_HASH);
        store.set(SecretItem::Session, &session).unwrap();
        let mut script = super::review6c::demo();
        script.env = env;
        let (backend, fake) = FakeBackend::new(script);
        let setup = Setup {
            store: Some(store),
            marker_unreadable: true,
            ..Setup::default()
        };
        let mut h = start(backend, fake, setup).await;
        h.auth(|s| matches!(s, AuthState::StartupBlocked { .. }))
            .await;
        h.faults.heal_item(SecretItem::LogoutPending);
        h.send(Command::RetryStartup).await;
        let want = AuthState::NeedCredentials {
            error: Some(error),
            source: Some(CredentialSource::Env),
        };
        h.auth(|s| *s == want).await;
        h.stop().await;
    }
}

/// Finding 5: the user's logout could not start, then the server revokes
/// the session. The screen loses "Stay signed in" (the trigger is now the
/// server's), and Back does not sign in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revocation_after_a_failed_marker_takes_stay_signed_in_away() {
    let (backend, fake) = FakeBackend::new(super::review6c::demo());
    let mut h = start(backend, fake, Setup::default()).await;
    h.online().await;
    h.faults.fail_set_of(SecretItem::LogoutPending);
    h.send(Command::Logout).await;
    h.auth(|s| {
        matches!(
            s,
            AuthState::LogoutNotStarted {
                trigger: LogoutTrigger::User,
                ..
            }
        )
    })
    .await;
    h.fake.revoke_session();
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    let server = |s: &AuthState| {
        matches!(
            s,
            AuthState::LogoutNotStarted {
                trigger: LogoutTrigger::Server,
                ..
            }
        )
    };
    h.auth(server).await;
    // Back cannot lift it: it answers with the same screen.
    h.send(Command::Back).await;
    let (ev, _) = h.until(|e| matches!(e, Event::Auth(_))).await;
    assert!(matches!(&ev, Event::Auth(s) if server(s)), "{ev:?}");
    h.send(Command::LoadDialogs).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen = h.drain();
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, Event::Auth(s) if s.is_signed_in())),
        "stayed signed in on a revoked session: {seen:?}"
    );
    h.stop().await;
}
