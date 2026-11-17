//! The backend contract suite (plan section 12): what core relies on from any
//! [`Backend`], checked against `FakeBackend` in CI and against
//! `GrammersBackend` under `#[ignore]` (needs credentials and a signed-in
//! session, task 12).
//!
//! Checked: a build from stored state; the baseline before any update; dialog
//! paging without overlap; history order and anchors; one `random_id` never
//! producing two messages; read marks; `quit` stopping the runner; `retire`
//! returning a usable snapshot. All values must pass the domain validators.

use std::collections::HashSet;
use std::time::Duration;

use super::{
    Backend, BackendError, Client, HistoryQuery, Instance, InstanceControl, Seed, SessionSeed,
    SessionSnapshot, StreamError, StreamItem, Update, UpdateStream,
};
use crate::model::{ChatId, Message, MsgId, ThreadKey};

/// Core's bootstrap waits this long for the baseline (plan section 2).
const BASELINE_WAIT: Duration = Duration::from_secs(10);
/// Core's runner cap after `quit` (plan section 2, rebuild step 3).
const RUNNER_WAIT: Duration = Duration::from_secs(5);
const PAGE: u32 = 20;
const HISTORY: u32 = 10;
/// The target chat must hold this many messages, so `After` has known
/// successors to return.
const MIN_FIXTURE_MESSAGES: usize = 3;
/// Errors the background consumer tolerates before it stops reading.
const MAX_STREAM_ERRORS: usize = 3;

/// Whether the suite may send a message to the target chat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sends {
    Check,
    Skip,
}

fn assert_history(messages: &[Message], chat: ChatId, max: u32) {
    assert!(messages.len() <= usize::try_from(max).unwrap());
    assert!(
        messages.windows(2).all(|w| w[0].id < w[1].id),
        "ascending, distinct"
    );
    for m in messages {
        assert_eq!(m.thread.chat, chat);
        assert!(m.is_valid());
    }
}

fn random_id() -> i64 {
    loop {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let id = i64::from_le_bytes(bytes);
        if id != 0 {
            return id;
        }
    }
}

/// Reads the stream after the baseline: never a second baseline, only valid
/// messages. Stops after a few errors or a fatal one.
async fn consume(mut updates: impl UpdateStream) {
    let mut errors = 0;
    loop {
        match updates.next().await {
            Ok(StreamItem::Baseline) => panic!("a second baseline on the same stream"),
            Ok(StreamItem::Update(Update::NewMessage(m) | Update::Edited(m))) => {
                assert!(m.is_valid());
            }
            Ok(StreamItem::Update(_)) => {}
            Err(StreamError::Fatal | StreamError::Unauthorized | StreamError::Migrated) => return,
            Err(StreamError::Transient) => {
                errors += 1;
                if errors >= MAX_STREAM_ERRORS {
                    return;
                }
            }
        }
    }
}

async fn check_dialogs(client: &impl Client) -> Vec<ChatId> {
    let first = client.dialogs(None, PAGE).await.expect("dialogs");
    assert!(first.chats.len() <= usize::try_from(PAGE).unwrap());
    assert!(first.chats.iter().all(crate::model::ChatSummary::is_valid));
    let ids: HashSet<ChatId> = first.chats.iter().map(|c| c.id).collect();
    assert_eq!(ids.len(), first.chats.len(), "no duplicate chats");
    if let Some(next) = first.next {
        let second = client.dialogs(Some(next), PAGE).await.expect("next page");
        assert!(second.chats.iter().all(crate::model::ChatSummary::is_valid));
        assert!(
            second.chats.iter().all(|c| !ids.contains(&c.id)),
            "pages do not overlap"
        );
    }
    first
        .chats
        .iter()
        .filter(|c| c.can_send)
        .map(|c| c.id)
        .collect()
}

async fn check_history(client: &impl Client, chat: ChatId) -> Option<MsgId> {
    let tail = client
        .history(ThreadKey::whole(chat), HistoryQuery::Tail, HISTORY)
        .await
        .expect("tail");
    assert_history(&tail.messages, chat, HISTORY);
    assert!(
        tail.messages.len() >= MIN_FIXTURE_MESSAGES,
        "the contract chat needs at least {MIN_FIXTURE_MESSAGES} messages"
    );
    let first = tail.messages.first()?;
    let older = client
        .history(ThreadKey::whole(chat), HistoryQuery::Before(first.id), 5)
        .await
        .expect("older");
    assert_history(&older.messages, chat, 5);
    assert!(older.messages.iter().all(|m| m.id < first.id));
    if tail.has_more {
        assert!(
            !older.messages.is_empty(),
            "has_more promised older history"
        );
    }
    let newer = client
        .history(ThreadKey::whole(chat), HistoryQuery::After(first.id), 5)
        .await
        .expect("newer");
    assert_history(&newer.messages, chat, 5);
    // The anchor's successors are known from the tail: After must return
    // exactly them, in order (a message arriving meanwhile may follow them
    // when the tail had fewer than five).
    let expected: Vec<MsgId> = tail.messages.iter().skip(1).take(5).map(|m| m.id).collect();
    let got: Vec<MsgId> = newer.messages.iter().map(|m| m.id).collect();
    assert!(
        got.starts_with(&expected),
        "After returns the anchor's successors: {got:?} vs {expected:?}"
    );
    tail.messages.last().map(|m| m.id)
}

/// Builds a new instance from the retired one's snapshot, carrying `chat`
/// as core does for an open window, and proves it usable: a baseline, and
/// `chat` addressable without a dialogs reload, still holding `newest`.
async fn check_rebuild<B: Backend>(
    backend: &B,
    snapshot: SessionSnapshot,
    chat: ChatId,
    newest: Option<MsgId>,
) -> SessionSnapshot {
    let seed = Seed {
        session: SessionSeed::Live(snapshot),
    };
    let Instance {
        client,
        mut updates,
        runner,
        control,
    } = backend
        .build(seed)
        .await
        .expect("rebuild from the snapshot");
    let runner = tokio::spawn(runner);
    let first = tokio::time::timeout(BASELINE_WAIT, updates.next())
        .await
        .expect("baseline after a rebuild")
        .expect("stream");
    assert!(matches!(first, StreamItem::Baseline));
    let tail = client
        .history(ThreadKey::whole(chat), HistoryQuery::Tail, HISTORY)
        .await
        .expect("the carried chat is addressable after a rebuild");
    assert_history(&tail.messages, chat, HISTORY);
    if let Some(id) = newest {
        assert!(tail.messages.iter().any(|m| m.id == id), "same history");
    }
    control.quit();
    tokio::time::timeout(RUNNER_WAIT, runner)
        .await
        .expect("the rebuilt runner stops after quit")
        .expect("the rebuilt runner did not panic");
    control.retire()
}

/// One logical send, attempted twice with the same `random_id`, is one message.
async fn check_send(client: &impl Client, chat: ChatId) -> MsgId {
    let rid = random_id();
    let text = format!("abyss contract check {}", rid.unsigned_abs() % 10_000);
    let id = client
        .send_text(ThreadKey::whole(chat), text.clone(), rid)
        .await
        .expect("send");
    match client
        .send_text(ThreadKey::whole(chat), text.clone(), rid)
        .await
    {
        Ok(again) => assert_eq!(again, id, "a resend must not create a message"),
        Err(e) => assert_eq!(e, BackendError::SentUnconfirmed),
    }
    let tail = client
        .history(ThreadKey::whole(chat), HistoryQuery::Tail, HISTORY)
        .await
        .expect("tail after send");
    let copies: Vec<&Message> = tail.messages.iter().filter(|m| m.id == id).collect();
    assert_eq!(copies.len(), 1);
    assert!(copies[0].outgoing);
    let same_text = tail
        .messages
        .iter()
        .filter(|m| matches!(&m.body, crate::model::Body::Text(rt) if rt.text() == text))
        .count();
    assert_eq!(same_text, 1, "the duplicate was not delivered");
    id
}

/// Runs the suite. `target` is the chat to read (and, with `Sends::Check`,
/// write); by default the first sendable chat of the first dialogs page.
pub(crate) async fn run<B: Backend>(backend: &B, target: Option<ChatId>, sends: Sends) {
    let seed = Seed {
        session: SessionSeed::Stored,
    };
    let Instance {
        client,
        mut updates,
        runner,
        control,
    } = backend.build(seed).await.expect("build");
    let runner = tokio::spawn(runner);
    let first = tokio::time::timeout(BASELINE_WAIT, updates.next())
        .await
        .expect("baseline within the bootstrap wait")
        .expect("stream");
    assert!(
        matches!(first, StreamItem::Baseline),
        "baseline comes first"
    );
    let consumer = tokio::spawn(consume(updates));

    let sendable = check_dialogs(&client).await;
    let chat = target
        .or_else(|| sendable.first().copied())
        .expect("a chat to check");
    // As core does once the window opens: the chat's peer must survive the
    // rebuild below.
    client.retain_peers(&[chat]);
    let mut newest = check_history(&client, chat).await;
    if sends == Sends::Check {
        newest = Some(check_send(&client, chat).await);
    }
    if let Some(max_id) = newest {
        client
            .read_history(ThreadKey::whole(chat), max_id)
            .await
            .expect("read mark");
    }

    control.quit();
    tokio::time::timeout(RUNNER_WAIT, runner)
        .await
        .expect("the runner stops after quit")
        .expect("the runner did not panic");
    let snapshot = control.retire();
    let rev = snapshot.rev().expect("a readable snapshot");
    consumer.abort();
    if let Err(e) = consumer.await {
        assert!(e.is_cancelled(), "the stream consumer panicked");
    }
    let again = check_rebuild(backend, snapshot, chat, newest).await;
    assert!(again.rev().expect("a readable snapshot") >= rev);
}

#[cfg(feature = "fake")]
#[tokio::test(start_paused = true)]
async fn fake_backend_passes_the_contract() {
    let (backend, fake) = super::fake::FakeBackend::new(super::fake::FakeScript::demo(25, 30));
    run(&backend, None, Sends::Check).await;
    assert_eq!(fake.builds(), 2, "the suite rebuilds once");
    assert_eq!(fake.retired(), 2);
    assert_eq!(fake.runners_finished(), 2);
}

/// Needs `ABYSS_API_ID`/`ABYSS_API_HASH` and a signed-in session in the Keychain. With
/// `ABYSS_CONTRACT_CHAT=user:<id>` (or `chat:`/`channel:`), the suite also
/// sends one message there; the chat must be in the first dialogs pages or be
/// Saved Messages, so its access hash is known.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Telegram credentials and a signed-in session (task 12)"]
async fn grammers_backend_passes_the_contract() {
    let env = crate::credentials::from_env(|k| std::env::var(k).ok());
    let crate::credentials::EnvResolution::Pair(creds) = env.clone() else {
        panic!("set ABYSS_API_ID and ABYSS_API_HASH");
    };
    let slot = crate::credentials::CredentialSlot::new(env);
    slot.set(Some(creds));
    let persist = crate::session::PersistHandle::spawn(crate::secret::KeychainStore::new())
        .expect("persistence worker");
    let backend = super::grammers::GrammersBackend::new(slot, persist);
    let target = std::env::var("ABYSS_CONTRACT_CHAT").ok().map(|s| {
        let (kind, id) = s.split_once(':').expect("kind:id");
        let kind = match kind {
            "user" => crate::model::PeerKind::User,
            "chat" => crate::model::PeerKind::Chat,
            "channel" => crate::model::PeerKind::Channel,
            other => panic!("unknown chat kind {other}"),
        };
        ChatId {
            kind,
            id: id.parse().expect("numeric chat id"),
        }
    });
    let sends = if target.is_some() {
        Sends::Check
    } else {
        Sends::Skip
    };
    run(&backend, target, sends).await;
}
