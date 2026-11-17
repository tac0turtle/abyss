//! The peer map's three states (pinned, held, queued) and their bounds.

use grammers_session::types::{PeerAuth, PeerId, PeerInfo};

use super::{
    MAX_FIFO, MAX_HANDOFF_PEERS, MAX_HANDOFFS, MAX_PINNED_PEERS, MAX_UNPINNED_PEERS, PeerMap,
    PinRequest, Pinned,
};

/// `ids` as core's required set.
fn req(ids: &[PeerId]) -> PinRequest<'_> {
    PinRequest {
        required: ids,
        transient: &[],
    }
}

fn set(map: &mut PeerMap, ids: &[PeerId]) -> Pinned {
    map.set_pins(req(ids))
}

fn hashless(id: i64) -> PeerInfo {
    PeerInfo::User {
        id,
        auth: None,
        bot: None,
        is_self: None,
    }
}

fn user(id: i64) -> PeerInfo {
    PeerInfo::User {
        id,
        auth: Some(PeerAuth::from_hash(id * 7)),
        bot: None,
        is_self: None,
    }
}

fn pid(id: i64) -> PeerId {
    PeerId::user(id).unwrap()
}

fn pids(ids: impl IntoIterator<Item = i64>) -> Vec<PeerId> {
    ids.into_iter().map(pid).collect()
}

fn known(map: &PeerMap, id: i64) -> bool {
    map.get(pid(id)).is_some()
}

fn full_queue() -> PeerMap {
    let mut map = PeerMap::default();
    for id in 1..=i64::try_from(MAX_UNPINNED_PEERS).unwrap() {
        map.insert(user(id));
    }
    map
}

fn bound() -> usize {
    MAX_PINNED_PEERS + MAX_HANDOFFS * MAX_HANDOFF_PEERS + MAX_UNPINNED_PEERS
}

/// The same two-peer response as a dialogs page: both are held for core.
#[test]
fn a_two_peer_handoff_survives_a_full_queue_and_more() {
    let mut map = full_queue();
    map.cache_response(&pids([1, 5000]), vec![user(1), user(5000)]);
    let aux: Vec<PeerInfo> = (10_000..14_096).map(user).collect();
    map.cache_response(&[], aux);
    assert!(known(&map, 1) && known(&map, 5000));
    assert_eq!(
        set(&mut map, &pids([1, 5000])),
        Pinned {
            pinned: 2,
            unknown: 0
        }
    );
}

/// The same with core's set present: a required id ends its hold, a
/// transient one in the same request does not.
#[test]
fn only_required_ids_end_holds() {
    let mut map = PeerMap::default();
    map.cache_response(&pids([1, 2]), vec![user(1), user(2)]);
    let (required, transient) = (pids([1]), pids([2]));
    map.set_pins(PinRequest {
        required: &required,
        transient: &transient,
    });
    assert!(!map.held.contains_key(&pid(1)));
    assert!(map.held.contains_key(&pid(2)));
}

/// A known peer without its hash becomes addressable when a later insertion
/// merges one in; a hashless insertion pins nothing.
#[test]
fn a_hash_merged_into_a_known_required_peer_pins_it() {
    let mut map = PeerMap::default();
    map.insert(hashless(8));
    assert_eq!(set(&mut map, &pids([8])).unknown, 1);
    map.insert(hashless(8));
    assert!(map.pinned.is_empty());
    map.insert(user(8));
    assert!(map.pinned.contains(&pid(8)));
    assert_eq!(map.queued, 0, "pinned, not queued");
}

/// A promoted required peer that was held leaves its handoff, as a
/// `set_pins` would have ended it.
#[test]
fn promotion_of_a_required_held_peer_ends_its_hold() {
    let mut map = PeerMap::default();
    map.cache_response(&pids([8]), vec![hashless(8)]);
    assert_eq!(set(&mut map, &pids([8])).unknown, 1);
    assert!(map.held.contains_key(&pid(8)));
    map.insert(user(8));
    assert!(map.pinned.contains(&pid(8)));
    assert!(map.held.is_empty() && map.handoffs.is_empty());
}

/// The desired set is capped like the pinned set; ids past the cap are
/// never promoted.
#[test]
fn the_desired_set_is_bounded() {
    let mut map = PeerMap::default();
    let many = pids(1..=3000);
    set(&mut map, &many);
    assert_eq!(map.desired.len(), MAX_PINNED_PEERS);
    for id in 1..=3000 {
        map.insert(user(id));
    }
    assert_eq!(map.pinned.len(), MAX_PINNED_PEERS);
    assert!(!map.pinned.contains(&pid(3000)));
    assert!(map.len() <= bound());
}

/// Two pages arrive (and a huge auxiliary response) before core reconciles
/// either: both handoffs survive. A third releases the oldest into the queue
/// as its newest entries.
#[test]
fn concurrent_handoffs_survive_until_reconciled_and_stay_bounded() {
    let mut map = PeerMap::default();
    map.cache_response(&pids(1..=100), (1..=100).map(user).collect());
    map.cache_response(&pids(101..=200), (101..=200).map(user).collect());
    map.cache_response(&[], (1000..5096).map(user).collect());
    assert_eq!(set(&mut map, &pids(1..=200)).unknown, 0);

    let mut map = PeerMap::default();
    for page in 0..3_i64 {
        let ids = page * 100 + 1..=page * 100 + 100;
        map.cache_response(&pids(ids.clone()), ids.map(user).collect());
    }
    assert_eq!(map.handoffs.len(), MAX_HANDOFFS);
    assert!(!map.held.contains_key(&pid(1)), "the oldest was released");
    assert!(known(&map, 1), "released into the queue, not dropped");
}

/// A handoff id core does not pin stays held; one core pins and then drops
/// goes to the queue.
#[test]
fn a_hold_ends_when_core_pins_the_peer() {
    let mut map = PeerMap::default();
    map.cache_response(&pids([1, 2]), vec![user(1), user(2)]);
    set(&mut map, &pids([1]));
    assert!(!map.held.contains_key(&pid(1)));
    assert!(map.held.contains_key(&pid(2)));
    set(&mut map, &[]);
    assert!(map.slots.get(&pid(1)).unwrap().queued.is_some());
    assert!(
        map.slots.get(&pid(2)).unwrap().queued.is_none(),
        "still held"
    );
}

#[test]
fn a_handoff_is_bounded() {
    let mut map = PeerMap::default();
    map.cache_response(&pids(1..=500), (1..=500).map(user).collect());
    assert_eq!(map.held.len(), MAX_HANDOFF_PEERS);
}

/// Touching never grows the queue's bookkeeping without bound.
#[test]
fn repeated_touches_keep_the_queue_bounded() {
    let mut map = PeerMap::default();
    for round in 0..10_000 {
        map.insert(user(round % 10 + 1));
    }
    assert!(map.fifo.len() <= MAX_FIFO);
    assert_eq!(map.queued, 10);
    let mut map = full_queue();
    set(&mut map, &pids(1..=1264));
    set(&mut map, &[]);
    assert_eq!(map.queued, MAX_UNPINNED_PEERS);
    assert!(map.len() <= bound());
}
