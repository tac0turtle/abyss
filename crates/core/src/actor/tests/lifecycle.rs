//! Client lifecycle tests (plan section 2): rebuild triggers, retirement order,
//! bootstrap merging, refresh policy, sends across rebuilds, shutdown.

use std::collections::BTreeMap;

use tokio::time::Instant;

use super::{Harness, chat, harness, harness_with, history_ids};
use crate::actor::jobs::{DialogsStep, Done, Outcome};
use crate::actor::{Actor, ActorInit, Internal, MAX_CONCURRENT};
use crate::api::WindowState;
use crate::api::{ClientGen, Command, ConnState, Event, HistoryDir, SendState, UserError};
use crate::backend::fake::{
    BuildFault, FakeBackend, FakeScript, Fault, Op, QuitBehavior, text_message,
};
use crate::backend::{
    BackendError, DialogsPage, HistoryPage, HistoryQuery, RejectReason, SessionSeed, StreamError,
    Update,
};
use crate::events;
use crate::model::{LocalId, Message, MsgId, ThreadKey};

use std::sync::Arc;
use std::time::Duration;

fn rebuilding(e: &Event) -> bool {
    matches!(e, Event::Connection(ConnState::Rebuilding { .. }))
}

/// After a trigger: exactly one rebuild, after at least the first backoff step,
/// with the old runner finished and its instance retired.
async fn assert_one_rebuild(h: &mut Harness, builds_before: usize) {
    let (ev, _) = h.until(rebuilding).await;
    assert!(matches!(
        ev,
        Event::Connection(ConnState::Rebuilding { attempt: 1 })
    ));
    let started = Instant::now();
    h.online().await;
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "rebuild must wait for backoff"
    );
    assert_eq!(h.fake.builds(), builds_before + 1);
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(h.fake.builds(), builds_before + 1, "exactly one rebuild");
    assert_eq!(h.fake.runners_finished(), 1);
    assert_eq!(h.fake.retired(), 1);
    assert!(!h.drain().iter().any(rebuilding));
}

#[derive(Clone, Copy, Debug)]
enum Trigger {
    Wake,
    RunnerExit,
    RunnerPanic,
    StreamFatal,
    StreamPersistent,
    StreamPanic,
    Stall,
    Migrated,
}

async fn fire(h: &mut Harness, trigger: Trigger) {
    match trigger {
        Trigger::Wake => h.internal.send(Internal::Wake).await.unwrap(),
        Trigger::RunnerExit => h.fake.runner_exit(),
        Trigger::RunnerPanic => h.fake.runner_panic(),
        Trigger::StreamFatal => h.fake.stream_error(StreamError::Fatal).await,
        Trigger::StreamPersistent => {
            for _ in 0..4 {
                h.fake.stream_error(StreamError::Transient).await;
            }
        }
        Trigger::StreamPanic => h.fake.stream_panic().await,
        Trigger::Stall => {
            for i in 1..=3 {
                h.fake.inject(Op::Read, Fault::Hang);
                h.send(Command::MarkRead {
                    thread: ThreadKey::whole(chat(i)),
                    max_id: MsgId(1),
                })
                .await;
            }
        }
        Trigger::Migrated => {
            h.fake
                .inject(Op::History, Fault::Error(BackendError::Migrated));
            h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn each_trigger_causes_exactly_one_rebuild_with_backoff() {
    for trigger in [
        Trigger::Wake,
        Trigger::RunnerExit,
        Trigger::RunnerPanic,
        Trigger::StreamFatal,
        Trigger::StreamPersistent,
        Trigger::StreamPanic,
        Trigger::Stall,
        Trigger::Migrated,
    ] {
        let mut h = harness(FakeScript::demo(2, 10), None).await;
        h.online().await;
        fire(&mut h, trigger).await;
        assert_one_rebuild(&mut h, 1).await;
        h.stop().await;
    }
}

#[tokio::test(start_paused = true)]
async fn recoverable_stream_errors_retry_on_the_same_stream() {
    let mut h = harness(FakeScript::demo(2, 10), None).await;
    h.online().await;
    for _ in 0..3 {
        h.fake.stream_error(StreamError::Transient).await;
    }
    h.fake
        .push_update(Update::NewMessage(text_message(chat(2), 11, "x")))
        .await;
    h.until(|e| matches!(e, Event::ChatBumped(s) if s.id == chat(2)))
        .await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(h.fake.builds(), 1, "no rebuild for errors that clear");
    assert!(!h.drain().iter().any(rebuilding));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn baseline_timeout_rebuilds() {
    let (backend, fake) = FakeBackend::new(FakeScript::demo(2, 10));
    fake.on_next_build(BuildFault::NoBaseline);
    let mut h = harness_with(backend, fake, None).await;
    let started = Instant::now();
    let (_, skipped) = h.until(rebuilding).await;
    assert!(started.elapsed() >= Duration::from_secs(10));
    assert!(
        !skipped.iter().any(|e| matches!(
            e,
            Event::DialogsReset(_) | Event::Connection(ConnState::Online)
        )),
        "no snapshot before the baseline"
    );
    assert_eq!(h.fake.calls(Op::Dialogs), 0);
    h.online().await;
    assert_eq!(h.fake.builds(), 2);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn failed_builds_go_offline_and_back_off() {
    let (backend, fake) = FakeBackend::new(FakeScript::demo(2, 10));
    fake.on_next_build(BuildFault::Fail(BackendError::Transient));
    fake.on_next_build(BuildFault::Fail(BackendError::Transient));
    let mut h = harness_with(backend, fake, None).await;
    let started = Instant::now();
    let skipped = h.online().await;
    let offline = skipped
        .iter()
        .filter(|e| matches!(e, Event::Connection(ConnState::Offline)))
        .count();
    assert_eq!(offline, 2);
    assert!(started.elapsed() >= Duration::from_secs(3), "1 s then 2 s");
    assert_eq!(h.fake.builds(), 3);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn second_migration_halts_offline() {
    let mut h = harness(FakeScript::demo(2, 10), None).await;
    h.online().await;
    fire(&mut h, Trigger::Migrated).await;
    h.until(|e| matches!(e, Event::Error(UserError::AccountMigrated)))
        .await;
    h.online().await;
    fire(&mut h, Trigger::Migrated).await;
    h.until(|e| matches!(e, Event::Error(UserError::AccountMigrated)))
        .await;
    h.until(|e| matches!(e, Event::Connection(ConnState::Offline)))
        .await;
    tokio::time::sleep(Duration::from_secs(300)).await;
    // Task 6c: no signed-in rebuild; the account is torn down and one
    // signed-out instance waits for a new login.
    assert_eq!(h.fake.builds(), 3, "no rebuild after a repeated migration");
    assert!(h.fake.seeds().last().unwrap().session.is_stored());
    assert_eq!(h.fake.runners_finished(), 2);
    h.stop().await;
}

/// Sixteen calls time out, so the permits are parked; the third time-out is a
/// stall and rebuilds. The runner takes 4 s to quit, and no permit may return
/// before it has. The timed-out reads are sent again on the new instance.
/// Then retried reads, whose time-outs the stall window doesn't count, park
/// every permit: that rebuilds too, once, and their reads land after it.
#[tokio::test(start_paused = true)]
async fn parked_permits_recycle_only_after_the_runner_completes() {
    let mut h = harness(FakeScript::default(), None).await;
    h.online().await;
    h.fake.set_quit(QuitBehavior::Slow(Duration::from_secs(4)));
    let total = MAX_CONCURRENT + 4;
    for _ in 0..MAX_CONCURRENT {
        h.fake.inject(Op::Read, Fault::Hang);
    }
    for i in 0..total {
        let id = i64::try_from(i).unwrap() + 1;
        h.send(Command::MarkRead {
            thread: ThreadKey::whole(chat(id)),
            max_id: MsgId(1),
        })
        .await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(h.fake.calls(Op::Read), MAX_CONCURRENT, "17th call waits");
    assert_eq!(h.fake.max_in_flight(), MAX_CONCURRENT);
    h.until(rebuilding).await;
    let triggered = Instant::now();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(h.fake.runners_finished(), 0, "runner still quitting");
    assert_eq!(
        h.permits.available_permits(),
        0,
        "no permit before retirement"
    );
    assert_eq!(h.fake.calls(Op::Read), MAX_CONCURRENT);
    h.online().await;
    assert!(triggered.elapsed() >= Duration::from_secs(4));
    assert_eq!(h.fake.runners_finished(), 1);
    tokio::time::sleep(Duration::from_secs(1)).await;
    // The rest, and the timed-out reads again: every read lands.
    let landed: std::collections::HashSet<ThreadKey> =
        h.fake.read_calls().into_iter().map(|(t, _)| t).collect();
    assert_eq!(landed.len(), total, "recycled permits admit the rest");
    assert_eq!(h.permits.available_permits(), MAX_CONCURRENT);

    // Retried reads (their time-outs don't count for the stall window)
    // time out until every permit is parked: that alone rebuilds, once.
    for fault in [Fault::Error(BackendError::Transient), Fault::Hang] {
        for _ in 0..MAX_CONCURRENT {
            h.fake.inject(Op::Read, fault);
        }
    }
    let retried: Vec<ThreadKey> = (0..MAX_CONCURRENT)
        .map(|i| ThreadKey::whole(chat(i64::try_from(i).unwrap() + 100)))
        .collect();
    for thread in &retried {
        h.send(Command::MarkRead {
            thread: *thread,
            max_id: MsgId(1),
        })
        .await;
    }
    h.until(rebuilding).await;
    assert_eq!(h.permits.available_permits(), 0, "every permit parked");
    h.online().await;
    assert_eq!(h.fake.runners_finished(), 2);
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(h.permits.available_permits(), MAX_CONCURRENT);
    assert_eq!(h.fake.builds(), 3, "no further rebuild");
    let landed: std::collections::HashSet<ThreadKey> =
        h.fake.read_calls().into_iter().map(|(t, _)| t).collect();
    assert!(retried.iter().all(|t| landed.contains(t)), "{landed:?}");
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn runner_ignoring_quit_is_aborted_and_awaited() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    h.fake.set_quit(QuitBehavior::Ignore);
    h.internal.send(Internal::Wake).await.unwrap();
    h.until(rebuilding).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(h.fake.runners_finished(), 0);
    assert_eq!(h.fake.builds(), 1, "no build before retirement completes");
    h.online().await;
    assert_eq!(
        h.fake.runners_finished(),
        1,
        "abort completed before the build"
    );
    assert_eq!(h.fake.builds(), 2);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn rebuild_carries_the_live_session_and_peers() {
    let mut h = harness(FakeScript::demo(3, 10), None).await;
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(2)))).await;
    h.history(chat(2), HistoryDir::Tail).await;
    h.fake.inject(Op::Send, Fault::Hang);
    h.send(Command::SendText {
        thread: ThreadKey::whole(chat(1)),
        text: "hi".into(),
        local_id: LocalId(1),
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    h.internal.send(Internal::Wake).await.unwrap();
    h.online().await;
    let seeds = h.fake.seeds();
    assert_eq!(seeds.len(), 2);
    assert!(seeds[0].session.is_stored());
    let SessionSeed::Live(snapshot) = &seeds[1].session else {
        panic!("the retired instance's snapshot, not storage");
    };
    // Each fake instance makes one critical write; `rev` continues across the
    // rebuild because both instances share the account's critical cell.
    assert_eq!(snapshot.rev().unwrap(), 2);
    assert_eq!(h.fake.stored_loads(), 1);
    // A write arriving on the retired instance after retirement is dropped.
    assert!(h.fake.late_critical_write().await);
    assert_eq!(snapshot.rev().unwrap(), 2);
    // The peers come with the session: core hands the rebuilt instance the
    // open window and the pending send's chat to keep pinned.
    let peers = h
        .fake
        .retained_peers()
        .expect("core reconciled the rebuilt instance");
    assert!(peers.contains(&chat(2)) && peers.contains(&chat(1)));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn stale_generation_completions_are_dropped() {
    let (backend, _fake) = FakeBackend::new(FakeScript::demo(2, 10));
    let (_cmd, commands) = tokio::sync::mpsc::channel(1);
    let (_internal, internal) = tokio::sync::mpsc::channel(1);
    let (events_tx, mut events) = events::channel();
    let persist = crate::session::PersistHandle::spawn(crate::secret::MemoryStore::new()).unwrap();
    let account = persist.fresh_session();
    let (mut actor, _channels) = Actor::new(ActorInit {
        backend: Arc::new(backend),
        events: events_tx,
        cache: None,
        commands,
        internal,
        cancel: tokio_util::sync::CancellationToken::new(),
        persist,
        account,
        cache_parts: None,
        pending_logout: None,
    });
    let view = actor.windows.open(ThreadKey::whole(chat(1)));
    let stale = ClientGen(actor.cgen.0 - 1);
    let page = DialogsPage {
        chats: vec![crate::backend::fake::summary(chat(1), "c")],
        next: None,
    };
    actor
        .on_done(Done {
            cgen: stale,
            parked: None,
            outcome: Outcome::Dialogs {
                step: DialogsStep::First,
                stamps: super::super::jobs::Stamps::default(),
                res: Ok(page),
            },
        })
        .await;
    let messages = vec![text_message(chat(1), 1, "x")];
    actor
        .on_done(Done {
            cgen: stale,
            parked: None,
            outcome: Outcome::History {
                load: crate::actor::jobs::Load {
                    thread: ThreadKey::whole(chat(1)),
                    view,
                    req: crate::actor::jobs::ReqId(1),
                },
                dir: HistoryDir::Tail,
                query: HistoryQuery::Tail,
                floor: None,
                since: actor.windows.continuity_mark(),
                res: Ok(HistoryPage {
                    messages,
                    has_more: false,
                }),
            },
        })
        .await;
    assert!(
        events.try_recv().is_none(),
        "stale completions emit nothing"
    );
    assert!(actor.dialogs.is_empty());
    assert!(
        actor
            .windows
            .get(ThreadKey::whole(chat(1)))
            .unwrap()
            .messages()
            .is_empty()
    );
}

#[tokio::test(start_paused = true)]
async fn in_flight_call_of_a_retired_instance_never_reaches_the_ui() {
    let mut script = FakeScript::demo(3, 10);
    script.latency = Duration::from_secs(1);
    let mut h = harness(script, None).await;
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.internal.send(Internal::Wake).await.unwrap();
    let (_, before) = h
        .until(|e| matches!(e, Event::ThreadInvalidated(c) if *c == ThreadKey::whole(chat(1))))
        .await;
    assert!(
        !before.iter().any(|e| matches!(e, Event::History { .. })),
        "the old instance's history load leaked"
    );
    let ev = h.history(chat(1), HistoryDir::Tail).await;
    assert_eq!(
        history_ids(&ev).len(),
        10,
        "bootstrap refetches the open tail"
    );
    h.stop().await;
}

/// The window as the UI would rebuild it from events, with each message's
/// edit date.
fn fold(events: &[Event]) -> BTreeMap<i32, Option<i64>> {
    let mut window = BTreeMap::new();
    for ev in events {
        match ev {
            Event::ThreadInvalidated(_) => window.clear(),
            Event::History {
                dir: HistoryDir::Tail,
                messages,
                removed,
                ..
            } => {
                window.clear();
                apply_page(&mut window, messages, removed);
            }
            Event::History {
                messages, removed, ..
            } => apply_page(&mut window, messages, removed),
            Event::Edited(m) => {
                if let Some(slot) = window.get_mut(&m.id.0) {
                    *slot = m.edit_date;
                }
            }
            Event::Deleted { ids, .. } => {
                for id in ids {
                    window.remove(&id.0);
                }
            }
            _ => {}
        }
    }
    window
}

fn apply_page(window: &mut BTreeMap<i32, Option<i64>>, messages: &[Message], removed: &[MsgId]) {
    for m in messages {
        window.insert(m.id.0, m.edit_date);
    }
    for id in removed {
        window.remove(&id.0);
    }
}

fn edited(id: i32, edit_date: i64) -> Message {
    let mut m = text_message(chat(1), id, "edited");
    m.edit_date = Some(edit_date);
    m
}

fn deleted(id: i32) -> Update {
    Update::Deleted {
        chat: Some(chat(1)),
        ids: vec![MsgId(id)],
    }
}

/// Opens chat 1 (ids 1..=10), then sets up what the next instance's snapshots
/// return: ids 1..=11 with 5 edited at 100, the tail after 2 s and the dialogs
/// after 4 s.
async fn open_with_slow_snapshots(h: &mut Harness) {
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h.history(chat(1), HistoryDir::Tail).await;
    let server = (1..=11).map(|i| match i {
        5 => edited(5, 100),
        _ => text_message(chat(1), i, "m"),
    });
    h.fake.set_history(chat(1), server.collect());
    h.fake
        .inject(Op::History, Fault::Delay(Duration::from_secs(2)));
    h.fake
        .inject(Op::Dialogs, Fault::Delay(Duration::from_secs(4)));
}

/// Updates arriving before, during and after the snapshot fetch of a bootstrap
/// are neither lost nor overwritten by the (older) snapshots, and a deletion is
/// never resurrected.
#[tokio::test(start_paused = true)]
async fn bootstrap_merges_updates_around_the_snapshots() {
    let mut h = harness(FakeScript::demo(2, 10), None).await;
    open_with_slow_snapshots(&mut h).await;
    h.internal.send(Internal::Wake).await.unwrap();
    h.until(rebuilding).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Before the snapshots: queued for the new stream.
    let new = text_message(chat(1), 12, "new");
    h.fake.push_update(Update::NewMessage(new)).await;
    h.fake.push_update(deleted(7)).await;
    let (invalidated, _) = h
        .until(|e| matches!(e, Event::ThreadInvalidated(c) if *c == ThreadKey::whole(chat(1))))
        .await;
    let baseline = Instant::now();
    // During the tail fetch.
    tokio::time::sleep(Duration::from_millis(500)).await;
    h.fake.push_update(Update::Edited(edited(3, 200))).await;
    h.fake.push_update(Update::Edited(edited(5, 50))).await;
    // After the tail snapshot, before the dialogs snapshot.
    tokio::time::sleep_until(baseline + Duration::from_secs(3)).await;
    let dup = text_message(chat(1), 11, "m");
    h.fake.push_update(Update::NewMessage(dup)).await;
    h.fake.push_update(deleted(8)).await;
    h.fake.push_update(Update::Edited(edited(4, 300))).await;
    let mut events = vec![invalidated];
    events.extend(h.online().await);
    tokio::time::sleep(Duration::from_secs(1)).await;
    events.extend(h.drain());
    let is_tail = |e: &&Event| {
        matches!(
            e,
            Event::History {
                dir: HistoryDir::Tail,
                ..
            }
        )
    };
    let tail = events.iter().find(is_tail).unwrap();
    assert!(
        !history_ids(tail).contains(&7),
        "tombstone filters the snapshot"
    );
    let edits = [(3, 200), (4, 300), (5, 100)];
    let expected: BTreeMap<i32, Option<i64>> = [1, 2, 3, 4, 5, 6, 9, 10, 11, 12]
        .into_iter()
        .map(|id| (id, edits.iter().find(|e| e.0 == id).map(|e| e.1)))
        .collect();
    assert_eq!(fold(&events), expected);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn refresh_on_activation_and_every_five_minutes() {
    let mut h = harness(FakeScript::demo(2, 10), None).await;
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h.history(chat(1), HistoryDir::Tail).await;
    h.send(Command::Window(WindowState::Inactive)).await;
    let (dialogs, history) = (h.fake.calls(Op::Dialogs), h.fake.calls(Op::History));
    tokio::time::sleep(Duration::from_mins(6)).await;
    assert_eq!(
        h.fake.calls(Op::Dialogs),
        dialogs,
        "no refresh while inactive"
    );
    // The server changed meanwhile: 4 deleted, 2 edited, 11 new.
    let mut server: Vec<Message> = (1..=11)
        .filter(|i| *i != 4)
        .map(|i| text_message(chat(1), i, "m"))
        .collect();
    server[1] = edited(2, 50);
    h.fake.set_history(chat(1), server);
    h.drain();
    h.send(Command::Window(WindowState::Active)).await;
    let (_, skipped) = h
        .until(|e| {
            matches!(
                e,
                Event::History {
                    dir: HistoryDir::Newer,
                    ..
                }
            )
        })
        .await;
    assert_eq!(h.fake.calls(Op::Dialogs), dialogs + 1);
    assert_eq!(h.fake.calls(Op::History), history + 1);
    assert!(skipped.iter().any(|e| matches!(e, Event::DialogsReset(_))));
    assert!(
        skipped
            .iter()
            .any(|e| matches!(e, Event::Deleted { ids, .. } if ids == &vec![MsgId(4)]))
    );
    assert!(
        skipped
            .iter()
            .any(|e| matches!(e, Event::Edited(m) if m.id == MsgId(2)))
    );
    assert!(
        !skipped.iter().any(|e| matches!(
            e,
            Event::History {
                dir: HistoryDir::Tail,
                ..
            }
        )),
        "a refresh merges; it does not reset the window"
    );
    tokio::time::sleep(Duration::from_secs(5 * 60 + 1)).await;
    assert_eq!(h.fake.calls(Op::Dialogs), dialogs + 2, "periodic refresh");
    assert_eq!(h.fake.builds(), 1, "refresh never rebuilds");
    h.stop().await;
}

fn send_settled(id: LocalId) -> impl Fn(&Event) -> bool {
    move |e| matches!(e, Event::SendState { local_id, state } if *local_id == id && *state != SendState::Sending)
}

#[tokio::test(start_paused = true)]
async fn pending_send_replays_with_the_same_random_id_across_a_rebuild() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    h.fake.inject(Op::Send, Fault::Hang);
    h.send(Command::SendText {
        thread: ThreadKey::whole(chat(1)),
        text: "hi".into(),
        local_id: LocalId(1),
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(h.fake.send_attempts().len(), 1);
    h.internal.send(Internal::Wake).await.unwrap();
    let (ev, _) = h.until(send_settled(LocalId(1))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::Sent { .. },
            ..
        }
    ));
    let attempts = h.fake.send_attempts();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0], attempts[1], "same thread and random_id");
    assert_eq!(h.fake.sent(), vec![attempts[0]]);
    assert_eq!(h.fake.builds(), 2);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn discard_removes_failed_sends_but_not_in_flight_ones() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    h.fake.inject(
        Op::Send,
        Fault::Error(BackendError::Rejected(RejectReason::Other)),
    );
    h.send(Command::SendText {
        thread: ThreadKey::whole(chat(1)),
        text: "hi".into(),
        local_id: LocalId(1),
    })
    .await;
    let (ev, _) = h.until(send_settled(LocalId(1))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::Failed,
            ..
        }
    ));
    h.fake.inject(Op::Send, Fault::Hang);
    h.send(Command::SendText {
        thread: ThreadKey::whole(chat(1)),
        text: "slow".into(),
        local_id: LocalId(2),
    })
    .await;
    h.send(Command::DiscardSend(LocalId(2))).await;
    h.until(|e| matches!(e, Event::Error(UserError::Busy)))
        .await;
    h.send(Command::DiscardSend(LocalId(1))).await;
    h.send(Command::RetrySend(LocalId(1))).await;
    let (_, skipped) = h
        .until(|e| matches!(e, Event::Error(UserError::NotFound)))
        .await;
    assert!(!skipped.iter().any(|e| matches!(e, Event::Error(_))));
    assert_eq!(h.fake.calls(Op::Send), 2, "a discarded send is not retried");
    h.stop().await;
}

/// The UI stops draining; core fills the event queue and blocks in `emit`.
/// Cancellation must still stop it, including retiring the instance.
#[tokio::test(start_paused = true)]
async fn shutdown_completes_while_the_event_queue_is_full() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    let fake = h.fake.clone();
    let pusher = tokio::spawn(async move {
        for i in 0..3000 {
            let still_unread = u32::try_from(i).unwrap();
            fake.push_update(Update::ReadInbox {
                chat: chat(1),
                topic: None,
                still_unread,
            })
            .await;
        }
    });
    tokio::time::sleep(Duration::from_secs(5)).await;
    let Harness {
        cancel,
        join,
        mut events,
        fake,
        ..
    } = h;
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("shutdown blocked on the event queue")
        .unwrap();
    pusher.abort();
    assert_eq!(fake.runners_finished(), 1, "the instance was retired");
    let queued = std::iter::from_fn(|| events.try_recv()).count();
    assert!(
        queued >= crate::events::EVENT_CAPACITY - 1,
        "queue was full: {queued}"
    );
}
