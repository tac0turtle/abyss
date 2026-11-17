//! The pin selection: core's set first, then in-flight extras; bounded.

use grammers_client::session::types::PeerId;

use crate::backend::grammers::pins::{MAX_IN_FLIGHT, Pins};
use crate::model::{ChatId, PeerKind};
use crate::session::MAX_PINNED_PEERS;

fn chat(id: i64) -> ChatId {
    ChatId {
        kind: PeerKind::User,
        id,
    }
}

fn pid(id: i64) -> PeerId {
    PeerId::user(id).unwrap()
}

fn chats(ids: impl IntoIterator<Item = i64>) -> Vec<ChatId> {
    ids.into_iter().map(chat).collect()
}

#[test]
fn inherited_pins_are_kept_as_they_are_until_core_reconciles() {
    let inherited: Vec<PeerId> = (1..=1264).map(pid).collect();
    let mut pins = Pins::new(inherited);
    assert_eq!(pins.changed(), None, "already pinned");
    assert!(pins.begin(pid(1)));
    assert_eq!(pins.changed(), None, "an inherited chat adds nothing");
    assert!(pins.begin(pid(5000)));
    // At the cap, an extra never displaces an inherited peer.
    let selected = pins.selected();
    assert_eq!(selected.len(), MAX_PINNED_PEERS);
    assert!(!selected.contains(&pid(5000)));
}

#[test]
fn core_set_comes_first_then_in_flight_extras() {
    let mut pins = Pins::new(vec![pid(99)]);
    assert!(pins.begin(pid(9)));
    assert_eq!(pins.require(&chats([3, 1, 3, 2])), 0);
    assert_eq!(pins.selected(), vec![pid(3), pid(1), pid(2), pid(9)]);
    pins.applied(&pins.selected(), 0);
    // A second call on the same chat; the first one ending keeps the pin.
    assert!(pins.begin(pid(9)));
    pins.end(pid(9));
    assert_eq!(pins.changed(), None);
    pins.end(pid(9));
    assert_eq!(pins.changed(), Some(vec![pid(3), pid(1), pid(2)]));
}

#[test]
fn every_part_is_bounded() {
    let mut pins = Pins::new(Vec::new());
    assert_eq!(pins.require(&chats(1..=5000)), 0);
    for id in 10_000..10_000 + i64::try_from(MAX_IN_FLIGHT).unwrap() {
        assert!(pins.begin(pid(id)));
    }
    assert!(!pins.begin(pid(1)), "tracked chats are bounded");
    assert_eq!(pins.selected().len(), MAX_PINNED_PEERS);
    // Out-of-range ids are reported, not selected.
    let bad = ChatId {
        kind: PeerKind::User,
        id: -1,
    };
    assert_eq!(pins.require(&[bad, chat(1)]), 1);
}

#[test]
fn a_reorder_that_changes_the_capped_membership_is_a_change() {
    let mut pins = Pins::new(Vec::new());
    pins.require(&chats(1..=1264));
    pins.applied(&pins.selected(), 0);
    // Same order, same set: nothing to do.
    pins.require(&chats(1..=1264));
    assert_eq!(pins.changed(), None);
    // A reorder alone keeps the membership: nothing to do either.
    pins.require(&chats((1..=1264).rev()));
    assert_eq!(pins.changed(), None);
    // A new head pushes the tail out of the cap.
    pins.require(&chats(std::iter::once(2000).chain(1..=1264)));
    let next = pins.changed().expect("membership changed");
    assert!(next.contains(&pid(2000)) && !next.contains(&pid(1264)));
}
