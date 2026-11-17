//! Logout and shutdown (plan sections 5 and 16).

use std::path::Path;
use std::time::Duration;

use tokio::time::Instant;

use super::{Harness, Internal, Setup, chat, harness, start};
use crate::SHUTDOWN_DEADLINE;
use crate::api::{
    AuthState, Command, ConnState, Event, LocalDeletion, LogoutStatus, RemoteLogout, SaveStatus,
    UserError,
};
use crate::backend::fake::{FakeBackend, FakeScript, Fault, Op, QuitBehavior};
use crate::model::ThreadKey;
use crate::secret::{MemoryStore, SecretItem, SecretStore};
use crate::session::PersistHandle;

/// Whether the store holds a signed-in session (a fresh worker reads it).
async fn stored_signed_in(store: &MemoryStore) -> bool {
    let persist = PersistHandle::spawn(store.clone()).unwrap();
    let loaded = persist.load_session().await.unwrap();
    loaded.snapshot.is_signed_in().unwrap()
}

fn files_in(dir: &Path) -> usize {
    std::fs::read_dir(dir).map_or(0, Iterator::count)
}

async fn with_cache(dir: &Path) -> Harness {
    let (backend, fake) = FakeBackend::new(FakeScript::demo(2, 5));
    let setup = Setup {
        cache_dir: Some(dir.to_path_buf()),
        ..Setup::default()
    };
    start(backend, fake, setup).await
}

impl Harness {
    async fn logged_out(&mut self) -> (RemoteLogout, LocalDeletion) {
        match self
            .auth(|s| matches!(s, AuthState::LoggedOut { .. }))
            .await
        {
            AuthState::LoggedOut { remote, local } => (remote, local),
            _ => unreachable!(),
        }
    }
}

/// Remote revocation first, then the session item, then the cache: its key
/// is deleted before its files, and the next account gets a new key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_revokes_remotely_then_wipes_local_data() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = with_cache(dir.path()).await;
    h.online().await;
    let old_key = h.store.get(SecretItem::CacheKey).unwrap().unwrap();
    h.send(Command::Logout).await;
    h.auth(|s| *s == AuthState::LoggingOut).await;
    let (remote, local) = h.logged_out().await;
    assert_eq!(
        (remote, local),
        (RemoteLogout::Revoked, LocalDeletion::Deleted)
    );
    assert_eq!(h.fake.signed_out(), 1);
    h.auth(|s| matches!(s, AuthState::Phone { error: None }))
        .await;
    assert!(!stored_signed_in(&h.store).await);
    let new_key = h.store.get(SecretItem::CacheKey).unwrap().unwrap();
    assert_ne!(*old_key, *new_key, "the cache was re-keyed");
    let report = h.stop().await;
    assert_eq!(report.save, SaveStatus::Confirmed);
}

/// The cache files stay when their key cannot be deleted: without the key
/// they are unreadable, and deleting them first would leave a usable key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_delete_failure_keeps_the_cache_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = with_cache(dir.path()).await;
    h.online().await;
    h.faults.fail_delete_of(SecretItem::CacheKey);
    h.send(Command::Logout).await;
    let (remote, local) = h.logged_out().await;
    assert_eq!(remote, RemoteLogout::Revoked);
    assert!(matches!(local, LocalDeletion::Failed(_)), "{local:?}");
    assert!(files_in(dir.path()) > 0, "files kept");
    assert!(h.store.contains(SecretItem::CacheKey));
    assert!(!stored_signed_in(&h.store).await, "the session is gone");
    h.stop().await;
}

/// A cached run leaves a key and files; the next run has no cache (an
/// unusable directory, so no cache parts). Its logout restarts over the same
/// store.
async fn cached_then_disabled(dir: &Path) -> Harness {
    let mut h = with_cache(dir).await;
    h.online().await;
    let store = h.store.clone();
    h.stop().await;
    assert!(store.contains(SecretItem::CacheKey));
    assert!(files_in(dir) > 0);
    let (backend, fake) = FakeBackend::new(FakeScript::demo(2, 5));
    let setup = Setup {
        store: Some(store),
        ..Setup::default()
    };
    let mut h = start(backend, fake, setup).await;
    h.online().await;
    h
}

/// Round 26, finding 1: a logout in a run without a cache still deletes
/// the cache key an earlier run left, so its files are undecryptable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_without_a_cache_still_deletes_the_cache_key() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = cached_then_disabled(dir.path()).await;
    h.send(Command::Logout).await;
    let (_, local) = h.logged_out().await;
    assert_eq!(local, LocalDeletion::Deleted);
    assert!(!h.store.contains(SecretItem::CacheKey), "the key is gone");
    assert!(!h.store.contains(SecretItem::LogoutPending));
    h.stop().await;
}

/// Round 26, finding 1: if that key delete fails, the logout is not done:
/// the marker stays, so the next start resumes the deletion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_without_a_cache_keeps_the_marker_when_the_key_delete_fails() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = cached_then_disabled(dir.path()).await;
    h.faults.fail_delete_of(SecretItem::CacheKey);
    h.send(Command::Logout).await;
    let (_, local) = h.logged_out().await;
    assert!(matches!(local, LocalDeletion::Failed(_)), "{local:?}");
    assert!(h.store.contains(SecretItem::CacheKey));
    assert!(h.store.contains(SecretItem::LogoutPending), "marker kept");
    assert!(!stored_signed_in(&h.store).await, "the session is gone");
    h.stop().await;
}

/// Round 29, item 4: cached run, then a cache-less logout (key deleted,
/// files kept), then a cached restart: the new run makes a new key, cannot
/// open the old database with it, and destroys and recreates it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cached_restart_after_a_cacheless_logout_recreates_the_database() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cache.db");
    let mut h = cached_then_disabled(dir.path()).await;
    let old_inode = std::fs::metadata(&db).unwrap().ino();
    h.send(Command::Logout).await;
    assert_eq!(h.logged_out().await.1, LocalDeletion::Deleted);
    let store = h.store.clone();
    h.stop().await;
    assert!(db.exists(), "the files outlive the key");
    assert!(!store.contains(SecretItem::CacheKey));

    let (backend, fake) = FakeBackend::new(FakeScript::demo(2, 5));
    let setup = Setup {
        store: Some(store),
        cache_dir: Some(dir.path().to_path_buf()),
        ..Setup::default()
    };
    let h = start(backend, fake, setup).await;
    assert!(h.store.contains(SecretItem::CacheKey), "a new key");
    let new_inode = std::fs::metadata(&db).unwrap().ino();
    assert_ne!(old_inode, new_inode, "the old database was destroyed");
    h.stop().await;
}

/// `AUTH_KEY_UNREGISTERED` mid-session: the local part of logout runs without
/// a remote call, and the user is asked to sign in again.
#[tokio::test(start_paused = true)]
async fn remote_revocation_wipes_local_data() {
    let mut h = harness(FakeScript::demo(2, 5), None).await;
    h.online().await;
    h.fake.revoke_session();
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    let (remote, local) = h.logged_out().await;
    assert_eq!(
        (remote, local),
        (RemoteLogout::Revoked, LocalDeletion::Deleted)
    );
    h.auth(|s| {
        matches!(
            s,
            AuthState::Phone {
                error: Some(UserError::SessionRevoked)
            }
        )
    })
    .await;
    assert_eq!(h.fake.signed_out(), 0, "no remote call");
    assert!(!stored_signed_in(&h.store).await);
    h.stop().await;
}

/// A stored session the server no longer accepts is wiped at startup.
#[tokio::test(start_paused = true)]
async fn revoked_stored_session_is_wiped_at_startup() {
    let (backend, fake) = FakeBackend::new(FakeScript::demo(2, 5));
    fake.revoke_session();
    let mut h = start(backend, fake, Setup::default()).await;
    let (remote, _) = h.logged_out().await;
    assert_eq!(remote, RemoteLogout::Revoked);
    h.auth(|s| {
        matches!(
            s,
            AuthState::Phone {
                error: Some(UserError::SessionRevoked)
            }
        )
    })
    .await;
    assert!(!stored_signed_in(&h.store).await);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn remote_timeout_still_deletes_local_data() {
    let mut h = harness(FakeScript::demo(2, 5), None).await;
    h.online().await;
    h.fake.inject(Op::SignOut, Fault::Hang);
    h.send(Command::Logout).await;
    let started = Instant::now();
    let (remote, local) = h.logged_out().await;
    assert_eq!(
        (remote, local),
        (RemoteLogout::TimedOut, LocalDeletion::Deleted)
    );
    assert!(started.elapsed() >= Duration::from_secs(10));
    assert!(!stored_signed_in(&h.store).await);
    h.stop().await;
}

/// Shutdown during the remote step: the remote call is cut short, the local
/// deletion still runs, and the report says so.
///
/// Real time (review round 28): the local deletion waits on the persistence
/// worker, an OS thread the test clock cannot see. On a paused clock the
/// idle runtime jumped straight to the shutdown deadline whenever that
/// thread was slow to answer (a loaded machine), and the report read
/// `Incomplete`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_during_remote_logout_still_deletes_local_data() {
    let mut h = harness(FakeScript::demo(2, 5), None).await;
    h.online().await;
    h.fake.inject(Op::SignOut, Fault::Hang);
    h.send(Command::Logout).await;
    h.auth(|s| *s == AuthState::LoggingOut).await;
    let store = h.store.clone();
    let report = h.stop().await;
    assert_eq!(
        report.logout,
        Some(LogoutStatus::Done {
            remote: RemoteLogout::Failed(UserError::ShuttingDown),
            local: LocalDeletion::Deleted,
        })
    );
    assert!(!stored_signed_in(&store).await);
}

/// Shutdown while the local step cannot reach the Keychain: bounded by the
/// deadline and reported as incomplete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_during_a_stalled_local_logout_is_incomplete() {
    let mut h = harness(FakeScript::demo(2, 5), None).await;
    h.online().await;
    h.faults.hold();
    h.send(Command::Logout).await;
    h.auth(|s| *s == AuthState::LoggingOut).await;
    // Let the remote step finish; the local one then waits on the store.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let faults = h.faults.clone();
    let started = std::time::Instant::now();
    let report = h.stop().await;
    faults.release();
    assert!(started.elapsed() < SHUTDOWN_DEADLINE + Duration::from_secs(2));
    assert_eq!(report.logout, Some(LogoutStatus::Incomplete));
}

/// The session reached the Keychain before shutdown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_reports_confirmed_when_saved() {
    let mut h = harness(FakeScript::demo(2, 5), None).await;
    // The startup check can confirm the session before or after the bootstrap
    // goes online; waiting for it only after `online` hung when it came first.
    let before = h.online().await;
    if !before
        .iter()
        .any(|e| matches!(e, Event::Auth(s) if s.is_signed_in()))
    {
        h.auth(AuthState::is_signed_in).await;
    }
    let report = h.stop().await;
    assert_eq!(report.save, SaveStatus::Confirmed);
    assert_eq!(report.logout, None);
}

/// The store stalls with a write queued: shutdown gives up at the deadline
/// and says the save is unconfirmed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_reports_unconfirmed_when_the_store_stalls() {
    let (backend, fake) = FakeBackend::new(FakeScript::demo(2, 5));
    let setup = Setup {
        hold_store: true,
        ..Setup::default()
    };
    let mut h = start(backend, fake, setup).await;
    // Every build makes a critical write, which now waits for the store.
    h.online().await;
    let faults = h.faults.clone();
    let started = std::time::Instant::now();
    let report = h.stop().await;
    faults.release();
    assert_eq!(report.save, SaveStatus::Unconfirmed);
    assert!(started.elapsed() < SHUTDOWN_DEADLINE + Duration::from_secs(2));
}

/// Review finding (task 6): shutdown while a retirement runs must not detach
/// the runner. The retirement's caps shrink to the shutdown caps, the runner
/// is aborted and joined, and the session is revoked before shutdown returns.
#[tokio::test(start_paused = true)]
async fn shutdown_during_retirement_still_revokes_within_the_bound() {
    let mut h = harness(FakeScript::demo(2, 5), None).await;
    h.online().await;
    h.fake.set_quit(QuitBehavior::Ignore);
    h.internal.send(Internal::Wake).await.unwrap();
    h.until(|e| matches!(e, Event::Connection(ConnState::Rebuilding { .. })))
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.fake.revoked(), 0, "still retiring");
    let fake = h.fake.clone();
    let started = Instant::now();
    h.stop().await;
    assert_eq!(fake.revoked(), 1, "the retired instance was revoked");
    assert_eq!(fake.runners_finished(), 1, "the runner was joined");
    assert!(
        started.elapsed() <= SHUTDOWN_DEADLINE,
        "shutdown took {:?}",
        started.elapsed()
    );
}
