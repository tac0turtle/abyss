//! Regression tests for the task 4b review findings (plan section 15, rounds
//! 4 to 6). Each one fails on the code before the fix.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::{CacheDir, Harness, cache_at, chat, harness, history_ids, session};
use crate::actor::jobs::{Done, Job, Outcome};
use crate::actor::{Actor, ActorInit, Internal};
use crate::api::WindowState;
use crate::api::{AuthState, Command, ConnState, Event, HistoryDir, SendState, UserError};
use crate::backend::fake::{FakeBackend, FakeScript, Fault, Op, text_message};
use crate::backend::{BackendError, HistoryPage, HistoryQuery, StreamError, Update};
use crate::cache::CacheKey;
use crate::events::{self, EventReceiver};
use crate::model::{ChatId, LocalId, Message, MsgId, ThreadKey};
use crate::session::MAX_PINNED_PEERS;

const FAIL_FAST: Duration = Duration::from_secs(120);

fn rebuilding(e: &Event) -> bool {
    matches!(e, Event::Connection(ConnState::Rebuilding { .. }))
}

fn tail_of(c: ChatId) -> impl Fn(&Event) -> bool {
    move |e| matches!(e, Event::History { thread, dir: HistoryDir::Tail, .. } if *thread == ThreadKey::whole(c))
}

fn state_of(id: LocalId) -> impl Fn(&Event) -> bool {
    move |e| matches!(e, Event::SendState { local_id, state } if *local_id == id && *state != SendState::Sending)
}

async fn send_text(h: &Harness, c: ChatId, id: u64) {
    h.send(Command::SendText {
        thread: ThreadKey::whole(c),
        text: "hi".into(),
        local_id: LocalId(id),
    })
    .await;
}

/// Rebuilds and waits for `Online`; returns every event on the way.
async fn wake_and_settle(h: &mut Harness) -> Vec<Event> {
    h.internal.send(Internal::Wake).await.unwrap();
    let (ev, mut seen) = h.until(rebuilding).await;
    seen.push(ev);
    seen.extend(h.online().await);
    seen
}

/// Finding 1 (high): reopening the bootstrap's tail chat while its snapshot
/// is in flight used to orphan the snapshot, so `Online` never came.
#[tokio::test(start_paused = true)]
async fn reopening_the_tail_chat_during_bootstrap_still_goes_online() {
    let mut h = harness(FakeScript::demo(3, 10), None).await;
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h.until(tail_of(chat(1))).await;
    h.fake
        .inject(Op::History, Fault::Delay(Duration::from_secs(3)));
    h.internal.send(Internal::Wake).await.unwrap();
    h.until(|e| matches!(e, Event::ThreadInvalidated(c) if *c == ThreadKey::whole(chat(1))))
        .await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(2)))).await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    let mut events = tokio::time::timeout(FAIL_FAST, h.online())
        .await
        .expect("bootstrap deadlocked: Online never came");
    tokio::time::sleep(Duration::from_secs(5)).await;
    events.extend(h.drain());
    let last_tail = events.iter().rfind(|e| tail_of(chat(1))(e)).unwrap();
    assert_eq!(history_ids(last_tail), (1..=10).collect::<Vec<_>>());
    h.stop().await;
}

/// Finding 2: a refresh removes a deleted newest message, and everything
/// when the server's history is empty.
#[tokio::test(start_paused = true)]
async fn refresh_removes_a_deleted_newest_message_and_empty_history() {
    let mut h = harness(FakeScript::demo(2, 10), None).await;
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h.until(tail_of(chat(1))).await;
    let server: Vec<Message> = (1..=9).map(|i| text_message(chat(1), i, "m")).collect();
    h.fake.set_history(chat(1), server);
    tokio::time::sleep(Duration::from_secs(16)).await;
    h.send(Command::Window(WindowState::Active)).await;
    let (ev, _) = h.until(|e| matches!(e, Event::Deleted { .. })).await;
    assert!(matches!(ev, Event::Deleted { ids, .. } if ids == vec![MsgId(10)]));
    let (ev, _) = h.until(|e| matches!(e, Event::Reconciled { .. })).await;
    assert!(matches!(
        ev,
        Event::Reconciled {
            newest: Some(MsgId(9)),
            ..
        }
    ));

    h.fake.set_history(chat(1), Vec::new());
    tokio::time::sleep(Duration::from_secs(16)).await;
    h.send(Command::Window(WindowState::Active)).await;
    let (ev, _) = h.until(|e| matches!(e, Event::Deleted { .. })).await;
    let expected: Vec<MsgId> = (1..=9).map(MsgId).collect();
    assert!(matches!(ev, Event::Deleted { ids, .. } if ids == expected));
    h.stop().await;
}

/// Finding 2: cached messages the server no longer has are dropped by the
/// server tail, including the newest one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_newest_message_deleted_on_the_server_is_dropped() {
    let dir = CacheDir::new();
    let key = CacheKey::generate().unwrap();
    session(FakeScript::demo(1, 60), cache_at(dir.path(), &key), 0).await;
    let mut script = FakeScript::demo(1, 60);
    script.history.get_mut(&chat(1)).unwrap().pop();
    let cache = cache_at(dir.path(), &key);
    let mut h = harness(script, Some(cache.clone())).await;
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    let (cached, _) = h.until(tail_of(chat(1))).await;
    assert_eq!(history_ids(&cached).last(), Some(&60), "the cache held 60");
    let (server, _) = h.until(tail_of(chat(1))).await;
    assert_eq!(history_ids(&server).last(), Some(&59));
    h.stop().await;
    cache.barrier().await.unwrap();
    let tail = cache.tail(ThreadKey::whole(chat(1)), 50).await.unwrap();
    assert_eq!(tail.last().map(|m| m.id), Some(MsgId(59)));
}

/// Finding 3: two messages in the same second, one reflected by the dialogs
/// snapshot and one newer: the newer one still bumps and counts.
#[tokio::test(start_paused = true)]
async fn same_second_message_during_bootstrap_still_bumps_and_counts() {
    let mut script = FakeScript::demo(2, 10);
    script.dialogs[1].last_id = Some(MsgId(10));
    script.dialogs[1].last_date = 100;
    let mut h = harness(script, None).await;
    h.online().await;
    h.fake
        .inject(Op::Dialogs, Fault::Delay(Duration::from_secs(4)));
    h.internal.send(Internal::Wake).await.unwrap();
    h.until(rebuilding).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    for id in [10, 11] {
        let mut m = text_message(chat(2), id, "same second");
        m.date = 100;
        h.fake.push_update(Update::NewMessage(m)).await;
    }
    let mut events = h.online().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    events.extend(h.drain());
    let bumps: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::ChatBumped(s) if s.id == chat(2) => Some((s.last_id, s.unread)),
            _ => None,
        })
        .collect();
    assert_eq!(bumps, vec![(Some(MsgId(11)), 1)]);
    h.stop().await;
}

/// Finding 5: a flood wait holds the send and resends it with the same
/// `random_id`, without consuming an attempt.
#[tokio::test(start_paused = true)]
async fn flood_wait_send_is_held_then_resent_with_the_same_random_id() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    h.fake.inject(
        Op::Send,
        Fault::Error(BackendError::FloodWait { seconds: 30 }),
    );
    send_text(&h, chat(1), 1).await;
    let (ev, _) = h.until(state_of(LocalId(1))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::FloodWait { seconds: 30 },
            ..
        }
    ));
    let held = Instant::now();
    let (ev, _) = h.until(state_of(LocalId(1))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::Sent { .. },
            ..
        }
    ));
    assert!(held.elapsed() >= Duration::from_secs(30));
    let attempts = h.fake.send_attempts();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0], attempts[1]);
    h.stop().await;
}

/// Finding 5: a migrated send is not a rejection; it replays after the
/// rebuild with the same `random_id`.
#[tokio::test(start_paused = true)]
async fn migrated_send_replays_after_the_rebuild() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    h.fake
        .inject(Op::Send, Fault::Error(BackendError::Migrated));
    send_text(&h, chat(1), 1).await;
    h.until(|e| matches!(e, Event::Error(UserError::AccountMigrated)))
        .await;
    let (ev, skipped) = h.until(state_of(LocalId(1))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::Sent { .. },
            ..
        }
    ));
    assert!(skipped.iter().any(rebuilding), "sent after the rebuild");
    let attempts = h.fake.send_attempts();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0], attempts[1]);
    h.stop().await;
}

/// Finding 6: the server accepted the first attempt but its answer was lost.
/// Across several rebuilds there is exactly one server message, the attempt
/// cap holds, and a manual retry is deduplicated by the server.
#[tokio::test(start_paused = true)]
async fn lost_answer_gives_one_message_and_the_cap_holds_across_rebuilds() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    h.fake.inject(Op::Send, Fault::CompleteThenHang);
    h.fake.inject(Op::Send, Fault::Hang);
    h.fake.inject(Op::Send, Fault::Hang);
    send_text(&h, chat(1), 1).await;
    let mut seen = Vec::new();
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        seen.extend(wake_and_settle(&mut h).await);
    }
    let settled: Vec<&Event> = seen.iter().filter(|e| state_of(LocalId(1))(e)).collect();
    assert!(
        matches!(
            settled.as_slice(),
            [Event::SendState {
                state: SendState::Unknown { attempts: 3 },
                ..
            }]
        ),
        "{settled:?}"
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    wake_and_settle(&mut h).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let attempts = h.fake.send_attempts();
    assert_eq!(attempts.len(), 3, "no automatic attempt past the cap");
    assert!(attempts.iter().all(|a| *a == attempts[0]));
    let own = |h: &Harness| {
        h.fake
            .server_history(chat(1))
            .iter()
            .filter(|m| m.outgoing)
            .count()
    };
    assert_eq!(own(&h), 1);
    assert!(h.fake.accepted(attempts[0].1).is_some());
    h.send(Command::RetrySend(LocalId(1))).await;
    let (ev, _) = h.until(state_of(LocalId(1))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::SentUnconfirmed,
            ..
        }
    ));
    assert_eq!(own(&h), 1, "the server deduplicated the retry");
    h.stop().await;
}

/// Finding 7: auth loss on the update stream runs the local logout.
#[tokio::test(start_paused = true)]
async fn stream_auth_loss_runs_the_local_logout() {
    let mut h = harness(FakeScript::demo(2, 5), None).await;
    h.online().await;
    h.fake.stream_error(StreamError::Unauthorized).await;
    h.auth(|s| {
        matches!(
            s,
            AuthState::Phone {
                error: Some(UserError::SessionRevoked)
            }
        )
    })
    .await;
    assert_eq!(h.fake.signed_out(), 0, "no remote call");
    h.stop().await;
}

/// Finding 7: a migration on the update stream follows the migration
/// policy: one rebuild, then a halt.
#[tokio::test(start_paused = true)]
async fn stream_migration_applies_the_migration_policy() {
    let mut h = harness(FakeScript::demo(2, 5), None).await;
    h.online().await;
    h.fake.stream_error(StreamError::Migrated).await;
    h.until(|e| matches!(e, Event::Error(UserError::AccountMigrated)))
        .await;
    h.online().await;
    h.fake.stream_error(StreamError::Migrated).await;
    h.until(|e| matches!(e, Event::Connection(ConnState::Offline)))
        .await;
    tokio::time::sleep(Duration::from_secs(300)).await;
    // Task 6c: the halt tears the account down; the only build after it is
    // the signed-out instance for the next login.
    assert_eq!(h.fake.builds(), 3, "halted after a repeated migration");
    assert!(h.fake.seeds().last().unwrap().session.is_stored());
    h.stop().await;
}

/// Finding 8: a stream `updateMessageID` confirms a send before its RPC
/// answer; the late answer (here: the rebuild dropping it) changes nothing.
#[tokio::test(start_paused = true)]
async fn stream_message_id_confirms_a_send_before_its_answer() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    h.fake.inject(Op::Send, Fault::Hang);
    send_text(&h, chat(1), 1).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let random_id = h.fake.send_attempts()[0].1;
    h.fake
        .push_update(Update::MessageId {
            random_id,
            id: MsgId(77),
        })
        .await;
    let (ev, _) = h.until(state_of(LocalId(1))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::Sent { id: MsgId(77) },
            ..
        }
    ));
    wake_and_settle(&mut h).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(h.fake.send_attempts().len(), 1, "not replayed");
    assert!(
        !h.drain()
            .iter()
            .any(|e| matches!(e, Event::SendState { .. })),
        "the dropped answer is a no-op"
    );
    h.stop().await;
}

/// Finding 8: a `SentUnconfirmed` send is resolved by a later stream
/// `updateMessageID`, once.
#[tokio::test(start_paused = true)]
async fn stream_message_id_resolves_a_sent_unconfirmed_send() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    h.fake
        .inject(Op::Send, Fault::Error(BackendError::SentUnconfirmed));
    send_text(&h, chat(1), 1).await;
    let (ev, _) = h.until(state_of(LocalId(1))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::SentUnconfirmed,
            ..
        }
    ));
    let random_id = h.fake.send_attempts()[0].1;
    for _ in 0..2 {
        h.fake
            .push_update(Update::MessageId {
                random_id,
                id: MsgId(5),
            })
            .await;
    }
    let (ev, _) = h.until(state_of(LocalId(1))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::Sent { id: MsgId(5) },
            ..
        }
    ));
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        !h.drain()
            .iter()
            .any(|e| matches!(e, Event::SendState { .. }))
    );
    h.stop().await;
}

async fn unread_after(h: &mut Harness, id: i32) -> u32 {
    h.fake
        .push_update(Update::NewMessage(text_message(chat(1), id, "in")))
        .await;
    let (ev, _) = h
        .until(|e| matches!(e, Event::ChatBumped(s) if s.id == chat(1)))
        .await;
    match ev {
        Event::ChatBumped(s) => s.unread,
        _ => unreachable!(),
    }
}

/// Finding 10: an attached window is not proof the user sees the tail; only
/// `ViewingTail` while the window is active suppresses the unread count.
#[tokio::test(start_paused = true)]
async fn unread_counts_unless_the_tail_is_on_screen() {
    let mut h = harness(FakeScript::demo(2, 10), None).await;
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h.until(tail_of(chat(1))).await;
    assert_eq!(
        unread_after(&mut h, 11).await,
        1,
        "open but not reported seen"
    );
    h.send(Command::ViewingTail(Some(ThreadKey::whole(chat(1)))))
        .await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(unread_after(&mut h, 12).await, 1, "tail on screen");
    h.send(Command::Window(WindowState::Inactive)).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(unread_after(&mut h, 13).await, 2, "app in the background");
    h.stop().await;
}

/// Finding 11: reopening a retained chat shows its window at once, as a full
/// `Tail` of the new view, before the server answers.
#[tokio::test(start_paused = true)]
async fn reopening_a_retained_chat_shows_it_at_once() {
    let mut h = harness(FakeScript::demo(3, 10), None).await;
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    let (first, _) = h.until(tail_of(chat(1))).await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(2)))).await;
    h.until(tail_of(chat(2))).await;
    h.fake.inject(Op::History, Fault::Hang);
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    let (ev, _) = tokio::time::timeout(Duration::from_secs(5), h.until(tail_of(chat(1))))
        .await
        .expect("the reopened view stayed empty");
    assert_eq!(history_ids(&ev), (1..=10).collect::<Vec<_>>());
    let view = |e: &Event| match e {
        Event::History { view, .. } => Some(*view),
        _ => None,
    };
    assert!(view(&ev) > view(&first), "a new view");
    h.stop().await;
}

fn bare_actor() -> (Actor<FakeBackend>, EventReceiver) {
    let (backend, _fake) = FakeBackend::new(FakeScript::demo(1, 10));
    let (_cmd, commands) = tokio::sync::mpsc::channel(1);
    let (_internal, internal) = tokio::sync::mpsc::channel(1);
    let (events_tx, events) = events::channel();
    let persist = crate::session::PersistHandle::spawn(crate::secret::MemoryStore::new()).unwrap();
    let account = persist.fresh_session();
    let (actor, _channels) = Actor::new(ActorInit {
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
    (actor, events)
}

/// Finding 12: a short page from the cache while scrolling back down does
/// not declare the live tail; the server is asked to continue.
#[tokio::test(start_paused = true)]
async fn short_cached_newer_page_does_not_claim_the_live_tail() {
    let (mut actor, _events) = bare_actor();
    let c = chat(1);
    let t = ThreadKey::whole(c);
    let view = actor.windows.open(t);
    actor.open = Some(t);
    let first: Vec<Message> = (1..=10).map(|i| text_message(c, i, "m")).collect();
    let page = HistoryPage {
        messages: first,
        has_more: true,
    };
    actor.windows.append_newer(t, view, page).unwrap();
    let load = actor.begin_load(t, view);
    let cached: Vec<Message> = (11..=15).map(|i| text_message(c, i, "m")).collect();
    let cgen = actor.cgen;
    actor
        .on_done(Done {
            cgen,
            parked: None,
            outcome: Outcome::CacheHistory {
                load,
                dir: HistoryDir::Newer,
                anchor: Some(MsgId(10)),
                messages: Some(cached),
            },
        })
        .await;
    let w = actor.windows.get(t).unwrap();
    assert_eq!(w.messages().back().map(|m| m.id), Some(MsgId(15)));
    assert!(w.has_newer(), "only the server proves the live tail");
    assert!(actor.queue.iter().any(|j| matches!(
        j,
        Job::History {
            query: HistoryQuery::After(MsgId(15)),
            ..
        }
    )));
    assert!(actor.history_busy.contains_key(&t), "still single-flight");
}

/// Coordinator add-on: core hands the backend the peers it still needs:
/// pending sends, then windows, then every retained dialog in list order.
#[tokio::test(start_paused = true)]
async fn retained_peers_follow_sends_windows_and_dialogs_at_capacity() {
    let mut h = harness(FakeScript::demo(1200, 0), None).await;
    h.online().await;
    for _ in 0..9 {
        h.send(Command::LoadDialogs).await;
        h.until(|e| matches!(e, Event::DialogsAppended(_))).await;
    }
    h.send(Command::OpenThread(ThreadKey::whole(chat(1100))))
        .await;
    h.until(tail_of(chat(1100))).await;
    h.fake.inject(Op::Send, Fault::Hang);
    send_text(&h, chat(1151), 1).await;
    tokio::time::sleep(Duration::from_secs(16)).await;
    h.send(Command::Window(WindowState::Active)).await;
    let (ev, _) = h.until(|e| matches!(e, Event::DialogsReset(_))).await;
    let Event::DialogsReset(list) = ev else {
        unreachable!()
    };
    assert_eq!(list.len(), crate::store::MAX_DIALOGS, "at capacity");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut expected = vec![chat(1151), chat(1100)];
    expected.extend(list.iter().map(|c| c.id));
    let set = h.fake.retained_peers().unwrap();
    assert_eq!(set, expected);
    assert!(set.len() <= MAX_PINNED_PEERS);
    h.stop().await;
}
