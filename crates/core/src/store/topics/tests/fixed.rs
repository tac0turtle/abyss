//! The review counterexamples as fixed cases (plan section 16).

use std::time::{Duration, Instant};

use super::*;
use crate::store::topics::entry::Deletion;
use crate::store::topics::{MAX_TOMBSTONES, MAX_TOPICS_BY_ID, Paging, Tail};

fn with_unread(n: i32, unread: u32, last: i32) -> TopicSnapshot {
    snap(Topic {
        unread,
        last_id: Some(MsgId(last)),
        ..topic(n)
    })
}

/// A root deletion the by-id result contradicts: the topic stays listed.
#[test]
fn a_contradicted_root_deletion_keeps_the_topic() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[3], now);
    l.on_deleted(&[MsgId(3)], &[(MsgId(3), tid(3))], now);
    assert_eq!(l.entry(tid(3)).unwrap().deletion, Deletion::Suspected);
    assert_eq!(l.topics().count(), 1, "suspected topics are still emitted");
    let (seq, _) = dispatch(&mut l, now);
    l.apply_by_id(seq, by_id(snaps(&[3]), vec![]), now).unwrap();
    assert_eq!(l.entry(tid(3)).unwrap().deletion, Deletion::None);
    assert!(!l.is_due(tid(3)));
}

/// A page dispatched before a by-id fetch but completing after it doesn't
/// regress the by-id state, and can't re-insert a topic it removed.
#[test]
fn an_older_page_doesnt_regress_newer_by_id_state() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[3, 2], now);
    l.request_page(PageKind::First);
    let old = l.dispatch_page().unwrap();
    l.mark_due(tid(3), now);
    l.mark_due(tid(2), now);
    let (seq, _) = dispatch(&mut l, now);
    l.apply_by_id(seq, by_id(vec![with_unread(2, 4, 20)], vec![tid(3)]), now)
        .unwrap();
    let stale = page(
        TopicOrder::LastActivity,
        vec![with_unread(3, 0, 30), with_unread(2, 9, 20)],
        Rest::Last,
    );
    l.apply_page(old.seq, stale, now).unwrap();
    assert!(
        l.get(tid(3)).is_none(),
        "the tombstone blocks the older page"
    );
    assert_eq!(l.get(tid(2)).unwrap().unread, 4);
    assert_eq!(
        l.tombstones.len(),
        0,
        "dropped once nothing older is in flight"
    );
}

/// 65 confirmed deletions while an old page is in flight: before the
/// oldest tombstone is discarded, the page is invalidated (its result is
/// dropped) and re-queued, so it can't re-insert anything.
#[test]
fn tombstone_overflow_invalidates_older_fetches_first() {
    let now = Instant::now();
    let ns: Vec<i32> = (1..=70).rev().collect();
    let mut l = loaded(TopicOrder::LastActivity, &ns, now);
    l.request_page(PageKind::First);
    let old = l.dispatch_page().unwrap();
    let doomed: Vec<TopicId> = (1..=65).map(tid).collect();
    for chunk in doomed.chunks(MAX_TOPICS_BY_ID) {
        for id in chunk {
            l.mark_due(*id, now);
        }
        let (seq, ids) = dispatch(&mut l, now);
        assert_eq!(ids, chunk);
        l.apply_by_id(seq, by_id(vec![], chunk.to_vec()), now)
            .unwrap();
        assert!(l.tombstones.len() <= MAX_TOMBSTONES);
    }
    assert_eq!(l.len(), 5);
    let stale = page(TopicOrder::LastActivity, snaps(&ns), Rest::Last);
    assert!(l.apply_page(old.seq, stale, now).is_err(), "invalidated");
    let again = l.dispatch_page().expect("re-queued");
    assert_eq!(again.kind, PageKind::First);
    for id in &doomed {
        assert!(l.get(*id).is_none());
    }
}

/// Round 3, finding 1: a snapshot that already includes message 101 sets
/// unread = 1; the delayed live 101 then arrives. It is a trigger, not an
/// increment, and the refetch gives the server's count again. Never 2.
#[test]
fn round3_finding1_snapshot_then_the_same_live_message() {
    let now = Instant::now();
    let mut l = list();
    load(
        &mut l,
        PageKind::First,
        page(
            TopicOrder::LastActivity,
            vec![with_unread(7, 0, 100)],
            Rest::Last,
        ),
        now,
    );
    l.mark_due(tid(7), now);
    let (seq, _) = dispatch(&mut l, now);
    l.apply_by_id(seq, by_id(vec![with_unread(7, 1, 101)], vec![]), now)
        .unwrap();
    assert_eq!(l.get(tid(7)).unwrap().unread, 1);
    assert!(!l.is_due(tid(7)));

    l.on_new_message(&message(tid(7), 101), Tail::Hidden, now);
    let t = l.get(tid(7)).unwrap();
    assert_eq!((t.unread, t.freshness), (1, Freshness::NewSince));
    assert!(l.is_due(tid(7)));

    let (seq, ids) = dispatch(&mut l, now);
    assert_eq!(ids, [tid(7)]);
    l.apply_by_id(seq, by_id(vec![with_unread(7, 1, 101)], vec![]), now)
        .unwrap();
    let t = l.get(tid(7)).unwrap();
    assert_eq!((t.unread, t.freshness), (1, Freshness::Current));
    // Then a later snapshot with an absolute read count applies as is.
    l.trigger_unread(tid(7), crate::store::UnreadTrigger::ReadRemotely, now);
    let (seq, _) = dispatch(&mut l, now);
    l.apply_by_id(seq, by_id(vec![with_unread(7, 0, 101)], vec![]), now)
        .unwrap();
    assert_eq!(l.get(tid(7)).unwrap().unread, 0);
}

/// Round 3, finding 2: deleting the top message 200 while an older
/// snapshot (still showing 200) is in flight. The old snapshot conflicts;
/// the refetch sets 199. Deleting the last message: a lower (absent)
/// `last_id` is accepted.
#[test]
fn round3_finding2_top_message_deletion_with_an_older_snapshot_in_flight() {
    let now = Instant::now();
    let mut l = list();
    load(
        &mut l,
        PageKind::First,
        page(
            TopicOrder::LastActivity,
            vec![with_unread(7, 0, 200)],
            Rest::Last,
        ),
        now,
    );
    l.request_page(PageKind::First);
    let old = l.dispatch_page().unwrap();
    l.on_deleted(&[MsgId(200)], &[(MsgId(200), tid(7))], now);
    assert!(l.is_due(tid(7)));
    let stale = page(
        TopicOrder::LastActivity,
        vec![with_unread(7, 0, 200)],
        Rest::Last,
    );
    l.apply_page(old.seq, stale, now).unwrap();
    assert_eq!(
        l.get(tid(7)).unwrap().last_id,
        Some(MsgId(200)),
        "conflict: skipped"
    );
    assert!(l.is_due(tid(7)));

    let (seq, _) = dispatch(&mut l, now);
    l.apply_by_id(seq, by_id(vec![with_unread(7, 0, 199)], vec![]), now)
        .unwrap();
    assert_eq!(l.get(tid(7)).unwrap().last_id, Some(MsgId(199)));

    // The last remaining message goes too: the server has no top message.
    l.on_deleted(&[MsgId(199)], &[(MsgId(199), tid(7))], now);
    let (seq, _) = dispatch(&mut l, now + Duration::from_secs(1));
    let mut empty = topic(7);
    empty.last_id = None;
    empty.last = None;
    l.apply_by_id(seq, by_id(vec![snap(empty)], vec![]), now)
        .unwrap();
    assert_eq!(l.get(tid(7)).unwrap().last_id, None);
    assert!(!l.is_due(tid(7)));
}

/// Round 3, finding 7: hidden → suspected → found-hidden stays hidden.
#[test]
fn round3_finding7_hidden_suspected_found_hidden_stays_hidden() {
    let now = Instant::now();
    let mut general = snap(topic(1));
    general.visibility = Visibility::Hidden;
    let mut l = list();
    let first = vec![general.clone(), snap(topic(2))];
    load(
        &mut l,
        PageKind::First,
        page(TopicOrder::LastActivity, first, Rest::Last),
        now,
    );
    assert_eq!(l.topics().count(), 1);
    // Its root "deleted": a suspicion only.
    l.on_deleted(&[MsgId(1)], &[], now);
    assert_eq!(
        l.entry(TopicId::GENERAL).unwrap().deletion,
        Deletion::Suspected
    );
    let (seq, ids) = dispatch(&mut l, now);
    assert!(ids.contains(&TopicId::GENERAL));
    let found: Vec<TopicSnapshot> = ids
        .iter()
        .map(|id| {
            if *id == TopicId::GENERAL {
                general.clone()
            } else {
                snap(topic(2))
            }
        })
        .collect();
    let changes = l.apply_by_id(seq, by_id(found, vec![]), now).unwrap();
    let entry = l.entry(TopicId::GENERAL).unwrap();
    assert_eq!(
        (entry.deletion, entry.visibility),
        (Deletion::None, Visibility::Hidden)
    );
    assert!(l.topics().all(|t| t.id != TopicId::GENERAL));
    check_emitted(&l, &changes).unwrap();
}

/// Round 5, finding 1: T stores 0; incoming messages make T due; a by-id
/// snapshot is dispatched and captures 2; a non-top message held by no
/// window is deleted before the response. The response conflicts, T stays
/// due, and the next refetch gives the server's 1.
#[test]
fn round5_crossing_deletion() {
    let now = Instant::now();
    let mut l = list();
    load(
        &mut l,
        PageKind::First,
        page(
            TopicOrder::LastActivity,
            vec![with_unread(7, 0, 100)],
            Rest::Last,
        ),
        now,
    );
    l.on_new_message(&message(tid(7), 101), Tail::Hidden, now);
    l.on_new_message(&message(tid(7), 102), Tail::Hidden, now);
    let (seq, _) = dispatch(&mut l, now);
    l.on_deleted(&[MsgId(101)], &[], now);
    l.apply_by_id(seq, by_id(vec![with_unread(7, 2, 102)], vec![]), now)
        .unwrap();
    assert_eq!(
        l.get(tid(7)).unwrap().unread,
        0,
        "the crossing snapshot doesn't apply"
    );
    assert!(l.is_due(tid(7)));
    let (seq, _) = dispatch(&mut l, now + Duration::from_secs(1));
    l.apply_by_id(seq, by_id(vec![with_unread(7, 1, 102)], vec![]), now)
        .unwrap();
    assert_eq!(l.get(tid(7)).unwrap().unread, 1);
}

/// Unassignable deletions mark every listed topic, zero counts included.
#[test]
fn unassignable_deletions_mark_every_topic() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[3, 2, 1], now);
    l.on_deleted(&[MsgId(5_000)], &[], now);
    for n in 1..=3 {
        assert!(l.is_due(tid(n)), "topic {n}");
    }
    let mut held = loaded(TopicOrder::LastActivity, &[3, 2, 1], now);
    held.on_deleted(&[MsgId(5_000)], &[(MsgId(5_000), tid(2))], now);
    assert!(held.is_due(tid(2)) && !held.is_due(tid(1)) && !held.is_due(tid(3)));
}

/// Round 3, finding 6: a topic deleted while offline (no suspicion) is
/// removed by a fresh by-id `deleted`; one crossing a live meta change
/// doesn't remove.
#[test]
fn a_fresh_by_id_deletion_removes_without_a_suspicion() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[3, 2], now);
    l.set_open(Some(tid(3)));
    l.mark_due(tid(3), now);
    let (seq, _) = dispatch(&mut l, now);
    let changes = l
        .apply_by_id(seq, by_id(vec![], vec![tid(3)]), now)
        .unwrap();
    assert_eq!(changes, [ListChange::Removed(tid(3), TopicGone::Deleted)]);
    assert!(l.get(tid(3)).is_none());

    l.mark_due(tid(2), now);
    let (seq, _) = dispatch(&mut l, now);
    let edit = crate::store::TopicEdit {
        closed: Some(true),
        ..Default::default()
    };
    l.on_topic_edited(tid(2), edit, now);
    assert!(
        l.apply_by_id(seq, by_id(vec![], vec![tid(2)]), now)
            .unwrap()
            .is_empty()
    );
    assert!(l.get(tid(2)).is_some() && l.is_due(tid(2)));
}

/// Task 4 review: a non-empty later page that adds no topic (the server
/// repeated a page) ends paging, whatever its cursor says; one that adds a
/// topic keeps paging.
#[test]
fn a_page_of_duplicates_ends_paging() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[5, 4, 3], now);
    assert_eq!(l.paging(), Paging::More);
    load(
        &mut l,
        PageKind::Next,
        page(TopicOrder::LastActivity, snaps(&[4, 3]), Rest::More),
        now,
    );
    assert_eq!(l.paging(), Paging::Complete, "nothing new: paging stops");
    l.request_page(PageKind::Next);
    assert!(l.dispatch_page().is_none(), "no redundant request");

    let mut l = loaded(TopicOrder::LastActivity, &[5, 4, 3], now);
    load(
        &mut l,
        PageKind::Next,
        page(TopicOrder::LastActivity, snaps(&[3, 2]), Rest::More),
        now,
    );
    assert_eq!(l.paging(), Paging::More, "one new topic keeps paging");
}

/// When the drain has something to fetch: at once for an unlisted open
/// topic or discovery, at a due topic's backoff end, never while a call is
/// in flight.
#[test]
fn next_by_id_follows_the_backoff() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[3, 2], now);
    assert_eq!(l.next_by_id(now), None, "nothing owed");
    l.mark_due(tid(3), now);
    assert_eq!(l.next_by_id(now), Some(now));
    let (seq, _) = dispatch(&mut l, now);
    assert_eq!(l.next_by_id(now), None, "one call in flight");
    l.fail(seq, now).unwrap();
    assert_eq!(l.next_by_id(now), Some(now + Duration::from_secs(1)));
    l.set_open(Some(tid(9)));
    assert_eq!(l.next_by_id(now), Some(now), "the unlisted open topic");
}

/// Task 6 review: an unlisted open topic that responses neither find nor
/// delete (or whose calls fail) backs off up to 30 s, instead of costing a
/// call per second; a new open intent or a refresh makes it eligible again.
#[test]
fn an_unanswered_open_topic_backs_off() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[3, 2], now);
    l.set_open(Some(tid(9)));
    let mut at = now;
    let mut waits = Vec::new();
    for _ in 0..8 {
        let next = l.next_by_id(at).unwrap();
        waits.push((next - at).as_secs());
        at = next;
        let (seq, ids) = dispatch(&mut l, at);
        assert_eq!(ids, [tid(9)]);
        l.apply_by_id(seq, by_id(vec![], vec![]), at).unwrap();
    }
    assert_eq!(waits, [0, 1, 2, 4, 8, 16, 30, 30]);
    assert!(
        l.dispatch_by_id(at).is_none(),
        "not before its backoff ends"
    );

    // A failed call backs off too.
    l.reset_backoff(at);
    assert_eq!(l.next_by_id(at), Some(at), "a refresh resets it");
    let (seq, _) = dispatch(&mut l, at);
    l.fail(seq, at).unwrap();
    assert_eq!(l.next_by_id(at), Some(at + Duration::from_secs(1)));
    assert_eq!(l.discover(tid(9)), crate::store::Discovered::Queued);
    assert_eq!(
        l.next_by_id(at),
        Some(at + Duration::from_secs(1)),
        "discovery doesn't bypass the open topic's backoff"
    );
    assert!(l.dispatch_by_id(at).is_none());

    l.set_open(Some(tid(9)));
    assert_eq!(l.next_by_id(at), Some(at), "a new open intent resets it");
}

/// Review round 2: an unlisted open topic whose new message arrives while
/// nothing can be fetched (a flood wait), then is closed: its discovery
/// intent survives, so the next call fetches it. The same when it was
/// discovered before it was opened.
#[test]
fn closing_an_unlisted_open_topic_keeps_its_discovery() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[3, 2], now);
    l.set_open(Some(tid(9)));
    l.on_new_message(&message(tid(9), 95), Tail::Hidden, now);
    l.set_open(None);
    assert_eq!(l.next_by_id(now), Some(now));
    let (_, ids) = dispatch(&mut l, now);
    assert_eq!(ids, [tid(9)], "fetched as discovery");

    let mut l = loaded(TopicOrder::LastActivity, &[3, 2], now);
    l.on_new_message(&message(tid(8), 85), Tail::Hidden, now);
    l.set_open(Some(tid(8)));
    let (seq, ids) = dispatch(&mut l, now);
    assert_eq!(ids, [tid(8)], "once, in the open slot");
    l.apply_by_id(seq, by_id(vec![], vec![]), now).unwrap();
    assert!(l.dispatch_by_id(now).is_none(), "the open slot backs off");
    l.set_open(None);
    let (_, ids) = dispatch(&mut l, now);
    assert_eq!(ids, [tid(8)], "discovered again once closed");
}
