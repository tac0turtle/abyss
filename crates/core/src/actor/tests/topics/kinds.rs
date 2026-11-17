//! Forum knowledge outlives the chat list (plan section 3): the retained
//! views remember the kind a summary last stated for their chat, so a chat
//! that leaves the dialogs keeps it, and a view's shape never overrides it.
//! The memory goes with the views. (Fencing and cleanup after a kind
//! change are task 8's.)

use std::time::Duration;

use super::super::chat;
use super::{Harness, add_big_forum, forum, forum_summary, online, reconciled, run_for, topic_key};
use crate::api::{Command, Event, HistoryDir, UserError, WindowState};
use crate::backend::Update;
use crate::backend::fake::{FakeScript, text_message};
use crate::model::{ChatId, ChatKind, PeerFolder, ThreadKey};

const PLAIN_GROUP: ChatKind = ChatKind::Supergroup {
    forum: false,
    gigagroup: false,
};

/// A light refresh (window activation past the minimum gap): the first
/// dialogs page, which here is the whole list, replaces core's.
async fn refresh_dialogs(h: &mut Harness) {
    run_for(Duration::from_secs(16)).await;
    h.send(Command::Window(WindowState::Active)).await;
    h.until(|e| matches!(e, Event::DialogsReset(_))).await;
}

/// The server's dialog of `f` changes kind, and core's list learns it.
async fn set_kind(h: &mut Harness, f: ChatId, kind: ChatKind) {
    h.fake.edit_dialogs(|d| {
        for c in d.iter_mut().filter(|c| c.id == f) {
            c.kind = kind;
        }
    });
    refresh_dialogs(h).await;
}

/// `f` leaves the server's dialogs, and core's list.
async fn leave_dialogs(h: &mut Harness, f: ChatId) {
    h.fake.edit_dialogs(|d| d.retain(|c| c.id != f));
    refresh_dialogs(h).await;
}

/// Opens `thread` and waits until it is reconciled, failing if refused.
async fn opened(h: &mut Harness, thread: ThreadKey) {
    h.send(Command::OpenThread(thread)).await;
    let skipped = reconciled(h, thread).await;
    assert!(
        !skipped
            .iter()
            .any(|e| matches!(e, Event::Error(UserError::Rejected))),
        "{thread:?} refused: {skipped:?}"
    );
}

async fn refused(h: &mut Harness, thread: ThreadKey) {
    h.send(Command::OpenThread(thread)).await;
    h.until(|e| matches!(e, Event::Error(UserError::Rejected)))
        .await;
}

/// A plain supergroup open through its whole-chat key becomes a forum,
/// then leaves the chat list. Its view remembers the forum: a headerless
/// (General) message stays out of the whole-chat window, and the whole-chat
/// key is now refused.
#[tokio::test(start_paused = true)]
async fn a_forum_that_leaves_the_chat_list_stays_a_forum() {
    let f = forum(100);
    let mut script = FakeScript::demo(1, 5);
    add_big_forum(&mut script, f, 2);
    for c in script.dialogs.iter_mut().filter(|c| c.id == f) {
        c.kind = PLAIN_GROUP;
    }
    let whole = ThreadKey::whole(f);
    let mut h = online(script).await;
    opened(&mut h, ThreadKey::whole(chat(1))).await;
    opened(&mut h, whole).await;
    set_kind(&mut h, f, forum_summary(f).kind).await;
    leave_dialogs(&mut h, f).await;

    h.fake.incoming(whole, "General, no header").await.unwrap();
    // Updates apply in order: chat 1's message comes after it.
    h.fake
        .push_update(Update::NewMessage(text_message(chat(1), 6, "barrier")))
        .await;
    let (_, skipped) = h
        .until(|e| matches!(e, Event::History { thread, dir: HistoryDir::Newer, .. } if *thread == ThreadKey::whole(chat(1))))
        .await;
    assert!(
        !skipped
            .iter()
            .any(|e| matches!(e, Event::History { thread, messages, .. } if *thread == whole && !messages.is_empty())),
        "{skipped:?}"
    );
    refused(&mut h, whole).await;
    h.stop().await;
}

/// A forum with an open topic becomes a plain group, then leaves the chat
/// list. The topic view does not make it a forum again: its whole-chat key
/// opens.
#[tokio::test(start_paused = true)]
async fn a_topic_view_does_not_resurrect_a_forum() {
    let f = forum(100);
    let mut script = FakeScript::demo(1, 1);
    add_big_forum(&mut script, f, 2);
    let mut h = online(script).await;
    opened(&mut h, topic_key(f, 2)).await;
    set_kind(&mut h, f, PLAIN_GROUP).await;
    leave_dialogs(&mut h, f).await;
    opened(&mut h, ThreadKey::whole(f)).await;
    h.stop().await;
}

/// The memory is pruned with the views: once a forum's last view is
/// evicted (eight other chats opened) and it left the chat list, nothing
/// is known about it, so its whole-chat key is no longer refused.
#[tokio::test(start_paused = true)]
async fn the_kind_is_forgotten_with_the_last_view() {
    let f = forum(100);
    let mut script = FakeScript::demo(8, 1);
    add_big_forum(&mut script, f, 2);
    let mut h = online(script).await;
    opened(&mut h, topic_key(f, 2)).await;
    leave_dialogs(&mut h, f).await;
    refused(&mut h, ThreadKey::whole(f)).await;
    for n in 1..=8 {
        opened(&mut h, ThreadKey::whole(chat(n))).await;
    }
    opened(&mut h, ThreadKey::whole(f)).await;
    h.stop().await;
}

/// Folders plan part A: the peer folder and the kind are independent. A
/// forum moved into the archive is still a forum: its summary keeps the
/// kind, the whole-chat key stays refused, and its topics still open.
#[tokio::test(start_paused = true)]
async fn an_archived_forum_is_still_a_forum() {
    let f = forum(100);
    let mut script = FakeScript::demo(1, 1);
    add_big_forum(&mut script, f, 2);
    let mut h = online(script).await;
    h.until(|e| matches!(e, Event::DialogsReset(c) if c.iter().any(|c| c.id == f)))
        .await;
    h.fake
        .push_update(Update::Folders(vec![(f, PeerFolder::Archive)]))
        .await;
    let (ev, _) = h
        .until(|e| matches!(e, Event::ChatBumped(s) if s.id == f))
        .await;
    let Event::ChatBumped(summary) = ev else {
        unreachable!()
    };
    assert_eq!(summary.folder, PeerFolder::Archive);
    assert_eq!(summary.kind, forum_summary(f).kind);
    refused(&mut h, ThreadKey::whole(f)).await;
    opened(&mut h, topic_key(f, 2)).await;
    h.stop().await;
}

/// Folders plan part C: archiving a forum from the client (the server
/// accepts, core applies the move) leaves it a forum.
#[tokio::test(start_paused = true)]
async fn a_forum_archived_from_here_is_still_a_forum() {
    let f = forum(100);
    let mut script = FakeScript::demo(1, 1);
    add_big_forum(&mut script, f, 2);
    let mut h = online(script).await;
    h.until(|e| matches!(e, Event::DialogsReset(c) if c.iter().any(|c| c.id == f)))
        .await;
    h.send(Command::SetPeerFolder {
        chat: f,
        folder: PeerFolder::Archive,
    })
    .await;
    let (ev, _) = h
        .until(|e| matches!(e, Event::ChatBumped(s) if s.id == f))
        .await;
    assert!(
        matches!(ev, Event::ChatBumped(s) if s.folder == PeerFolder::Archive && s.kind == forum_summary(f).kind)
    );
    refused(&mut h, ThreadKey::whole(f)).await;
    opened(&mut h, topic_key(f, 2)).await;
    h.stop().await;
}
