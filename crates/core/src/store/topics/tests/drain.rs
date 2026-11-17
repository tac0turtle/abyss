//! The by-id drain: reserved open slot, round-robin in id order, backoff,
//! disposable discovery (plan sections 6.4, 11).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::*;
use crate::store::topics::due::backoff;
use crate::store::topics::entry::TopicEntry;
use crate::store::topics::{Discovered, MAX_DISCOVERY, MAX_TOPICS_BY_ID, Tail};

/// `ceil(500 / 19)`: the bound on calls before any eligible due topic is served.
const DRAIN_BOUND: usize = MAX_TOPICS_PER_FORUM.div_ceil(MAX_TOPICS_BY_ID - 1);

/// A full list (500 topics, ids 2..=501 by descending activity) plus the
/// open topic 1 beyond the cap.
fn full_list(now: Instant) -> TopicList {
    let mut l = list();
    l.set_open(Some(TopicId::GENERAL));
    let mut kind = PageKind::First;
    for chunk in (2..=501).rev().collect::<Vec<i32>>().chunks(50) {
        load(
            &mut l,
            kind,
            page(TopicOrder::LastActivity, snaps(chunk), Rest::More),
            now,
        );
        kind = PageKind::Next;
    }
    let (seq, call) = dispatch(&mut l, now);
    assert_eq!(call, [TopicId::GENERAL]);
    l.apply_by_id(seq, by_id(snaps(&[1]), vec![]), now).unwrap();
    assert_eq!(l.len(), MAX_TOPICS_PER_FORUM + 1);
    l
}

/// Every topic due on every call, the open one included, while live
/// messages keep reordering the display: each topic is served at least once
/// every 27 calls, and the open topic takes its slot every call.
#[test]
fn drain_is_fair_under_continuous_reordering_and_a_due_open_topic() {
    assert_eq!(DRAIN_BOUND, 27);
    let mut now = Instant::now();
    let mut l = full_list(now);
    let all: Vec<TopicId> = l.entries.iter().map(TopicEntry::id).collect();
    let mut last_served: BTreeMap<TopicId, usize> = all.iter().map(|id| (*id, 0)).collect();
    let mut next_msg = 10_000;
    for call_no in 1..=200 {
        for id in &all {
            l.mark_due(*id, now);
        }
        // Reorder the display: bump a spread of topics to the top.
        for k in 0..25 {
            let t = all[(call_no * 37 + k * 11) % all.len()];
            next_msg += 1;
            l.on_new_message(&message(t, next_msg), Tail::Visible, now);
        }
        let (seq, ids) = dispatch(&mut l, now);
        assert_eq!(ids.len(), MAX_TOPICS_BY_ID);
        assert_eq!(ids[0], TopicId::GENERAL, "the open topic's reserved slot");
        let found: Vec<TopicSnapshot> = ids
            .iter()
            .map(|id| snap(l.get(*id).cloned().expect("listed")))
            .collect();
        l.apply_by_id(seq, by_id(found, vec![]), now).unwrap();
        for id in &ids {
            let gap = call_no - last_served[id];
            assert!(gap <= DRAIN_BOUND, "{id:?} waited {gap} calls");
            last_served.insert(*id, call_no);
            assert!(!l.is_due(*id), "a clean result settles it");
        }
        now += Duration::from_secs(1);
    }
    for (id, at) in last_served {
        assert!(200 - at <= DRAIN_BOUND, "{id:?} last served at call {at}");
    }
    check_invariants(&l).unwrap();
}

/// 80 due topics beyond page one: each is served within the bound, and a
/// discovery overflow during the drain loses none.
#[test]
fn discovery_overflow_loses_no_due_topic() {
    let now = Instant::now();
    let mut l = full_list(now);
    let due: Vec<TopicId> = (300..380).map(tid).collect();
    for id in &due {
        l.mark_due(*id, now);
    }
    let mut overflowed = false;
    for unknown in 10_000..10_100 {
        overflowed |= l.discover(tid(unknown)) == Discovered::Overflowed;
    }
    assert!(overflowed && l.needs_first_page());
    assert_eq!(
        l.due_len(),
        80,
        "required work is not in the disposable set"
    );
    let mut served = Vec::new();
    for _ in 0..DRAIN_BOUND {
        let Some(call) = l.dispatch_by_id(now) else {
            break;
        };
        assert!(call.ids.len() <= MAX_TOPICS_BY_ID);
        let found = call
            .ids
            .iter()
            .filter_map(|id| l.get(*id).cloned())
            .map(snap)
            .collect();
        l.apply_by_id(call.seq, by_id(found, vec![]), now).unwrap();
        served.extend(call.ids);
    }
    for id in &due {
        assert!(
            served.contains(id) && !l.is_due(*id),
            "{id:?} served and settled"
        );
    }
}

#[test]
fn discovery_fills_leftover_room_and_is_bounded() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[5, 4], now);
    l.mark_due(tid(4), now);
    for n in 100..100 + i32::try_from(MAX_DISCOVERY).unwrap() {
        assert_eq!(l.discover(tid(n)), Discovered::Queued);
    }
    assert_eq!(l.discover(tid(4)), Discovered::Known);
    let (_, ids) = dispatch(&mut l, now);
    assert_eq!(ids.len(), MAX_TOPICS_BY_ID);
    assert_eq!(ids[0], tid(4), "due work first");
    assert_eq!(ids[1], tid(100), "then discovery, ascending");
    assert_eq!(l.discover(tid(101)), Discovered::Known, "in flight");
}

#[test]
fn conflicts_back_off_and_refreshes_reset_the_backoff() {
    assert_eq!(
        (0..8).map(|s| backoff(s).as_secs()).collect::<Vec<_>>(),
        [0, 1, 2, 4, 8, 16, 30, 30]
    );
    let t0 = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[5], t0);
    l.trigger_unread(tid(5), crate::store::UnreadTrigger::Incoming, t0);
    let (seq, _) = dispatch(&mut l, t0);
    // A trigger crossing the call: the result conflicts, the topic stays due.
    l.trigger_unread(tid(5), crate::store::UnreadTrigger::Incoming, t0);
    l.apply_by_id(seq, by_id(snaps(&[5]), vec![]), t0).unwrap();
    assert!(l.is_due(tid(5)));
    assert!(l.dispatch_by_id(t0).is_none(), "backing off for 1 s");
    assert!(l.dispatch_by_id(t0 + Duration::from_millis(999)).is_none());
    let (seq, _) = dispatch(&mut l, t0 + Duration::from_secs(1));
    l.trigger_unread(tid(5), crate::store::UnreadTrigger::Incoming, t0);
    l.apply_by_id(seq, by_id(snaps(&[5]), vec![]), t0).unwrap();
    assert!(
        l.dispatch_by_id(t0 + Duration::from_secs(1)).is_none(),
        "then 2 s"
    );
    l.reset_backoff(t0);
    let (seq, _) = dispatch(&mut l, t0);
    l.apply_by_id(seq, by_id(snaps(&[5]), vec![]), t0).unwrap();
    assert!(!l.is_due(tid(5)), "a clean result settles it");
}

#[test]
fn a_failed_call_returns_its_ids() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[5], now);
    l.mark_due(tid(5), now);
    l.discover(tid(9));
    let (seq, ids) = dispatch(&mut l, now);
    assert_eq!(ids, [tid(5), tid(9)]);
    l.fail(seq, now).unwrap();
    assert!(l.fail(seq, now).is_err(), "only once");
    let later = now + Duration::from_secs(1);
    let (_, ids) = dispatch(&mut l, later);
    assert_eq!(ids, [tid(5), tid(9)]);
}

#[test]
fn a_generation_bump_makes_in_flight_results_stale() {
    let now = Instant::now();
    let mut topics = crate::store::Topics::default();
    let view = topics.open(chat()).view();
    let l = topics.current(chat(), view).unwrap();
    l.request_page(PageKind::First);
    let call = l.dispatch_page().unwrap();
    topics.close(chat());
    assert!(topics.current(chat(), view).is_err());
    let l = topics.get_mut(chat()).unwrap();
    assert!(
        l.apply_page(
            call.seq,
            page(TopicOrder::LastActivity, snaps(&[1]), Rest::Last),
            now
        )
        .is_err()
    );
}

#[test]
fn lists_are_lru_bounded_and_the_shown_one_stays() {
    let mut topics = crate::store::Topics::default();
    let forum = |id| ChatId {
        kind: PeerKind::Channel,
        id,
    };
    for id in 0..12 {
        topics.open(forum(id));
    }
    assert_eq!(topics.chats().len(), crate::store::MAX_TOPIC_LISTS);
    assert!(topics.get(forum(11)).is_some() && topics.get(forum(0)).is_none());
    assert_eq!(topics.shown(), Some(forum(11)));
}
