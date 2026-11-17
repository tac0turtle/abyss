//! Regression tests for review round 11 (task 6d) and round 15 (re-check of
//! task 6c). Each one fails on the code before the fix.

use std::time::Duration;

use super::{Harness, Setup, chat, harness, start};
use crate::actor::CALL_DEADLINE;
use crate::api::{
    ApiCredentials, ApiHash, AuthState, Command, CredentialItem, Event, SendState, StorageError,
    UserError,
};
use crate::backend::BackendError;
use crate::backend::fake::{
    BuildFault, FAKE_API_ID, FAKE_CODE, FakeBackend, FakeScript, Fault, Op, fake_credentials,
};
use crate::credentials::EnvResolution;
use crate::model::{LocalId, ThreadKey};
use crate::secret::{SecretItem, SecretStore};

const PHONE: &str = "+44 7700 900123";
pub(super) const HASH: &str = "00112233445566778899aabbccddeeff";
/// Polls of the store (10 ms apart, real time) before a test gives up.
const STORE_POLLS: usize = 300;

fn script(env: EnvResolution) -> FakeScript {
    let mut script = FakeScript::demo(2, 5);
    script.login.password = None;
    script.env = env;
    script
}

pub(super) fn entered() -> ApiCredentials {
    ApiCredentials {
        api_id: crate::api::ApiId::new(FAKE_API_ID).unwrap(),
        api_hash: ApiHash::parse(HASH).unwrap(),
    }
}

pub(super) async fn signed_out_with(env: EnvResolution, setup: Setup) -> Harness {
    let (backend, fake) = FakeBackend::new(script(env));
    let setup = Setup {
        signed_out: true,
        ..setup
    };
    start(backend, fake, setup).await
}

fn is_code(s: &AuthState) -> bool {
    matches!(s, AuthState::Code { .. })
}

fn is_phone(s: &AuthState) -> bool {
    matches!(s, AuthState::Phone { error: None })
}

impl Harness {
    pub(super) async fn reach_code(&mut self) {
        self.auth(is_phone).await;
        self.send(Command::SubmitPhone(PHONE.into())).await;
        self.auth(is_code).await;
    }

    /// Whether the credential item is stored within the poll budget (real
    /// time: the persistence worker is an OS thread).
    async fn item_present(&self) -> bool {
        self.item_reaches(|present| present).await
    }

    /// Whether the credential item is gone within the poll budget.
    pub(super) async fn item_absent(&self) -> bool {
        self.item_reaches(|present| !present).await
    }

    async fn item_reaches(&self, want: impl Fn(bool) -> bool) -> bool {
        for _ in 0..STORE_POLLS {
            if want(self.store.contains(SecretItem::ApiCredentials)) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }
}

/// Finding 1 (high): Back during a sign-in that the sender still has queued.
/// The server authorizes it 5 s later, after a probe would have answered
/// "not authorized". Core supervises the original call instead: it never
/// shows the phone screen and ends signed in.
#[tokio::test(start_paused = true)]
async fn back_waits_for_the_queued_sign_in_instead_of_probing() {
    let env = EnvResolution::Pair(fake_credentials());
    let mut h = signed_out_with(env, Setup::default()).await;
    h.reach_code().await;
    h.fake
        .inject(Op::SignIn, Fault::Queued(Duration::from_secs(5)));
    h.send(Command::SubmitCode(FAKE_CODE.into())).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    h.send(Command::Back).await;
    h.auth(|s| matches!(s, AuthState::Verifying { .. })).await;
    let (_, skipped) = h
        .until(|e| matches!(e, Event::Auth(s) if s.is_signed_in()))
        .await;
    assert!(
        !skipped
            .iter()
            .any(|e| matches!(e, Event::Auth(s) if is_phone(s))),
        "a negative probe overtook the sign-in: {skipped:?}"
    );
    assert!(h.fake.authorized());
    h.stop().await;
}

/// Finding 1: a sign-in times out while still queued. The probe must not
/// run on that instance: it is retired first (dropping the queue), and the
/// probe runs on a fresh one. The queued request then never lands.
#[tokio::test(start_paused = true)]
async fn timed_out_sign_in_is_probed_on_a_fresh_instance() {
    let env = EnvResolution::Pair(fake_credentials());
    let mut h = signed_out_with(env, Setup::default()).await;
    h.reach_code().await;
    let late = CALL_DEADLINE + Duration::from_secs(10);
    h.fake.inject(Op::SignIn, Fault::Queued(late));
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
    assert_eq!(h.fake.retired(), 1, "retired before the probe");
    assert_eq!(h.fake.builds(), 2);
    tokio::time::sleep(late * 2).await;
    assert!(
        !h.fake.authorized(),
        "the probe said 'not signed in' but the queued sign-in landed later"
    );
    h.stop().await;
}

/// Finding 2: Forget while a sign-in is in flight that goes through. The
/// stored pair is neither dropped nor deleted; the login completes and the
/// forget is refused (log out first).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forget_during_a_successful_sign_in_requires_logout() {
    let setup = Setup {
        stored_creds: Some(entered()),
        ..Setup::default()
    };
    let mut h = signed_out_with(EnvResolution::Absent, setup).await;
    h.reach_code().await;
    h.fake
        .inject(Op::SignIn, Fault::Queued(Duration::from_millis(300)));
    h.send(Command::SubmitCode(FAKE_CODE.into())).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    h.send(Command::ForgetStoredCredentials).await;
    let (_, skipped) = h
        .until(|e| matches!(e, Event::Auth(s) if s.is_signed_in()))
        .await;
    assert!(
        !skipped
            .iter()
            .any(|e| matches!(e, Event::Auth(AuthState::NeedCredentials { .. }))),
        "{skipped:?}"
    );
    h.until(|e| matches!(e, Event::Error(UserError::SignOutFirst)))
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(h.store.contains(SecretItem::ApiCredentials));
    h.stop().await;
}

/// Finding 2: Forget while a sign-in is in flight that fails (wrong code).
/// Once that is settled the forget completes, and the user is not stranded:
/// a new pair is accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forget_during_a_failed_sign_in_completes_after_it() {
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
    h.send(Command::ForgetStoredCredentials).await;
    h.auth(|s| matches!(s, AuthState::NeedCredentials { .. }))
        .await;
    assert!(h.item_absent().await);
    h.send(Command::SubmitCredentials {
        api_id: FAKE_API_ID,
        api_hash: ApiHash::parse(HASH).unwrap(),
    })
    .await;
    h.auth(is_phone).await;
    h.stop().await;
}

/// Finding 4: a flood wait on the first code request proves nothing, so
/// `RetryCredentialSave` must not save the entered pair.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flood_wait_on_the_first_code_request_saves_nothing() {
    let mut h = signed_out_with(EnvResolution::Absent, Setup::default()).await;
    h.auth(|s| matches!(s, AuthState::NeedCredentials { .. }))
        .await;
    h.send(Command::SubmitCredentials {
        api_id: FAKE_API_ID,
        api_hash: ApiHash::parse(HASH).unwrap(),
    })
    .await;
    h.auth(is_phone).await;
    h.fake.inject(
        Op::RequestCode,
        Fault::Error(BackendError::FloodWait { seconds: 600 }),
    );
    h.send(Command::SubmitPhone(PHONE.into())).await;
    h.auth(|s| matches!(s, AuthState::FloodWait { .. })).await;
    h.send(Command::RetryCredentialSave).await;
    h.until(|e| matches!(e, Event::CredentialItem(CredentialItem::Settled)))
        .await;
    assert!(!h.item_present().await, "an unproven pair was saved");
    h.stop().await;
}

/// Finding 6: a failed delete is explicit and retryable, and the pair is
/// not claimed absent until the delete succeeded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_forget_is_retryable_and_never_claims_absence() {
    let setup = Setup {
        stored_creds: Some(entered()),
        ..Setup::default()
    };
    let mut h = signed_out_with(EnvResolution::Absent, setup).await;
    h.auth(is_phone).await;
    h.faults.fail_delete_of(SecretItem::ApiCredentials);
    h.send(Command::ForgetStoredCredentials).await;
    let (_, skipped) = h
        .until(|e| matches!(e, Event::CredentialItem(CredentialItem::DeleteFailed(_))))
        .await;
    let claimed_absent = skipped.iter().any(|e| {
        matches!(
            e,
            Event::Auth(AuthState::NeedCredentials {
                source: None,
                error: None
            })
        )
    });
    assert!(
        !claimed_absent,
        "absence claimed before the delete: {skipped:?}"
    );
    assert!(h.store.contains(SecretItem::ApiCredentials));
    h.faults.heal_item(SecretItem::ApiCredentials);
    h.send(Command::ForgetStoredCredentials).await;
    h.auth(|s| {
        *s == AuthState::NeedCredentials {
            error: None,
            source: None,
        }
    })
    .await;
    assert!(!h.store.contains(SecretItem::ApiCredentials));
    h.stop().await;
}

/// Finding 6: a failed save is explicit and retryable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_save_is_explicit_and_retry_saves() {
    let mut h = signed_out_with(EnvResolution::Absent, Setup::default()).await;
    h.auth(|s| matches!(s, AuthState::NeedCredentials { .. }))
        .await;
    h.faults.fail_set_of(SecretItem::ApiCredentials);
    h.send(Command::SubmitCredentials {
        api_id: FAKE_API_ID,
        api_hash: ApiHash::parse(HASH).unwrap(),
    })
    .await;
    h.auth(is_phone).await;
    h.send(Command::SubmitPhone(PHONE.into())).await;
    h.until(|e| {
        matches!(
            e,
            Event::CredentialItem(CredentialItem::SaveFailed(StorageError::Keychain))
        )
    })
    .await;
    h.faults.heal_item(SecretItem::ApiCredentials);
    h.send(Command::RetryCredentialSave).await;
    h.until(|e| matches!(e, Event::CredentialItem(CredentialItem::Settled)))
        .await;
    assert!(h.item_present().await);
    assert_eq!(
        h.store
            .get(SecretItem::ApiCredentials)
            .unwrap()
            .and_then(|b| crate::credentials::decode(&b)),
        Some(entered())
    );
    h.stop().await;
}

/// Finding 7: a flood-waited send whose timer ends while the client is held
/// offline must not be replayed once the 10-minute window has passed.
#[tokio::test(start_paused = true)]
async fn flood_window_is_enforced_at_replay_after_a_long_outage() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    h.fake.inject(
        Op::Send,
        Fault::Error(BackendError::FloodWait { seconds: 500 }),
    );
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
                state: SendState::FloodWait { .. },
                ..
            }
        )
    })
    .await;
    // Offline from now until past the window: 16 failed builds back off
    // 1, 2, 4, … 60 s (about 11 to 13 minutes in all).
    for _ in 0..16 {
        h.fake
            .on_next_build(BuildFault::Fail(BackendError::Transient));
    }
    h.internal.send(super::Internal::Wake).await.unwrap();
    tokio::time::sleep(Duration::from_mins(15)).await;
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
    assert!(
        matches!(
            ev,
            Event::SendState {
                state: SendState::Failed,
                ..
            }
        ),
        "replayed past the window: {ev:?}"
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::Error(UserError::FloodWait { seconds: 500 })))
    );
    assert_eq!(h.fake.send_attempts().len(), 1);
    h.stop().await;
}

/// Round 15, D: a re-binding reports before its task has returned its
/// result. The report must still adopt the new cache: rows of the new
/// account's chats are cached and served after a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebind_report_before_the_task_finishes_still_adopts_the_cache() {
    use super::review6c::{OTHER_ID, cache_the_demo_account, demo, empty_server, first_tail};
    use super::{StoredSession, history_ids};
    use crate::api::HistoryDir;
    use tokio::sync::Semaphore;

    let dir = tempfile::tempdir().unwrap();
    let store = cache_the_demo_account(dir.path()).await;
    let old_key = store.get(SecretItem::CacheKey).unwrap().unwrap();
    let hold = std::sync::Arc::new(Semaphore::new(0));
    let (backend, fake) = FakeBackend::new(demo());
    let setup = Setup {
        cache_dir: Some(dir.path().to_path_buf()),
        store: Some(store.clone()),
        session: StoredSession::Unreadable,
        hold_settle: Some(std::sync::Arc::clone(&hold)),
        ..Setup::default()
    };
    let mut h = start(backend, fake, setup).await;
    h.fake.sign_in_as(OTHER_ID);
    h.log_in().await;
    // The erase and recreate ran; its report is sent while the task waits.
    for _ in 0..STORE_POLLS {
        if h.store
            .get(SecretItem::CacheKey)
            .unwrap()
            .is_some_and(|k| *k != *old_key)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    hold.add_permits(1);
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    let tail = h.history(chat(1), HistoryDir::Tail).await;
    assert!(!history_ids(&tail).is_empty());
    let store = h.store.clone();
    h.stop().await;

    let (backend, fake) = FakeBackend::new(empty_server());
    let setup = Setup {
        cache_dir: Some(dir.path().to_path_buf()),
        store: Some(store),
        ..Setup::default()
    };
    let mut h = start(backend, fake, setup).await;
    assert!(
        !first_tail(&mut h).await.is_empty(),
        "the re-bound cache was never served"
    );
    h.stop().await;
}

/// Round 15, C (HIGH): commands the UI queued behind a Logout (text typed
/// for account A) must not act for the next account B, and a command the
/// UI stamped in an earlier account epoch is refused after the transition.
#[tokio::test(start_paused = true)]
async fn commands_queued_after_logout_never_act_for_the_next_account() {
    use super::review6c::{OTHER_ID, demo};

    let (backend, fake) = FakeBackend::new(demo());
    let mut h = start(backend, fake, Setup::default()).await;
    h.online().await;
    h.send(Command::Logout).await;
    // Queued behind the Logout before the UI saw any of its events.
    h.send(Command::SendText {
        thread: ThreadKey::whole(chat(1)),
        text: "for account A".into(),
        local_id: LocalId(7),
    })
    .await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h.send(Command::RetrySend(LocalId(7))).await;
    h.auth(|s| matches!(s, AuthState::LoggedOut { .. })).await;
    h.fake.sign_in_as(OTHER_ID);
    h.log_in().await;
    // Made in account A's epoch, delivered after B signed in.
    let stale = Command::SendText {
        thread: ThreadKey::whole(chat(1)),
        text: "stale".into(),
        local_id: LocalId(8),
    };
    h.cmd
        .send(super::Stamped {
            epoch: 0,
            cmd: stale,
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(120)).await;
    let seen = h.drain();
    assert!(
        h.fake.send_attempts().is_empty(),
        "A's text was sent as {OTHER_ID}"
    );
    assert!(!seen.iter().any(|e| matches!(
        e,
        Event::SendState {
            local_id: LocalId(7 | 8),
            state: SendState::Sent { .. } | SendState::Sending,
        }
    )));
    h.stop().await;
}

/// Whether the store holds a signed-in session item.
pub(super) async fn stored_signed_in(store: &crate::secret::MemoryStore) -> bool {
    let persist = crate::session::PersistHandle::spawn(store.clone()).unwrap();
    let loaded = persist.load_session().await.unwrap();
    loaded.snapshot.is_signed_in().unwrap()
}

/// Restarts over `store` with a demo server, as the same device would.
pub(super) async fn restart_over(store: &crate::secret::MemoryStore, setup: Setup) -> Harness {
    let (backend, fake) = FakeBackend::new(super::review6c::demo());
    let setup = Setup {
        store: Some(store.clone()),
        ..setup
    };
    start(backend, fake, setup).await
}

/// Round 15, A: the remote logout fails and so does the session delete;
/// after a restart the old account is never signed in. The marker resumes
/// the logout before the session is loaded or a client is built, and is
/// cleared only once both deletions succeed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_after_a_failed_logout_never_signs_the_old_account_in() {
    use crate::api::{LocalDeletion, RemoteLogout};

    let (backend, fake) = FakeBackend::new(super::review6c::demo());
    let mut h = start(backend, fake, Setup::default()).await;
    h.online().await;
    h.fake
        .inject(Op::SignOut, Fault::Error(BackendError::Transient));
    h.faults.fail_delete_of(SecretItem::Session);
    h.send(Command::Logout).await;
    let state = h.auth(|s| matches!(s, AuthState::LoggedOut { .. })).await;
    assert!(matches!(
        state,
        AuthState::LoggedOut {
            remote: RemoteLogout::Failed(_),
            local: LocalDeletion::Failed(_),
        }
    ));
    let store = h.store.clone();
    h.stop().await;
    assert!(stored_signed_in(&store).await, "the session item remains");
    assert!(store.get(SecretItem::LogoutPending).unwrap().is_some());

    let mut h = restart_over(&store, Setup::default()).await;
    let (ev, seen) = h
        .until(|e| matches!(e, Event::Auth(AuthState::LoggedOut { .. })))
        .await;
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, Event::Auth(s) if s.is_signed_in())),
        "the old account was signed in: {seen:?}"
    );
    assert!(matches!(
        ev,
        Event::Auth(AuthState::LoggedOut {
            remote: RemoteLogout::Failed(_),
            local: LocalDeletion::Deleted,
        })
    ));
    assert!(!stored_signed_in(&h.store).await);
    assert!(h.store.get(SecretItem::LogoutPending).unwrap().is_none());
    h.until(|e| matches!(e, Event::Auth(AuthState::Phone { .. })))
        .await;
    h.stop().await;
}

/// Round 15, A: a crash after the marker was recorded (before any deletion)
/// resumes the logout at startup; nothing of the old account is served.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn marker_left_by_a_crash_resumes_the_logout() {
    let store = crate::secret::MemoryStore::new();
    store
        .set(
            SecretItem::Session,
            &crate::session::signed_in_blob(
                crate::backend::fake::FAKE_SELF_ID,
                crate::backend::fake::FAKE_SELF_HASH,
            ),
        )
        .unwrap();
    store.set(SecretItem::LogoutPending, &[0]).unwrap();
    let mut h = restart_over(&store, Setup::default()).await;
    let (_, seen) = h
        .until(|e| matches!(e, Event::Auth(AuthState::LoggedOut { .. })))
        .await;
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, Event::Auth(s) if s.is_signed_in())
                || matches!(e, Event::DialogsReset(v) if !v.is_empty()))
    );
    assert!(!stored_signed_in(&h.store).await);
    assert!(h.store.get(SecretItem::LogoutPending).unwrap().is_none());
    h.stop().await;
}
