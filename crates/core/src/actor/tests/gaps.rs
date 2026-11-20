//! A known update gap (`StreamItem::CatchUpGap`) needs a snapshot taken after
//! it, and the checkpoint past it is accepted only once that snapshot exists
//! (stage 3 review A10).

use std::time::Duration;

use super::{CacheDir, Harness, cache_at, chat, harness, history_ids};
use crate::api::{Command, Event, HistoryDir, Notice, UserError, WindowState};
use crate::backend::fake::{FakeScript, Fault, Op, summary, text_message};
use crate::backend::{BackendError, NotifyScope, StaleHistory, StreamItem};
use crate::cache::{CacheCmd, CacheHandle, CacheKey};
use crate::model::{ChatId, MsgId, PeerKind, ThreadKey, TopicId};

async fn wait_for_call(h: &Harness, op: Op, count: usize) {
    for _ in 0..1000 {
        if h.fake.calls(op) >= count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected {count} {op:?} calls, saw {}", h.fake.calls(op));
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(100)).await;
}

/// Starts a light refresh and holds its dialogs snapshot.
async fn hold_a_refresh(h: &Harness) -> std::sync::Arc<tokio::sync::Notify> {
    let calls = h.fake.calls(Op::Dialogs);
    let gate = h.fake.hold(Op::Dialogs, calls + 1);
    tokio::time::sleep(Duration::from_secs(20)).await;
    h.send(Command::Window(WindowState::Active)).await;
    wait_for_call(h, Op::Dialogs, calls + 1).await;
    gate
}

#[tokio::test(start_paused = true)]
async fn a_gap_during_a_refresh_gets_a_second_refresh_before_its_checkpoint() {
    let mut h = harness(FakeScript::demo(2, 0), None).await;
    h.online().await;
    let calls = h.fake.calls(Op::Dialogs);
    let trailing = h.fake.hold(Op::Dialogs, calls + 2);
    let gate = hold_a_refresh(&h).await;

    h.fake.catch_up_gap().await;
    h.fake.checkpoint(99).await;
    settle().await;
    gate.notify_one();

    wait_for_call(&h, Op::Dialogs, calls + 2).await;
    settle().await;
    assert!(
        h.fake.recorded_checkpoints().is_empty(),
        "the position past the gap waits for a snapshot taken after it"
    );
    trailing.notify_one();
    settle().await;
    assert_eq!(h.fake.recorded_checkpoints(), [99]);
    assert_eq!(h.fake.calls(Op::Dialogs), calls + 2, "one trailing refresh");
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_gap_while_idle_refreshes_once_and_then_records() {
    let mut h = harness(FakeScript::demo(2, 0), None).await;
    h.online().await;
    let calls = h.fake.calls(Op::Dialogs);
    h.fake.catch_up_gap().await;
    h.fake.checkpoint(7).await;
    settle().await;
    assert_eq!(h.fake.calls(Op::Dialogs), calls + 1);
    assert_eq!(h.fake.recorded_checkpoints(), [7]);
    h.stop().await;
}

#[tokio::test]
async fn reopening_the_cache_serves_stale_history_from_the_server_and_keeps_other_chats() {
    let dir = CacheDir::new();
    let key = CacheKey::generate().unwrap();
    let cache = cache_at(dir.path(), &key);
    let channel = ChatId {
        kind: PeerKind::Channel,
        id: 42,
    };
    let stale = ThreadKey::whole(channel);
    let sibling = ThreadKey {
        chat: channel,
        topic: Some(TopicId::try_from(9).unwrap()),
    };
    let trusted = ThreadKey::whole(chat(2));
    seed_history(&cache, [(stale, 43), (sibling, 44), (trusted, 43)]).await;
    let mut h = harness(FakeScript::demo(2, 0), Some(cache.clone())).await;
    h.online().await;
    let calls = h.fake.calls(Op::Dialogs);
    h.fake
        .push_stream_item(StreamItem::HistoryStale(StaleHistory(Some(channel))))
        .await;
    h.fake.catch_up_gap().await;
    h.fake.push_stream_item(StreamItem::CatchUpEnded).await;
    h.fake.checkpoint(7).await;
    wait_for_call(&h, Op::Dialogs, calls + 1).await;
    settle().await;
    assert_eq!(h.fake.recorded_checkpoints(), [7]);
    h.stop().await;
    cache.close().await.unwrap();

    let cache = cache_at(dir.path(), &key);
    assert!(
        cache.tail(sibling, 10).await.unwrap().is_empty(),
        "all threads of the stale chat"
    );
    let mut script = FakeScript::demo(2, 0);
    script.dialogs.push(summary(channel, "channel"));
    script
        .history
        .insert(channel, vec![text_message(channel, 42, "server copy")]);
    let mut h = harness(script, Some(cache.clone())).await;
    h.online().await;
    let fresh = h.fake.hold(Op::History, 1);
    h.send(Command::OpenThread(stale)).await;
    wait_for_call(&h, Op::History, 1).await;
    assert!(
        h.drain()
            .iter()
            .all(|event| !matches!(event, Event::History { .. }))
    );
    fresh.notify_one();
    assert_eq!(
        history_ids(&h.history(channel, HistoryDir::Tail).await),
        [42]
    );
    let cached = cache.tail(stale, 10).await.unwrap();
    assert_eq!(cached.iter().map(|m| m.id).collect::<Vec<_>>(), [MsgId(42)]);

    let trusted_server = h.fake.hold(Op::History, 2);
    h.send(Command::OpenThread(trusted)).await;
    assert_eq!(
        history_ids(&h.history(trusted.chat, HistoryDir::Tail).await),
        [43]
    );
    trusted_server.notify_one();
    h.stop().await;
}

async fn seed_history(cache: &CacheHandle, rows: [(ThreadKey, i32); 3]) {
    for (thread, id) in rows {
        let mut message = text_message(thread.chat, id, "old cache");
        message.thread = thread;
        cache
            .write(CacheCmd::PutMessages(thread, vec![message]))
            .await;
        assert_eq!(cache.tail(thread, 10).await.unwrap().len(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn a_failed_gap_snapshot_keeps_the_previous_checkpoint() {
    let mut h = harness(FakeScript::demo(2, 0), None).await;
    h.online().await;
    h.send(Command::Window(WindowState::Inactive)).await;
    h.fake.checkpoint(5).await;
    let calls = h.fake.calls(Op::Dialogs);
    let gate = h.fake.hold(Op::Dialogs, calls + 1);
    let retry = h.fake.hold(Op::Dialogs, calls + 2);
    h.fake
        .inject(Op::Dialogs, Fault::Error(BackendError::Transient));
    h.fake.catch_up_gap().await;
    h.fake.checkpoint(7).await;
    wait_for_call(&h, Op::Dialogs, calls + 1).await;
    settle().await;
    gate.notify_one();
    h.until(|e| matches!(e, Event::LoadEnded { error: Some(_), .. }))
        .await;
    settle().await;
    assert_eq!(h.fake.recorded_checkpoints(), [5]);
    wait_for_call(&h, Op::Dialogs, calls + 2).await;
    h.fake.checkpoint(8).await;
    settle().await;
    assert_eq!(h.fake.recorded_checkpoints(), [5]);
    retry.notify_one();
    h.online().await;
    assert_eq!(h.fake.recorded_checkpoints(), [5, 8]);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_failed_gap_tail_keeps_the_previous_checkpoint() {
    let mut h = harness(FakeScript::demo(2, 0), None).await;
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h.until(|event| matches!(event, Event::Reconciled { .. }))
        .await;
    let calls = h.fake.calls(Op::History);
    let tail = h.fake.hold(Op::History, calls + 1);
    h.fake
        .inject(Op::History, Fault::Error(BackendError::Transient));
    h.fake.catch_up_gap().await;
    h.fake.checkpoint(7).await;
    wait_for_call(&h, Op::History, calls + 1).await;
    settle().await;
    tail.notify_one();
    h.until(|event| matches!(event, Event::Error(UserError::Network)))
        .await;
    settle().await;
    assert!(h.fake.recorded_checkpoints().is_empty());
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_failed_gap_scope_snapshot_keeps_the_previous_checkpoint() {
    let mut h = harness(FakeScript::demo(1, 0), None).await;
    h.online().await;
    h.fake.checkpoint(5).await;
    settle().await;
    let op = Op::NotifySettings(NotifyScope::Groups);
    let gap_read = h.fake.hold(op, 2);
    let retry = h.fake.hold(op, 3);
    h.fake.inject(op, Fault::Error(BackendError::Transient));
    h.fake.catch_up_gap().await;
    h.fake.checkpoint(7).await;
    wait_for_call(&h, op, 2).await;
    assert_eq!(h.fake.recorded_checkpoints(), [5]);
    gap_read.notify_one();
    wait_for_call(&h, op, 3).await;
    h.fake.checkpoint(8).await;
    settle().await;
    assert_eq!(h.fake.recorded_checkpoints(), [5]);
    retry.notify_one();
    h.online().await;
    assert_eq!(h.fake.recorded_checkpoints(), [5, 8]);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_gap_waits_for_its_own_first_page_behind_pagination() {
    let mut h = harness(FakeScript::demo(101, 0), None).await;
    h.online().await;
    h.send(Command::Window(WindowState::Inactive)).await;
    let calls = h.fake.calls(Op::Dialogs);
    let pagination = h.fake.hold(Op::Dialogs, calls + 1);
    let first_page = h.fake.hold(Op::Dialogs, calls + 2);
    h.send(Command::LoadDialogs).await;
    wait_for_call(&h, Op::Dialogs, calls + 1).await;
    h.fake.catch_up_gap().await;
    h.fake.checkpoint(7).await;
    settle().await;
    assert!(h.fake.recorded_checkpoints().is_empty());
    pagination.notify_one();
    wait_for_call(&h, Op::Dialogs, calls + 2).await;
    settle().await;
    assert!(h.fake.recorded_checkpoints().is_empty());
    first_page.notify_one();
    settle().await;
    assert_eq!(h.fake.recorded_checkpoints(), [7]);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_gap_summarizes_only_an_increase_in_unread_unmuted_chats() {
    let mut script = FakeScript::demo(4, 0);
    script.dialogs[0].read.unread = 1;
    script.dialogs[1].muted_until = Some(i64::MAX);
    let mut h = harness(script, None).await;
    assert!(
        h.online()
            .await
            .iter()
            .all(|e| !matches!(e, Event::Notify(_)))
    );

    let calls = h.fake.calls(Op::Dialogs);
    h.fake.edit_dialogs(|rows| {
        rows[0].read.unread = 0;
        rows[1].read.unread = 100;
        rows[2].read.unread = 10;
    });
    h.fake.catch_up_gap().await;
    wait_for_call(&h, Op::Dialogs, calls + 1).await;
    settle().await;
    assert!(h.drain().iter().all(|e| !matches!(e, Event::Notify(_))));

    h.fake.edit_dialogs(|rows| rows[3].read.unread = 1);
    let gate = h.fake.hold(Op::Dialogs, calls + 2);
    h.fake.catch_up_gap().await;
    wait_for_call(&h, Op::Dialogs, calls + 2).await;
    settle().await;
    assert!(h.drain().iter().all(|e| !matches!(e, Event::Notify(_))));
    gate.notify_one();
    let (notice, _) = h.until(|e| matches!(e, Event::Notify(_))).await;
    assert!(matches!(notice, Event::Notify(Notice::Summary)));
    settle().await;
    assert!(h.drain().iter().all(|e| !matches!(e, Event::Notify(_))));
    h.stop().await;
}
