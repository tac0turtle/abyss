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
