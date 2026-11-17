use super::*;
use crate::model::{Body, PeerKind, ThreadKey};
use crate::rich_text::RichText;

fn chat(kind: PeerKind, id: i64) -> ChatId {
    ChatId { kind, id }
}

fn user(id: i64) -> ChatId {
    chat(PeerKind::User, id)
}

fn msg(c: ChatId, id: i32, text: &str) -> Message {
    Message {
        id: MsgId(id),
        thread: ThreadKey {
            chat: c,
            topic: None,
        },
        sender: None,
        date: 0,
        edit_date: None,
        outgoing: false,
        reply_to: None,
        body: Body::Text(RichText::plain(text)),
    }
}

/// A page with more history beyond it.
fn more(messages: Vec<Message>) -> HistoryPage {
    HistoryPage {
        messages,
        has_more: true,
    }
}

/// The last page in its direction.
fn last(messages: Vec<Message>) -> HistoryPage {
    HistoryPage {
        messages,
        has_more: false,
    }
}

fn ids(w: &Windows, c: ChatId) -> Vec<i32> {
    w.get(ThreadKey::whole(c))
        .map(|w| w.messages().iter().map(|m| m.id.0).collect())
        .unwrap_or_default()
}

#[test]
fn stale_generation_is_rejected() {
    let mut w = Windows::default();
    let c = user(1);
    let g1 = w.open(ThreadKey::whole(c));
    w.close(ThreadKey::whole(c));
    assert_eq!(
        w.prepend(ThreadKey::whole(c), g1, more(vec![msg(c, 1, "a")])),
        Err(Stale)
    );
    let g2 = w.open(ThreadKey::whole(c));
    assert!(g2 > g1);
    assert!(
        w.prepend(ThreadKey::whole(c), g2, more(vec![msg(c, 1, "a")]))
            .is_ok()
    );
}

#[test]
fn prepend_orders_dedups_and_ignores_newer() {
    let mut w = Windows::default();
    let c = user(1);
    let g = w.open(ThreadKey::whole(c));
    w.upsert(ThreadKey::whole(c), msg(c, 10, "x"));
    let page = vec![
        msg(c, 5, ""),
        msg(c, 7, ""),
        msg(c, 5, ""),
        msg(c, 12, ""),
        msg(c, 10, ""),
    ];
    let change = w.prepend(ThreadKey::whole(c), g, more(page)).unwrap();
    assert_eq!(ids(&w, c), vec![5, 7, 10]);
    assert_eq!(change.inserted.len(), 2);
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "TryFrom is not const; the cap (2000) fits in i32"
)]
const CAP: i32 = MAX_MESSAGES_PER_CHAT as i32;

#[test]
fn prepend_past_cap_slides_and_detaches_from_tail() {
    let mut w = Windows::default();
    let c = user(1);
    let g = w.open(ThreadKey::whole(c));
    for i in 1..=CAP {
        w.upsert(ThreadKey::whole(c), msg(c, CAP + i, "")); // live tail: CAP+1 ..= 2*CAP
    }
    let older: Vec<_> = (1..=10).map(|i| msg(c, CAP + 1 - i, "")).collect();
    let change = w.prepend(ThreadKey::whole(c), g, more(older)).unwrap();
    let win = w.get(ThreadKey::whole(c)).unwrap();
    assert_eq!(win.messages().len(), MAX_MESSAGES_PER_CHAT);
    assert_eq!(win.messages().front().unwrap().id.0, CAP - 9);
    assert_eq!(win.messages().back().unwrap().id.0, 2 * CAP - 10);
    assert!(win.has_older() && win.has_newer());
    assert_eq!(change.removed.len(), 10);
    assert_eq!(
        w.chat_of(MsgId(2 * CAP)),
        None,
        "dropped newest leave the index"
    );
}

/// Task 10d, finding 6: a page core fetches on its own fills only the room
/// left and never drops the newest messages (the end the user looks at).
#[test]
fn prepend_fitting_never_trims_and_stays_contiguous() {
    let mut w = Windows::default();
    let c = user(1);
    let g = w.open(ThreadKey::whole(c));
    for i in 1..=CAP - 3 {
        w.upsert(ThreadKey::whole(c), msg(c, CAP + i, "")); // CAP+1 ..= 2*CAP-3
    }
    let older: Vec<_> = (1..=10).map(|i| msg(c, CAP + 1 - i, "")).collect();
    let change = w
        .prepend_fitting(ThreadKey::whole(c), g, last(older))
        .unwrap();
    let win = w.get(ThreadKey::whole(c)).unwrap();
    assert_eq!(win.messages().len(), MAX_MESSAGES_PER_CHAT);
    assert!(change.removed.is_empty(), "nothing dropped");
    assert_eq!(ids(&w, c).first(), Some(&(CAP - 2)), "the 3 adjacent ones");
    assert_eq!(ids(&w, c).last(), Some(&(2 * CAP - 3)), "the tail stays");
    let win = w.get(ThreadKey::whole(c)).unwrap();
    assert!(!win.has_newer(), "still attached");
    assert!(win.has_older(), "the rest did not fit: more exists");
    let full = w
        .prepend_fitting(ThreadKey::whole(c), g, last(vec![msg(c, 1, "")]))
        .unwrap();
    assert_eq!(full, Change::default(), "no room: nothing changes");
}

#[test]
fn detached_window_ignores_live_messages_then_reattaches() {
    let mut w = Windows::default();
    let c = user(1);
    let g = w.open(ThreadKey::whole(c));
    for i in 1..=CAP {
        w.upsert(ThreadKey::whole(c), msg(c, CAP + i, ""));
    }
    w.prepend(ThreadKey::whole(c), g, more(vec![msg(c, CAP, "")]))
        .unwrap(); // detaches
    let oldest_before = ids(&w, c)[0];
    assert!(
        w.upsert(ThreadKey::whole(c), msg(c, 3 * CAP, "live"))
            .inserted
            .is_empty()
    );
    assert_eq!(ids(&w, c)[0], oldest_before, "browsing position preserved");

    let newer = vec![msg(c, 2 * CAP, ""), msg(c, 3 * CAP, "")];
    w.append_newer(ThreadKey::whole(c), g, last(newer)).unwrap();
    let win = w.get(ThreadKey::whole(c)).unwrap();
    assert!(!win.has_newer());
    assert_eq!(win.messages().back().unwrap().id.0, 3 * CAP);
    assert_eq!(win.messages().len(), MAX_MESSAGES_PER_CHAT);
    assert!(
        w.upsert(ThreadKey::whole(c), msg(c, 3 * CAP + 1, ""))
            .inserted
            .len()
            == 1,
        "attached again"
    );
}

#[test]
fn delete_does_not_latch_history_state() {
    let mut w = Windows::default();
    let c = user(1);
    let g = w.open(ThreadKey::whole(c));
    let page: Vec<_> = (1..=CAP + 1).map(|i| msg(c, i, "")).collect();
    w.prepend(ThreadKey::whole(c), g, more(page)).unwrap();
    w.delete(c, &[MsgId(5)]);
    let win = w.get(ThreadKey::whole(c)).unwrap();
    assert_eq!(win.messages().len(), MAX_MESSAGES_PER_CHAT - 1);
    assert!(win.has_older(), "older loading remains possible");
}

#[test]
fn repeated_invalidation_keeps_windows_under_lru_control() {
    let mut w = Windows::default();
    for i in 0..16 {
        let c = user(i);
        w.open(ThreadKey::whole(c));
        w.invalidate(ThreadKey::whole(c)).unwrap();
        w.upsert(
            ThreadKey::whole(c),
            msg(c, i32::try_from(i).unwrap() + 1, ""),
        );
    }
    assert_eq!(
        (0..16)
            .filter(|i| w.get(ThreadKey::whole(user(*i))).is_some())
            .count(),
        MAX_WINDOWS
    );
    assert_eq!(w.invalidate_all().len(), MAX_WINDOWS);
    assert_eq!(w.total_bytes(), 0);
    assert_eq!(w.chat_of(MsgId(16)), None);
}

#[test]
fn new_messages_beyond_cap_drop_oldest() {
    let mut w = Windows::default();
    let c = user(1);
    w.open(ThreadKey::whole(c));
    for i in 1..=(CAP + 3) {
        w.upsert(ThreadKey::whole(c), msg(c, i, ""));
    }
    let got = ids(&w, c);
    assert_eq!(got.len(), MAX_MESSAGES_PER_CHAT);
    assert_eq!(got.first(), Some(&4));
    assert_eq!(w.chat_of(MsgId(1)), None, "evicted ids leave the index");
    assert_eq!(w.chat_of(MsgId(4)), Some(c));
}

#[test]
fn upsert_ignores_messages_below_partial_window() {
    let mut w = Windows::default();
    let c = user(1);
    w.open(ThreadKey::whole(c));
    w.upsert(ThreadKey::whole(c), msg(c, 10, ""));
    w.upsert(ThreadKey::whole(c), msg(c, 3, "")); // older than window, history not loaded
    assert_eq!(ids(&w, c), vec![10]);
}

#[test]
fn replace_never_inserts() {
    let mut w = Windows::default();
    let c = user(1);
    w.open(ThreadKey::whole(c));
    w.upsert(ThreadKey::whole(c), msg(c, 10, "a"));
    assert!(!w.replace(ThreadKey::whole(c), msg(c, 11, "b")));
    assert!(w.replace(ThreadKey::whole(c), msg(c, 10, "edited")));
    assert_eq!(ids(&w, c), vec![10]);
}

#[test]
fn lru_evicts_beyond_max_windows() {
    let mut w = Windows::default();
    for i in 0..=i64::try_from(MAX_WINDOWS).unwrap() {
        let c = user(i);
        w.open(ThreadKey::whole(c));
        w.upsert(
            ThreadKey::whole(c),
            msg(c, i32::try_from(i).unwrap() + 1, ""),
        );
    }
    assert!(w.get(ThreadKey::whole(user(0))).is_none());
    assert_eq!(w.chat_of(MsgId(1)), None);
    assert!(
        w.get(ThreadKey::whole(user(i64::try_from(MAX_WINDOWS).unwrap())))
            .is_some()
    );
}

#[test]
fn unscoped_delete_uses_index_and_skips_channels() {
    let mut w = Windows::default();
    let a = user(1);
    let ch = chat(PeerKind::Channel, 2);
    w.open(ThreadKey::whole(a));
    w.open(ThreadKey::whole(ch));
    w.upsert(ThreadKey::whole(a), msg(a, 5, ""));
    w.upsert(ThreadKey::whole(ch), msg(ch, 6, ""));
    let out = w.delete_unscoped(&[MsgId(5), MsgId(6), MsgId(99)]);
    assert_eq!(out, vec![(a, vec![MsgId(5)])]);
    assert_eq!(ids(&w, ch), vec![6], "channel ids are not account-unique");
    assert_eq!(w.delete(ch, &[MsgId(6)]), vec![MsgId(6)]);
}

#[test]
fn invalidate_clears_and_bumps_generation() {
    let mut w = Windows::default();
    let c = user(1);
    let g = w.open(ThreadKey::whole(c));
    w.upsert(ThreadKey::whole(c), msg(c, 1, "abc"));
    let g2 = w.invalidate(ThreadKey::whole(c)).unwrap();
    assert!(g2 > g);
    assert!(ids(&w, c).is_empty());
    assert_eq!(w.total_bytes(), 0);
    assert_eq!(w.invalidate(ThreadKey::whole(user(42))), None);
}

#[test]
fn byte_budget_evicts_lru_then_trims() {
    let big = "x".repeat(crate::rich_text::MAX_TEXT_BYTES);
    let per_msg = msg(user(0), 0, &big).retained_bytes();
    // Full windows of maximal messages; enough chats to exceed the byte budget.
    let per_chat = MAX_MESSAGES_PER_CHAT;
    let chats = MAX_STORE_BYTES / (per_msg * per_chat) + 1;
    assert!(
        chats < MAX_WINDOWS,
        "budget must bind before the LRU count does"
    );
    let mut w = Windows::default();
    for c in (1..=i64::try_from(chats).unwrap()).map(user) {
        w.open(ThreadKey::whole(c));
        for i in 0..i32::try_from(per_chat).unwrap() {
            w.upsert(ThreadKey::whole(c), msg(c, i + 1, &big));
        }
    }
    assert!(w.total_bytes() <= MAX_STORE_BYTES);
    assert!(
        w.get(ThreadKey::whole(user(1))).is_none(),
        "least recently used window evicted"
    );
    let last = user(i64::try_from(chats).unwrap());
    assert_eq!(
        w.get(ThreadKey::whole(last)).map(|w| w.messages().len()),
        Some(per_chat)
    );
}

#[test]
fn byte_accounting_is_exact_after_mixed_ops() {
    let mut w = Windows::default();
    let c = user(1);
    let g = w.open(ThreadKey::whole(c));
    w.upsert(ThreadKey::whole(c), msg(c, 10, "hello"));
    w.prepend(ThreadKey::whole(c), g, more(vec![msg(c, 5, "older")]))
        .unwrap();
    w.replace(ThreadKey::whole(c), msg(c, 10, "a much longer edited text"));
    w.delete(c, &[MsgId(5)]);
    let expected: usize = w
        .get(ThreadKey::whole(c))
        .unwrap()
        .messages()
        .iter()
        .map(Message::retained_bytes)
        .sum();
    assert_eq!(w.total_bytes(), expected);
}

/// Cache contiguity lives on the window: it is bounded by `MAX_WINDOWS` and
/// gone with eviction, close, reopen and invalidation (it used to be an
/// unbounded per-chat set in the actor).
#[test]
fn cache_contiguity_is_bounded_by_the_windows() {
    let mut w = Windows::default();
    let max = i64::try_from(MAX_WINDOWS).unwrap();
    for id in 1..=max * 4 {
        w.open(ThreadKey::whole(user(id)));
        w.set_cache_contiguous(ThreadKey::whole(user(id)));
    }
    let marked = (1..=max * 4)
        .filter(|id| w.is_cache_contiguous(ThreadKey::whole(user(*id))))
        .count();
    assert_eq!(marked, MAX_WINDOWS, "evicted windows took their flag along");
    let c = user(max * 4);
    w.close(ThreadKey::whole(c));
    assert!(!w.is_cache_contiguous(ThreadKey::whole(c)));
    w.set_cache_contiguous(ThreadKey::whole(c));
    w.open(ThreadKey::whole(c));
    assert!(
        !w.is_cache_contiguous(ThreadKey::whole(c)),
        "a reopen re-establishes it"
    );
    w.set_cache_contiguous(ThreadKey::whole(c));
    w.invalidate(ThreadKey::whole(c));
    assert!(!w.is_cache_contiguous(ThreadKey::whole(c)));
    w.set_cache_contiguous(ThreadKey::whole(user(1)));
    assert!(
        !w.is_cache_contiguous(ThreadKey::whole(user(1))),
        "no window, no flag"
    );
}

/// A message retaining more than the whole byte budget: its sender name's
/// capacity is charged, though it holds one byte.
fn huge(c: ChatId, id: i32) -> Message {
    let mut name = String::with_capacity(MAX_STORE_BYTES + 1);
    name.push('x');
    Message {
        sender: Some(crate::model::Sender { id: c, name }),
        ..msg(c, id, "big")
    }
}

/// Task 10g (round 21, finding 1): a background chat's growth never evicts
/// the open chat's window, whose loads and owed answers rely on it. The
/// background windows go first, then the growing chat trims itself.
#[test]
fn byte_budget_never_evicts_the_open_window() {
    let mut w = Windows::default();
    let (a, b, c) = (user(1), user(2), user(3));
    w.open(ThreadKey::whole(c));
    w.upsert(ThreadKey::whole(c), msg(c, 1, "c"));
    w.close(ThreadKey::whole(c));
    w.open(ThreadKey::whole(b));
    w.upsert(ThreadKey::whole(b), msg(b, 1, "b"));
    w.close(ThreadKey::whole(b));
    let view = w.open(ThreadKey::whole(a));
    w.upsert(ThreadKey::whole(a), msg(a, 1, "a"));
    assert_eq!(w.shown(), Some(ThreadKey::whole(a)));
    // Least recently used first: c, b, a. The new message is b's.
    w.upsert(ThreadKey::whole(b), huge(b, 2));
    assert!(
        w.get(ThreadKey::whole(c)).is_none(),
        "background window evicted"
    );
    assert_eq!(ids(&w, a), vec![1], "the open window stays whole");
    assert_eq!(w.get(ThreadKey::whole(a)).map(Window::view_gen), Some(view));
    assert!(ids(&w, b).is_empty(), "the growing chat trimmed itself");
    assert!(w.total_bytes() <= MAX_STORE_BYTES);
    // An edit of a retained chat's message is held to the same rule.
    w.upsert(ThreadKey::whole(b), msg(b, 3, "b"));
    assert!(w.replace(ThreadKey::whole(b), huge(b, 3)));
    assert_eq!(ids(&w, a), vec![1]);
    assert!(w.total_bytes() <= MAX_STORE_BYTES);
    // Once closed, it is a background window like any other.
    w.close(ThreadKey::whole(a));
    assert_eq!(w.shown(), None);
    w.upsert(ThreadKey::whole(b), huge(b, 4));
    assert!(w.get(ThreadKey::whole(a)).is_none());
}

/// The open window's own growth trims it within itself: it is never
/// evicted, even when it alone would exceed the budget.
#[test]
fn the_open_window_alone_over_budget_trims_itself() {
    let mut w = Windows::default();
    let a = user(1);
    w.open(ThreadKey::whole(a));
    w.upsert(ThreadKey::whole(a), msg(a, 1, "a"));
    w.upsert(ThreadKey::whole(a), huge(a, 2));
    assert!(w.get(ThreadKey::whole(a)).is_some(), "trimmed, not evicted");
    assert!(w.total_bytes() <= MAX_STORE_BYTES);
}
