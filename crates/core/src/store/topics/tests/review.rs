//! Regressions from the task 2 review (plan section 16).

use std::time::{Duration, Instant};

use super::*;
use crate::rich_text::RichText;
use crate::store::topics::{Tail, TopicEdit};

/// A consumer of emitted changes, kept apart from the store.
struct Consumer {
    order: TopicOrder,
    shown: Vec<Topic>,
}

impl Consumer {
    fn of(list: &TopicList) -> Self {
        Self {
            order: list.order(),
            shown: list.emitted(),
        }
    }

    fn apply(&mut self, changes: &[ListChange]) {
        for c in changes {
            c.apply(&mut self.order, &mut self.shown);
        }
    }

    fn assert_matches(&self, list: &TopicList) {
        assert_eq!(self.order, list.order());
        assert_eq!(self.shown, list.emitted());
    }
}

fn edited(topic: TopicId, id: i32, body: &str, version: i64) -> Message {
    Message {
        body: Body::Text(RichText::plain(body)),
        edit_date: Some(version),
        ..message(topic, id)
    }
}

/// Finding 1: a page that adds an unseen topic is dispatched; an edit of
/// that topic's top message arrives; the old page completes. The topic must
/// not be inserted as settled with the stale preview.
#[test]
fn an_unlisted_edit_crossing_a_page_leaves_the_topic_due() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[5], now);
    l.request_page(PageKind::First);
    let old = l.dispatch_page().unwrap();
    assert!(l.on_edit(&edited(tid(4), 40, "new", 9), now).is_empty());
    l.apply_page(
        old.seq,
        page(TopicOrder::LastActivity, snaps(&[5, 4]), Rest::Last),
        now,
    )
    .unwrap();
    assert!(l.is_due(tid(4)), "inserted unsettled");
    let (seq, ids) = dispatch(&mut l, now);
    assert!(ids.contains(&tid(4)));
    let mut fresh = snap(topic(4));
    fresh.topic.last = Some(Preview::Text("new".into()));
    fresh.top_version = 9;
    let found = ids
        .iter()
        .map(|id| {
            if *id == tid(4) {
                fresh.clone()
            } else {
                snap(topic(id.get()))
            }
        })
        .collect();
    l.apply_by_id(seq, by_id(found, vec![]), now).unwrap();
    assert_eq!(
        l.get(tid(4)).unwrap().last,
        Some(Preview::Text("new".into()))
    );
    assert!(!l.is_due(tid(4)));
}

/// Finding 2: A(100), B(90); a first page with A, B is dispatched; live
/// B(110) moves B first; the old page completes. B keeps the place its
/// newer activity earned.
#[test]
fn a_crossing_first_page_keeps_newer_activity_order() {
    let now = Instant::now();
    let a = |last| {
        snap(Topic {
            last_id: Some(MsgId(last)),
            ..topic(1)
        })
    };
    let b = |last| {
        snap(Topic {
            last_id: Some(MsgId(last)),
            ..topic(2)
        })
    };
    let mut l = list();
    load(
        &mut l,
        PageKind::First,
        page(TopicOrder::LastActivity, vec![a(100), b(90)], Rest::Last),
        now,
    );
    l.request_page(PageKind::First);
    let old = l.dispatch_page().unwrap();
    l.on_new_message(&message(tid(2), 110), Tail::Visible, now);
    assert_eq!(ids(&l), [2, 1]);
    let out = l
        .apply_page(
            old.seq,
            page(TopicOrder::LastActivity, vec![a(100), b(90)], Rest::Last),
            now,
        )
        .unwrap();
    assert_eq!(ids(&l), [2, 1], "the stale page doesn't undo the bump");
    assert!(matches!(out.as_slice(), [ListChange::Reset { topics, .. }] if topics[0].id == tid(2)));
}

/// Finding 2: an accepted later page that lowers a topic's top id moves it
/// down to where it now ranks, and the consumer follows.
#[test]
fn a_later_page_lowering_activity_repositions() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[9, 8, 7], now);
    let mut consumer = Consumer::of(&l);
    let mut low = topic(9);
    low.last_id = Some(MsgId(75));
    let out = load(
        &mut l,
        PageKind::Next,
        page(
            TopicOrder::LastActivity,
            vec![snap(low), snap(topic(6))],
            Rest::More,
        ),
        now,
    );
    assert_eq!(ids(&l), [8, 9, 7, 6]);
    consumer.apply(&out);
    consumer.assert_matches(&l);
}

/// Finding 3: at the cap, a discovered topic ranking high evicts the
/// lowest-ranked one, and the eviction is emitted.
#[test]
fn eviction_at_the_cap_is_emitted() {
    let now = Instant::now();
    let mut l = list();
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
    let mut consumer = Consumer::of(&l);
    assert_eq!(consumer.shown.len(), MAX_TOPICS_PER_FORUM);
    l.discover(tid(9_000));
    l.mark_due(tid(2), now);
    let (seq, ids) = dispatch(&mut l, now);
    assert!(
        ids.contains(&tid(2)) && ids.contains(&tid(9_000)),
        "{ids:?}"
    );
    // The new topic evicts topic 2, which the same result reports deleted:
    // the deletion is told too (round 5 of the task 10 review).
    let out = l
        .apply_by_id(seq, by_id(snaps(&[9_000]), vec![tid(2)]), now)
        .unwrap();
    let evicted = ListChange::Removed(tid(2), TopicGone::Evicted);
    let deleted = ListChange::Removed(tid(2), TopicGone::Deleted);
    assert!(out.contains(&evicted) && out.contains(&deleted), "{out:?}");
    consumer.apply(&out);
    consumer.assert_matches(&l);
    assert_eq!(consumer.shown.len(), MAX_TOPICS_PER_FORUM);
}

/// Finding 3: closing an open topic kept past the cap enforces the cap.
#[test]
fn closing_an_open_topic_past_the_cap_enforces_it() {
    let now = Instant::now();
    let mut l = list();
    assert!(l.set_open(Some(tid(1))).is_empty());
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
    let (seq, _) = dispatch(&mut l, now);
    l.apply_by_id(seq, by_id(snaps(&[1]), vec![]), now).unwrap();
    assert_eq!(l.len(), MAX_TOPICS_PER_FORUM + 1);
    let mut consumer = Consumer::of(&l);
    let out = l.set_open(None);
    assert_eq!(out, [ListChange::Removed(tid(1), TopicGone::Evicted)]);
    assert_eq!(l.len(), MAX_TOPICS_PER_FORUM);
    consumer.apply(&out);
    consumer.assert_matches(&l);
}

/// Finding 4: posting permission follows dispatch order, in both
/// completion orders; a discarded page changes nothing.
#[test]
fn posting_permission_follows_dispatch_order() {
    let now = Instant::now();
    for by_id_first in [true, false] {
        let mut l = loaded(TopicOrder::LastActivity, &[5], now);
        l.request_page(PageKind::First);
        let older = l.dispatch_page().unwrap();
        l.mark_due(tid(5), now);
        let (newer, _) = dispatch(&mut l, now);
        let refused = page(TopicOrder::LastActivity, snaps(&[5]), Rest::Last);
        let allowed = ByIdSnapshot {
            post_closed: PostClosed::Allowed,
            ..by_id(snaps(&[5]), vec![])
        };
        if by_id_first {
            l.apply_by_id(newer, allowed, now).unwrap();
            l.apply_page(older.seq, refused, now).unwrap();
        } else {
            l.apply_page(older.seq, refused, now).unwrap();
            l.apply_by_id(newer, allowed, now).unwrap();
        }
        assert_eq!(
            l.post_closed(),
            PostClosed::Allowed,
            "by-id first: {by_id_first}"
        );
    }
    let mut l = loaded(TopicOrder::LastActivity, &[5], now);
    l.request_page(PageKind::Next);
    let call = l.dispatch_page().unwrap();
    let mismatched = PageSnapshot {
        post_closed: PostClosed::Allowed,
        ..page(TopicOrder::Created, snaps(&[4]), Rest::More)
    };
    l.apply_page(call.seq, mismatched, now).unwrap();
    assert_eq!(
        l.post_closed(),
        PostClosed::Refused,
        "a discarded page mutates nothing"
    );
}

/// Finding 5: every live change of a group makes its topic due, with no
/// fetch in flight: a meta edit, a top-message edit, and activity growth
/// from an outgoing message or a visible tail.
#[test]
fn live_changes_make_topics_due() {
    let now = Instant::now();
    let fresh = || loaded(TopicOrder::LastActivity, &[3], now);
    let mut l = fresh();
    l.on_topic_edited(
        tid(3),
        TopicEdit {
            closed: Some(true),
            ..TopicEdit::default()
        },
        now,
    );
    assert!(l.is_due(tid(3)), "meta edit");
    let mut l = fresh();
    l.on_edit(&edited(tid(3), 30, "x", 5), now);
    assert!(l.is_due(tid(3)), "top-message edit");
    let mut l = fresh();
    let mut own = message(tid(3), 31);
    own.outgoing = true;
    l.on_new_message(&own, Tail::Hidden, now);
    assert!(l.is_due(tid(3)), "outgoing growth");
    let mut l = fresh();
    l.on_new_message(&message(tid(3), 32), Tail::Visible, now);
    assert!(l.is_due(tid(3)), "visible-tail growth");
    // Drained with a fresh snapshot, each settles.
    let (seq, _) = dispatch(&mut l, now + Duration::from_secs(1));
    l.apply_by_id(
        seq,
        by_id(
            vec![snap(Topic {
                last_id: Some(MsgId(32)),
                ..topic(3)
            })],
            vec![],
        ),
        now,
    )
    .unwrap();
    assert!(!l.is_due(tid(3)));
}

/// Review round 2, finding 1: topics [5 (top 50), 4 (top 40)]; a refresh is
/// dispatched; a live message bumps 4 to 100; the refresh contains only 5.
/// The omitted 4 keeps the rank its newer activity earned, and a later
/// by-id result settles it without changing the order.
#[test]
fn a_refresh_omitting_a_bumped_topic_keeps_its_rank() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[5, 4], now);
    let mut consumer = Consumer::of(&l);
    l.request_page(PageKind::First);
    let refresh = l.dispatch_page().unwrap();
    consumer.apply(&l.on_new_message(&message(tid(4), 100), Tail::Visible, now));
    let out = l
        .apply_page(
            refresh.seq,
            page(TopicOrder::LastActivity, snaps(&[5]), Rest::More),
            now,
        )
        .unwrap();
    consumer.apply(&out);
    consumer.assert_matches(&l);
    // The ordering property: unpinned topics rank by their known top message.
    let sorted = |l: &TopicList| {
        l.entries.windows(2).all(|w| {
            w[0].topic.pinned
                || crate::store::topics::order::rank(
                    TopicOrder::LastActivity,
                    &w[0].topic,
                    &w[1].topic,
                ) != std::cmp::Ordering::Greater
        })
    };
    assert!(sorted(&l), "{:?}", ids(&l));
    assert_eq!(ids(&l), [4, 5]);
    assert!(l.is_due(tid(4)), "the live change is still owed a refetch");

    let (seq, ids_sent) = dispatch(&mut l, now);
    assert!(ids_sent.contains(&tid(4)));
    let found = ids_sent
        .iter()
        .map(|id| {
            snap(Topic {
                last_id: Some(MsgId(if *id == tid(4) { 100 } else { 50 })),
                ..topic(id.get())
            })
        })
        .collect();
    let out = l.apply_by_id(seq, by_id(found, vec![]), now).unwrap();
    consumer.apply(&out);
    consumer.assert_matches(&l);
    assert!(sorted(&l) && !l.is_due(tid(4)));
    assert_eq!(ids(&l), [4, 5]);
}

/// Review round 2, finding 2: pinned [A, hidden General, B]. Unhiding
/// General is an incremental change the consumer reproduces exactly; no
/// reset repairs it.
#[test]
fn unhiding_a_pinned_general_between_pinned_topics_is_incremental() {
    let now = Instant::now();
    let pinned = |n: i32| {
        snap(Topic {
            pinned: true,
            ..topic(n)
        })
    };
    let mut general = pinned(1);
    general.visibility = Visibility::Hidden;
    let mut l = list();
    let first = vec![pinned(5), general, pinned(3), snap(topic(2))];
    load(
        &mut l,
        PageKind::First,
        page(TopicOrder::LastActivity, first, Rest::Last),
        now,
    );
    assert_eq!(ids(&l), [5, 1, 3, 2]);
    let mut consumer = Consumer::of(&l);
    let edit = TopicEdit {
        visibility: Some(Visibility::Shown),
        ..TopicEdit::default()
    };
    let out = l.on_topic_edited(TopicId::GENERAL, edit, now);
    assert!(
        matches!(out.as_slice(), [ListChange::Appended(t)] if t.len() == 1 && t[0].id == TopicId::GENERAL),
        "{out:?}"
    );
    consumer.apply(&out);
    consumer.assert_matches(&l);
    assert_eq!(
        ids(&l),
        [5, 3, 1, 2],
        "at the end of the pinned prefix, its rank"
    );
    // A first page hides it again: the reset comes after a `Hidden`.
    let mut general = pinned(1);
    general.visibility = Visibility::Hidden;
    let first = vec![pinned(5), general, pinned(3), snap(topic(2))];
    let out = load(
        &mut l,
        PageKind::First,
        page(TopicOrder::LastActivity, first, Rest::Last),
        now,
    );
    assert!(
        matches!(
            out.as_slice(),
            [
                ListChange::Removed(TopicId::GENERAL, TopicGone::Hidden),
                ListChange::Reset { .. }
            ]
        ),
        "{out:?}"
    );
}

/// Review round 3: A (top 50), B (top 40); a refresh S2 is dispatched; a
/// later by-id S3 moves B to 100; S2 returns only A. B keeps the rank its
/// newer snapshot earned, and a later by-id settles it in that order.
#[test]
fn a_refresh_omitting_a_topic_with_a_newer_snapshot_keeps_its_rank() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[5, 4], now);
    let mut consumer = Consumer::of(&l);
    l.request_page(PageKind::First);
    let refresh = l.dispatch_page().unwrap();
    l.mark_due(tid(4), now);
    let (seq, _) = dispatch(&mut l, now);
    let b = |last| {
        snap(Topic {
            last_id: Some(MsgId(last)),
            ..topic(4)
        })
    };
    consumer.apply(
        &l.apply_by_id(seq, by_id(vec![b(100)], vec![]), now)
            .unwrap(),
    );
    assert_eq!(ids(&l), [4, 5]);
    let out = l
        .apply_page(
            refresh.seq,
            page(TopicOrder::LastActivity, snaps(&[5]), Rest::More),
            now,
        )
        .unwrap();
    consumer.apply(&out);
    consumer.assert_matches(&l);
    assert_eq!(ids(&l), [4, 5], "the older page doesn't demote B");

    // Missed by a page it outranks: refetched, and settled in place.
    assert!(l.is_due(tid(4)));
    let (seq, sent) = dispatch(&mut l, now);
    assert_eq!(sent, [tid(4)]);
    consumer.apply(
        &l.apply_by_id(seq, by_id(vec![b(100)], vec![]), now)
            .unwrap(),
    );
    consumer.assert_matches(&l);
    assert_eq!(ids(&l), [4, 5]);
    assert!(!l.is_due(tid(4)));
}

/// The same crossing at the cap: the topic a newer snapshot moved to the
/// top must not be cut as if it were the lowest-ranked.
#[test]
fn a_refresh_at_the_cap_doesnt_cut_a_topic_a_newer_snapshot_promoted() {
    let now = Instant::now();
    let mut l = list();
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
    assert_eq!(l.len(), MAX_TOPICS_PER_FORUM);
    l.request_page(PageKind::First);
    let refresh = l.dispatch_page().unwrap();
    // Topic 2, last in the list, is promoted by a later-dispatched snapshot.
    l.mark_due(tid(2), now);
    let (seq, _) = dispatch(&mut l, now);
    let promoted = snap(Topic {
        last_id: Some(MsgId(100_000)),
        ..topic(2)
    });
    l.apply_by_id(seq, by_id(vec![promoted], vec![]), now)
        .unwrap();
    assert_eq!(ids(&l)[0], 2);
    // The older refresh: a new topic at the top, the rest of its first 50,
    // and not topic 2. One topic must be cut.
    let mut fresh: Vec<i32> = vec![9_000];
    fresh.extend((453..=501).rev());
    let out = l
        .apply_page(
            refresh.seq,
            page(TopicOrder::LastActivity, snaps(&fresh), Rest::More),
            now,
        )
        .unwrap();
    assert!(matches!(out.as_slice(), [ListChange::Reset { .. }]));
    assert_eq!(l.len(), MAX_TOPICS_PER_FORUM);
    assert_eq!(ids(&l)[0], 2, "kept, at the rank its newer snapshot earned");
    assert!(
        l.get(tid(3)).is_none(),
        "the lowest-ranked topic is the one cut"
    );
    check_invariants(&l).unwrap();
}
