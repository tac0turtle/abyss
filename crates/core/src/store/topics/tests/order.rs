//! `TopicList` ordering per mode, caps and emission (plan section 11).

use std::cmp::Ordering;
use std::time::{Duration, Instant};

use proptest::prelude::*;

use super::*;
use crate::store::topics::order::rank;
use crate::store::topics::{PageCall, Paging, Tail, TopicEdit};

#[derive(Clone, Debug)]
enum Op {
    /// A first page dispatched and completed at once (`order` may differ).
    First(TopicOrder, Vec<TopicSnapshot>),
    /// A later page dispatched and completed at once (`order` may differ).
    Next(TopicOrder, Vec<TopicSnapshot>),
    /// A page dispatched now and completed by a later `Complete`, so live
    /// changes land in between.
    Dispatch(PageKind),
    Complete(TopicOrder, Vec<TopicSnapshot>),
    Message(i32, i32),
    Edit(i32, i32),
    TopicEdit(i32, bool, bool),
    Deleted(i32),
    /// A by-id result for one topic (new or known).
    ById(TopicSnapshot),
    /// A fresh by-id `deleted`.
    Remove(i32),
    Open(Option<i32>),
}

fn snapshot(ids: std::ops::Range<i32>) -> impl Strategy<Value = TopicSnapshot> {
    (ids, any::<bool>(), 0i32..4000, any::<bool>(), "\\PC{0,8}").prop_map(
        |(n, pinned, last, hidden, title)| TopicSnapshot {
            topic: Topic {
                pinned,
                last_id: (last > 0).then_some(MsgId(last)),
                title: crate::sanitize::display_name(&title),
                ..topic(n)
            },
            visibility: if hidden && n == 1 {
                Visibility::Hidden
            } else {
                Visibility::Shown
            },
            top_version: i64::MIN,
        },
    )
}

fn order_mode() -> impl Strategy<Value = TopicOrder> {
    prop_oneof![Just(TopicOrder::LastActivity), Just(TopicOrder::Created)]
}

fn kind() -> impl Strategy<Value = PageKind> {
    prop_oneof![Just(PageKind::First), Just(PageKind::Next)]
}

fn op(ids: std::ops::Range<i32>) -> impl Strategy<Value = Op> {
    let snaps = |r: std::ops::Range<i32>| proptest::collection::vec(snapshot(r), 0..12);
    let t = ids.clone();
    prop_oneof![
        1 => (order_mode(), snaps(ids.clone())).prop_map(|(o, s)| Op::First(o, s)),
        2 => (order_mode(), snaps(ids.clone())).prop_map(|(o, s)| Op::Next(o, s)),
        2 => kind().prop_map(Op::Dispatch),
        2 => (order_mode(), snaps(ids.clone())).prop_map(|(o, s)| Op::Complete(o, s)),
        4 => (t.clone(), 1i32..5000).prop_map(|(t, id)| Op::Message(t, id)),
        2 => (t.clone(), 1i32..5000).prop_map(|(t, id)| Op::Edit(t, id)),
        1 => (t.clone(), any::<bool>(), any::<bool>()).prop_map(|(t, c, h)| Op::TopicEdit(t, c, h)),
        1 => (1i32..5000).prop_map(Op::Deleted),
        2 => snapshot(ids).prop_map(Op::ById),
        1 => t.clone().prop_map(Op::Remove),
        1 => proptest::option::of(t).prop_map(Op::Open),
    ]
}

/// The list under test, its page in flight, and an independent consumer of
/// everything it emits.
struct Harness {
    list: TopicList,
    pending: Option<PageCall>,
    order: TopicOrder,
    shown: Vec<Topic>,
}

impl Harness {
    fn new(
        order: TopicOrder,
        first: Vec<TopicSnapshot>,
        now: Instant,
    ) -> Result<Self, TestCaseError> {
        let mut list = list();
        let changes = load(
            &mut list,
            PageKind::First,
            page(order, first, Rest::More),
            now,
        );
        let reset = matches!(changes.as_slice(), [ListChange::Reset { .. }]);
        prop_assert!(reset);
        let mut h = Self {
            list,
            pending: None,
            order: TopicOrder::LastActivity,
            shown: Vec::new(),
        };
        h.consume(&changes)?;
        Ok(h)
    }

    /// Applies emitted changes to the consumer, which must then hold
    /// exactly the store's emitted list.
    fn consume(&mut self, changes: &[ListChange]) -> Result<(), TestCaseError> {
        for c in changes {
            c.apply(&mut self.order, &mut self.shown);
        }
        prop_assert_eq!(self.order, self.list.order());
        prop_assert_eq!(
            &self.shown,
            &self.list.emitted(),
            "consumer/store equivalence"
        );
        Ok(())
    }

    /// Runs one operation. Only page completions may reset (deliberately:
    /// first pages always, later pages when server order and the rank rule
    /// disagree). Any other reset would be a repair of a diverged replay,
    /// which debug builds also refuse in `checked`.
    fn step(&mut self, op: Op, now: Instant) -> Result<(), TestCaseError> {
        let page_op = matches!(op, Op::First(..) | Op::Next(..) | Op::Complete(..));
        let changes = run(self, op, now)?;
        let reset = changes
            .iter()
            .any(|c| matches!(c, ListChange::Reset { .. }));
        prop_assert!(page_op || !reset, "a non-page operation reset the list");
        check_invariants(&self.list).map_err(TestCaseError::fail)?;
        check_emitted(&self.list, &changes).map_err(TestCaseError::fail)?;
        self.consume(&changes)
    }
}

/// Runs `op`, checking the order rules that apply to it.
fn run(h: &mut Harness, op: Op, now: Instant) -> Result<Vec<ListChange>, TestCaseError> {
    let before = ids(&h.list);
    let list = &mut h.list;
    Ok(match op {
        Op::First(order, s) if h.pending.is_none() => {
            load(list, PageKind::First, page(order, s, Rest::More), now)
        }
        Op::Next(order, s) if h.pending.is_none() => {
            list.request_page(PageKind::Next);
            let Some(call) = list.dispatch_page() else {
                return Ok(Vec::new());
            };
            complete(list, call, order, s, before, now)?
        }
        Op::Dispatch(kind) if h.pending.is_none() => {
            list.request_page(kind);
            h.pending = list.dispatch_page();
            Vec::new()
        }
        Op::Complete(order, s) => match h.pending.take() {
            Some(call) => complete(list, call, order, s, before, now)?,
            None => Vec::new(),
        },
        Op::First(..) | Op::Next(..) | Op::Dispatch(_) => Vec::new(),
        Op::Message(t, id) => run_message(list, t, id, before, now)?,
        Op::Edit(t, id) => {
            let changes = list.on_edit(&message(tid(t), id), now);
            prop_assert_eq!(ids(list), before, "edits never move");
            changes
        }
        Op::TopicEdit(t, closed, hidden) => {
            let visibility = Some(if hidden {
                Visibility::Hidden
            } else {
                Visibility::Shown
            });
            let edit = TopicEdit {
                closed: Some(closed),
                visibility,
                ..TopicEdit::default()
            };
            let unhidden = !hidden && list.entry(tid(t)).is_some_and(|e| !e.shown());
            let changes = list.on_topic_edited(tid(t), edit, now);
            if !unhidden {
                prop_assert_eq!(ids(list), before, "topic edits move only on unhide");
            }
            changes
        }
        Op::Deleted(id) => {
            let changes = list.on_deleted(&[MsgId(id)], &[], now);
            prop_assert_eq!(ids(list), before, "deletions never move or remove");
            changes
        }
        Op::ById(s) => run_by_id(list, s, now)?,
        Op::Remove(t) => {
            list.mark_due(tid(t), now);
            list.discover(tid(t));
            let Some(call) = list.dispatch_by_id(now) else {
                return Ok(Vec::new());
            };
            let changes = list
                .apply_by_id(call.seq, by_id(vec![], vec![tid(t)]), now)
                .expect("current");
            if call.ids.contains(&tid(t)) {
                prop_assert!(list.get(tid(t)).is_none(), "a fresh deletion removes");
            }
            changes
        }
        Op::Open(t) => list.set_open(t.map(tid)),
    })
}

/// Completes a page; a later page in another order is discarded.
fn complete(
    list: &mut TopicList,
    call: PageCall,
    order: TopicOrder,
    s: Vec<TopicSnapshot>,
    before: Vec<i32>,
    now: Instant,
) -> Result<Vec<ListChange>, TestCaseError> {
    let mismatched = call.kind == PageKind::Next && order != list.order();
    let changes = list
        .apply_page(call.seq, page(order, s, Rest::More), now)
        .expect("current");
    if mismatched {
        prop_assert!(changes.is_empty(), "a mismatched later page is discarded");
        prop_assert!(list.needs_first_page(), "and asks for the first page");
        prop_assert_eq!(ids(list), before);
    }
    Ok(changes)
}

/// A live message: in `LastActivity`, an unpinned topic moves to the top
/// exactly when its `last_id` grows; otherwise nothing moves.
fn run_message(
    list: &mut TopicList,
    t: i32,
    id: i32,
    mut want: Vec<i32>,
    now: Instant,
) -> Result<Vec<ListChange>, TestCaseError> {
    let known = list.get(tid(t)).map(|x| (x.last_id, x.pinned));
    let changes = list.on_new_message(&message(tid(t), id), Tail::Visible, now);
    if let Some((last, false)) = known
        && list.order() == TopicOrder::LastActivity
        && last.is_none_or(|l| MsgId(id) > l)
    {
        want.retain(|x| *x != t);
        let pinned = list.entries.iter().take_while(|e| e.topic.pinned).count();
        want.insert(pinned, t);
    }
    prop_assert_eq!(
        ids(list),
        want,
        "messages move only on growth, only by activity"
    );
    Ok(changes)
}

/// A by-id result for one topic; a new one lands by `created` in `Created`.
fn run_by_id(
    list: &mut TopicList,
    s: TopicSnapshot,
    now: Instant,
) -> Result<Vec<ListChange>, TestCaseError> {
    let id = s.topic.id;
    let new = list.get(id).is_none();
    if new {
        list.discover(id);
    } else {
        list.mark_due(id, now);
    }
    let Some(call) = list.dispatch_by_id(now) else {
        return Ok(Vec::new());
    };
    let changes = list
        .apply_by_id(call.seq, by_id(vec![s], vec![]), now)
        .expect("current");
    if new && call.ids.contains(&id) && list.order() == TopicOrder::Created {
        check_created_neighbours(list, id)?;
    }
    Ok(changes)
}

/// A topic learned by id in `Created` order sits where its `created` ranks
/// among the shown topics (hidden ones aren't displayed or replayed).
fn check_created_neighbours(list: &TopicList, id: TopicId) -> Result<(), TestCaseError> {
    let shown: Vec<&Topic> = list.topics().collect();
    let Some(at) = shown.iter().position(|t| t.id == id) else {
        return Ok(());
    };
    let me = shown[at];
    if me.pinned {
        return Ok(());
    }
    if let Some(prev) = at.checked_sub(1).map(|i| shown[i])
        && !prev.pinned
    {
        prop_assert_ne!(rank(TopicOrder::Created, me, prev), Ordering::Less);
    }
    if let Some(next) = shown.get(at + 1) {
        prop_assert_eq!(rank(TopicOrder::Created, me, next), Ordering::Less);
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn ordering_invariants_hold(
        order in order_mode(),
        first in proptest::collection::vec(snapshot(1..40), 0..20),
        ops in proptest::collection::vec(op(1..40), 1..60),
    ) {
        let mut now = Instant::now();
        let mut h = Harness::new(order, first, now)?;
        for op in ops {
            now += Duration::from_secs(1);
            h.step(op, now)?;
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// The same properties at the cap: a list paged to 490 topics, then
    /// random operations over ids that overlap it and new ones.
    #[test]
    fn ordering_invariants_hold_at_the_cap(
        order in order_mode(),
        ops in proptest::collection::vec(op(1..700), 1..80),
    ) {
        let mut now = Instant::now();
        let first: Vec<TopicSnapshot> = (1..=50).rev().map(|n| snap(Topic { last_id: Some(MsgId(5000 - n)), ..topic(n) })).collect();
        let mut h = Harness::new(order, first, now)?;
        for chunk in (51..=490).rev().collect::<Vec<i32>>().chunks(50) {
            let s = chunk.iter().map(|&n| snap(Topic { last_id: Some(MsgId(5000 - n)), ..topic(n) })).collect();
            h.step(Op::Next(order, s), now)?;
        }
        prop_assert!(h.list.len() >= 480);
        for op in ops {
            now += Duration::from_secs(1);
            h.step(op, now)?;
        }
    }
}

#[test]
fn pinned_first_then_activity_or_creation() {
    let now = Instant::now();
    let mut s = snaps(&[5, 3, 9]);
    s[2].topic.pinned = true;
    let mut l = list();
    load(
        &mut l,
        PageKind::First,
        page(TopicOrder::Created, s, Rest::More),
        now,
    );
    assert_eq!(ids(&l), [9, 5, 3]);
    assert_eq!(l.order(), TopicOrder::Created);
    // A topic learned by id lands by `created` in creation order.
    l.discover(tid(4));
    let (seq, _) = dispatch(&mut l, now);
    l.apply_by_id(seq, by_id(snaps(&[4]), vec![]), now).unwrap();
    assert_eq!(ids(&l), [9, 5, 4, 3]);
    // ... and never moves on messages.
    l.on_new_message(&message(tid(3), 1000), Tail::Visible, now);
    assert_eq!(ids(&l), [9, 5, 4, 3]);
}

#[test]
fn last_activity_ranks_by_id_learned_topics_and_snapshot_lowering() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[5, 4, 2], now);
    // Topic 3's top message (30) ranks between 4 (40) and 2 (20).
    l.discover(tid(3));
    let (seq, _) = dispatch(&mut l, now);
    let changes = l.apply_by_id(seq, by_id(snaps(&[3]), vec![]), now).unwrap();
    assert_eq!(ids(&l), [5, 4, 3, 2]);
    assert!(matches!(changes.as_slice(), [ListChange::Appended(t)] if t[0].id == tid(3)));
    // A snapshot that lowers 5's last id (its top message was deleted) moves it down.
    l.mark_due(tid(5), now);
    let (seq, _) = dispatch(&mut l, now);
    let mut low = topic(5);
    low.last_id = Some(MsgId(25));
    l.apply_by_id(seq, by_id(vec![snap(low)], vec![]), now)
        .unwrap();
    assert_eq!(ids(&l), [4, 3, 5, 2]);
}

#[test]
fn a_later_page_in_another_order_is_discarded_then_the_first_page_resets() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[5, 4], now);
    assert!(!l.needs_first_page());
    let out = load(
        &mut l,
        PageKind::Next,
        page(TopicOrder::Created, snaps(&[3]), Rest::More),
        now,
    );
    assert!(out.is_empty());
    assert_eq!(ids(&l), [5, 4]);
    assert!(l.needs_first_page());
    let out = load(
        &mut l,
        PageKind::First,
        page(TopicOrder::Created, snaps(&[6, 3]), Rest::More),
        now,
    );
    assert!(matches!(
        out.as_slice(),
        [ListChange::Reset { order: TopicOrder::Created, topics }] if topics.len() == 2
    ));
    assert_eq!(ids(&l), [6, 3], "an order change resets the list");
}

#[test]
fn the_cap_stops_paging_and_keeps_the_open_topic() {
    let now = Instant::now();
    let mut l = list();
    l.set_open(Some(tid(9_999)));
    let mut next = 2_000;
    let mut kind = PageKind::First;
    while l.paging() == Paging::More {
        let ns: Vec<i32> = (0..50).map(|i| next - i).collect();
        next -= 50;
        load(
            &mut l,
            kind,
            page(TopicOrder::LastActivity, snaps(&ns), Rest::More),
            now,
        );
        kind = PageKind::Next;
    }
    assert_eq!(l.paging(), Paging::Capped);
    assert_eq!(l.len(), MAX_TOPICS_PER_FORUM);
    // The open topic, learned by id, ranks last but is kept past the cap.
    let (seq, call) = dispatch(&mut l, now);
    assert_eq!(call, [tid(9_999)]);
    let mut open = topic(9_999);
    open.last_id = Some(MsgId(1));
    l.apply_by_id(seq, by_id(vec![snap(open)], vec![]), now)
        .unwrap();
    assert_eq!(l.len(), MAX_TOPICS_PER_FORUM + 1);
    assert!(l.get(tid(9_999)).is_some());
    // A non-open topic ranking last is not admitted past the cap.
    l.discover(tid(9_998));
    let (seq, _) = dispatch(&mut l, now);
    let mut low = topic(9_998);
    low.last_id = None;
    l.apply_by_id(seq, by_id(vec![snap(low)], vec![]), now)
        .unwrap();
    assert!(l.get(tid(9_998)).is_none());
    check_invariants(&l).unwrap();
}

#[test]
fn hidden_topics_are_kept_but_never_emitted() {
    let now = Instant::now();
    let mut s = snaps(&[1, 2]);
    s[0].visibility = Visibility::Hidden;
    let mut l = list();
    let out = load(
        &mut l,
        PageKind::First,
        page(TopicOrder::LastActivity, s, Rest::Last),
        now,
    );
    assert!(matches!(out.as_slice(), [ListChange::Reset { topics, .. }] if topics.len() == 1));
    assert!(l.get(TopicId::GENERAL).is_some(), "kept for routing");
    assert_eq!(
        l.on_new_message(&message(TopicId::GENERAL, 99), Tail::Hidden, now),
        []
    );
}
