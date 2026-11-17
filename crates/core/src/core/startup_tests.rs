//! Startup and shutdown of the core thread (review round 8, task 6c).

use std::time::{Duration, Instant};

use super::CacheConfig;
use super::{CoreConfig, SendError, start_with};
use crate::SHUTDOWN_DEADLINE;
use crate::api::{AuthState, Command, Event, SaveStatus};
use crate::backend::fake::{FAKE_SELF_HASH, FAKE_SELF_ID, FakeBackend, FakeScript};
use crate::model::ThreadKey;
use crate::secret::{FaultyStore, MemoryStore, SecretItem, SecretStore};

/// Finding 4: a stalled Keychain read at startup (a prompt nobody answers)
/// must not hold up the shutdown acknowledgement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_during_a_stalled_session_load_acks_promptly() {
    let (fake, _h) = FakeBackend::new(FakeScript::default());
    let (store, control) = FaultyStore::new(MemoryStore::new());
    control.hold();
    let mut core = start_with(CoreConfig::default(), move |_| fake, store, || {}).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = Instant::now();
    let done = core.begin_shutdown().unwrap();
    let report = tokio::time::timeout(Duration::from_secs(3), done)
        .await
        .expect("the ack waited for the stalled store")
        .unwrap();
    control.release();
    assert_eq!(report.save, SaveStatus::Unconfirmed);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

/// Finding 5: shutdown while the *command* queue is full. Nobody reads
/// events, so the actor blocks emitting and commands pile up; shutdown runs
/// on the control plane and still acknowledges within its deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_with_a_full_command_queue_acks_within_the_deadline() {
    let (fake, _h) = FakeBackend::new(FakeScript::default());
    let (store, _control) = FaultyStore::new(MemoryStore::new());
    let mut core = start_with(CoreConfig::default(), move |_| fake, store, || {}).unwrap();
    let mut full = false;
    for _ in 0..100_000 {
        // Each invalid phone number is answered with an auth event.
        match core.send(Command::SubmitPhone("x".into())) {
            Ok(()) => {}
            Err(SendError::Full(_)) => {
                full = true;
                break;
            }
            Err(SendError::Closed(_)) => break,
        }
        tokio::task::yield_now().await;
    }
    assert!(full, "the command queue never filled");
    let started = Instant::now();
    let report = tokio::time::timeout(
        SHUTDOWN_DEADLINE + Duration::from_secs(2),
        core.begin_shutdown().unwrap(),
    )
    .await
    .expect("shutdown was blocked by the full queue")
    .unwrap();
    assert!(started.elapsed() <= SHUTDOWN_DEADLINE + Duration::from_secs(1));
    assert_eq!(report.logout, None);
}

/// The next event matching `pred`, within 30 s.
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "test helper, not a #[test] item, so allow-*-in-tests does not cover it"
)]
async fn next_matching(
    core: &mut super::CoreHandle,
    pred: impl Fn(&crate::api::Event) -> bool,
) -> crate::api::Event {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match core.recv().await {
                Some(e) if pred(&e) => return e,
                Some(_) => {}
                None => panic!("core stopped"),
            }
        }
    })
    .await
    .expect("timed out waiting for an event")
}

/// Task 9b: history cached through `core::start` with a `CacheConfig`
/// survives a full core restart over the same directory and key store: the
/// second run serves the chat's tail from disk while the server's history
/// call hangs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_history_survives_a_core_restart() {
    use crate::api::{ConnState, Event, HistoryDir};
    use crate::backend::fake::{FAKE_SELF_HASH, FAKE_SELF_ID, Fault, Op};
    use crate::model::{ChatId, PeerKind};
    use crate::secret::{SecretItem, SecretStore};

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("cache");
    // The Keychain stand-in: the session and, after the first run, the
    // cache key both survive the restart.
    let store = MemoryStore::new();
    let blob = crate::session::signed_in_blob(FAKE_SELF_ID, FAKE_SELF_HASH);
    store.set(SecretItem::Session, &blob).unwrap();
    let chat = ChatId {
        kind: PeerKind::User,
        id: 1,
    };
    let config = || CoreConfig {
        cache: Some(super::CacheConfig { dir: dir.clone() }),
        ..CoreConfig::default()
    };
    let online = |e: &Event| matches!(e, Event::Connection(ConnState::Online));
    let tail = |e: &Event| {
        matches!(e, Event::History { thread: c, dir: HistoryDir::Tail, messages, .. }
            if *c == ThreadKey::whole(chat) && !messages.is_empty())
    };

    for run in 0..2 {
        let (fake, handle) = FakeBackend::new(FakeScript::demo(1, 120));
        if run == 1 {
            // Only the disk cache can answer the second run's open.
            handle.inject(Op::History, Fault::Hang);
        }
        let mut core = start_with(config(), move |_| fake, store.clone(), || {}).unwrap();
        next_matching(&mut core, online).await;
        core.send(Command::OpenThread(ThreadKey::whole(chat)))
            .unwrap();
        let Event::History { messages, .. } = next_matching(&mut core, tail).await else {
            unreachable!()
        };
        let ids: Vec<i32> = messages.iter().map(|m| m.id.0).collect();
        assert_eq!(ids, (71..=120).collect::<Vec<_>>(), "run {run}");
        tokio::time::timeout(Duration::from_secs(10), core.begin_shutdown().unwrap())
            .await
            .unwrap()
            .unwrap();
        drop(handle);
    }
    assert!(
        store.get(SecretItem::CacheKey).unwrap().is_some(),
        "the key is in the store"
    );
    assert!(crate::cache::wait_closed(
        Some(&dir),
        Duration::from_secs(10)
    ));
}

/// Review round 25, finding 4: an unreadable logout marker means unknown,
/// so startup opens no cache: a malformed cache key (which opening answers
/// by destroying the cache and replacing the key) is left alone until
/// `RetryStartup` reads the marker as absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreadable_marker_opens_no_cache_until_the_retry() {
    const MALFORMED: &[u8] = &[1, 2, 3];
    let dir = tempfile::tempdir().unwrap();
    let inner = MemoryStore::new();
    let session = crate::session::signed_in_blob(FAKE_SELF_ID, FAKE_SELF_HASH);
    inner.set(SecretItem::Session, &session).unwrap();
    inner.set(SecretItem::CacheKey, MALFORMED).unwrap();
    let (store, control) = FaultyStore::new(inner.clone());
    control.fail_get_of(SecretItem::LogoutPending);
    let config = CoreConfig {
        cache: Some(CacheConfig {
            dir: dir.path().to_path_buf(),
        }),
        ..CoreConfig::default()
    };
    let (fake, _h) = FakeBackend::new(FakeScript::demo(1, 1));
    let mut core = start_with(config, move |_| fake, store, || {}).unwrap();
    next_matching(&mut core, |e| {
        matches!(e, Event::Auth(AuthState::StartupBlocked { .. }))
    })
    .await;
    let key = inner.get(SecretItem::CacheKey).unwrap().unwrap();
    assert_eq!(key.as_slice(), MALFORMED, "cache provisioned while unknown");
    control.heal_item(SecretItem::LogoutPending);
    core.send(Command::RetryStartup).unwrap();
    next_matching(
        &mut core,
        |e| matches!(e, Event::Auth(s) if s.is_signed_in()),
    )
    .await;
    let key = inner.get(SecretItem::CacheKey).unwrap().unwrap();
    assert_ne!(key.as_slice(), MALFORMED, "not provisioned after the retry");
    let done = core.begin_shutdown().unwrap();
    tokio::time::timeout(SHUTDOWN_DEADLINE + Duration::from_secs(2), done)
        .await
        .unwrap()
        .unwrap();
    assert!(crate::cache::wait_closed(
        Some(dir.path()),
        Duration::from_secs(10)
    ));
}

/// Review round 28, finding 1: `RetryStartup` finds the marker absent and
/// provisions the cache, whose key read stalls (a Keychain prompt nobody
/// answers). Shutdown is still acknowledged by its deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_during_a_stalled_deferred_cache_provisioning_acks_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let inner = MemoryStore::new();
    let session = crate::session::signed_in_blob(FAKE_SELF_ID, FAKE_SELF_HASH);
    inner.set(SecretItem::Session, &session).unwrap();
    let (store, control) = FaultyStore::new(inner);
    control.fail_get_of(SecretItem::LogoutPending);
    let config = CoreConfig {
        cache: Some(CacheConfig {
            dir: dir.path().to_path_buf(),
        }),
        ..CoreConfig::default()
    };
    let (fake, _h) = FakeBackend::new(FakeScript::demo(1, 1));
    let mut core = start_with(config, move |_| fake, store, || {}).unwrap();
    next_matching(&mut core, |e| {
        matches!(e, Event::Auth(AuthState::StartupBlocked { .. }))
    })
    .await;
    control.hold_item(SecretItem::CacheKey);
    control.heal_item(SecretItem::LogoutPending);
    core.send(Command::RetryStartup).unwrap();
    // Until the provisioning is stalled on the key (bounded in wall time).
    let entered = Instant::now();
    while !control.hold_entered(SecretItem::CacheKey) {
        assert!(
            entered.elapsed() < Duration::from_secs(30),
            "the key was never read"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let started = Instant::now();
    let done = core.begin_shutdown().unwrap();
    let acked = tokio::time::timeout(SHUTDOWN_DEADLINE + Duration::from_secs(2), done).await;
    let elapsed = started.elapsed();
    control.release();
    let report = acked
        .unwrap_or_else(|_| panic!("no shutdown ack after {elapsed:?}"))
        .expect("the core thread dropped the ack");
    assert_eq!(report.logout, None, "{report:?}");
    assert!(crate::cache::wait_closed(
        Some(dir.path()),
        Duration::from_secs(15)
    ));
}
