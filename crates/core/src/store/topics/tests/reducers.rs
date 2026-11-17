//! The separate new-message and edit reducers (plan sections 6.3, 11).

use std::time::Instant;

use proptest::prelude::*;

use super::*;
use crate::rich_text::RichText;
use crate::store::topics::{Tail, TopicEdit, UnreadTrigger};

fn text(topic: TopicId, id: i32, body: &str, edit_date: Option<i64>) -> Message {
    Message {
        body: Body::Text(RichText::plain(body)),
        edit_date,
        ..message(topic, id)
    }
}

fn with_unread(list: &TopicList, n: i32) -> u32 {
    list.get(tid(n)).map_or(0, |t| t.unread)
}

#[test]
fn an_edit_of_the_top_message_updates_the_preview_only_when_newer() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[3, 2], now);
    assert!(
        l.on_edit(&text(tid(2), 15, "old", Some(9)), now).is_empty(),
        "not the top message"
    );
    let changes = l.on_edit(&text(tid(2), 20, "v2", Some(20)), now);
    let want = Preview::Text("v2".into());
    assert!(
        matches!(changes.as_slice(), [ListChange::Bumped(t)] if t.last.as_ref() == Some(&want))
    );
    assert_eq!(ids(&l), [3, 2], "never moves");
    // A stale edit (older version) is ignored.
    assert!(l.on_edit(&text(tid(2), 20, "v1", Some(10)), now).is_empty());
    assert_eq!(l.get(tid(2)).unwrap().last, Some(want));
}

#[test]
fn a_stale_edit_doesnt_override_a_snapshot_preview() {
    let now = Instant::now();
    let mut s = snap(topic(2));
    s.top_version = 50;
    let mut l = list();
    load(
        &mut l,
        PageKind::First,
        page(TopicOrder::LastActivity, vec![s], Rest::Last),
        now,
    );
    assert!(
        l.on_edit(&text(tid(2), 20, "older", Some(40)), now)
            .is_empty()
    );
    assert_eq!(
        l.on_edit(&text(tid(2), 20, "newer", Some(60)), now).len(),
        1
    );
}

#[test]
fn an_edit_never_inserts() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[3], now);
    assert!(l.on_edit(&text(tid(9), 90, "x", Some(1)), now).is_empty());
    assert!(l.get(tid(9)).is_none());
    assert_eq!(l.dispatch_by_id(now), None, "not even discovered");
}

#[test]
fn a_message_not_newer_than_last_id_doesnt_move_activity() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[3, 2], now);
    assert!(
        l.on_new_message(&message(tid(2), 20), Tail::Visible, now)
            .is_empty()
    );
    assert!(
        l.on_new_message(&message(tid(2), 11), Tail::Visible, now)
            .is_empty()
    );
    assert_eq!(ids(&l), [3, 2]);
    assert_eq!(l.get(tid(2)).unwrap().last_id, Some(MsgId(20)));
    assert!(!l.is_due(tid(2)), "nothing changed");
    // Newer: moves to the top; no unread trigger with the tail visible, but
    // a live activity change makes the topic due (plan section 6.4).
    assert_eq!(
        l.on_new_message(&message(tid(2), 31), Tail::Visible, now)
            .len(),
        1
    );
    assert_eq!(ids(&l), [2, 3]);
    assert_eq!(l.get(tid(2)).unwrap().freshness, Freshness::Current);
    assert!(l.is_due(tid(2)));
}

#[test]
fn outgoing_messages_and_visible_tails_are_not_unread_triggers() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[3], now);
    let mut own = message(tid(3), 40);
    own.outgoing = true;
    l.on_new_message(&own, Tail::Hidden, now);
    assert_eq!(l.get(tid(3)).unwrap().freshness, Freshness::Current);
    // Its activity grew, a live change: due all the same (plan section 6.4).
    assert!(l.is_due(tid(3)));
    l.on_new_message(&message(tid(3), 41), Tail::Visible, now);
    assert_eq!(l.get(tid(3)).unwrap().freshness, Freshness::Current);
    l.on_new_message(&message(tid(3), 42), Tail::Hidden, now);
    assert_eq!(l.get(tid(3)).unwrap().freshness, Freshness::NewSince);
    assert_eq!(l.get(tid(3)).unwrap().unread, 0);
}

#[test]
fn unknown_topics_are_discovered() {
    let now = Instant::now();
    let mut l = loaded(TopicOrder::LastActivity, &[3], now);
    assert!(
        l.on_new_message(&message(tid(8), 80), Tail::Hidden, now)
            .is_empty()
    );
    let (_, ids) = dispatch(&mut l, now);
    assert_eq!(ids, [tid(8)]);
}

#[derive(Clone, Debug)]
enum Live {
    New(i32, i32, bool),
    Edit(i32, i32, i64),
    Trigger(i32, u8),
    Deleted(Vec<i32>, bool),
    TopicEdited(i32, bool),
}

fn live() -> impl Strategy<Value = Live> {
    prop_oneof![
        (1i32..8, 1i32..200, any::<bool>()).prop_map(|(t, id, hidden)| Live::New(t, id, hidden)),
        (1i32..8, 1i32..200, 0i64..100).prop_map(|(t, id, v)| Live::Edit(t, id, v)),
        (1i32..8, 0u8..4).prop_map(|(t, k)| Live::Trigger(t, k)),
        (proptest::collection::vec(1i32..200, 1..4), any::<bool>())
            .prop_map(|(ids, h)| Live::Deleted(ids, h)),
        (1i32..8, any::<bool>()).prop_map(|(t, c)| Live::TopicEdited(t, c)),
    ]
}

proptest! {
    /// No reducer or trigger changes an unread count, and every trigger
    /// leaves its topic due.
    #[test]
    fn no_reducer_changes_an_unread_count(events in proptest::collection::vec(live(), 1..80)) {
        let now = Instant::now();
        let mut s = snaps(&[7, 6, 5, 4, 3, 2, 1]);
        for (i, x) in s.iter_mut().enumerate() {
            x.topic.unread = u32::try_from(i).unwrap() * 3;
        }
        let mut l = list();
        load(&mut l, PageKind::First, page(TopicOrder::LastActivity, s, Rest::Last), now);
        let counts: Vec<u32> = (1..8).map(|n| with_unread(&l, n)).collect();
        for event in events {
            let triggered: Vec<i32> = match event {
                Live::New(t, id, hidden) => {
                    let tail = if hidden { Tail::Hidden } else { Tail::Visible };
                    l.on_new_message(&message(tid(t), id), tail, now);
                    if hidden { vec![t] } else { vec![] }
                }
                Live::Edit(t, id, v) => {
                    l.on_edit(&text(tid(t), id, "e", Some(v)), now);
                    vec![]
                }
                Live::Trigger(t, k) => {
                    let trigger = [UnreadTrigger::Incoming, UnreadTrigger::ReadLocally, UnreadTrigger::ReadRemotely, UnreadTrigger::Deleted][usize::from(k)];
                    l.trigger_unread(tid(t), trigger, now);
                    vec![t]
                }
                Live::Deleted(ids, held) => {
                    let msgs: Vec<MsgId> = ids.iter().map(|&i| MsgId(i)).collect();
                    let held: Vec<(MsgId, TopicId)> = if held { msgs.iter().map(|&m| (m, tid(1))).collect() } else { vec![] };
                    l.on_deleted(&msgs, &held, now);
                    if held.is_empty() { (1..8).collect() } else { vec![1] }
                }
                Live::TopicEdited(t, closed) => {
                    l.on_topic_edited(tid(t), TopicEdit { closed: Some(closed), ..TopicEdit::default() }, now);
                    vec![]
                }
            };
            for t in triggered {
                prop_assert!(l.is_due(tid(t)), "a trigger leaves {} due", t);
            }
            let now_counts: Vec<u32> = (1..8).map(|n| with_unread(&l, n)).collect();
            prop_assert_eq!(&now_counts, &counts);
            check_invariants(&l).map_err(TestCaseError::fail)?;
        }
    }
}
