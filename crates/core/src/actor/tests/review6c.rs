//! Regression tests for the round 8 findings left for task 6c (plan section
//! 15): account isolation of the cache, the second-migration relogin, the
//! incomplete logout, and the auth-key test gap. Each fails on the code
//! before the fix.

use std::path::Path;
use std::time::Duration;

use super::{Harness, Setup, StoredSession, chat, start};
use crate::api::{
    AuthState, Command, Event, HistoryDir, LocalDeletion, LogoutStatus, RemoteLogout, UserError,
};
use crate::backend::BackendError;
use crate::backend::fake::{
    FAKE_CODE, FAKE_SELF_HASH, FAKE_SELF_ID, FakeBackend, FakeScript, Fault, Op,
};
use crate::model::{LocalId, ThreadKey};
use crate::secret::{MemoryStore, SecretItem, SecretStore};
use crate::session::{PersistHandle, signed_in_blob_with_key};

/// Another account than the fake's default one.
pub(super) const OTHER_ID: i64 = 424_242;
/// The byte the old account's auth keys are made of.
const OLD_KEY: u8 = 0xA5;

/// A demo account without 2FA.
pub(super) fn demo() -> FakeScript {
    let mut script = FakeScript::demo(2, 5);
    script.login.password = None;
    script
}

/// A server with no chats: anything shown after a login comes from the cache.
pub(super) fn empty_server() -> FakeScript {
    let mut script = FakeScript::default();
    script.login.password = None;
    script
}

/// Whether `blob` holds a run of the old account's auth key bytes.
fn has_old_key(blob: &[u8]) -> bool {
    blob.windows(32).any(|w| w.iter().all(|b| *b == OLD_KEY))
}

fn stored_has_old_key(store: &MemoryStore) -> bool {
    store
        .get(SecretItem::Session)
        .unwrap()
        .is_some_and(|b| has_old_key(&b))
}

/// The self user id of the stored session, read by a fresh worker.
async fn stored_self(store: &MemoryStore) -> Option<i64> {
    let persist = PersistHandle::spawn(store.clone()).unwrap();
    let loaded = persist.load_session().await.unwrap();
    loaded.snapshot.self_id().unwrap()
}

/// Shows rows of the old account: a non-empty chat list or history.
fn shows_rows(e: &Event) -> bool {
    match e {
        Event::DialogsReset(v) | Event::DialogsAppended(v) => !v.is_empty(),
        Event::History { messages, .. } => !messages.is_empty(),
        _ => false,
    }
}

impl Harness {
    /// Waits for the phone prompt, then logs in; returns every event seen.
    pub(super) async fn log_in(&mut self) -> Vec<Event> {
        let (_, mut seen) = self
            .until(|e| matches!(e, Event::Auth(AuthState::Phone { .. })))
            .await;
        seen.extend(self.submit_login().await);
        seen
    }

    /// Logs in from the phone prompt.
    pub(super) async fn submit_login(&mut self) -> Vec<Event> {
        let mut seen = Vec::new();
        self.send(Command::SubmitPhone("+44 7700 900123".into()))
            .await;
        let (_, more) = self
            .until(|e| matches!(e, Event::Auth(AuthState::Code { .. })))
            .await;
        seen.extend(more);
        self.send(Command::SubmitCode(FAKE_CODE.into())).await;
        let (_, more) = self
            .until(|e| matches!(e, Event::Auth(s) if s.is_signed_in()))
            .await;
        seen.extend(more);
        seen.extend(self.online().await);
        seen
    }

    async fn logged_out_6c(&mut self) -> (RemoteLogout, LocalDeletion) {
        match self
            .auth(|s| matches!(s, AuthState::LoggedOut { .. }))
            .await
        {
            AuthState::LoggedOut { remote, local } => (remote, local),
            _ => unreachable!(),
        }
    }
}

/// Signs in to the demo account with a cache in `dir`, caches chat 1's
/// tail, then stops. Returns the store.
pub(super) async fn cache_the_demo_account(dir: &Path) -> MemoryStore {
    let (backend, fake) = FakeBackend::new(demo());
    let setup = Setup {
        cache_dir: Some(dir.to_path_buf()),
        ..Setup::default()
    };
    let mut h = start(backend, fake, setup).await;
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h.history(chat(1), HistoryDir::Tail).await;
    let store = h.store.clone();
    h.stop().await;
    store
}

/// Restarts over `store` and `dir` with an empty server.
async fn restart(store: &MemoryStore, dir: &Path, session: StoredSession) -> Harness {
    let (backend, fake) = FakeBackend::new(empty_server());
    let setup = Setup {
        cache_dir: Some(dir.to_path_buf()),
        store: Some(store.clone()),
        session,
        ..Setup::default()
    };
    start(backend, fake, setup).await
}

/// The first tail shown for chat 1 after opening it.
pub(super) async fn first_tail(h: &mut Harness) -> Vec<i32> {
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    match h.history(chat(1), HistoryDir::Tail).await {
        Event::History { messages, .. } => messages.iter().map(|m| m.id.0).collect(),
        _ => unreachable!(),
    }
}

/// Finding 1 (HIGH): the session item is deleted but the cache key is not,
/// so the files and their usable key stay. After a restart the cache must
/// not be served while signed out, and another account's login must not see
/// its rows. The erase is retried at startup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_cache_after_key_delete_failure_is_not_shown_to_another_account() {
    let dir = tempfile::tempdir().unwrap();
    let (backend, fake) = FakeBackend::new(demo());
    let setup = Setup {
        cache_dir: Some(dir.path().to_path_buf()),
        ..Setup::default()
    };
    let mut h = start(backend, fake, setup).await;
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h.history(chat(1), HistoryDir::Tail).await;
    h.faults.fail_delete_of(SecretItem::CacheKey);
    h.send(Command::Logout).await;
    let (_, local) = h.logged_out_6c().await;
    assert!(matches!(local, LocalDeletion::Failed(_)));
    let old_key = h.store.get(SecretItem::CacheKey).unwrap().unwrap();
    let store = h.store.clone();
    h.stop().await;

    let mut h = restart(&store, dir.path(), StoredSession::Default).await;
    h.fake.sign_in_as(OTHER_ID);
    let seen = h.log_in().await;
    assert!(!seen.iter().any(shows_rows), "old rows shown: {seen:?}");
    assert!(first_tail(&mut h).await.is_empty());
    let new_key = h.store.get(SecretItem::CacheKey).unwrap().unwrap();
    assert_ne!(*old_key, *new_key, "the old cache was erased at startup");
    assert_eq!(stored_self(&h.store).await, Some(OTHER_ID));
    h.stop().await;
}

/// Finding 1, same account: with the session unreadable at startup the cache
/// is quarantined (nothing shown while signed out); the same account's login
/// binds it, and its rows are served again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quarantined_cache_is_served_after_the_same_account_logs_in() {
    let dir = tempfile::tempdir().unwrap();
    let store = cache_the_demo_account(dir.path()).await;
    let key = store.get(SecretItem::CacheKey).unwrap().unwrap();
    let mut h = restart(&store, dir.path(), StoredSession::Unreadable).await;
    let seen = h.log_in().await;
    assert!(
        !seen.iter().any(shows_rows),
        "nothing served while quarantined"
    );
    assert_eq!(first_tail(&mut h).await, vec![1, 2, 3, 4, 5]);
    let kept = h.store.get(SecretItem::CacheKey).unwrap().unwrap();
    assert_eq!(*key, *kept, "the cache was kept");
    h.stop().await;
}

/// Finding 1, another account: the quarantined cache is crypto-erased and
/// recreated, and none of its rows reach the new account.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quarantined_cache_is_erased_when_another_account_logs_in() {
    let dir = tempfile::tempdir().unwrap();
    let store = cache_the_demo_account(dir.path()).await;
    let key = store.get(SecretItem::CacheKey).unwrap().unwrap();
    let mut h = restart(&store, dir.path(), StoredSession::Unreadable).await;
    h.fake.sign_in_as(OTHER_ID);
    let seen = h.log_in().await;
    assert!(!seen.iter().any(shows_rows));
    assert!(first_tail(&mut h).await.is_empty());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while h
        .store
        .get(SecretItem::CacheKey)
        .unwrap()
        .is_some_and(|k| *k == *key)
    {
        assert!(std::time::Instant::now() < deadline, "never re-keyed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    h.stop().await;
}

/// Finding 2 (HIGH): the relogin after a second migration is a new account.
/// The old snapshot (self user and auth keys) and the old pending sends do
/// not reach another account's session.
#[tokio::test(start_paused = true)]
async fn second_migration_relogin_starts_a_fresh_account() {
    let (backend, fake) = FakeBackend::new(demo());
    let setup = Setup {
        session: StoredSession::Blob(signed_in_blob_with_key(
            FAKE_SELF_ID,
            FAKE_SELF_HASH,
            OLD_KEY,
        )),
        ..Setup::default()
    };
    let mut h = start(backend, fake, setup).await;
    h.online().await;
    assert!(stored_has_old_key(&h.store));
    for _ in 0..3 {
        h.fake.inject(Op::Send, Fault::Hang);
    }
    h.send(Command::SendText {
        thread: ThreadKey::whole(chat(1)),
        text: "old account".into(),
        local_id: LocalId(1),
    })
    .await;
    for first in [true, false] {
        h.fake
            .inject(Op::History, Fault::Error(BackendError::Migrated));
        h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
        h.until(|e| matches!(e, Event::Error(UserError::AccountMigrated)))
            .await;
        if first {
            h.online().await;
        }
    }
    h.auth(|s| {
        matches!(
            s,
            AuthState::Phone {
                error: Some(UserError::AccountMigrated)
            }
        )
    })
    .await;
    assert_eq!(stored_self(&h.store).await, None, "the old session is gone");
    assert!(!stored_has_old_key(&h.store), "the old auth keys are gone");
    let sends = h.fake.calls(Op::Send);
    h.fake.sign_in_as(OTHER_ID);
    h.submit_login().await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(h.fake.calls(Op::Send), sends, "no old send replayed");
    assert!(h.fake.sent().is_empty());
    assert_eq!(stored_self(&h.store).await, Some(OTHER_ID));
    assert!(!stored_has_old_key(&h.store));
    h.stop().await;
}

/// Finding 3: a failed local deletion stays "Logout incomplete": no login
/// until Retry succeeds, and the first attempt's remote result is kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_logout_stays_incomplete_until_retry() {
    let dir = tempfile::tempdir().unwrap();
    let (backend, fake) = FakeBackend::new(demo());
    let setup = Setup {
        cache_dir: Some(dir.path().to_path_buf()),
        ..Setup::default()
    };
    let mut h = start(backend, fake, setup).await;
    h.online().await;
    h.faults.fail_delete_of(SecretItem::CacheKey);
    h.send(Command::Logout).await;
    let (remote, local) = h.logged_out_6c().await;
    assert_eq!(remote, RemoteLogout::Revoked);
    assert!(matches!(local, LocalDeletion::Failed(_)));
    let builds = h.fake.builds();
    h.send(Command::SubmitPhone("+44 7700 900123".into())).await;
    let (ev, skipped) = h.until(|e| matches!(e, Event::Auth(_))).await;
    assert!(
        matches!(ev, Event::Auth(AuthState::LoggedOut { .. })),
        "{ev:?}"
    );
    assert!(!skipped.iter().any(|e| matches!(e, Event::Auth(_))));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.fake.builds(), builds, "no instance for a login");
    assert!(h.store.contains(SecretItem::CacheKey));

    h.faults.heal_item(SecretItem::CacheKey);
    h.send(Command::Logout).await;
    h.auth(|s| *s == AuthState::LoggingOut).await;
    let (remote, local) = h.logged_out_6c().await;
    assert_eq!(
        (remote, local),
        (RemoteLogout::Revoked, LocalDeletion::Deleted)
    );
    h.auth(|s| matches!(s, AuthState::Phone { error: None }))
        .await;
    // The next instance starts from the (fresh) account, never the old
    // instance's snapshot: no pinned peers, desired set or holds carry over.
    let seeds = h.fake.seeds();
    assert_eq!(seeds.len(), builds + 1);
    assert!(seeds.last().is_some_and(|s| s.session.is_stored()));
    let report = h.stop().await;
    assert_eq!(report.logout, None, "nothing left over");
}

/// Finding 3: shutdown reports a logout that is still incomplete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_reports_an_incomplete_logout() {
    let dir = tempfile::tempdir().unwrap();
    let (backend, fake) = FakeBackend::new(demo());
    let setup = Setup {
        cache_dir: Some(dir.path().to_path_buf()),
        ..Setup::default()
    };
    let mut h = start(backend, fake, setup).await;
    h.online().await;
    h.faults.fail_delete_of(SecretItem::CacheKey);
    h.send(Command::Logout).await;
    h.logged_out_6c().await;
    let report = h.stop().await;
    assert!(
        matches!(
            report.logout,
            Some(LogoutStatus::Done {
                remote: RemoteLogout::Revoked,
                local: LocalDeletion::Failed(_)
            })
        ),
        "{report:?}"
    );
}

/// Finding 5: after a logout the old account's auth keys are gone from the
/// store, not just its self user.
#[tokio::test(start_paused = true)]
async fn logout_removes_the_old_auth_keys() {
    let (backend, fake) = FakeBackend::new(demo());
    let setup = Setup {
        session: StoredSession::Blob(signed_in_blob_with_key(
            FAKE_SELF_ID,
            FAKE_SELF_HASH,
            OLD_KEY,
        )),
        ..Setup::default()
    };
    let mut h = start(backend, fake, setup).await;
    h.online().await;
    assert!(stored_has_old_key(&h.store));
    h.send(Command::Logout).await;
    let (_, local) = h.logged_out_6c().await;
    assert_eq!(local, LocalDeletion::Deleted);
    h.auth(|s| matches!(s, AuthState::Phone { .. })).await;
    // The signed-out instance writes its own DC state again.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!stored_has_old_key(&h.store), "old auth keys remain");
    assert_eq!(stored_self(&h.store).await, None);
    h.stop().await;
}
