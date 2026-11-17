use std::time::{Duration, Instant};

use grammers_session::SessionData;
use grammers_session::types::{ChannelKind, PeerAuth, PeerKind, UpdatesState};

use super::critical::{CriticalState, Decoded, MAX_DC_OPTIONS, decode, encode};
use super::worker::PersistMsg;
use super::*;
use crate::secret::{
    FaultControl, FaultyStore, MemoryStore, SecretItem, SecretStore, StoreError, StoreOp,
};

const SELF_ID: i64 = 777_000;
const LIMIT: Duration = Duration::from_secs(5);

fn spawn_memory() -> (PersistHandle, MemoryStore) {
    let store = MemoryStore::new();
    (PersistHandle::spawn(store.clone()).unwrap(), store)
}

fn spawn_faulty() -> (PersistHandle, MemoryStore, FaultControl) {
    let store = MemoryStore::new();
    let (faulty, control) = FaultyStore::new(store.clone());
    (PersistHandle::spawn(faulty).unwrap(), store, control)
}

fn dc(id: i32, key_byte: u8) -> DcOption {
    let mut option = SessionData::default().dc_options.remove(&id).unwrap();
    option.auth_key = Some([key_byte; 256]);
    option
}

fn self_user() -> PeerInfo {
    PeerInfo::User {
        id: SELF_ID,
        auth: Some(PeerAuth::from_hash(42)),
        bot: Some(false),
        is_self: Some(true),
    }
}

fn user(id: i64, hash: Option<i64>) -> PeerInfo {
    PeerInfo::User {
        id,
        auth: hash.map(PeerAuth::from_hash),
        bot: None,
        is_self: None,
    }
}

fn channel(id: i64, hash: Option<i64>, kind: Option<ChannelKind>) -> PeerInfo {
    PeerInfo::Channel {
        id,
        auth: hash.map(PeerAuth::from_hash),
        kind,
    }
}

fn session(persist: &PersistHandle) -> AbyssSession {
    AbyssSession::from_snapshot(persist.fresh_session()).unwrap()
}

fn rev(session: &AbyssSession) -> u64 {
    session.snapshot().rev().unwrap()
}

async fn barrier(session: &AbyssSession) -> Result<(), PersistError> {
    tokio::time::timeout(LIMIT, session.barrier())
        .await
        .expect("barrier must resolve")
}

fn stored_state(store: &MemoryStore) -> CriticalState {
    match decode(&store.get(SecretItem::Session).unwrap().unwrap()) {
        Decoded::State(state) => state,
        other => panic!("stored blob does not decode: {other:?}"),
    }
}

// --- AbyssSession ---

#[tokio::test]
async fn only_critical_changes_write() {
    let (persist, store) = spawn_memory();
    let s = session(&persist);

    s.cache_peer(user(5, Some(1))).await.unwrap();
    s.cache_peer(channel(9, Some(2), None)).await.unwrap();
    s.set_update_state(UpdateState::All(UpdatesState::default()))
        .await
        .unwrap();
    s.set_update_state(UpdateState::Channel { id: 9, pts: 3 })
        .await
        .unwrap();
    assert_eq!(rev(&s), 0);
    barrier(&s).await.unwrap();
    assert!(
        !store.contains(SecretItem::Session),
        "peers and updates are never persisted"
    );

    s.set_dc_option(&dc(2, 7)).await.unwrap();
    assert_eq!(rev(&s), 1);
    s.set_home_dc_id(4).await.unwrap();
    assert_eq!(rev(&s), 2);
    s.cache_peer(self_user()).await.unwrap();
    assert_eq!(rev(&s), 3);
    barrier(&s).await.unwrap();

    let stored = stored_state(&store);
    assert_eq!(stored.home_dc, 4);
    assert_eq!(stored.dc_option(2).unwrap().auth_key, Some([7; 256]));
    assert_eq!(stored.self_user.unwrap().id, SELF_ID);
}

#[tokio::test]
async fn identical_dc_reset_keeps_rev_and_barrier_returns_previous_outcome() {
    let (persist, _store, control) = spawn_faulty();
    let s = session(&persist);
    s.set_dc_option(&dc(2, 7)).await.unwrap();
    assert_eq!(rev(&s), 1);
    barrier(&s).await.unwrap();
    let calls = control.calls();

    // The runner re-sets the same option on every reconnect.
    s.set_dc_option(&dc(2, 7)).await.unwrap();
    s.set_home_dc_id(s.home_dc_id().unwrap()).await.unwrap();
    assert_eq!(rev(&s), 1);
    barrier(&s).await.unwrap();
    assert_eq!(control.calls(), calls, "no store call for unchanged state");

    // A failed write is the "previous outcome" an unchanged barrier reports.
    control.fail(StoreOp::Set);
    s.set_dc_option(&dc(2, 8)).await.unwrap();
    assert_eq!(
        barrier(&s).await,
        Err(PersistError::Store(StoreError::Injected))
    );
    s.set_dc_option(&dc(2, 8)).await.unwrap();
    assert_eq!(
        barrier(&s).await,
        Err(PersistError::Store(StoreError::Injected))
    );

    // A later successful write covers the failed revision.
    control.heal(StoreOp::Set);
    s.set_dc_option(&dc(2, 9)).await.unwrap();
    barrier(&s).await.unwrap();
}

#[tokio::test]
async fn barrier_at_startup_with_loaded_state_resolves_ok() {
    let (persist, store) = spawn_memory();
    let mut state = CriticalState::default();
    state.set_dc_option(&dc(4, 1)).unwrap();
    state.merge_self(&self_user());
    store
        .set(SecretItem::Session, &encode(&state).unwrap())
        .unwrap();

    let loaded = persist.load_session().await.unwrap();
    assert_eq!(loaded.status, LoadStatus::Loaded);
    assert!(loaded.snapshot.is_signed_in().unwrap());
    assert_eq!(loaded.snapshot.rev().unwrap(), 0);
    tokio::time::timeout(LIMIT, loaded.snapshot.barrier())
        .await
        .unwrap()
        .unwrap();

    let s = AbyssSession::from_snapshot(loaded.snapshot).unwrap();
    assert_eq!(s.dc_option(4).unwrap().unwrap().auth_key, Some([1; 256]));
    // Re-setting what was loaded writes nothing.
    s.set_dc_option(&dc(4, 1)).await.unwrap();
    assert_eq!(rev(&s), 0);
}

#[tokio::test]
async fn missing_item_is_logged_out() {
    let (persist, _store) = spawn_memory();
    let loaded = persist.load_session().await.unwrap();
    assert_eq!(loaded.status, LoadStatus::Missing);
    assert!(!loaded.snapshot.is_signed_in().unwrap());
}

#[tokio::test]
async fn unknown_blob_version_or_corrupt_blob_is_logged_out() {
    let (persist, store) = spawn_memory();
    let mut state = CriticalState::default();
    state.merge_self(&self_user());
    let mut blob = encode(&state).unwrap().to_vec();
    blob[0] = 9; // postcard varint u16: version 9
    store.set(SecretItem::Session, &blob).unwrap();
    let loaded = persist.load_session().await.unwrap();
    assert_eq!(loaded.status, LoadStatus::UnknownVersion(9));
    assert!(!loaded.snapshot.is_signed_in().unwrap());

    let mut truncated = encode(&state).unwrap().to_vec();
    truncated.truncate(truncated.len() - 3);
    store.set(SecretItem::Session, &truncated).unwrap();
    assert_eq!(
        persist.load_session().await.unwrap().status,
        LoadStatus::Corrupt
    );

    let mut trailing = encode(&state).unwrap().to_vec();
    trailing.push(0);
    store.set(SecretItem::Session, &trailing).unwrap();
    assert_eq!(
        persist.load_session().await.unwrap().status,
        LoadStatus::Corrupt
    );
}

#[test]
fn blob_roundtrip_keeps_keys_addresses_and_self_user() {
    let mut state = CriticalState::default();
    state.home_dc = 4;
    state.set_dc_option(&dc(4, 0xab)).unwrap();
    state.merge_self(&self_user());
    let blob = encode(&state).unwrap();
    let Decoded::State(back) = decode(&blob) else {
        panic!("roundtrip failed")
    };
    assert_eq!(back.home_dc, 4);
    assert_eq!(back.dc_option(4), Some(dc(4, 0xab)));
    assert_eq!(back.dc_option(1), state.dc_option(1));
    assert_eq!(back.self_info(), Some(self_user()));
    assert_eq!(*encode(&back).unwrap(), *blob, "encoding is deterministic");
    assert!(
        !format!("{back:?}").contains("171"),
        "Debug never shows key bytes"
    );
}

#[test]
fn dc_options_are_bounded() {
    let mut state = CriticalState::default();
    let template = dc(2, 1);
    let mut added = 0;
    for id in 100..200 {
        let mut option = template.clone();
        option.id = id;
        if state.set_dc_option(&option).is_err() {
            break;
        }
        added += 1;
    }
    assert_eq!(
        added + SessionData::default().dc_options.len(),
        MAX_DC_OPTIONS
    );
    let mut extra = template;
    extra.id = 999;
    assert!(state.set_dc_option(&extra).is_err());
    // Existing ids can still be updated at the bound.
    assert!(state.set_dc_option(&dc(2, 3)).is_ok());
}

#[tokio::test]
async fn self_user_survives_eviction_and_sentinel_resolves() {
    let (persist, _store) = spawn_memory();
    let s = session(&persist);
    assert_eq!(s.peer(PeerId::self_user()).await.unwrap(), None);

    s.cache_peer(self_user()).await.unwrap();
    let pinned = channel(1_000, Some(11), Some(ChannelKind::Megagroup));
    s.pin_peers([pinned.clone()]);
    for id in 1..=10_000 {
        s.cache_peer(user(id, Some(id))).await.unwrap();
    }

    assert_eq!(
        s.peer(PeerId::self_user()).await.unwrap(),
        Some(self_user())
    );
    assert_eq!(
        s.peer(PeerId::user(SELF_ID).unwrap()).await.unwrap(),
        Some(self_user())
    );
    assert_eq!(s.peer(pinned.id()).await.unwrap(), Some(pinned));
    assert_eq!(s.peer(PeerId::user(1).unwrap()).await.unwrap(), None);
    assert!(
        s.peer(PeerId::user(10_000).unwrap())
            .await
            .unwrap()
            .is_some()
    );
    assert!(s.peers().len() <= MAX_UNPINNED_PEERS + 1);

    // A non-self message about the self user never clears the self flag.
    s.cache_peer(user(SELF_ID, None)).await.unwrap();
    assert_eq!(
        s.peer(PeerId::self_user()).await.unwrap(),
        Some(self_user())
    );
}

#[tokio::test]
async fn peer_merge_never_loses_a_hash() {
    let (persist, _store) = spawn_memory();
    let s = session(&persist);
    s.cache_peer(channel(5, Some(55), None)).await.unwrap();
    s.cache_peer(channel(5, None, Some(ChannelKind::Broadcast)))
        .await
        .unwrap();
    assert_eq!(
        s.peer(PeerId::channel(5).unwrap()).await.unwrap(),
        Some(channel(5, Some(55), Some(ChannelKind::Broadcast)))
    );

    // Same through the pinned path.
    s.pin_peers([user(6, Some(66))]);
    s.pin_peers([user(6, None)]);
    s.cache_peer(user(6, None)).await.unwrap();
    assert_eq!(
        s.peer(PeerId::user(6).unwrap()).await.unwrap(),
        Some(user(6, Some(66)))
    );

    // Self-user merges keep the hash too, without a new revision.
    s.cache_peer(self_user()).await.unwrap();
    let before = rev(&s);
    s.cache_peer(PeerInfo::User {
        id: SELF_ID,
        auth: None,
        bot: None,
        is_self: Some(true),
    })
    .await
    .unwrap();
    assert_eq!(rev(&s), before);
    assert_eq!(
        s.peer(PeerId::self_user()).await.unwrap(),
        Some(self_user())
    );
}

#[tokio::test]
async fn unpinned_peers_return_to_fifo_on_replace() {
    let (persist, _store) = spawn_memory();
    let s = session(&persist);
    s.pin_peers([user(1, Some(1))]);
    s.cache_peer(user(2, Some(2))).await.unwrap();
    s.cache_peer(user(3, None)).await.unwrap();
    let ids = [2, 3, 4].map(|id| PeerId::user(id).unwrap());
    // 3 is known without a hash and 4 not at all: neither is pinned.
    assert_eq!(
        s.retain_pinned(&ids),
        crate::session::Pinned {
            pinned: 1,
            unknown: 2
        }
    );
    for id in 10..10 + i64::try_from(MAX_UNPINNED_PEERS).unwrap() {
        s.cache_peer(user(id, Some(id))).await.unwrap();
    }
    assert_eq!(s.peer(PeerId::user(1).unwrap()).await.unwrap(), None);
    assert!(s.peer(PeerId::user(2).unwrap()).await.unwrap().is_some());
}

#[tokio::test]
async fn snapshot_restore_roundtrip_and_revoked_writer() {
    let (persist, store) = spawn_memory();
    let old = session(&persist);
    old.set_dc_option(&dc(2, 3)).await.unwrap();
    old.set_home_dc_id(2).await.unwrap();
    old.cache_peer(self_user()).await.unwrap();
    let pinned = channel(77, Some(7), None);
    old.pin_peers([pinned.clone()]);
    old.cache_peer(user(88, Some(8))).await.unwrap();
    old.set_update_state(UpdateState::All(UpdatesState {
        pts: 10,
        ..UpdatesState::default()
    }))
    .await
    .unwrap();
    let rev_before = rev(&old);

    let new = AbyssSession::from_snapshot(old.snapshot()).unwrap();
    assert_eq!(new.home_dc_id().unwrap(), 2);
    assert_eq!(new.dc_option(2).unwrap(), Some(dc(2, 3)));
    assert_eq!(
        new.peer(PeerId::self_user()).await.unwrap(),
        Some(self_user())
    );
    assert_eq!(new.peer(pinned.id()).await.unwrap(), Some(pinned));
    assert_eq!(new.peer(PeerId::user(88).unwrap()).await.unwrap(), None);
    assert_eq!(new.updates_state().await.unwrap(), UpdatesState::default());
    assert!(!*new.baseline_established().borrow());
    assert_eq!(rev(&new), rev_before, "rev continues across rebuilds");
    assert_eq!(
        new.snapshot().generation().unwrap(),
        old.snapshot().generation().unwrap()
    );

    // A late write from the retired instance is dropped.
    old.set_dc_option(&dc(2, 99)).await.unwrap();
    assert_eq!(rev(&new), rev_before);
    assert_eq!(new.dc_option(2).unwrap(), Some(dc(2, 3)));

    new.set_dc_option(&dc(2, 4)).await.unwrap();
    assert_eq!(rev(&new), rev_before + 1);
    barrier(&new).await.unwrap();
    assert_eq!(stored_state(&store).dc_option(2), Some(dc(2, 4)));

    new.revoke().unwrap();
    new.set_dc_option(&dc(2, 5)).await.unwrap();
    assert_eq!(rev(&new), rev_before + 1);
}

#[tokio::test]
async fn carried_peer_refs_convert_to_pinnable_infos() {
    let peer = PeerRef {
        id: PeerId::channel(12).unwrap(),
        auth: PeerAuth::from_hash(34),
    };
    let info = AbyssSession::info_from_ref(peer).unwrap();
    assert_eq!(info, channel(12, Some(34), None));
    assert_eq!(info.id().kind(), PeerKind::Channel);
    assert_eq!(
        AbyssSession::info_from_ref(PeerId::self_user().to_ambient_ref()),
        None
    );
}

#[tokio::test]
async fn first_full_update_state_signals_baseline() {
    let (persist, _store) = spawn_memory();
    let s = session(&persist);
    let mut baseline = s.baseline_established();
    s.set_update_state(UpdateState::Primary {
        pts: 1,
        date: 2,
        seq: 3,
    })
    .await
    .unwrap();
    assert!(!*baseline.borrow());
    s.set_update_state(UpdateState::All(UpdatesState {
        pts: 5,
        ..UpdatesState::default()
    }))
    .await
    .unwrap();
    tokio::time::timeout(LIMIT, baseline.wait_for(|b| *b))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(s.updates_state().await.unwrap().pts, 5);
}

// --- Persistence worker ---

async fn send(persist: &PersistHandle, msg: PersistMsg) {
    persist.sender().send(msg).await.unwrap();
}

async fn raw_barrier(
    persist: &PersistHandle,
    account: AccountGen,
    rev: u64,
) -> Result<(), PersistError> {
    let (reply, rx) = tokio::sync::oneshot::channel();
    send(
        persist,
        PersistMsg::Barrier {
            account,
            rev,
            reply,
        },
    )
    .await;
    tokio::time::timeout(LIMIT, rx).await.unwrap().unwrap()
}

fn blob(byte: u8) -> zeroize::Zeroizing<Vec<u8>> {
    zeroize::Zeroizing::new(vec![byte; 4])
}

#[tokio::test]
async fn worker_applies_in_order_and_rejects_stale_revisions() {
    let (persist, store) = spawn_memory();
    let account = persist.fresh_session().generation().unwrap();
    send(
        &persist,
        PersistMsg::Write {
            account,
            rev: 2,
            blob: blob(2),
        },
    )
    .await;
    send(
        &persist,
        PersistMsg::Write {
            account,
            rev: 1,
            blob: blob(1),
        },
    )
    .await;
    send(
        &persist,
        PersistMsg::Write {
            account,
            rev: 2,
            blob: blob(9),
        },
    )
    .await;
    raw_barrier(&persist, account, 1).await.unwrap();
    raw_barrier(&persist, account, 2).await.unwrap();
    assert_eq!(
        store.get(SecretItem::Session).unwrap().unwrap().as_slice(),
        &[2; 4]
    );
    // A barrier for a revision that was never queued reports it, not Ok.
    assert_eq!(
        raw_barrier(&persist, account, 3).await,
        Err(PersistError::NotQueued)
    );

    // A newer generation supersedes; the older one is stale.
    let newer = persist.fresh_session().generation().unwrap();
    send(
        &persist,
        PersistMsg::Write {
            account: newer,
            rev: 1,
            blob: blob(5),
        },
    )
    .await;
    send(
        &persist,
        PersistMsg::Write {
            account,
            rev: 3,
            blob: blob(3),
        },
    )
    .await;
    raw_barrier(&persist, newer, 1).await.unwrap();
    assert_eq!(
        store.get(SecretItem::Session).unwrap().unwrap().as_slice(),
        &[5; 4]
    );
}

#[tokio::test]
async fn delayed_write_then_logout_leaves_no_item() {
    let (persist, store, control) = spawn_faulty();
    let s = session(&persist);
    let account = s.snapshot().generation().unwrap();
    control.set_delay(Duration::from_millis(100));
    s.set_dc_option(&dc(2, 1)).await.unwrap();
    s.cache_peer(self_user()).await.unwrap();

    // Logout: terminate, then delete after everything queued before it.
    persist.terminate(account).await.unwrap();
    s.set_dc_option(&dc(2, 2)).await.unwrap(); // the runner is still alive
    persist.delete_session(account).await.unwrap();
    assert!(!store.contains(SecretItem::Session));

    // Nothing for the retired generation writes after the delete.
    control.set_delay(Duration::ZERO);
    s.set_dc_option(&dc(2, 3)).await.unwrap();
    assert_eq!(barrier(&s).await, Err(PersistError::Terminated));
    send(
        &persist,
        PersistMsg::Write {
            account,
            rev: 99,
            blob: blob(1),
        },
    )
    .await;
    assert_eq!(
        raw_barrier(&persist, account, 99).await,
        Err(PersistError::Terminated)
    );
    assert!(!store.contains(SecretItem::Session));

    // The next login uses a new generation and writes again.
    let next = session(&persist);
    next.set_dc_option(&dc(2, 4)).await.unwrap();
    barrier(&next).await.unwrap();
    assert!(store.contains(SecretItem::Session));
}

#[tokio::test]
async fn write_in_flight_during_delete_is_ordered_before_it() {
    let (persist, store, control) = spawn_faulty();
    let s = session(&persist);
    let account = s.snapshot().generation().unwrap();
    control.hold();
    s.set_dc_option(&dc(2, 1)).await.unwrap();
    let delete = {
        let persist = persist.clone();
        tokio::spawn(async move { persist.delete_session(account).await })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!delete.is_finished());
    control.release();
    tokio::time::timeout(LIMIT, delete)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!store.contains(SecretItem::Session));
}

#[tokio::test]
async fn delete_failure_is_surfaced_and_retry_succeeds() {
    let (persist, store, control) = spawn_faulty();
    let s = session(&persist);
    let account = s.snapshot().generation().unwrap();
    s.set_dc_option(&dc(2, 1)).await.unwrap();
    barrier(&s).await.unwrap();

    control.fail(StoreOp::Delete);
    assert_eq!(
        persist.delete_session(account).await,
        Err(PersistError::Store(StoreError::Injected))
    );
    assert!(store.contains(SecretItem::Session));
    // The failed delete still retired the generation.
    s.set_dc_option(&dc(2, 2)).await.unwrap();
    control.heal(StoreOp::Delete);
    persist.delete_session(account).await.unwrap();
    assert!(!store.contains(SecretItem::Session));
}

#[tokio::test]
async fn failed_write_fails_the_login_barrier() {
    let (persist, _store, control) = spawn_faulty();
    let s = session(&persist);
    control.fail(StoreOp::Set);
    s.cache_peer(self_user()).await.unwrap();
    assert_eq!(
        barrier(&s).await,
        Err(PersistError::Store(StoreError::Injected))
    );
}

#[tokio::test]
async fn slow_store_does_not_block_the_async_runtime() {
    // `#[tokio::test]` is single-threaded: if a store call ran on it, the
    // short sleep below could not finish before the slow write.
    let (persist, store, control) = spawn_faulty();
    control.set_delay(Duration::from_millis(400));
    let s = session(&persist);
    s.set_dc_option(&dc(2, 1)).await.unwrap();
    let pending = tokio::spawn(async move { s.barrier().await });

    let start = Instant::now();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(start.elapsed() < Duration::from_millis(300));
    assert!(!pending.is_finished());

    tokio::time::timeout(LIMIT, pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(store.contains(SecretItem::Session));
}

#[tokio::test]
async fn cache_key_goes_through_the_worker() {
    let (persist, store) = spawn_memory();
    let first = persist.load_or_create_cache_key().await.unwrap();
    let again = persist.load_or_create_cache_key().await.unwrap();
    assert_eq!(first.as_bytes(), again.as_bytes());
    let replaced = persist.create_cache_key().await.unwrap();
    assert_ne!(first.as_bytes(), replaced.as_bytes());

    // On an async thread the deleter refuses to block.
    let deleter = persist.cache_key_deleter();
    assert!(deleter().is_err());
    assert!(store.contains(SecretItem::CacheKey));

    // On a plain thread (like the cache worker) it deletes.
    let deleter = persist.cache_key_deleter();
    std::thread::spawn(deleter).join().unwrap().unwrap();
    assert!(!store.contains(SecretItem::CacheKey));
}

#[tokio::test]
async fn cache_key_deleter_surfaces_store_failure() {
    let (persist, store, control) = spawn_faulty();
    persist.load_or_create_cache_key().await.unwrap();
    control.fail(StoreOp::Delete);
    let deleter = persist.cache_key_deleter();
    let err = std::thread::spawn(deleter).join().unwrap().unwrap_err();
    assert!(err.to_string().contains("injected"));
    assert!(store.contains(SecretItem::CacheKey));
}
