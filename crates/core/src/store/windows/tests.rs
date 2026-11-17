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

/// Windows keyed by thread (plan stage 2, section 11): random histories of
/// a forum's topics, its whole-chat thread and another chat, with messages
/// that sometimes name another thread than the window they are offered to.
mod threads {
    use proptest::prelude::*;

    use super::*;
    use crate::model::TopicId;

    /// A forum (a channel) and a private chat.
    fn forum() -> ChatId {
        chat(PeerKind::Channel, 9)
    }

    /// The threads under test: topics 1–3 and the whole chat of the forum,
    /// and the private chat.
    fn key(n: usize) -> ThreadKey {
        match n {
            0..=2 => ThreadKey {
                chat: forum(),
                topic: TopicId::try_from(i32::try_from(n).unwrap() + 1).ok(),
            },
            3 => ThreadKey::whole(forum()),
            _ => ThreadKey::whole(user(1)),
        }
    }

    const KEYS: usize = 5;

    fn post(thread: ThreadKey, id: i32) -> Message {
        Message {
            thread,
            ..msg(thread.chat, id, "")
        }
    }

    #[derive(Clone, Debug)]
    enum Op {
        Open(usize),
        Close(usize),
        Invalidate(usize),
        /// A page for window `.0` whose messages name thread `.1`.
        Prepend(usize, usize, Vec<i32>),
        Append(usize, usize, Vec<i32>),
        Upsert(usize, usize, i32),
        Replace(usize, usize, i32),
        Delete(usize, Vec<i32>),
        LoseContinuity(usize),
        Contiguous(usize),
    }

    fn op() -> impl Strategy<Value = Op> {
        let k = 0..KEYS;
        let ids = prop::collection::vec(1..200i32, 0..12);
        prop_oneof![
            k.clone().prop_map(Op::Open),
            k.clone().prop_map(Op::Close),
            k.clone().prop_map(Op::Invalidate),
            (k.clone(), k.clone(), ids.clone()).prop_map(|(w, t, v)| Op::Prepend(w, t, v)),
            (k.clone(), k.clone(), ids.clone()).prop_map(|(w, t, v)| Op::Append(w, t, v)),
            (k.clone(), k.clone(), 1..200i32).prop_map(|(w, t, id)| Op::Upsert(w, t, id)),
            (k.clone(), k.clone(), 1..200i32).prop_map(|(w, t, id)| Op::Replace(w, t, id)),
            (k.clone(), ids).prop_map(|(c, v)| Op::Delete(c, v)),
            k.clone().prop_map(Op::LoseContinuity),
            k.prop_map(Op::Contiguous),
        ]
    }

    /// Every window's ids, by thread.
    fn snapshot(w: &Windows) -> Vec<(ThreadKey, Vec<MsgId>, bool)> {
        w.threads()
            .filter_map(|t| {
                let win = w.get(t)?;
                let ids = win.messages().iter().map(|m| m.id).collect();
                Some((t, ids, win.cache_contiguous()))
            })
            .collect()
    }

    fn apply(w: &mut Windows, op: &Op) {
        let page = |t: usize, ids: &[i32]| -> Vec<Message> {
            ids.iter().map(|id| post(key(t), *id)).collect()
        };
        let view = |w: &Windows, n: usize| w.get(key(n)).map(Window::view_gen);
        match op {
            Op::Open(n) => drop(w.open(key(*n))),
            Op::Close(n) => w.close(key(*n)),
            Op::Invalidate(n) => drop(w.invalidate(key(*n))),
            Op::Prepend(n, t, ids) => {
                if let Some(g) = view(w, *n) {
                    w.prepend(key(*n), g, more(page(*t, ids))).unwrap();
                }
            }
            Op::Append(n, t, ids) => {
                if let Some(g) = view(w, *n) {
                    w.append_newer(key(*n), g, last(page(*t, ids))).unwrap();
                }
            }
            Op::Upsert(n, t, id) => drop(w.upsert(key(*n), post(key(*t), *id))),
            Op::Replace(n, t, id) => drop(w.replace(key(*n), post(key(*t), *id))),
            Op::Delete(n, ids) => {
                let ids: Vec<MsgId> = ids.iter().map(|id| MsgId(*id)).collect();
                w.delete(key(*n).chat, &ids);
            }
            Op::LoseContinuity(n) => w.lose_continuity(key(*n).chat),
            Op::Contiguous(n) => w.set_cache_contiguous(key(*n)),
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        /// Every message in a window has `thread == key` after any sequence
        /// of loads, live messages and edits; a message offered to another
        /// thread's window is rejected (never stored). A chat-level delete
        /// removes the ids from every thread window of that chat and from
        /// no other chat, and losing continuity is per chat too.
        #[test]
        fn every_window_holds_only_its_thread(ops in prop::collection::vec(op(), 1..60)) {
            let mut w = Windows::default();
            for op in &ops {
                let before = snapshot(&w);
                apply(&mut w, op);
                for t in w.threads() {
                    let win = w.get(t).unwrap();
                    prop_assert!(win.messages().iter().all(|m| m.thread == t), "{op:?}");
                }
                match op {
                    Op::Prepend(n, t, _)
                    | Op::Append(n, t, _)
                    | Op::Upsert(n, t, _)
                    | Op::Replace(n, t, _)
                        if n != t =>
                    {
                        prop_assert_eq!(snapshot(&w), before.clone(), "rejected: {:?}", op);
                    }
                    Op::Delete(n, ids) => {
                        let chat = key(*n).chat;
                        for (t, held, contiguous) in snapshot(&w) {
                            let was = before.iter().find(|(b, _, _)| *b == t).unwrap();
                            if t.chat == chat {
                                prop_assert!(held.iter().all(|id| !ids.contains(&id.0)));
                                let kept: Vec<MsgId> = was.1.iter().copied()
                                    .filter(|id| !ids.contains(&id.0)).collect();
                                prop_assert_eq!(held, kept);
                            } else {
                                prop_assert_eq!(&held, &was.1, "another chat");
                            }
                            prop_assert_eq!(contiguous, was.2);
                        }
                    }
                    Op::LoseContinuity(n) => {
                        let chat = key(*n).chat;
                        for (t, _, contiguous) in snapshot(&w) {
                            let was = before.iter().find(|(b, _, _)| *b == t).unwrap();
                            prop_assert_eq!(contiguous, was.2 && t.chat != chat);
                        }
                    }
                    _ => {}
                }
            }
        }

        /// `holder` finds the window of the asked chat that holds an id,
        /// the preferred thread first.
        #[test]
        fn holder_finds_the_copy_in_its_chat(ops in prop::collection::vec(op(), 1..40), id in 1..200i32, n in 0..KEYS) {
            let mut w = Windows::default();
            for op in &ops {
                apply(&mut w, op);
            }
            let prefer = key(n);
            let holding: Vec<ThreadKey> = w
                .threads()
                .filter(|t| t.chat == prefer.chat && w.get(*t).unwrap().get(MsgId(id)).is_some())
                .collect();
            match w.holder(prefer, MsgId(id)) {
                Some(t) => {
                    prop_assert!(holding.contains(&t));
                    prop_assert!(!holding.contains(&prefer) || t == prefer);
                }
                None => prop_assert!(holding.is_empty()),
            }
        }
    }
}
