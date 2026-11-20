//! End-to-end notice policy, mute settings and replay against the fake.

use super::{CacheDir, Harness, Internal, Setup, cache_at, chat, harness, harness_with, start};
use crate::actor::notices::unix_now;
use crate::api::{Command, Event, HistoryDir, Notice, WindowState};
use crate::backend::fake::{FakeBackend, FakeScript, Fault, Op, summary, text_message};
use crate::backend::{
    BackendError, NewMessage, NotifyScope, NotifyTarget, StaleHistory, StreamError, StreamItem,
    Update,
};
use crate::cache::{CacheCmd, CacheKey};
use crate::events::NotificationCleanupPolicy;
use crate::model::{
    Body, ChatFolder, ChatId, ChatKind, FolderRules, MsgId, PeerKind, ReadRule, Sender, ThreadKey,
    TopicId,
};
use std::time::Duration;

const FOREVER: i64 = i64::MAX;

fn group(id: i64) -> ChatId {
    ChatId {
        kind: PeerKind::Chat,
        id,
    }
}

fn incoming(chat: ChatId, id: i32) -> NewMessage {
    let mut message = text_message(chat, id, "body");
    message.date = unix_now();
    message.into()
}

async fn push(h: &Harness, new: NewMessage) {
    h.fake.push_update(Update::NewMessage(new)).await;
}

async fn mute(h: &Harness, target: NotifyTarget, until: Option<i64>) {
    h.fake
        .push_update(Update::NotifySettings { target, until })
        .await;
}

async fn notices_for(h: &mut Harness, duration: Duration) -> Vec<Notice> {
    let deadline = tokio::time::Instant::now() + duration;
    let mut notices = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, h.events.recv()).await {
        if let Event::Notify(notice) = event {
            notices.push(notice);
        }
    }
    notices
}

async fn singles(h: &mut Harness) -> Vec<ChatId> {
    let mut chats = Vec::new();
    for notice in notices_for(h, Duration::from_secs(3)).await {
        let Notice::Message { chat, .. } = notice else {
            panic!("unexpected summary")
        };
        chats.push(chat);
    }
    chats.sort_unstable();
    chats
}

#[tokio::test(start_paused = true)]
async fn live_notice_rules_are_independent() {
    let mut script = FakeScript {
        dialogs: (1..=9).map(|i| summary(chat(i), "Peer")).collect(),
        ..FakeScript::default()
    };
    script.dialogs[4].muted_until = Some(FOREVER);
    script.dialogs[5].muted_until = Some(unix_now());
    script.dialogs[6].muted_until = Some(FOREVER);
    script.dialogs[7].muted_until = Some(FOREVER);
    let mut h = harness(script, None).await;
    h.online().await;

    let mut outgoing = incoming(chat(2), 102);
    outgoing.message.outgoing = true;
    let mut silent = incoming(chat(3), 103);
    silent.silent = true;
    let mut stale = incoming(chat(4), 104);
    stale.message.date -= 301;
    let mut mention = incoming(chat(7), 107);
    mention.mentioned = true;
    let mut silent_mention = incoming(chat(8), 108);
    silent_mention.mentioned = true;
    silent_mention.silent = true;
    let mut unrendered = incoming(chat(9), 109);
    unrendered.message.body = Body::Unsupported;
    let channel = ChatId {
        kind: PeerKind::Channel,
        id: 100,
    };
    for new in [
        incoming(chat(1), 101),
        outgoing,
        silent,
        stale,
        incoming(chat(5), 105),
        incoming(chat(6), 106),
        mention,
        silent_mention,
        unrendered,
        incoming(channel, 110),
    ] {
        push(&h, new).await;
    }
    assert_eq!(singles(&mut h).await, [chat(1), chat(6), chat(7)]);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn history_and_edits_never_create_notices() {
    let history = incoming(chat(1), 4).message;
    let script = FakeScript {
        dialogs: vec![summary(chat(1), "Peer")],
        history: [(chat(1), vec![history.clone()])].into(),
        ..FakeScript::default()
    };
    let mut h = harness(script, None).await;
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h.history(chat(1), HistoryDir::Tail).await;
    let edit = crate::model::Message {
        edit_date: Some(unix_now()),
        ..history
    };
    h.fake.push_update(Update::Edited(edit)).await;
    h.until(|e| matches!(e, Event::Edited(_))).await;
    assert!(singles(&mut h).await.is_empty());
    h.stop().await;
}

fn scoped_script() -> FakeScript {
    let own = |id, until| crate::model::ChatSummary {
        muted_until: Some(until),
        ..summary(group(id), "Group")
    };
    FakeScript {
        dialogs: vec![
            summary(group(1), "Group"),
            own(2, 0),
            own(3, unix_now()),
            summary(chat(4), "Peer"),
        ],
        notify_groups: Some(FOREVER),
        ..FakeScript::default()
    }
}

#[tokio::test(start_paused = true)]
async fn scope_defaults_and_explicit_overrides_also_drive_folders() {
    let mut h = harness(scoped_script(), None).await;
    let startup = h.online().await;
    let rows = startup
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::DialogsReset(rows) => Some(rows),
            _ => None,
        })
        .unwrap();
    let folder = ChatFolder {
        id: 2,
        title: "Unmuted groups".into(),
        rules: FolderRules::GROUPS.with(FolderRules::EXCLUDE_MUTED),
        pinned: Vec::new(),
        include: Vec::new(),
        exclude: Vec::new(),
        shared: false,
    };
    for id in 1..=3 {
        let row = rows.iter().find(|row| row.id == group(id)).unwrap();
        assert_eq!(row.muted_at(unix_now()), id == 1);
        assert_eq!(folder.contains(row, unix_now(), ReadRule::Applies), id != 1);
        push(&h, incoming(group(id), 101)).await;
    }
    push(&h, incoming(chat(4), 102)).await;
    assert_eq!(singles(&mut h).await, [chat(4), group(2), group(3)]);

    let pages = h.fake.calls(Op::Dialogs);
    mute(&h, NotifyTarget::Scope(NotifyScope::Groups), None).await;
    mute(&h, NotifyTarget::Scope(NotifyScope::Users), Some(FOREVER)).await;
    push(&h, incoming(group(1), 103)).await;
    push(&h, incoming(chat(4), 104)).await;
    assert_eq!(singles(&mut h).await, [group(1)]);
    assert_eq!(h.fake.calls(Op::Dialogs), pages);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn failed_scope_is_unmuted_and_retried_on_rebuild() {
    let script = FakeScript {
        notify_users: Some(FOREVER),
        ..scoped_script()
    };
    let (backend, fake) = FakeBackend::new(script);
    let groups = Op::NotifySettings(NotifyScope::Groups);
    let users = Op::NotifySettings(NotifyScope::Users);
    fake.inject(groups, Fault::Error(BackendError::Transient));
    let mut h = harness_with(backend, fake, None).await;
    h.online().await;
    assert_eq!((h.fake.calls(groups), h.fake.calls(users)), (1, 1));
    push(&h, incoming(group(1), 101)).await;
    push(&h, incoming(chat(4), 102)).await;
    assert_eq!(singles(&mut h).await, [group(1)]);
    h.internal.send(Internal::Wake).await.unwrap();
    h.online().await;
    assert_eq!((h.fake.calls(groups), h.fake.calls(users)), (2, 2));
    push(&h, incoming(group(1), 103)).await;
    assert!(singles(&mut h).await.is_empty());
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn catch_up_burst_yields_one_summary_and_then_live_delivery_resumes() {
    let script = FakeScript {
        dialogs: (1..=20).map(|i| summary(chat(i), "Peer")).collect(),
        ..FakeScript::default()
    };
    let mut h = harness(script, None).await;
    h.online().await;
    let burst = (0..500).map(|i| incoming(chat(i64::from(i % 20) + 1), 100 + i));
    h.fake.edit_dialogs(|rows| {
        for row in rows {
            row.read.unread = 25;
        }
    });
    h.fake.catch_up(burst).await;
    assert_eq!(
        notices_for(&mut h, Duration::from_secs(3)).await,
        [Notice::Summary]
    );
    push(&h, incoming(chat(1), 100)).await;
    assert!(singles(&mut h).await.is_empty(), "live duplicate of replay");
    push(&h, incoming(chat(1), 603)).await;
    assert_eq!(singles(&mut h).await, [chat(1)]);
    h.stop().await;
}

async fn wait_for_call(h: &Harness, op: Op, count: usize) {
    for _ in 0..1000 {
        if h.fake.calls(op) >= count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected {op:?} call");
}

#[tokio::test(start_paused = true)]
async fn wake_reconcile_summarizes_server_changes_without_replayed_messages() {
    let mut h = harness(FakeScript::demo(2, 0), None).await;
    h.online().await;
    let gate = h.fake.hold(Op::Dialogs, 2);
    h.internal.send(Internal::Wake).await.unwrap();
    wait_for_call(&h, Op::Dialogs, 2).await;
    h.fake.edit_dialogs(|rows| {
        rows[0].read.unread = 1;
    });
    gate.notify_one();
    let startup = h.online().await;
    let notices: Vec<_> = startup
        .into_iter()
        .filter_map(|e| match e {
            Event::Notify(n) => Some(n),
            _ => None,
        })
        .collect();
    assert_eq!(notices, [Notice::Summary]);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn wake_with_replayed_reads_uses_the_post_replay_reconcile() {
    let mut h = harness(FakeScript::demo(1, 0), None).await;
    h.online().await;
    h.fake.edit_dialogs(|rows| {
        rows[0].read.unread = 1;
        rows[0].read.read_inbox_max = MsgId(10);
    });
    let initial = h.fake.hold(Op::Dialogs, 2);
    let after_replay = h.fake.hold(Op::Dialogs, 3);
    h.internal.send(Internal::Wake).await.unwrap();
    wait_for_call(&h, Op::Dialogs, 2).await;
    h.fake
        .push_stream_item(StreamItem::CatchUp(Update::ReadInbox {
            chat: chat(1),
            topic: None,
            still_unread: 0,
            max_id: MsgId(8),
        }))
        .await;
    h.fake.push_stream_item(StreamItem::CatchUpEnded).await;
    assert!(
        notices_for(&mut h, Duration::from_millis(100))
            .await
            .is_empty()
    );
    initial.notify_one();
    wait_for_call(&h, Op::Dialogs, 3).await;
    assert!(
        notices_for(&mut h, Duration::from_millis(100))
            .await
            .is_empty()
    );
    after_replay.notify_one();
    assert_eq!(
        notices_for(&mut h, Duration::from_secs(3)).await,
        [Notice::Summary]
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn coalesces_per_chat_and_releases_at_two_seconds() {
    let mut h = harness(FakeScript::demo(2, 0), None).await;
    h.online().await;
    for id in [101, 105, 102, 103, 103, 104] {
        push(&h, incoming(chat(1), id)).await;
    }
    assert!(
        notices_for(&mut h, Duration::from_millis(1999))
            .await
            .is_empty()
    );
    let notices = notices_for(&mut h, Duration::from_millis(1_001)).await;
    assert!(
        matches!(notices.as_slice(), [Notice::Message { chat: c, msg: MsgId(105), count: 5, .. }] if *c == chat(1))
    );
    push(&h, incoming(chat(1), 106)).await;
    push(&h, incoming(chat(2), 107)).await;
    assert_eq!(singles(&mut h).await, [chat(1), chat(2)]);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn notices_keep_sanitized_chat_and_sender_names() {
    let channel = ChatId {
        kind: PeerKind::Channel,
        id: 20,
    };
    let mut row = summary(channel, "Gre\u{202e}at\nGroup");
    row.kind = ChatKind::Supergroup {
        forum: true,
        gigagroup: false,
    };
    let mut h = harness(
        FakeScript {
            dialogs: vec![row],
            ..FakeScript::default()
        },
        None,
    )
    .await;
    h.online().await;
    let mut first = incoming(channel, 101);
    first.message.sender = Some(Sender {
        id: chat(1),
        name: "Sa\u{2068}m".into(),
    });
    push(&h, first).await;
    let notices = notices_for(&mut h, Duration::from_secs(3)).await;
    assert_eq!(
        notices,
        [Notice::Message {
            chat: channel,
            topic: Some(TopicId::GENERAL),
            msg: MsgId(101),
            chat_title: "Great Group".into(),
            sender: Some("Sam".into()),
            count: 1,
        }]
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_later_dialogs_page_crossing_an_unlisted_peer_mute_keeps_it() {
    let mut h = harness(FakeScript::demo(101, 0), None).await;
    h.online().await;
    h.fake
        .inject(Op::Dialogs, Fault::Delay(Duration::from_secs(2)));
    h.send(Command::LoadDialogs).await;
    wait_for_call(&h, Op::Dialogs, 2).await;
    let target = chat(101);
    mute(&h, NotifyTarget::Peer(target), Some(FOREVER)).await;
    push(&h, incoming(target, 111)).await;
    let (page, _) = h.until(|e| matches!(e, Event::DialogsAppended(rows) if rows.iter().any(|row| row.id == target))).await;
    let Event::DialogsAppended(rows) = page else {
        unreachable!()
    };
    let row = rows.iter().find(|row| row.id == target).unwrap();
    assert_eq!(row.muted_until, Some(FOREVER));
    assert!(singles(&mut h).await.is_empty());
    assert_eq!(h.fake.calls(Op::Dialogs), 3, "the guard refetches once");
    mute(&h, NotifyTarget::Peer(target), Some(0)).await;
    push(&h, incoming(target, 112)).await;
    assert_eq!(singles(&mut h).await, [target]);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn light_refresh_keeps_live_delivery_and_uses_the_loaded_scope_defaults() {
    let mut h = harness(scoped_script(), None).await;
    h.online().await;
    let gate = h.fake.hold(Op::Dialogs, 2);
    tokio::time::sleep(Duration::from_secs(16)).await;
    h.send(Command::Window(WindowState::Active)).await;
    wait_for_call(&h, Op::Dialogs, 2).await;
    assert_eq!(h.fake.calls(Op::NotifySettings(NotifyScope::Groups)), 1);
    push(&h, incoming(group(1), 101)).await;
    push(&h, incoming(group(2), 102)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate.notify_one();
    assert_eq!(singles(&mut h).await, [group(2)]);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_known_gap_refreshes_scope_defaults_before_releasing_notices() {
    let target = group(1);
    let mut h = harness(
        FakeScript {
            dialogs: vec![summary(target, "Group")],
            ..FakeScript::default()
        },
        None,
    )
    .await;
    h.online().await;
    push(&h, incoming(target, 101)).await;
    h.until(|e| matches!(e, Event::ChatBumped(row) if row.last_id == Some(MsgId(101))))
        .await;
    h.fake.drop_next_update();
    mute(&h, NotifyTarget::Scope(NotifyScope::Groups), Some(FOREVER)).await;
    h.fake.edit_dialogs(|rows| rows[0].read.unread = 1);
    h.fake.catch_up_gap().await;
    h.fake.checkpoint(7).await;
    let mut notices = notices_for(&mut h, Duration::from_secs(3)).await;
    assert_eq!(h.fake.checkpoint_pts(), 7);
    push(&h, incoming(target, 102)).await;
    let mut mention = incoming(target, 103);
    mention.mentioned = true;
    push(&h, mention).await;
    notices.extend(notices_for(&mut h, Duration::from_secs(3)).await);
    assert!(
        matches!(
            notices.as_slice(),
            [Notice::Message {
                msg: MsgId(103),
                count: 1,
                ..
            }]
        ),
        "{notices:?}"
    );
    for scope in [NotifyScope::Users, NotifyScope::Groups] {
        assert_eq!(h.fake.calls(Op::NotifySettings(scope)), 2);
    }
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_known_gap_rechecks_an_unlisted_peer_mute() {
    let target = chat(101);
    let mut h = harness(FakeScript::demo(101, 0), None).await;
    let startup = h.online().await;
    assert!(
        startup
            .iter()
            .any(|event| matches!(event, Event::DialogsReset(rows)
        if rows.len() == 100 && rows.iter().all(|row| row.id != target)))
    );
    let op = Op::PeerNotifySettings(target);
    push(&h, incoming(target, 101)).await;
    assert_eq!(singles(&mut h).await, [target]);
    assert_eq!(h.fake.calls(op), 1);

    h.fake.drop_next_update();
    mute(&h, NotifyTarget::Peer(target), Some(FOREVER)).await;
    h.fake.catch_up_gap().await;
    h.fake.checkpoint(99).await;
    assert!(singles(&mut h).await.is_empty());
    assert_eq!(h.fake.checkpoint_pts(), 99);

    push(&h, incoming(target, 102)).await;
    assert!(
        singles(&mut h).await.is_empty(),
        "ordinary message notified after a lost mute update"
    );
    assert_eq!(h.fake.calls(op), 2);

    let mut mention = incoming(target, 103);
    mention.mentioned = true;
    push(&h, mention).await;
    let covered = chat(1);
    push(&h, incoming(covered, 104)).await;
    assert_eq!(singles(&mut h).await, [covered, target]);
    assert_eq!(h.fake.calls(op), 2);
    assert_eq!(h.fake.calls(Op::PeerNotifySettings(covered)), 0);
    h.stop().await;
}

#[derive(Clone, Copy, Debug)]
enum RebuildTrigger {
    Wake,
    StreamFailed,
    InterruptedRecovery,
}

#[tokio::test(start_paused = true)]
async fn wake_rechecks_a_lost_mute_for_an_unlisted_peer() {
    rebuild_rechecks_lost_mute(RebuildTrigger::Wake).await;
}

#[tokio::test(start_paused = true)]
async fn stream_failure_rechecks_a_lost_mute_for_an_unlisted_peer() {
    rebuild_rechecks_lost_mute(RebuildTrigger::StreamFailed).await;
}

#[tokio::test(start_paused = true)]
async fn interrupted_recovery_rechecks_a_lost_mute_for_an_unlisted_peer() {
    rebuild_rechecks_lost_mute(RebuildTrigger::InterruptedRecovery).await;
}

async fn rebuild_rechecks_lost_mute(trigger: RebuildTrigger) {
    let target = chat(101);
    let mut h = harness(FakeScript::demo(101, 0), None).await;
    h.online().await;
    let op = Op::PeerNotifySettings(target);
    push(&h, incoming(target, 101)).await;
    assert_eq!(singles(&mut h).await, [target]);
    assert_eq!(h.fake.calls(op), 1);

    h.fake.drop_next_update();
    mute(&h, NotifyTarget::Peer(target), Some(FOREVER)).await;
    match trigger {
        RebuildTrigger::Wake => h.internal.send(Internal::Wake).await.unwrap(),
        RebuildTrigger::StreamFailed => h.fake.stream_error(StreamError::Fatal).await,
        RebuildTrigger::InterruptedRecovery => {
            let stale = ThreadKey::whole(chat(1));
            h.send(Command::OpenThread(stale)).await;
            h.history(stale.chat, HistoryDir::Tail).await;
            h.fake
                .push_stream_item(StreamItem::HistoryStale(StaleHistory(Some(stale.chat))))
                .await;
            h.until(|e| matches!(e, Event::ThreadInvalidated(thread) if *thread == stale))
                .await;
            h.fake.stream_error(StreamError::Fatal).await;
        }
    }
    h.online().await;
    push(&h, incoming(target, 102)).await;
    assert!(singles(&mut h).await.is_empty(), "{trigger:?}");
    assert_eq!(h.fake.calls(op), 2, "{trigger:?}");
    h.stop().await;
}

#[tokio::test]
async fn cold_start_rechecks_mute_for_a_provisional_cached_dialog() {
    let dir = CacheDir::new();
    let key = CacheKey::generate().unwrap();
    let target = chat(101);
    let mut script = FakeScript::demo(101, 0);
    let last_date = unix_now();
    for row in &mut script.dialogs {
        row.last_date = last_date;
    }
    let cached = script.dialogs.last().unwrap().clone();
    assert_eq!(cached.id, target);
    {
        let cache = cache_at(dir.path(), &key);
        cache.write(CacheCmd::PutChats(vec![cached])).await;
        cache.close().await.unwrap();
    }
    script.dialogs.last_mut().unwrap().muted_until = Some(FOREVER);
    let (backend, fake) = FakeBackend::new(script);
    let first = fake.hold(Op::Dialogs, 1);
    let mut h = harness_with(backend, fake, Some(cache_at(dir.path(), &key))).await;
    h.until(|e| matches!(e, Event::DialogsReset(rows) if rows.len() == 1 && rows[0].id == target))
        .await;
    first.notify_one();
    assert!(h.online().await.iter().any(|e| matches!(e, Event::DialogsReset(rows)
        if rows.len() == 101 && rows.last().is_some_and(|r| r.id == target && r.muted_until.is_none()))));

    push(&h, incoming(target, 101)).await;
    let notices = singles(&mut h).await;
    let lookups = h.fake.calls(Op::PeerNotifySettings(target));
    h.stop().await;
    assert!(notices.is_empty());
    assert_eq!(lookups, 1);
}

#[tokio::test(start_paused = true)]
async fn a_known_gap_rechecks_a_pending_notice_for_a_provisional_dialog() {
    let target = chat(101);
    let mut script = FakeScript::demo(101, 0);
    let mut message = incoming(target, 101);
    // Keep this row in the provisional tail of the gap's first page.
    for row in &mut script.dialogs {
        row.last_date = message.message.date;
    }
    let mut h = harness(script, None).await;
    h.online().await;
    h.send(Command::LoadDialogs).await;
    h.until(|e| matches!(e, Event::DialogsAppended(rows) if rows.iter().any(|r| r.id == target)))
        .await;

    push(&h, message.clone()).await;
    h.until(|e| matches!(e, Event::ChatBumped(row) if row.id == target && row.last_id == Some(MsgId(101))))
        .await;
    h.fake.drop_next_update();
    mute(&h, NotifyTarget::Peer(target), Some(FOREVER)).await;
    h.fake.catch_up_gap().await;
    h.fake.checkpoint(99).await;
    h.until(|e| {
        matches!(e, Event::DialogsReset(rows)
        if rows.iter().any(|r| r.id == target && r.muted_until.is_none()))
    })
    .await;

    assert!(singles(&mut h).await.is_empty());
    assert_eq!(h.fake.checkpoint_pts(), 99);
    assert_eq!(h.fake.calls(Op::PeerNotifySettings(target)), 1);

    message.message.id = MsgId(102);
    message.mentioned = true;
    push(&h, message).await;
    assert_eq!(singles(&mut h).await, [target]);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_peer_lookup_dispatched_before_a_gap_is_retried() {
    let target = chat(101);
    let mut h = harness(FakeScript::demo(101, 0), None).await;
    h.online().await;
    let op = Op::PeerNotifySettings(target);
    let old = h.fake.hold_snapshot(op);
    push(&h, incoming(target, 101)).await;
    wait_for_call(&h, op, 1).await;

    h.fake.drop_next_update();
    mute(&h, NotifyTarget::Peer(target), Some(FOREVER)).await;
    h.fake.catch_up_gap().await;
    h.fake.checkpoint(99).await;
    assert!(singles(&mut h).await.is_empty());
    assert_eq!(h.fake.checkpoint_pts(), 99);

    // The old reply captured inheritance. A fresh read must see the mute.
    old.notify_one();
    assert!(singles(&mut h).await.is_empty());
    assert_eq!(h.fake.calls(op), 2);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_full_peer_lookup_queue_drops_the_new_ordinary_candidate() {
    let mut h = harness(FakeScript::demo(1, 0), None).await;
    h.online().await;
    let first = Op::PeerNotifySettings(chat(101));
    let gate = h.fake.hold_snapshot(first);
    push(&h, incoming(chat(101), 101)).await;
    wait_for_call(&h, first, 1).await;
    for id in 102..=133 {
        push(&h, incoming(chat(i64::from(id)), id)).await;
    }
    let overflow = chat(133);
    let mut mention = incoming(overflow, 134);
    mention.mentioned = true;
    push(&h, mention).await;
    // Reaching the mention proves the preceding candidates have been considered.
    let (event, _) = h.until(|e| matches!(e, Event::Notify(_))).await;
    assert!(matches!(
        event,
        Event::Notify(Notice::Message { chat, msg: MsgId(134), count: 1, .. }) if chat == overflow
    ));
    gate.notify_one();
    assert_eq!(
        singles(&mut h).await,
        (101..=132).map(chat).collect::<Vec<_>>()
    );
    assert_eq!(h.fake.calls(Op::PeerNotifySettings(overflow)), 0);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_newer_live_scope_update_wins_over_the_gap_snapshot() {
    let target = group(1);
    let mut h = harness(
        FakeScript {
            dialogs: vec![summary(target, "Group")],
            ..FakeScript::default()
        },
        None,
    )
    .await;
    h.online().await;
    let scope = NotifyScope::Groups;
    let op = Op::NotifySettings(scope);
    h.fake.drop_next_update();
    mute(&h, NotifyTarget::Scope(scope), Some(FOREVER)).await;
    h.fake.edit_dialogs(|rows| rows[0].read.unread = 1);
    let snapshot = h.fake.hold_snapshot(op);
    h.fake.catch_up_gap().await;
    h.fake.checkpoint(7).await;
    wait_for_call(&h, op, 2).await;
    mute(&h, NotifyTarget::Scope(scope), None).await;
    assert!(
        notices_for(&mut h, Duration::from_millis(100))
            .await
            .is_empty()
    );
    assert_ne!(h.fake.checkpoint_pts(), 7);
    snapshot.notify_one();
    assert_eq!(
        notices_for(&mut h, Duration::from_secs(1)).await,
        [Notice::Summary]
    );
    assert_eq!(h.fake.checkpoint_pts(), 7);
    push(&h, incoming(target, 101)).await;
    assert_eq!(singles(&mut h).await, [target]);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_notice_routes_an_unlisted_forums_general_topic() {
    let mut h = harness(FakeScript::default(), None).await;
    h.online().await;
    let chat = ChatId {
        kind: PeerKind::Channel,
        id: 99,
    };
    let mut new = incoming(chat, 42);
    new.chat = Some((
        ChatKind::Supergroup {
            forum: true,
            gigagroup: false,
        },
        "Forum".into(),
    ));
    push(&h, new).await;
    let notices = notices_for(&mut h, Duration::from_secs(3)).await;
    assert!(matches!(notices.as_slice(), [Notice::Message {
        chat: c, topic: Some(TopicId::GENERAL), msg: MsgId(42), chat_title, ..
    }] if *c == chat && chat_title == "Forum"));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn unlisted_peer_overrides_are_resolved_before_notifying() {
    let mut script = FakeScript::demo(100, 0);
    script.notify_groups = Some(FOREVER);
    let muted = chat(101);
    let unmuted = group(102);
    let inherits = chat(103);
    for (chat, until) in [(muted, Some(FOREVER)), (unmuted, Some(0)), (inherits, None)] {
        let mut row = summary(chat, "Unlisted");
        row.muted_until = until;
        script.dialogs.push(row);
    }
    let mut h = harness(script, None).await;
    let startup = h.online().await;
    assert!(startup.iter().any(|e| matches!(e, Event::DialogsReset(rows) if rows.len() == 100 && rows.iter().all(|row| row.id != muted && row.id != unmuted && row.id != inherits))));
    for (chat, id) in [
        (muted, 101),
        (unmuted, 102),
        (inherits, 103),
        (inherits, 104),
    ] {
        push(&h, incoming(chat, id)).await;
    }
    assert_eq!(singles(&mut h).await, [inherits, unmuted]);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn revoked_scope_snapshot_logs_out_before_going_online() {
    use crate::api::{AuthState, ConnState, RemoteLogout, UserError};

    let (backend, fake) = FakeBackend::new(FakeScript::demo(1, 0));
    fake.lead(
        Op::NotifySettings(NotifyScope::Groups),
        Duration::from_secs(1),
    );
    fake.inject(
        Op::NotifySettings(NotifyScope::Groups),
        Fault::Error(BackendError::Unauthorized),
    );
    let mut h = harness_with(backend, fake, None).await;
    let (_, before) = h
        .until(|e| {
            matches!(
                e,
                Event::Connection(ConnState::Online)
                    | Event::Auth(AuthState::Phone {
                        error: Some(UserError::SessionRevoked)
                    })
            )
        })
        .await;
    assert!(before.iter().any(|e| matches!(
        e,
        Event::Auth(AuthState::LoggedOut {
            remote: RemoteLogout::Revoked,
            ..
        })
    )));
    assert_eq!(h.fake.signed_out(), 0);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn migrating_scope_snapshot_rebuilds_before_going_online() {
    use crate::api::{ConnState, UserError};

    let op = Op::NotifySettings(NotifyScope::Groups);
    let (backend, fake) = FakeBackend::new(FakeScript::demo(1, 0));
    fake.inject(op, Fault::Error(BackendError::Migrated));
    let mut h = harness_with(backend, fake, None).await;
    let before = h.online().await;
    assert!(
        before
            .iter()
            .any(|e| matches!(e, Event::Error(UserError::AccountMigrated)))
    );
    assert!(
        before
            .iter()
            .any(|e| matches!(e, Event::Connection(ConnState::Rebuilding { .. })))
    );
    assert_eq!(h.fake.calls(op), 2);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn rebuild_keeps_checkpointed_notices_and_deduplicates_replay() {
    let mut h = harness(FakeScript::demo(1, 0), None).await;
    h.online().await;
    let checkpointed = incoming(chat(1), 101);
    let replayed = incoming(chat(1), 102);
    push(&h, checkpointed).await;
    push(&h, replayed.clone()).await;
    h.fake.push_checkpoint(7).await;
    let before = notices_for(&mut h, Duration::from_millis(50)).await;
    assert!(before.is_empty());
    assert_eq!(h.fake.checkpoint_pts(), 7);

    let gate = h.fake.hold(Op::Dialogs, 2);
    h.internal.send(Internal::Wake).await.unwrap();
    wait_for_call(&h, Op::Dialogs, 2).await;
    h.fake.catch_up([replayed.clone()]).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate.notify_one();
    let startup = h.online().await;
    assert!(startup.iter().all(|e| !matches!(e, Event::Notify(_))));
    let notices = notices_for(&mut h, Duration::from_secs(3)).await;
    assert!(matches!(
        notices.as_slice(),
        [Notice::Message {
            msg: MsgId(102),
            count: 2,
            ..
        }]
    ));

    h.fake.catch_up([replayed]).await;
    assert!(notices_for(&mut h, Duration::from_secs(3)).await.is_empty());
    h.stop().await;
}

async fn with_notifications(script: FakeScript) -> Harness {
    let (backend, fake) = FakeBackend::new(script);
    start(
        backend,
        fake,
        Setup {
            notifications: NotificationCleanupPolicy::Required,
            ..Setup::default()
        },
    )
    .await
}

#[tokio::test(start_paused = true)]
async fn deleting_messages_without_windows_discards_pending_notices() {
    let scoped = group(2);
    let unscoped = chat(1);
    let mut h = with_notifications(FakeScript {
        dialogs: vec![summary(scoped, "Group"), summary(unscoped, "Peer")],
        ..FakeScript::default()
    })
    .await;
    h.online().await;
    push(&h, incoming(scoped, 101)).await;
    push(&h, incoming(scoped, 102)).await;
    push(&h, incoming(unscoped, 103)).await;
    h.fake
        .push_update(Update::Deleted {
            chat: Some(scoped),
            ids: vec![MsgId(102)],
        })
        .await;
    h.fake
        .push_update(Update::Deleted {
            chat: None,
            ids: vec![MsgId(103)],
        })
        .await;
    for (scope, id) in [(Some(scoped), MsgId(102)), (None, MsgId(103))] {
        let (_, before) = h.until(|event| matches!(event, Event::NotifyDeleted { chat, ids } if *chat == scope && ids == &[id])).await;
        assert!(
            before
                .iter()
                .all(|event| !matches!(event, Event::Deleted { .. } | Event::Notify(_)))
        );
    }
    let notices = notices_for(&mut h, Duration::from_secs(3)).await;
    assert!(matches!(
        notices.as_slice(),
        [Notice::Message {
            msg: MsgId(101),
            count: 1,
            ..
        }]
    ));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn tail_reconciliation_discards_pending_notices_in_merge_and_replacement() {
    let chat = chat(1);
    let thread = ThreadKey::whole(chat);
    for replace in [false, true] {
        let history = |ids: &[i32]| {
            ids.iter()
                .map(|id| text_message(chat, *id, "body"))
                .collect()
        };
        let mut h = with_notifications(FakeScript {
            dialogs: vec![summary(chat, "Peer")],
            history: [(chat, history(&[1, 3, 4]))].into(),
            ..FakeScript::default()
        })
        .await;
        h.online().await;
        h.send(Command::OpenThread(thread)).await;
        h.until(|e| matches!(e, Event::Reconciled { thread: key, .. } if *key == thread))
            .await;
        for id in [5, 6] {
            push(&h, incoming(chat, id)).await;
        }
        h.until(|e| matches!(e, Event::ChatBumped(row) if row.last_id == Some(MsgId(6))))
            .await;
        h.fake.set_history(chat, history(&[1, 3, 4, 5, 6]));
        let ids: &[i32] = if replace {
            &[1, 2, 3, 4, 6]
        } else {
            &[1, 3, 4, 6]
        };
        h.fake.set_history(chat, history(ids));
        h.fake.catch_up_gap().await;
        h.until(|e| matches!(e, Event::NotifyDeleted { chat: Some(c), ids } if *c == chat && ids == &[MsgId(5)]))
            .await;
        h.until(
            |e| matches!(e, Event::Deleted { chat: c, ids } if *c == chat && ids == &[MsgId(5)]),
        )
        .await;
        let (_, events) = h.until(|e| matches!(e, Event::Reconciled { .. })).await;
        assert_eq!(
            events.iter().any(|e| matches!(
                e,
                Event::History {
                    dir: HistoryDir::Tail,
                    ..
                }
            )),
            replace,
        );
        assert!(matches!(
            notices_for(&mut h, Duration::from_secs(3)).await.as_slice(),
            [Notice::Message {
                msg: MsgId(6),
                count: 1,
                ..
            }]
        ));
        h.stop().await;
    }
}

#[tokio::test(start_paused = true)]
async fn an_authoritative_tail_after_rebuild_discards_a_notice_without_cached_history() {
    let peer = chat(1);
    let thread = ThreadKey::whole(peer);
    let mut h = harness(FakeScript::demo(1, 4), None).await;
    h.online().await;
    h.send(Command::OpenThread(thread)).await;
    h.until(|e| matches!(e, Event::Reconciled { thread: key, .. } if *key == thread))
        .await;
    for id in [5, 6] {
        push(&h, incoming(peer, id)).await;
    }
    h.until(|e| matches!(e, Event::ChatBumped(row) if row.last_id == Some(MsgId(6))))
        .await;
    h.fake.set_history(
        peer,
        vec![incoming(peer, 4).message, incoming(peer, 5).message],
    );
    h.internal.send(Internal::Wake).await.unwrap();
    h.until(|e| matches!(e, Event::Reconciled { thread: key, .. } if *key == thread))
        .await;
    assert!(matches!(
        notices_for(&mut h, Duration::from_secs(3)).await.as_slice(),
        [Notice::Message {
            msg: MsgId(5),
            count: 1,
            ..
        }]
    ));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn an_in_flight_tail_keeps_a_newer_notice_contributor() {
    let peer = chat(1);
    let thread = ThreadKey::whole(peer);
    let mut h = harness(FakeScript::demo(1, 4), None).await;
    h.online().await;
    for id in [5, 6] {
        push(&h, incoming(peer, id)).await;
    }
    h.until(|e| matches!(e, Event::ChatBumped(row) if row.last_id == Some(MsgId(6))))
        .await;
    h.fake.set_history(
        peer,
        vec![incoming(peer, 4).message, incoming(peer, 5).message],
    );
    let tail = h.fake.hold(Op::History, 1);
    h.send(Command::OpenThread(thread)).await;
    wait_for_call(&h, Op::History, 1).await;
    push(&h, incoming(peer, 7)).await;
    h.until(|e| matches!(e, Event::ChatBumped(row) if row.last_id == Some(MsgId(7))))
        .await;
    tail.notify_one();
    h.until(|e| matches!(e, Event::Reconciled { thread: key, .. } if *key == thread))
        .await;
    assert!(matches!(
        notices_for(&mut h, Duration::from_secs(3)).await.as_slice(),
        [Notice::Message {
            msg: MsgId(7),
            count: 2,
            ..
        }]
    ));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn reading_one_topic_keeps_other_contributors_to_its_chat_notice() {
    let channel = ChatId {
        kind: PeerKind::Channel,
        id: 99,
    };
    let mut row = summary(channel, "Forum");
    row.kind = ChatKind::Supergroup {
        forum: true,
        gigagroup: false,
    };
    let mut h = with_notifications(FakeScript {
        dialogs: vec![row],
        ..FakeScript::default()
    })
    .await;
    h.online().await;
    let general = TopicId::GENERAL;
    let another = TopicId::try_from(2).unwrap();
    for (topic, id) in [(general, 101), (general, 102), (another, 103)] {
        let mut new = incoming(channel, id);
        new.message.thread.topic = Some(topic);
        push(&h, new).await;
    }
    for (topic, max_id) in [(general, MsgId(101)), (another, MsgId(103))] {
        h.fake
            .push_update(Update::ReadTopicMax {
                chat: channel,
                topic,
                max_id,
            })
            .await;
    }
    let notices = notices_for(&mut h, Duration::from_secs(3)).await;
    assert!(
        matches!(notices.as_slice(), [Notice::Message { chat, topic: Some(topic), msg: MsgId(102), count: 1, .. }] if *chat == channel && *topic == general)
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn crossing_peer_mute_updates_win_and_confirmed_inheritance_is_reused() {
    let target = chat(101);
    let mut h = harness(FakeScript::demo(101, 0), None).await;
    h.online().await;
    let op = Op::PeerNotifySettings(target);
    h.fake.inject(op, Fault::Delay(Duration::from_secs(1)));
    push(&h, incoming(target, 101)).await;
    wait_for_call(&h, op, 1).await;
    mute(&h, NotifyTarget::Peer(target), Some(FOREVER)).await;
    assert!(singles(&mut h).await.is_empty());

    mute(&h, NotifyTarget::Peer(target), None).await;
    push(&h, incoming(target, 102)).await;
    assert_eq!(singles(&mut h).await, [target]);
    assert_eq!(h.fake.calls(op), 1);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_delayed_mute_lookup_respects_the_mention_banners_cooldown() {
    let target = chat(101);
    let mut h = harness(FakeScript::demo(101, 0), None).await;
    h.online().await;
    let op = Op::PeerNotifySettings(target);
    let gate = h.fake.hold(op, 1);
    push(&h, incoming(target, 101)).await;
    wait_for_call(&h, op, 1).await;
    let mut mention = incoming(target, 102);
    mention.mentioned = true;
    push(&h, mention).await;
    let (first, _) = h.until(|e| matches!(e, Event::Notify(_))).await;
    assert!(matches!(
        first,
        Event::Notify(Notice::Message {
            msg: MsgId(102),
            ..
        })
    ));
    gate.notify_one();
    assert!(
        notices_for(&mut h, Duration::from_millis(1900))
            .await
            .is_empty()
    );
    assert!(matches!(
        notices_for(&mut h, Duration::from_millis(200))
            .await
            .as_slice(),
        [Notice::Message {
            msg: MsgId(101),
            ..
        }]
    ));
    h.stop().await;
}
