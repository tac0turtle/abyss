//! Logout and shutdown (plan sections 5 and 16).

use std::path::Path;
use std::time::Duration;

use tokio::time::Instant;

use super::{Harness, Setup, chat, harness, start};
use crate::SHUTDOWN_DEADLINE;
use crate::api::{
    AuthState, Command, Event, LocalDeletion, LogoutStatus, RemoteLogout, SaveStatus, UserError,
};
use crate::backend::fake::{FakeBackend, FakeScript, Fault, Op};
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
