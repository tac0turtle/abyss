use abyss_core::model::{Body, ChatId, PeerKind, ThreadKey};
use abyss_core::rich_text::RichText;

use super::super::retry::FIRST_DELAY;
use super::*;

const CHAT: ChatId = ChatId {
    kind: PeerKind::User,
    id: 1,
};

fn msg(id: i32) -> Message {
    Message {
        id: MsgId(id),
        thread: ThreadKey {
            chat: CHAT,
            topic: None,
        },
        sender: None,
        date: i64::from(id),
        edit_date: None,
        outgoing: false,
        reply_to: None,
        body: Body::Text(RichText::plain("m")),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum More {
    Yes,
    No,
}

fn batch(view: u64, dir: HistoryDir, ids: impl IntoIterator<Item = i32>, more: More) -> Batch {
    Batch {
        view: ViewGen(view),
        dir,
        messages: ids.into_iter().map(msg).collect(),
        removed: Vec::new(),
        has_more: more == More::Yes,
    }
}

fn ids(w: &MessageWindow) -> Vec<i32> {
    w.messages.iter().map(|m| m.id.0).collect()
}

/// A list model: applies row ops the way `ListState` does, with new rows as
/// placeholders. Rows that existed before must keep their message.
fn apply(rows: &mut Vec<Option<i32>>, applied: Applied) {
    let Applied::Changed(ops) = applied else {
        panic!("expected row ops, got {applied:?}");
    };
    for op in ops {
        if let RowOp::Splice { range, count } = op {
            rows.splice(range, std::iter::repeat_n(None, count));
        }
    }
}

fn assert_rows_track(rows: &[Option<i32>], w: &MessageWindow) {
    assert_eq!(rows.len(), w.len());
    for (row, id) in rows.iter().zip(ids(w)) {
        if let Some(row) = row {
            assert_eq!(*row, id, "an existing row moved to another message");
        }
    }
}

fn loaded(tail: std::ops::RangeInclusive<i32>) -> (MessageWindow, Vec<Option<i32>>) {
    let mut w = MessageWindow::new(ThreadKey::whole(CHAT));
    assert_eq!(
        w.apply(batch(1, HistoryDir::Tail, tail, More::Yes)),
        Applied::Reset
    );
    let rows = ids(&w).into_iter().map(Some).collect();
    (w, rows)
}

#[test]
fn prepend_is_one_splice_at_the_top() {
    let (mut w, mut rows) = loaded(51..=100);
    let applied = w.apply(batch(1, HistoryDir::Older, 1..=50, More::No));
    assert_eq!(
        applied,
        Applied::Changed(vec![RowOp::Splice {
            range: 0..0,
            count: 50
        }])
    );
    apply(&mut rows, applied);
    assert_rows_track(&rows, &w);
    assert!(!w.has_older());
}

#[test]
fn sliding_down_drops_the_oldest() {
    let (mut w, mut rows) = loaded(51..=100);
    // Scrolled back up past the cap earlier: detached.
    let mut older = batch(1, HistoryDir::Older, 1..=50, More::Yes);
    older.removed = vec![MsgId(99), MsgId(100)];
    apply(&mut rows, w.apply(older));
    assert!(w.has_newer(), "trimming the newest end detaches the window");
    assert_rows_track(&rows, &w);
    let mut newer = batch(1, HistoryDir::Newer, 99..=100, More::No);
    newer.removed = (1..=10).map(MsgId).collect();
    let applied = w.apply(newer);
    assert_eq!(
        applied,
        Applied::Changed(vec![
            RowOp::Splice {
                range: 0..10,
                count: 0
            },
            RowOp::Splice {
                range: 88..88,
                count: 2
            },
        ])
    );
    apply(&mut rows, applied);
    assert_rows_track(&rows, &w);
    assert!(!w.has_newer());
    assert!(w.has_older());
    assert_eq!(ids(&w).first(), Some(&11));
}

#[test]
fn views_order_events() {
    let mut w = MessageWindow::new(ThreadKey::whole(CHAT));
    assert_eq!(
        w.apply(batch(5, HistoryDir::Newer, [7], More::No)),
        Applied::Ignored,
        "nothing but a tail seeds an empty window"
    );
    w.apply(batch(5, HistoryDir::Tail, [1, 2], More::Yes));
    assert_eq!(
        w.apply(batch(4, HistoryDir::Tail, [9], More::No)),
        Applied::Ignored,
        "stale view"
    );
    assert_eq!(
        w.apply(batch(6, HistoryDir::Tail, [3, 4], More::No)),
        Applied::Reset
    );
    assert_eq!(ids(&w), [3, 4]);
    assert!(!w.has_older());
    // A cached tail first, then more for the same view, extends it.
    let applied = w.apply(batch(6, HistoryDir::Tail, [2, 5], More::Yes));
    assert!(matches!(applied, Applied::Changed(_)));
    assert_eq!(ids(&w), [2, 3, 4, 5]);
}

#[test]
fn edits_remeasure_and_deletes_coalesce() {
    let (mut w, mut rows) = loaded(1..=10);
    let mut edited = msg(4);
    edited.edit_date = Some(99);
    assert_eq!(w.edit(edited), Applied::Changed(vec![RowOp::Remeasure(3)]));
    assert_eq!(w.edit(msg(42)), Applied::Ignored, "not in the window");
    let applied = w.delete(&[MsgId(5), MsgId(6), MsgId(9), MsgId(77)]);
    assert_eq!(
        applied,
        Applied::Changed(vec![
            RowOp::Splice {
                range: 4..6,
                count: 0
            },
            RowOp::Splice {
                range: 6..7,
                count: 0
            },
        ])
    );
    apply(&mut rows, applied);
    assert_rows_track(&rows, &w);
    assert_eq!(ids(&w), [1, 2, 3, 4, 7, 8, 10]);
}

#[test]
fn live_message_appends_in_order() {
    let (mut w, mut rows) = loaded(1..=3);
    let applied = w.apply(batch(1, HistoryDir::Newer, [5, 4], More::No));
    apply(&mut rows, applied);
    assert_rows_track(&rows, &w);
    assert_eq!(ids(&w), [1, 2, 3, 4, 5]);
}

#[test]
fn scroll_loads_once_until_answered() {
    let now = Instant::now();
    let mut w = MessageWindow::new(ThreadKey::whole(CHAT));
    assert!(!w.wants_older(0, now), "nothing loaded yet");
    w.apply(batch(1, HistoryDir::Tail, 51..=100, More::Yes));
    assert!(!w.wants_older(LOAD_MARGIN, now), "not near the top");
    assert!(w.wants_older(3, now));
    assert!(
        w.wants_older(3, now),
        "asking is free until core admits the load"
    );
    w.requested(HistoryDir::Older);
    assert!(!w.wants_older(0, now), "already asked");
    w.apply(batch(1, HistoryDir::Older, 1..=50, More::No));
    assert!(!w.wants_older(0, now), "start of history");
    assert!(!w.wants_newer(99, now), "attached to the live tail");
    // Detach, then scroll back down.
    let mut older = batch(1, HistoryDir::Older, [], More::No);
    older.removed = vec![MsgId(100)];
    w.apply(older);
    assert!(!w.wants_newer(10, now));
    assert!(w.wants_newer(95, now));
    w.requested(HistoryDir::Newer);
    assert!(!w.wants_newer(98, now), "already asked");
    w.load_ended(HistoryDir::Newer, Some(UserError::Network), now);
    assert!(!w.wants_newer(98, now), "a failed load waits a moment");
    assert_eq!(w.retry_due(now), Some(now + FIRST_DELAY));
    assert!(w.wants_newer(98, now + FIRST_DELAY), "then is asked again");
}

#[test]
fn invalidation_clears_until_the_next_tail() {
    let (mut w, _) = loaded(1..=5);
    assert_eq!(w.invalidate(), Applied::Reset);
    assert_eq!(w.len(), 0);
    assert_eq!(
        w.apply(batch(2, HistoryDir::Tail, [6], More::No)),
        Applied::Reset
    );
    assert_eq!(ids(&w), [6]);
}

#[test]
fn window_is_bounded() {
    let cap = i32::try_from(MAX_MESSAGES_PER_CHAT).unwrap();
    let (mut w, mut rows) = loaded(cap + 1..=2 * cap);
    let applied = w.apply(batch(1, HistoryDir::Older, 1..=10, More::Yes));
    apply(&mut rows, applied);
    assert_eq!(w.len(), MAX_MESSAGES_PER_CHAT);
    assert_rows_track(&rows, &w);
    assert!(w.has_newer(), "the newest end was dropped");
    assert_eq!(w.newest(), Some(MsgId(2 * cap - 10)));
}
