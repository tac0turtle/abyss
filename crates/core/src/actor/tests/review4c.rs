//! Regression tests for the round 9 re-check of task 4b (plan section 15).
//! Each one fails on the code before the fix.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::{CacheDir, Harness, cache_at, chat, harness};
use crate::actor::boot::{Boot, BootKind};
use crate::actor::jobs::{Done, Outcome};
use crate::actor::sends::{MAX_FLOOD_WAITS, MIN_FLOOD_WAIT};
use crate::actor::{Actor, ActorInit, Internal};
use crate::api::{Command, ConnState, Event, HistoryDir, SendState, UserError, WindowState};
use crate::backend::fake::{FakeBackend, FakeScript, Fault, Op, summary, text_message};
use crate::backend::{BackendError, HistoryPage, HistoryQuery, Update};
use crate::cache::{CacheCmd, CacheHandle, CacheKey};
use crate::events::{self, EventReceiver};
use crate::model::{ChatId, LocalId, Message, MsgId, ThreadKey};
use crate::store::MAX_DIALOGS;

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

fn msgs(c: ChatId, ids: std::ops::RangeInclusive<i32>) -> Vec<Message> {
    ids.map(|i| text_message(c, i, "m")).collect()
}

fn ids_of(v: &[Message]) -> Vec<i32> {
    v.iter().map(|m| m.id.0).collect()
}

fn actor_with(cache: CacheHandle) -> (Actor<FakeBackend>, EventReceiver) {
    let (backend, _fake) = FakeBackend::new(FakeScript::demo(1, 0));
    let (_cmd, commands) = tokio::sync::mpsc::channel(1);
    let (_internal, internal) = tokio::sync::mpsc::channel(1);
    let (events_tx, events) = events::channel();
    let persist = crate::session::PersistHandle::spawn(crate::secret::MemoryStore::new()).unwrap();
    let account = persist.fresh_session();
    let (actor, _channels) = Actor::new(ActorInit {
        backend: Arc::new(backend),
        events: events_tx,
        cache: Some(cache),
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

/// The window and a contiguous cache hold 1..=100. A tail snapshot of `kind`
/// returns 51..=100 (`has_more`), and deletions of all of 51..=100 were
/// buffered before it arrived. Returns what the window and the cache hold
/// afterwards.
async fn emptied_tail(kind: BootKind) -> (Vec<i32>, Vec<i32>) {
    let dir = CacheDir::new();
    let key = CacheKey::generate().unwrap();
    let cache = cache_at(dir.path(), &key);
    let (mut actor, _events) = actor_with(cache.clone());
    let c = chat(1);
    let t = ThreadKey::whole(c);
    let view = actor.windows.open(t);
    actor.open = Some(t);
    let all = HistoryPage {
        messages: msgs(c, 1..=100),
        has_more: false,
    };
    cache
        .write(CacheCmd::PutMessages(t, all.messages.clone()))
        .await;
    actor.windows.prepend(t, view, all).unwrap();
    actor.windows.set_cache_contiguous(t);
    let load = actor.begin_load(t, view);
    let dead = Update::Deleted {
        chat: Some(c),
        ids: (51..=100).map(MsgId).collect(),
    };
    actor.boot = Boot::awaiting_tail(kind, load, dead);
    assert!(actor.boot.is_some());
    let page = HistoryPage {
        messages: msgs(c, 51..=100),
        has_more: true,
    };
    let cgen = actor.cgen;
    actor
        .on_done(Done {
            cgen,
            parked: None,
            outcome: Outcome::History {
                load,
                dir: HistoryDir::Tail,
                query: HistoryQuery::Tail,
                floor: Some(MsgId(100)),
                res: Ok(page),
            },
        })
        .await;
    let window = actor
        .windows
        .get(t)
        .map(|w| ids_of(&w.messages().iter().cloned().collect::<Vec<_>>()))
        .unwrap_or_default();
    cache.barrier().await.unwrap();
    let cached = ids_of(&cache.tail(t, 200).await.unwrap());
    (window, cached)
}

/// Finding 1 (high): a refresh whose full tail page was emptied by buffered
/// deletions used to delete the whole window and cache, though 1..=50 still
/// exist on the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emptied_refresh_tail_keeps_older_messages_in_window_and_cache() {
    let (window, cached) = emptied_tail(BootKind::Refresh).await;
    let older: Vec<i32> = (1..=50).collect();
    assert_eq!(window, older);
    assert_eq!(cached, older);
}

/// Finding 1: the same on a bootstrap tail, which replaces the window; the
/// cache keeps the history older than the page's coverage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emptied_bootstrap_tail_keeps_older_cached_messages() {
    let (window, cached) = emptied_tail(BootKind::Full).await;
    assert!(window.is_empty(), "the window is replaced by the page");
    assert_eq!(cached, (1..=50).collect::<Vec<_>>());
}

fn flood(seconds: u32) -> Fault {
    Fault::Error(BackendError::FloodWait { seconds })
}

fn is_flood_wait(e: &Event) -> bool {
    matches!(
        e,
        Event::SendState {
            state: SendState::FloodWait { .. },
            ..
        }
    )
}

/// Finding 2: zero-second flood waits forever, with a rebuild during a
/// wait. Each wait lasts at least a second, the count is bounded across the
/// rebuild, and the send then needs the user (Failed, then Retry works).
#[tokio::test(start_paused = true)]
async fn repeated_flood_waits_end_failed_across_rebuilds() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    for _ in 0..=MAX_FLOOD_WAITS {
        h.fake.inject(Op::Send, flood(0));
    }
    let started = Instant::now();
    send_text(&h, chat(1), 1).await;
    let (first, _) = h.until(state_of(LocalId(1))).await;
    assert!(is_flood_wait(&first));
    h.internal.send(Internal::Wake).await.unwrap();
    let failed = |e: &Event| {
        matches!(
            e,
            Event::SendState {
                local_id: LocalId(1),
                state: SendState::Failed
            }
        )
    };
    let (_, seen) = h.until(failed).await;
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::Connection(ConnState::Rebuilding { .. })))
    );
    let waits = 1 + seen.iter().filter(|e| is_flood_wait(e)).count();
    assert_eq!(waits, usize::from(MAX_FLOOD_WAITS));
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::Error(UserError::FloodWait { seconds: 0 })))
    );
    let min = MIN_FLOOD_WAIT * u32::from(MAX_FLOOD_WAITS);
    assert!(started.elapsed() >= min, "zero-second waits are clamped");
    let attempts = h.fake.send_attempts();
    assert_eq!(attempts.len(), usize::from(MAX_FLOOD_WAITS) + 1);
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(h.fake.send_attempts().len(), attempts.len(), "no more");
    h.send(Command::RetrySend(LocalId(1))).await;
    let (ev, _) = h.until(state_of(LocalId(1))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::Sent { .. },
            ..
        }
    ));
    let all = h.fake.send_attempts();
    assert!(all.iter().all(|a| *a == all[0]), "one random_id throughout");
    h.stop().await;
}

/// Finding 2: a flood wait that ends past the automatic-retry window is not
/// held; the send needs the user at once.
#[tokio::test(start_paused = true)]
async fn flood_wait_past_the_retry_window_fails_at_once() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    h.fake.inject(Op::Send, flood(86_400));
    send_text(&h, chat(1), 1).await;
    let (ev, _) = h.until(state_of(LocalId(1))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::Failed,
            ..
        }
    ));
    h.stop().await;
}

/// Finding 3: with the chat list at capacity, a chat that appeared at server
/// position 150 is shown once the restarted traversal reaches it.
#[tokio::test(start_paused = true)]
async fn refresh_at_capacity_admits_a_new_chat_in_server_order() {
    let mut h = harness(FakeScript::demo(1200, 0), None).await;
    h.online().await;
    for _ in 0..9 {
        h.send(Command::LoadDialogs).await;
        h.until(|e| matches!(e, Event::DialogsAppended(_))).await;
    }
    let newcomer = chat(9999);
    h.fake
        .edit_dialogs(|d| d.insert(150, summary(newcomer, "new")));
    tokio::time::sleep(Duration::from_secs(16)).await;
    h.send(Command::Window(WindowState::Active)).await;
    let (ev, _) = h.until(|e| matches!(e, Event::DialogsReset(_))).await;
    assert!(matches!(&ev, Event::DialogsReset(v) if v.len() == MAX_DIALOGS));
    h.until(|e| matches!(e, Event::DialogsPaging { .. })).await;
    h.send(Command::LoadDialogs).await;
    let (ev, _) = h
        .until(|e| matches!(e, Event::DialogsReset(_) | Event::DialogsAppended(_)))
        .await;
    let list = match ev {
        Event::DialogsReset(list) => list,
        other => unreachable!("a reordering page resends the list: {other:?}"),
    };
    assert_eq!(list.len(), MAX_DIALOGS);
    assert_eq!(list.get(150).map(|c| c.id), Some(newcomer));
    // The lowest-ranked provisional row made room.
    assert!(!list.iter().any(|c| c.id.id == 1000));
    assert_eq!(list.last().map(|c| c.id.id), Some(999));
    tokio::time::sleep(Duration::from_secs(1)).await;
    let peers = h.fake.retained_peers().unwrap();
    assert_eq!(peers.get(150), Some(&newcomer));
    h.stop().await;
}

/// Finding 4: a live bump after the initial sync reorders the retained
/// peers too.
#[tokio::test(start_paused = true)]
async fn retained_peers_follow_a_live_bump() {
    let mut h = harness(FakeScript::demo(5, 10), None).await;
    h.online().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let ids = |v: Vec<ChatId>| v.into_iter().map(|c| c.id).collect::<Vec<_>>();
    assert_eq!(ids(h.fake.retained_peers().unwrap()), vec![1, 2, 3, 4, 5]);
    h.fake
        .push_update(Update::NewMessage(text_message(chat(5), 11, "in")))
        .await;
    h.until(|e| matches!(e, Event::ChatBumped(s) if s.id == chat(5)))
        .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(ids(h.fake.retained_peers().unwrap()), vec![5, 1, 2, 3, 4]);
    h.stop().await;
}

/// After a send settled `Sent`, nothing else about it happens: no further
/// state, one server message, and the table no longer holds it.
async fn assert_settled_once(h: &mut Harness) {
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        !h.drain()
            .iter()
            .any(|e| matches!(e, Event::SendState { .. })),
        "a second application"
    );
    assert_eq!(h.fake.send_attempts().len(), 1, "not replayed");
    let own = h
        .fake
        .server_history(chat(1))
        .iter()
        .filter(|m| m.outgoing)
        .count();
    assert_eq!(own, 1);
    h.send(Command::RetrySend(LocalId(1))).await;
    h.until(|e| matches!(e, Event::Error(UserError::NotFound)))
        .await;
}

fn sent(id: i32) -> impl Fn(&Event) -> bool {
    move |e| matches!(e, Event::SendState { local_id: LocalId(1), state: SendState::Sent { id: got } } if *got == MsgId(id))
}

/// Finding 5: the stream `updateMessageID` arrives first and the RPC answer
/// succeeds later: one `Sent`, applied once.
#[tokio::test(start_paused = true)]
async fn stream_message_id_then_successful_answer_applies_once() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    h.fake
        .inject(Op::Send, Fault::Delay(Duration::from_secs(5)));
    send_text(&h, chat(1), 1).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let random_id = h.fake.send_attempts()[0].1;
    h.fake
        .push_update(Update::MessageId {
            random_id,
            id: MsgId(2),
        })
        .await;
    let (ev, _) = h.until(state_of(LocalId(1))).await;
    assert!(sent(2)(&ev), "{ev:?}");
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert!(h.fake.accepted(random_id).is_some(), "the answer arrived");
    assert_settled_once(&mut h).await;
    h.stop().await;
}

/// Finding 5: the successful RPC answer arrives first and the stream
/// `updateMessageID` later: one `Sent`, applied once.
#[tokio::test(start_paused = true)]
async fn successful_answer_then_stream_message_id_applies_once() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    send_text(&h, chat(1), 1).await;
    h.until(sent(2)).await;
    let random_id = h.fake.send_attempts()[0].1;
    h.fake
        .push_update(Update::MessageId {
            random_id,
            id: MsgId(2),
        })
        .await;
    assert_settled_once(&mut h).await;
    h.stop().await;
}
