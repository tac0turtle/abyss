//! The forum badge (plan sections 7 and 11, "Forum badge"): the chat
//! list's unread-topic count of a forum is only ever a server value, set
//! from dialogs pages, `peer_dialogs` results and chat-level reads; every
//! trigger makes it dirty, and the drain refreshes every dirty forum,
//! wherever it sits in the list, at the scheduler's rate.

use std::collections::BTreeSet;
use std::time::Duration;

use super::*;
use crate::backend::BackendError;
use crate::backend::fake::{Fault, Op};
use crate::store::{CURSOR_SLOTS, PEER_CALL_INTERVAL};

/// Unread counts the events report for `chat`'s row, in order.
fn badges_of(events: &[Event], chat: ChatId) -> Vec<u32> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::ChatBumped(s) if s.id == chat => Some(s.read.unread),
            Event::DialogsReset(v) | Event::DialogsAppended(v) => {
                v.iter().find(|s| s.id == chat).map(|s| s.read.unread)
            }
            _ => None,
        })
        .collect()
}

/// Messages into a loaded topic, an unloaded one and a forum with no list
/// change nothing locally: the badge only ever shows what the server sent,
/// refreshed by `peer_dialogs`, one call per interval. The interval counts
/// from a call's admission: here every permit is held for 5 s, so the first
/// call waits that long in the queue.
#[tokio::test(start_paused = true)]
async fn messages_only_trigger_a_refresh() {
    let (f, g) = (forum(100), forum(101));
    let mut script = FakeScript::demo(1, 1);
    add_big_forum(&mut script, f, 60);
    add_big_forum(&mut script, g, 2);
    let mut h = online(script).await;
    open_forum(&mut h, f).await;
    for i in 0..u64::try_from(crate::actor::MAX_CONCURRENT).unwrap() {
        h.fake
            .inject(Op::Send, Fault::Delay(Duration::from_secs(5)));
        h.send(Command::SendText {
            thread: ThreadKey::whole(super::super::chat(1)),
            text: format!("holds a permit {i}"),
            local_id: crate::model::LocalId(i),
        })
        .await;
    }
    let mut seen = Vec::new();
    // Topic 61 is loaded, topic 2 isn't (beyond page one); `g` has no list.
    for thread in [
        topic_key(f, 61),
        topic_key(f, 2),
        topic_key(f, 61),
        topic_key(g, 2),
    ] {
        h.fake.incoming(thread, "new").await.unwrap();
    }
    seen.extend(events_for(&mut h, Duration::from_secs(20)).await);
    // The server now counts two unread topics in `f` and one in `g`.
    for (chat, server) in [(f, 2), (g, 1)] {
        let shown = badges_of(&seen, chat);
        assert!(shown.iter().all(|n| [0, server].contains(n)), "{shown:?}");
        assert_eq!(shown.last(), Some(&server), "{chat:?}");
    }
    let calls = peer_calls_with(&h, f);
    assert!(!calls.is_empty() && calls.len() <= 2, "{calls:?}");
    assert_spaced(&h);
    h.stop().await;
}

/// Fifty forums beyond the first dialogs page get a remote read
/// each, then nothing else happens. Every one is refreshed by the dirty-bit
/// drain within `ceil(50 / 9)` calls (fewer with spill-over); no dialogs
/// page is substituted.
#[tokio::test(start_paused = true)]
async fn badge_overflow_beyond_page_one() {
    let mut script = FakeScript::demo(100, 1);
    let forums: Vec<ChatId> = (0..50).map(|n| forum(1000 + n)).collect();
    for f in &forums {
        add_unread_forum(&mut script, *f);
    }
    let mut h = online(script).await;
    load_all_dialogs(&mut h).await;
    let dialogs = h.fake.calls(Op::Dialogs);
    let before = peer_calls(&h).len();
    for f in &forums {
        h.fake
            .read_remotely(topic_key(*f, 2), MsgId(10))
            .await
            .unwrap();
    }
    let calls = 50usize.div_ceil(CURSOR_SLOTS);
    let seen = peer_calls_reached(&mut h, before + 1).await;
    let mut seen = seen;
    seen.extend(events_for(&mut h, PEER_CALL_INTERVAL * u32::try_from(calls).unwrap()).await);
    let made = &peer_calls(&h)[before..];
    assert!(made.len() <= calls, "{made:?}");
    let asked: BTreeSet<ChatId> = made.iter().flat_map(|r| r.chats.clone()).collect();
    for f in &forums {
        assert!(asked.contains(f), "{f:?} never refreshed");
        assert_eq!(badges_of(&seen, *f).last(), Some(&0), "{f:?}");
    }
    assert_eq!(
        h.fake.calls(Op::Dialogs),
        dialogs,
        "no dialogs page instead"
    );
    assert_spaced(&h);
    h.stop().await;
}

/// Calls are never closer than the interval, and a flood wait pauses the
/// scheduler until its deadline (kept across navigation). A dirty forum that
/// becomes a plain group meanwhile is refreshed once: its non-forum summary
/// answers the refresh.
#[tokio::test(start_paused = true)]
async fn a_flood_wait_pauses_the_scheduler() {
    let (f, g) = (forum(100), forum(101));
    let mut script = FakeScript::demo(1, 1);
    add_big_forum(&mut script, f, 2);
    add_big_forum(&mut script, g, 2);
    let mut h = online(script).await;
    h.fake.inject(
        Op::PeerDialogs,
        Fault::Error(BackendError::FloodWait { seconds: 60 }),
    );
    h.fake.incoming(topic_key(f, 2), "new").await.unwrap();
    peer_calls_reached(&mut h, 1).await;
    let flooded = peer_calls(&h)[0].at;
    h.fake.incoming(topic_key(f, 2), "more").await.unwrap();
    h.fake.incoming(topic_key(g, 2), "dirty").await.unwrap();
    h.fake.edit_dialogs(|d| {
        for c in d.iter_mut().filter(|c| c.id == g) {
            c.kind = GROUP_KIND;
        }
    });
    open_forum(&mut h, f).await;
    let seen = events_for(&mut h, Duration::from_secs(70)).await;
    let calls = peer_calls_with(&h, f);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls[1] - flooded >= Duration::from_secs(60));
    assert_eq!(badges_of(&seen, f).last(), Some(&1));
    assert_eq!(peer_calls_with(&h, g).len(), 1, "not refreshed again");
    assert_spaced(&h);
    h.stop().await;
}
