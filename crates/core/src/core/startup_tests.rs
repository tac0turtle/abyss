//! Startup of the core thread over a cache.

use std::time::Duration;

use super::{CoreConfig, start_with};
use crate::api::Command;
use crate::backend::fake::{FakeBackend, FakeScript};
use crate::model::ThreadKey;
use crate::secret::MemoryStore;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_keeps_the_session_until_platform_notifications_are_cleared() {
    use crate::api::{AuthState, Event, LocalDeletion};
    use crate::backend::fake::{FAKE_SELF_HASH, FAKE_SELF_ID};
    use crate::events::NotificationCleanupPolicy;
    use crate::secret::{SecretItem, SecretStore};

    let store = MemoryStore::new();
    let blob = crate::session::signed_in_blob(FAKE_SELF_ID, FAKE_SELF_HASH);
    store.set(SecretItem::Session, &blob).unwrap();
    let (fake, _handle) = FakeBackend::new(FakeScript::demo(1, 0));
    let config = CoreConfig {
        notification_cleanup: NotificationCleanupPolicy::Required,
        ..CoreConfig::default()
    };
    let mut core = start_with(config, move |_| fake, store.clone(), || {}).unwrap();
    next_matching(
        &mut core,
        |event| matches!(event, Event::Auth(state) if state.is_signed_in()),
    )
    .await;
    let awaiting = |event: &Event| matches!(event, Event::ClearNotifications(_));
    core.send(Command::Logout).unwrap();
    let Event::ClearNotifications(first) = next_matching(&mut core, awaiting).await else {
        unreachable!();
    };
    assert!(store.get(SecretItem::Session).unwrap().is_some());
    drop(first);
    next_matching(&mut core, |event| {
        matches!(
            event,
            Event::Auth(AuthState::LoggedOut {
                local: LocalDeletion::Failed(_),
                ..
            })
        )
    })
    .await;
    assert!(store.get(SecretItem::Session).unwrap().is_some());
    assert!(store.get(SecretItem::LogoutPending).unwrap().is_some());

    core.send(Command::Logout).unwrap();
    let Event::ClearNotifications(retry) = next_matching(&mut core, awaiting).await else {
        unreachable!();
    };
    assert!(store.get(SecretItem::Session).unwrap().is_some());
    retry.complete();
    next_matching(&mut core, |event| {
        matches!(
            event,
            Event::Auth(AuthState::LoggedOut {
                local: LocalDeletion::Deleted,
                ..
            })
        )
    })
    .await;
    assert!(store.get(SecretItem::Session).unwrap().is_none());
    assert!(store.get(SecretItem::LogoutPending).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_resumed_without_a_center_erases_secrets_but_defers_notification_cleanup() {
    use crate::api::{AuthState, Event, LocalDeletion};
    use crate::backend::fake::{FAKE_SELF_HASH, FAKE_SELF_ID};
    use crate::events::NotificationCleanupPolicy::{Disabled, Required};
    use crate::secret::{SecretItem, SecretStore};

    let store = MemoryStore::new();
    let blob = crate::session::signed_in_blob(FAKE_SELF_ID, FAKE_SELF_HASH);
    store.set(SecretItem::Session, &blob).unwrap();
    store.set(SecretItem::CacheKey, &[17; 32]).unwrap();
    let launch = |notification_cleanup| {
        let (fake, handle) = FakeBackend::new(FakeScript::demo(1, 0));
        let config = CoreConfig {
            notification_cleanup,
            ..CoreConfig::default()
        };
        (
            start_with(config, move |_| fake, store.clone(), || {}).unwrap(),
            handle,
        )
    };
    let signed_in = |event: &Event| matches!(event, Event::Auth(state) if state.is_signed_in());
    let clear = |event: &Event| matches!(event, Event::ClearNotifications(_));
    let logged_out = |event: &Event| matches!(event, Event::Auth(AuthState::LoggedOut { .. }));

    let (mut bundled, _bundled_handle) = launch(Required);
    next_matching(&mut bundled, signed_in).await;
    bundled.send(Command::Logout).unwrap();
    drop(next_matching(&mut bundled, clear).await);
    next_matching(&mut bundled, logged_out).await;
    bundled.begin_shutdown().unwrap().await.unwrap();

    let (mut incapable, _incapable_handle) = launch(Disabled);
    assert!(matches!(
        next_matching(&mut incapable, |event| logged_out(event) || clear(event)).await,
        Event::Auth(AuthState::LoggedOut {
            local: LocalDeletion::Deleted,
            ..
        })
    ));
    assert!(store.get(SecretItem::Session).unwrap().is_none());
    assert!(store.get(SecretItem::CacheKey).unwrap().is_none());
    assert!(store.get(SecretItem::LogoutPending).unwrap().is_some());
    incapable.begin_shutdown().unwrap().await.unwrap();

    // A new session may have been saved while cleanup was deferred. The next
    // capable startup must clear the old banners before serving that session.
    store.set(SecretItem::Session, &blob).unwrap();
    let (mut bundled, _bundled_handle) = launch(Required);
    let clearance_or_session = |event: &Event| signed_in(event) || clear(event);
    let first = next_matching(&mut bundled, clearance_or_session).await;
    assert!(clear(&first));
    drop(first);
    let blocked = next_matching(&mut bundled, |event| {
        signed_in(event) || matches!(event, Event::Auth(AuthState::StartupBlocked { .. }))
    })
    .await;
    assert!(matches!(
        blocked,
        Event::Auth(AuthState::StartupBlocked { .. })
    ));
    assert!(store.get(SecretItem::LogoutPending).unwrap().is_some());
    bundled.send(Command::RetryStartup).unwrap();
    let Event::ClearNotifications(completion) =
        next_matching(&mut bundled, clearance_or_session).await
    else {
        unreachable!();
    };
    completion.complete();
    next_matching(&mut bundled, signed_in).await;
    assert!(store.get(SecretItem::LogoutPending).unwrap().is_none());
    assert!(store.get(SecretItem::Session).unwrap().is_some());
    bundled.begin_shutdown().unwrap().await.unwrap();
}
