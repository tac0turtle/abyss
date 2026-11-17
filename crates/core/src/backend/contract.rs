//! The backend contract suite (plan section 12): what core relies on from any
//! [`Backend`], checked against `FakeBackend` in CI and against
//! `GrammersBackend` under `#[ignore]` (needs credentials and a signed-in
//! session, task 12).
//!
//! Checked: a build from stored state; the baseline before any update; dialog
//! paging without overlap; history order and anchors (`Around` and its
//! bounds included); one `random_id` never producing two messages; read
//! marks; `quit` stopping the runner; `retire` returning a usable snapshot.
//! All values must pass the domain validators.
//!
//! Forums (Stage 2 task 3): topic pages in the reported order without
//! overlap, pinned topics first, cursors carrying their page's order; by-id
//! results that find or delete every requested id; topic and General history
//! holding only their thread; a send into a topic landing only there; a topic
//! read; `peer_dialogs` reporting the forum's kind; topics of a non-forum
//! rejected.

use std::collections::HashSet;
use std::time::Duration;

use super::{
    AROUND_CONTEXT, Backend, BackendError, Client, HistoryQuery, Instance, InstanceControl,
    MAX_HISTORY_PAGE, Seed, SessionSeed, SessionSnapshot, StreamError, StreamItem, Update,
    UpdateStream,
};
use crate::model::{
    ChatId, ChatKind, Message, MsgId, ThreadKey, TopicId, TopicOrder, TopicSnapshot,
};
use crate::store::{ForumStatus, Route, route};

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
/// Topics per page: small enough that a demo forum needs a second page.
const TOPICS: u32 = 20;
/// Topics asked for by id, besides one that doesn't exist.
const BY_ID: usize = 3;

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

fn is_forum(kind: ChatKind) -> bool {
    matches!(kind, ChatKind::Supergroup { forum: true, .. })
}

/// What the first dialogs page offers the suite.
struct Targets {
    sendable: Vec<ChatId>,
    forums: Vec<ChatId>,
}

async fn check_dialogs(client: &impl Client) -> Targets {
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
    let sendable = first
        .chats
        .iter()
        .filter(|c| c.can_send && !is_forum(c.kind))
        .map(|c| c.id)
        .collect();
    let forums = first
        .chats
        .iter()
        .filter(|c| is_forum(c.kind))
        .map(|c| c.id)
        .collect();
    Targets { sendable, forums }
}

/// Valid topics, at most `max`, without duplicates.
fn assert_topics(topics: &[TopicSnapshot], max: u32) {
    assert!(topics.len() <= usize::try_from(max).unwrap());
    assert!(topics.iter().all(|t| t.topic.is_valid()));
    let ids: HashSet<TopicId> = topics.iter().map(|t| t.topic.id).collect();
    assert_eq!(ids.len(), topics.len(), "no duplicate topics");
}

/// Pinned topics lead, and the rest follow the reported order, newest first.
fn assert_server_order(order: TopicOrder, topics: &[TopicSnapshot]) {
    let pinned = topics.iter().take_while(|t| t.topic.pinned).count();
    let rest = topics.get(pinned..).unwrap_or_default();
    assert!(rest.iter().all(|t| !t.topic.pinned), "pinned topics lead");
    assert!(
        rest.windows(2).all(|w| match order {
            TopicOrder::LastActivity => w[0].topic.last_id >= w[1].topic.last_id,
            TopicOrder::Created => w[0].topic.created >= w[1].topic.created,
        }),
        "topics follow the reported order"
    );
}

/// Ascending, valid messages of `thread` once normalized.
fn assert_thread(messages: &[Message], thread: ThreadKey, max: u32) {
    assert_history(messages, thread.chat, max);
    for m in messages {
        assert_eq!(
            route(m.thread.chat, m.thread.topic, ForumStatus::Forum),
            Route::Thread(thread),
            "only the requested thread"
        );
    }
}

async fn thread_tail(client: &impl Client, thread: ThreadKey) -> Vec<Message> {
    let tail = client
        .history(thread, HistoryQuery::Tail, HISTORY)
        .await
        .expect("thread tail");
    assert_thread(&tail.messages, thread, HISTORY);
    tail.messages
}

/// Both pages of a forum's topics: valid, in order, without overlap.
async fn check_topic_pages(client: &impl Client, forum: ChatId) -> Vec<TopicSnapshot> {
    let first = client.topics(forum, None, TOPICS).await.expect("topics");
    assert_topics(&first.topics, TOPICS);
    assert_server_order(first.order, &first.topics);
    let mut all = first.topics;
    if let Some(next) = first.next {
        assert_eq!(
            next.order(),
            first.order,
            "a cursor carries its page's order"
        );
        let second = client
            .topics(forum, Some(next), TOPICS)
            .await
            .expect("next topics page");
        assert_topics(&second.topics, TOPICS);
        // A page in another order is discarded by core (plan section 6.1).
        if second.order == first.order {
            assert_server_order(second.order, &second.topics);
            assert!(
                second.topics.iter().all(|t| !t.topic.pinned),
                "pinned topics lead the first page only"
            );
            assert!(
                second
                    .topics
                    .iter()
                    .all(|t| all.iter().all(|a| a.topic.id != t.topic.id)),
                "topic pages do not overlap"
            );
            all.extend(second.topics);
        }
    }
    all
}

/// Known ids are found as listed; an id that names no topic is deleted.
async fn check_topics_by_id(client: &impl Client, forum: ChatId, listed: &[TopicSnapshot]) {
    let known: Vec<TopicId> = listed.iter().take(BY_ID).map(|t| t.topic.id).collect();
    let missing = TopicId::try_from(i32::MAX).unwrap();
    let mut ask = known.clone();
    ask.push(missing);
    let got = client
        .topics_by_id(forum, &ask)
        .await
        .expect("topics by id");
    assert_topics(&got.found, u32::try_from(ask.len()).unwrap());
    let found: Vec<TopicId> = got.found.iter().map(|t| t.topic.id).collect();
    assert_eq!(
        found, known,
        "every listed topic is found, in request order"
    );
    assert_eq!(got.deleted, vec![missing]);
    for t in &got.found {
        let page = listed.iter().find(|l| l.topic.id == t.topic.id).unwrap();
        assert_eq!(t.topic.created, page.topic.created);
    }
}

/// Topic and General history, a send into a topic, and a topic read.
async fn check_topic_threads(
    client: &impl Client,
    forum: ChatId,
    listed: &[TopicSnapshot],
    sends: Sends,
) {
    let general = ThreadKey {
        chat: forum,
        topic: Some(TopicId::GENERAL),
    };
    thread_tail(client, general).await;
    let open = listed
        .iter()
        .find(|t| t.topic.id != TopicId::GENERAL && !t.topic.closed && t.topic.last_id.is_some())
        .expect("an open topic with messages");
    let thread = ThreadKey {
        chat: forum,
        topic: Some(open.topic.id),
    };
    let tail = thread_tail(client, thread).await;
    let newest = tail.last().map(|m| m.id).expect("the topic's messages");
    assert!(
        Some(newest) >= open.topic.last_id,
        "the top message is in the thread"
    );
    let mut read = newest;
    if sends == Sends::Check {
        let id = client
            .send_text(thread, "abyss contract topic check".into(), random_id())
            .await
            .expect("send into a topic");
        let tail = thread_tail(client, thread).await;
        assert!(tail.iter().any(|m| m.id == id && m.outgoing));
        let in_general = thread_tail(client, general).await;
        assert!(in_general.iter().all(|m| m.id != id), "only in its topic");
        read = id;
    }
    client
        .read_history(thread, read)
        .await
        .expect("topic read mark");
}

/// The forum's summary from `peer_dialogs`, with its kind; a non-forum's
/// topics are rejected.
async fn check_peer_dialogs(client: &impl Client, forum: ChatId, other: ChatId) {
    let peers = client
        .peer_dialogs(&[forum, other])
        .await
        .expect("peer dialogs");
    assert!(peers.iter().all(crate::model::ChatSummary::is_valid));
    let summary = peers
        .iter()
        .find(|c| c.id == forum)
        .expect("the forum's summary");
    assert!(is_forum(summary.kind));
    assert!(peers.iter().all(|c| c.id == forum || c.id == other));
    assert!(matches!(
        client.topics(other, None, TOPICS).await,
        Err(BackendError::Rejected(_))
    ));
}

async fn check_forum(client: &impl Client, forum: ChatId, other: ChatId, sends: Sends) {
    let listed = check_topic_pages(client, forum).await;
    assert!(!listed.is_empty(), "a forum lists at least General");
    check_topics_by_id(client, forum, &listed).await;
    check_topic_threads(client, forum, &listed, sends).await;
    check_peer_dialogs(client, forum, other).await;
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
    check_around(client, chat, &tail.messages).await;
    tail.messages.last().map(|m| m.id)
}

/// `Around` (Stage 2 plan section 17.6.4) at the tail's oldest message:
/// at most `AROUND_CONTEXT` messages at or below it, ending with it, and
/// the rest above it, starting with its successors from the tail. Outside
/// its bounds it is refused: no successor, a negative anchor, or a limit
/// outside `AROUND_CONTEXT < limit ≤ MAX_HISTORY_PAGE`.
async fn check_around(client: &impl Client, chat: ChatId, tail: &[Message]) {
    let thread = ThreadKey::whole(chat);
    let Some(anchor) = tail.first().map(|m| m.id) else {
        return;
    };
    let page = client
        .history(thread, HistoryQuery::Around(anchor), PAGE)
        .await
        .expect("around");
    assert_history(&page.messages, chat, PAGE);
    let (below, above): (Vec<MsgId>, Vec<MsgId>) = page
        .messages
        .iter()
        .map(|m| m.id)
        .partition(|id| *id <= anchor);
    assert!(below.len() <= usize::try_from(AROUND_CONTEXT).unwrap());
    assert!(above.len() <= usize::try_from(PAGE - AROUND_CONTEXT).unwrap());
    assert_eq!(below.last(), Some(&anchor), "the anchor closes the context");
    let successors: Vec<MsgId> = tail.iter().skip(1).map(|m| m.id).collect();
    assert!(
        above.starts_with(&successors),
        "Around continues with the anchor's successors: {above:?} vs {successors:?}"
    );
    for (anchor, limit) in [
        (MsgId(i32::MAX), PAGE),
        (MsgId(-1), PAGE),
        (anchor, 0),
        (anchor, AROUND_CONTEXT),
        (anchor, MAX_HISTORY_PAGE + 1),
    ] {
        assert!(
            matches!(
                client
                    .history(thread, HistoryQuery::Around(anchor), limit)
                    .await,
                Err(BackendError::Rejected(_))
            ),
            "Around({anchor:?}) of {limit} is refused"
        );
    }
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

/// Which forum the suite checks, and whether it may post there.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ForumTarget {
    pub(crate) chat: Option<ChatId>,
    pub(crate) sends: Sends,
}

/// Runs the suite. `target` is the chat to read (and, with `Sends::Check`,
/// write); by default the first sendable chat of the first dialogs page.
/// `forum` likewise, defaulting to the first forum of that page; the forum
/// checks are skipped when there is none.
pub(crate) async fn run<B: Backend>(
    backend: &B,
    target: Option<ChatId>,
    sends: Sends,
    forum: ForumTarget,
) {
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

    let targets = check_dialogs(&client).await;
    let chat = target
        .or_else(|| targets.sendable.first().copied())
        .expect("a chat to check");
    if let Some(f) = forum.chat.or_else(|| targets.forums.first().copied()) {
        check_forum(&client, f, chat, forum.sends).await;
    }
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
    run(&backend, None, Sends::Check, NO_FORUM).await;
    assert_eq!(fake.builds(), 2, "the suite rebuilds once");
    assert_eq!(fake.retired(), 2);
    assert_eq!(fake.runners_finished(), 2);
}

#[cfg(feature = "fake")]
const NO_FORUM: ForumTarget = ForumTarget {
    chat: None,
    sends: Sends::Skip,
};

/// The demo script with a demo forum (General plus 60 topics) on the first
/// dialogs page, served in `order`.
#[cfg(feature = "fake")]
fn forum_script(
    order: TopicOrder,
    general: crate::model::Visibility,
) -> (super::fake::FakeScript, ChatId) {
    use super::fake::{FakeScript, summary};
    let mut script = FakeScript::demo(25, 30);
    let forum = ChatId {
        kind: crate::model::PeerKind::Channel,
        id: 1_000,
    };
    let mut row = summary(forum, "Contract forum");
    row.kind = ChatKind::Supergroup {
        forum: true,
        gigagroup: false,
    };
    script.dialogs.insert(1, row);
    script.add_demo_forum(forum, 1_794_837_900, order);
    for t in script.topics.get_mut(&forum).into_iter().flatten() {
        if t.topic.id == TopicId::GENERAL {
            t.visibility = general;
        }
    }
    (script, forum)
}

#[cfg(feature = "fake")]
#[tokio::test(start_paused = true)]
async fn fake_forum_passes_the_contract_by_activity() {
    let (script, forum) = forum_script(TopicOrder::LastActivity, crate::model::Visibility::Shown);
    let (backend, fake) = super::fake::FakeBackend::new(script);
    let target = ForumTarget {
        chat: Some(forum),
        sends: Sends::Check,
    };
    run(&backend, None, Sends::Check, target).await;
    assert_eq!(
        fake.calls(super::fake::Op::Topics),
        3,
        "two pages and the non-forum"
    );
    let topic_sends = fake
        .sent()
        .iter()
        .filter(|(t, _)| t.topic.is_some())
        .count();
    assert_eq!(topic_sends, 1);
}

/// Creation order, with General hidden (it is still listed, with its
/// visibility, and its history is still served).
#[cfg(feature = "fake")]
#[tokio::test(start_paused = true)]
async fn fake_forum_passes_the_contract_by_creation() {
    let (script, forum) = forum_script(TopicOrder::Created, crate::model::Visibility::Hidden);
    let (backend, fake) = super::fake::FakeBackend::new(script);
    let target = ForumTarget {
        chat: Some(forum),
        sends: Sends::Check,
    };
    run(&backend, None, Sends::Check, target).await;
    assert_eq!(
        fake.calls(super::fake::Op::Topics),
        3,
        "two pages and the non-forum"
    );
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
    let target = std::env::var("ABYSS_CONTRACT_CHAT")
        .ok()
        .map(|s| parse_chat(&s));
    let sends = if target.is_some() {
        Sends::Check
    } else {
        Sends::Skip
    };
    // `ABYSS_CONTRACT_FORUM=channel:<id>` also posts one message into a topic
    // of that forum; otherwise the first forum of the first page is only read.
    let forum = std::env::var("ABYSS_CONTRACT_FORUM")
        .ok()
        .map(|s| parse_chat(&s));
    let forum = ForumTarget {
        chat: forum,
        sends: if forum.is_some() {
            Sends::Check
        } else {
            Sends::Skip
        },
    };
    run(&backend, target, sends, forum).await;
}

#[cfg(target_os = "macos")]
fn parse_chat(s: &str) -> ChatId {
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
}
