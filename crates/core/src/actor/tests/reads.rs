//! Chat read marks against the confirmed read state (stage 2 plan, section
//! 17.6): a read proves the server's pointer and no count. The row shows
//! Stage 1's 0 only while a read covering every known message is
//! outstanding, and its count comes from the server again afterwards.

use std::time::Duration;

use super::topics::{events_for, last_bump, online, peer_calls_with};
use super::{Harness, chat};
use crate::api::{Command, Event};
use crate::backend::fake::{FakeScript, Fault, Op, text_message};
use crate::backend::{BackendError, RejectReason, Update};
use crate::model::{ChatSummary, MsgId, ReadState, ThreadKey};

/// Chats 1 and 101 with ten incoming messages, all unread: the server's
/// pair is (10, 0). Chat 101 is beyond the first dialogs page. Chat 1's
/// thread is open, so the pane shows it.
async fn ten_unread() -> Harness {
    let mut script = FakeScript::demo(101, 10);
    for dialog in &mut script.dialogs {
        dialog.read.unread = 10;
        dialog.last_id = Some(MsgId(10));
    }
    let h = online(script).await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h
}

async fn mark_read(h: &Harness, id: i64, max_id: i32) {
    h.send(Command::MarkRead {
        thread: ThreadKey::whole(chat(id)),
        max_id: MsgId(max_id),
    })
    .await;
}

fn read(unread: u32, pointer: i32) -> ReadState {
    ReadState {
        unread,
        read_inbox_max: MsgId(pointer),
    }
}

/// The rows `events` report for chat 1, in order.
fn rows(events: &[Event]) -> Vec<ChatSummary> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::ChatBumped(s) if s.id == chat(1) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

/// A partial read keeps the confirmed count (no 0 at admission); landing
/// raises the pointer and refreshes the chat through `peer_dialogs`
/// (the shown slot), whose result sets the server's pair. So does a read
/// of a chat with a window only, which the page that admits it shows; it
/// is tracked only while the pane shows it.
#[tokio::test(start_paused = true)]
async fn a_partial_read_shows_the_servers_pair_after_one_refresh() {
    let mut h = ten_unread().await;
    events_for(&mut h, Duration::from_secs(5)).await;
    let peers = peer_calls_with(&h, chat(1)).len();
    mark_read(&h, 1, 4).await;
    let seen = events_for(&mut h, Duration::from_secs(5)).await;
    let rows = rows(&seen);
    let admitted = rows.first().unwrap();
    assert_eq!(admitted.reading, Some(MsgId(4)));
    assert_eq!(admitted.displayed_unread(), 10, "not covering: no 0");
    assert!(
        rows.iter()
            .any(|r| r.read == read(10, 4) && r.reading.is_none()),
        "landed: the pointer, and no count: {rows:?}"
    );
    let last = rows.last().unwrap();
    assert_eq!((last.read, last.reading), (read(6, 4), None));
    assert_eq!(last.displayed_unread(), 6);
    assert_eq!(peer_calls_with(&h, chat(1)).len(), peers + 1);

    h.send(Command::OpenThread(ThreadKey::whole(chat(101))))
        .await;
    events_for(&mut h, Duration::from_secs(5)).await;
    let peers = peer_calls_with(&h, chat(101)).len();
    mark_read(&h, 101, 4).await;
    events_for(&mut h, Duration::from_secs(10)).await;
    assert_eq!(peer_calls_with(&h, chat(101)).len(), peers + 1);
    // Its next refresh fails, and the pane moves to chat 1: chat 101 is
    // no longer tracked, so it isn't retried.
    h.fake
        .inject(Op::PeerDialogs, Fault::Error(BackendError::Transient));
    mark_read(&h, 101, 5).await;
    events_for(&mut h, Duration::from_secs(1)).await;
    assert_eq!(peer_calls_with(&h, chat(101)).len(), peers + 2);
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    events_for(&mut h, Duration::from_secs(15)).await;
    assert_eq!(peer_calls_with(&h, chat(101)).len(), peers + 2);
    h.send(Command::LoadDialogs).await;
    let (ev, _) = h
        .until(|e| matches!(e, Event::DialogsAppended(v) if v.iter().any(|c| c.id == chat(101))))
        .await;
    let Event::DialogsAppended(page) = ev else {
        unreachable!()
    };
    let admitted = page.iter().find(|c| c.id == chat(101)).unwrap();
    assert_eq!((admitted.read, admitted.reading), (read(5, 5), None));
    h.stop().await;
}

/// A covering read lands after newer server evidence (a counted message,
/// then a read on another device): that evidence stays, and the refresh
/// agrees with it.
#[tokio::test(start_paused = true)]
async fn a_late_covering_read_leaves_newer_evidence_in_place() {
    let mut h = ten_unread().await;
    events_for(&mut h, Duration::from_secs(5)).await;
    h.fake
        .inject(Op::Read, Fault::Delay(Duration::from_secs(10)));
    mark_read(&h, 1, 10).await;
    let covered = events_for(&mut h, Duration::from_secs(1)).await;
    assert_eq!(last_bump(&covered, chat(1)).unwrap().displayed_unread(), 0);

    let mut history = h.fake.server_history(chat(1));
    history.push(text_message(chat(1), 11, "new"));
    h.fake.set_history(chat(1), history);
    h.fake
        .push_update(Update::NewMessage(text_message(chat(1), 11, "new")))
        .await;
    h.fake
        .push_update(Update::ReadInbox {
            chat: chat(1),
            topic: None,
            still_unread: 1,
            max_id: MsgId(10),
        })
        .await;
    let crossing = events_for(&mut h, Duration::from_secs(1)).await;
    let row = last_bump(&crossing, chat(1)).unwrap();
    assert_eq!((row.read, row.displayed_unread()), (read(1, 10), 1));

    let late = events_for(&mut h, Duration::from_secs(20)).await;
    assert_eq!(
        h.fake.read_calls(),
        [(ThreadKey::whole(chat(1)), MsgId(10))]
    );
    let rows = rows(&late);
    assert!(
        rows.iter().all(|r| r.read == read(1, 10)),
        "no read writes a count: {rows:?}"
    );
    let last = rows.last().unwrap();
    assert_eq!((last.reading, last.displayed_unread()), (None, 1));
    h.stop().await;
}

/// A refused read shows the confirmed count again and changes no read
/// state.
#[tokio::test(start_paused = true)]
async fn a_refused_read_shows_the_confirmed_count_again() {
    let mut h = ten_unread().await;
    events_for(&mut h, Duration::from_secs(5)).await;
    h.fake.inject(
        Op::Read,
        Fault::Error(BackendError::Rejected(RejectReason::Other)),
    );
    mark_read(&h, 1, 10).await;
    let rows = rows(&events_for(&mut h, Duration::from_secs(5)).await);
    assert_eq!(rows.first().map(ChatSummary::displayed_unread), Some(0));
    let last = rows.last().unwrap();
    assert_eq!((last.read, last.reading), (read(10, 0), None));
    assert_eq!(last.displayed_unread(), 10);
    assert!(h.fake.read_calls().is_empty());
    h.stop().await;
}
