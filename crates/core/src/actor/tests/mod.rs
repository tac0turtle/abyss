//! Actor tests against the fake backend. Paused time unless the disk cache is
//! involved (its worker is an OS thread, which paused time cannot see).

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test harness helpers are not #[test] items, so allow-*-in-tests does not cover them"
)]

mod auth;
mod folders;
mod lifecycle;
mod logout;
mod onboarding;
mod topic_keys;
mod topics;
mod watchdog;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{Actor, ActorInit, Identity, Internal, Stamped, provision_cache};
use crate::ShutdownReport;
use crate::api::{AuthState, Command, ConnState, Event, HistoryDir, SendState, UserError};
use crate::backend::fake::{
    FAKE_SELF_HASH, FAKE_SELF_ID, FakeBackend, FakeHandle, FakeScript, Fault, Op, text_message,
};
use crate::backend::{BackendError, RejectReason, Update};
use crate::cache::{CacheHandle, CacheKey};
use crate::events::{self, EventReceiver};
use crate::model::{ChatId, LocalId, MsgId, PeerKind, ThreadKey};
use crate::secret::{FaultControl, FaultyStore, MemoryStore, SecretItem, SecretStore};
use crate::session::{PersistHandle, signed_in_blob};

const WAIT: Duration = Duration::from_secs(600);
/// Wall-clock bound of one wait, whatever the test's clock does (a
/// [`watchdog::Watchdog`] thread enforces it).
const REAL_WAIT: Duration = Duration::from_secs(120);

struct Harness {
    cmd: mpsc::Sender<Stamped>,
    internal: mpsc::Sender<Internal>,
    events: EventReceiver,
    fake: FakeHandle,
    /// The actor's admission semaphore, observed while it runs.
    permits: Arc<Semaphore>,
    cancel: CancellationToken,
    join: JoinHandle<ShutdownReport>,
    /// Shares storage with the persistence worker's store.
    store: MemoryStore,
    faults: FaultControl,
    watchdog: watchdog::Watchdog,
}

fn chat(id: i64) -> ChatId {
    ChatId {
        kind: PeerKind::User,
        id,
    }
}

/// How the harness prepares storage before the actor starts.
#[derive(Default)]
struct Setup {
    /// No stored session: the actor starts signed out.
    signed_out: bool,
    /// A cache handle opened by the test (no key bookkeeping).
    cache: Option<CacheHandle>,
    /// Open the cache here through the startup path (key from the store).
    cache_dir: Option<PathBuf>,
    /// Hold the store after the startup reads: every later write stalls.
    hold_store: bool,
    /// A stored API credential item (task 6b).
    stored_creds: Option<crate::api::ApiCredentials>,
    /// Reading the credential item fails (a locked Keychain).
    creds_locked: bool,
    /// Reuse this store (a restart) instead of a new one; `signed_out` is
    /// then ignored.
    store: Option<MemoryStore>,
}

/// The store the harness starts over.
fn prepare_store(setup: &Setup) -> MemoryStore {
    let Some(store) = setup.store.clone() else {
        let store = MemoryStore::new();
        if !setup.signed_out {
            let blob = signed_in_blob(FAKE_SELF_ID, FAKE_SELF_HASH);
            store.set(SecretItem::Session, &blob).unwrap();
        }
        return store;
    };
    store
}

/// Loads the session as `core::prepare` does.
async fn load(persist: &PersistHandle) -> (crate::session::SessionSnapshot, Identity) {
    match persist.load_session().await {
        Ok(loaded) => {
            let identity = loaded
                .snapshot
                .self_id()
                .unwrap()
                .map_or(Identity::SignedOut, Identity::Verified);
            (loaded.snapshot, identity)
        }
        Err(_) => (persist.fresh_session(), Identity::Unknown),
    }
}

async fn harness(script: FakeScript, cache: Option<CacheHandle>) -> Harness {
    let (backend, fake) = FakeBackend::new(script);
    harness_with(backend, fake, cache).await
}

/// Starts an actor over a prepared fake (faults may be queued before the first build).
async fn harness_with(
    backend: FakeBackend,
    fake: FakeHandle,
    cache: Option<CacheHandle>,
) -> Harness {
    let setup = Setup {
        cache,
        ..Setup::default()
    };
    start(backend, fake, setup).await
}

/// The store faults that apply to the startup reads.
fn startup_faults(setup: &Setup, faults: &FaultControl) {
    if setup.creds_locked {
        faults.fail_get_of(SecretItem::ApiCredentials);
    }
}

/// Writes the stored API credential item, if the setup has one.
fn store_creds(setup: &Setup, store: &MemoryStore) {
    if let Some(creds) = &setup.stored_creds {
        store
            .set(
                SecretItem::ApiCredentials,
                &crate::credentials::encode(creds),
            )
            .unwrap();
    }
}

async fn start(backend: FakeBackend, fake: FakeHandle, setup: Setup) -> Harness {
    let store = prepare_store(&setup);
    store_creds(&setup, &store);
    let (wrapped, faults) = FaultyStore::new(store.clone());
    startup_faults(&setup, &faults);
    let persist = PersistHandle::spawn(wrapped).unwrap();
    let pending_logout = super::read_pending_logout(&persist).await;
    let (account, identity) = match pending_logout {
        Some(_) => (persist.fresh_session(), Identity::Unknown),
        None => load(&persist).await,
    };
    let (cache, cache_parts) = match setup.cache_dir {
        Some(dir) => {
            let provisioned = provision_cache(&persist, dir, identity).await;
            (provisioned.served, Some(provisioned.parts))
        }
        None => (setup.cache, None),
    };
    if setup.hold_store {
        faults.hold();
    }
    let (cmd, cmd_rx) = mpsc::channel(256);
    let (internal, internal_rx) = mpsc::channel(8);
    let (events_tx, events) = events::channel();
    let cancel = CancellationToken::new();
    let (actor, channels) = Actor::new(ActorInit {
        backend: Arc::new(backend),
        events: events_tx,
        cache,
        commands: cmd_rx,
        internal: internal_rx,
        cancel: cancel.clone(),
        persist,
        account,
        cache_parts,
        pending_logout,
    });

    let permits = actor.admission.probe();
    let join = tokio::spawn(actor.run(channels));
    let watchdog = watchdog::Watchdog::new(faults.clock_gate(), REAL_WAIT);
    Harness {
        cmd,
        internal,
        events,
        fake,
        permits,
        cancel,
        join,
        store,
        faults,
        watchdog,
    }
}

impl Harness {
    /// Sends a command stamped with the epoch of the last event taken, as
    /// `CoreSender` does.
    async fn send(&self, c: Command) {
        let epoch = self.events.observer().get();
        self.cmd.send(Stamped { epoch, cmd: c }).await.unwrap();
    }

    /// Waits for an event matching `pred`, returning it and everything skipped.
    /// Bounded twice: [`WAIT`] on the test's clock (paused tests advance it
    /// freely) and the watchdog's wall-time bound, which holds even while the
    /// store's clock gate keeps a paused clock still (review round 32), so a
    /// hang fails the test with the events seen instead of stalling the run.
    async fn until(&mut self, pred: impl Fn(&Event) -> bool) -> (Event, Vec<Event>) {
        let mut skipped = Vec::new();
        let deadline = tokio::time::Instant::now() + WAIT;
        let fired = self.watchdog.fired();
        let _armed = self.watchdog.arm();
        loop {
            let next = tokio::select! {
                biased;
                () = fired.cancelled() => {
                    panic!("wall-clock watchdog fired waiting for an event; seen: {skipped:?}")
                }
                next = tokio::time::timeout_at(deadline, self.events.recv()) => next,
            };
            match next {
                Ok(Some(ev)) if pred(&ev) => return (ev, skipped),
                Ok(Some(ev)) => skipped.push(ev),
                Ok(None) => panic!("actor stopped; seen: {skipped:?}"),
                Err(elapsed) => panic!("{elapsed} waiting for an event; seen: {skipped:?}"),
            }
        }
    }

    /// Waits until the instance is online; returns the events before it.
    async fn online(&mut self) -> Vec<Event> {
        self.until(|e| matches!(e, Event::Connection(ConnState::Online)))
            .await
            .1
    }

    async fn history(&mut self, c: ChatId, d: HistoryDir) -> Event {
        self.until(|e| matches!(e, Event::History { thread, dir, .. } if *thread == ThreadKey::whole(c) && *dir == d))
            .await
            .0
    }

    /// Takes every queued event without waiting.
    fn drain(&mut self) -> Vec<Event> {
        std::iter::from_fn(|| self.events.try_recv()).collect()
    }

    /// Shuts the actor down; fails past the watchdog's wall-time bound
    /// rather than hang (review rounds 28 and 32).
    async fn stop(mut self) -> ShutdownReport {
        self.cancel.cancel();
        let fired = self.watchdog.fired();
        let _armed = self.watchdog.arm();
        tokio::select! {
            biased;
            () = fired.cancelled() => panic!("the actor did not shut down"),
            report = &mut self.join => report.unwrap(),
        }
    }

    /// Waits for an auth state matching `pred`.
    async fn auth(&mut self, pred: impl Fn(&AuthState) -> bool) -> AuthState {
        match self
            .until(|e| matches!(e, Event::Auth(s) if pred(s)))
            .await
            .0
        {
            Event::Auth(s) => s,
            _ => unreachable!(),
        }
    }
}

fn history_ids(ev: &Event) -> Vec<i32> {
    match ev {
        Event::History { messages, .. } => messages.iter().map(|m| m.id.0).collect(),
        _ => Vec::new(),
    }
}

#[tokio::test(start_paused = true)]
async fn loads_dialogs_and_tail() {
    let mut h = harness(FakeScript::demo(3, 120), None).await;
    let (ev, _) = h.until(|e| matches!(e, Event::DialogsReset(_))).await;
    assert!(matches!(ev, Event::DialogsReset(v) if v.len() == 3));
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    let ev = h.history(chat(1), HistoryDir::Tail).await;
    assert_eq!(history_ids(&ev), (71..=120).collect::<Vec<_>>());
    assert!(matches!(ev, Event::History { has_more: true, .. }));
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    let ev = h.history(chat(1), HistoryDir::Older).await;
    assert_eq!(history_ids(&ev), (21..=70).collect::<Vec<_>>());
    h.stop().await;
}

fn dialogs_len(ev: &Event) -> usize {
    match ev {
        Event::DialogsReset(v) | Event::DialogsAppended(v) => v.len(),
        _ => 0,
    }
}

#[tokio::test(start_paused = true)]
async fn refresh_merges_the_first_page_into_loaded_pages() {
    let mut h = harness(FakeScript::demo(250, 0), None).await;
    h.online().await;
    for expected in [100, 50] {
        h.send(Command::LoadDialogs).await;
        let (ev, _) = h.until(|e| matches!(e, Event::DialogsAppended(_))).await;
        assert_eq!(dialogs_len(&ev), expected);
    }
    h.until(|e| matches!(e, Event::DialogsPaging { has_more: false }))
        .await;
    // While the app was away, a chat appeared below the first page.
    let newcomer = chat(9999);
    h.fake.edit_dialogs(|d| {
        d.insert(150, crate::backend::fake::summary(newcomer, "new"));
    });
    tokio::time::sleep(Duration::from_secs(16)).await;
    h.send(Command::Window(crate::api::WindowState::Active))
        .await;
    let (ev, _) = h.until(|e| matches!(e, Event::DialogsReset(_))).await;
    assert_eq!(dialogs_len(&ev), 250, "later pages survive a refresh");
    // Pagination restarts from the fresh cursor instead of staying terminal.
    h.until(|e| matches!(e, Event::DialogsPaging { has_more: true }))
        .await;
    // The newcomer is placed in server order ahead of provisional rows, so
    // that page resends the whole list.
    let mut appended = Vec::new();
    for _ in 0..2 {
        h.send(Command::LoadDialogs).await;
        let (ev, _) = h
            .until(|e| matches!(e, Event::DialogsAppended(_) | Event::DialogsReset(_)))
            .await;
        if let Event::DialogsAppended(v) | Event::DialogsReset(v) = ev {
            appended.extend(v);
        }
    }
    assert!(
        appended.iter().any(|c| c.id == newcomer),
        "the chat below the first page is found"
    );
    let (ev, _) = h.until(|e| matches!(e, Event::DialogsPaging { .. })).await;
    assert!(matches!(ev, Event::DialogsPaging { has_more: false }));
    let calls = h.fake.calls(Op::Dialogs);
    h.send(Command::LoadDialogs).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(h.fake.calls(Op::Dialogs), calls, "every page is loaded");
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn commands_before_online_wait_for_the_bootstrap() {
    let mut h = harness(FakeScript::demo(2, 10), None).await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    let ev = h.history(chat(1), HistoryDir::Tail).await;
    assert_eq!(history_ids(&ev).len(), 10);
    h.online().await;
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn stale_completion_after_close_is_dropped() {
    let mut script = FakeScript::demo(3, 10);
    script.latency = Duration::from_secs(1);
    let mut h = harness(script, None).await;
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h.send(Command::CloseThread(ThreadKey::whole(chat(1))))
        .await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(2)))).await;
    let (_, skipped) = h
        .until(|e| matches!(e, Event::History { thread: c, .. } if *c == ThreadKey::whole(chat(2))))
        .await;
    assert!(
        !skipped.iter().any(
            |e| matches!(e, Event::History { thread: c, .. } if *c == ThreadKey::whole(chat(1)))
        )
    );
    assert_eq!(h.fake.calls(Op::History), 2);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn panic_in_backend_is_contained() {
    let mut h = harness(FakeScript::demo(2, 10), None).await;
    h.fake.inject(Op::History, Fault::Panic);
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h.until(|e| matches!(e, Event::Error(UserError::Internal)))
        .await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    let ev = h.history(chat(1), HistoryDir::Tail).await;
    assert_eq!(history_ids(&ev).len(), 10);
    h.stop().await;
}

fn send_state(id: LocalId) -> impl Fn(&Event) -> bool {
    move |e| matches!(e, Event::SendState { local_id, state } if *local_id == id && *state != SendState::Sending)
}

#[tokio::test(start_paused = true)]
async fn send_retries_transient_with_same_random_id() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    for _ in 0..2 {
        h.fake
            .inject(Op::Send, Fault::Error(BackendError::Transient));
    }
    h.send(Command::SendText {
        thread: ThreadKey::whole(chat(1)),
        text: "hi".into(),
        local_id: LocalId(1),
    })
    .await;
    let (ev, _) = h.until(send_state(LocalId(1))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::Sent { id: MsgId(2) },
            ..
        }
    ));
    let sent = h.fake.sent();
    assert_eq!(sent.len(), 1, "only the successful attempt is recorded");
    assert_eq!(h.fake.calls(Op::Send), 3);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn send_unknown_after_retries_then_manual_retry() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    for _ in 0..3 {
        h.fake.inject(Op::Send, Fault::Hang);
    }
    h.send(Command::SendText {
        thread: ThreadKey::whole(chat(1)),
        text: "hi".into(),
        local_id: LocalId(7),
    })
    .await;
    let (ev, _) = h.until(send_state(LocalId(7))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::Unknown { attempts: 3 },
            ..
        }
    ));
    h.send(Command::RetrySend(LocalId(7))).await;
    let (ev, _) = h.until(send_state(LocalId(7))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::Sent { .. },
            ..
        }
    ));
    h.send(Command::RetrySend(LocalId(7))).await;
    h.until(|e| matches!(e, Event::Error(UserError::NotFound)))
        .await;
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn rejected_send_fails_and_empty_text_is_refused() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
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
    let (ev, _) = h.until(send_state(LocalId(1))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::Failed,
            ..
        }
    ));
    h.send(Command::SendText {
        thread: ThreadKey::whole(chat(1)),
        text: "  ".into(),
        local_id: LocalId(2),
    })
    .await;
    // Refused: core holds nothing for it (task 6j).
    let (_, seen) = h
        .until(|e| matches!(e, Event::SendRefused { local_id } if *local_id == LocalId(2)))
        .await;
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, Event::SendState { local_id, .. } if *local_id == LocalId(2))),
        "{seen:?}"
    );
    assert_eq!(h.fake.calls(Op::Send), 1);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn live_messages_extend_open_window_and_bump_others() {
    let mut h = harness(FakeScript::demo(2, 10), None).await;
    h.until(|e| matches!(e, Event::DialogsReset(_))).await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h.history(chat(1), HistoryDir::Tail).await;
    h.fake
        .push_update(Update::NewMessage(text_message(chat(1), 11, "new")))
        .await;
    let ev = h.history(chat(1), HistoryDir::Newer).await;
    assert_eq!(history_ids(&ev), vec![11]);
    h.fake
        .push_update(Update::NewMessage(text_message(chat(2), 11, "other")))
        .await;
    let (ev, _) = h
        .until(|e| matches!(e, Event::ChatBumped(s) if s.id == chat(2)))
        .await;
    assert!(matches!(ev, Event::ChatBumped(s) if s.unread == 1));
    h.fake
        .push_update(Update::Deleted {
            chat: Some(chat(1)),
            ids: vec![MsgId(11), MsgId(99)],
        })
        .await;
    let (ev, _) = h.until(|e| matches!(e, Event::Deleted { .. })).await;
    assert!(matches!(ev, Event::Deleted { ids, .. } if ids == vec![MsgId(11)]));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn mark_read_coalesces_per_flush() {
    let h = harness(FakeScript::demo(1, 10), None).await;
    for id in [3, 7, 5] {
        h.send(Command::MarkRead {
            thread: ThreadKey::whole(chat(1)),
            max_id: MsgId(id),
        })
        .await;
    }
    tokio::time::sleep(super::READ_FLUSH * 3).await;
    assert_eq!(
        h.fake.read_calls(),
        vec![(ThreadKey::whole(chat(1)), MsgId(7))]
    );
    h.stop().await;
}

// ---- disk cache (real time) ----

/// A temporary cache directory. Declared before the handles, it drops after
/// them, and its drop waits until every worker on it has closed its
/// database: the test's own handles, which `close_caches` only flushes, the
/// actor's, and any open still on a blocking thread. So no SQLCipher close
/// outlives its test and races the harness's exit (plan section 13, exit
/// SIGSEGV fix).
struct CacheDir(tempfile::TempDir);

impl CacheDir {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }

    fn path(&self) -> &std::path::Path {
        self.0.path()
    }
}

impl Drop for CacheDir {
    fn drop(&mut self) {
        let closed = crate::cache::wait_closed(Some(self.path()), Duration::from_secs(10));
        if !std::thread::panicking() {
            assert!(closed, "a cache worker outlived its test");
        }
    }
}

fn cache_at(dir: &std::path::Path, key: &CacheKey) -> CacheHandle {
    CacheHandle::spawn(dir.to_path_buf(), key).unwrap()
}

/// Once online, opens chat 1 and waits until the server tail is applied: after
/// the cached tail, if the cache holds one.
async fn open_settled(h: &mut Harness, cache: &CacheHandle) {
    let cached = !cache
        .tail(ThreadKey::whole(chat(1)), 1)
        .await
        .unwrap()
        .is_empty();
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    for _ in 0..=usize::from(cached) {
        h.history(chat(1), HistoryDir::Tail).await;
    }
}

/// Opens chat 1, scrolls up `older` pages, then stops and flushes the cache.
async fn session(script: FakeScript, cache: CacheHandle, older: usize) -> FakeHandle {
    let mut h = harness(script, Some(cache.clone())).await;
    open_settled(&mut h, &cache).await;
    for _ in 0..older {
        h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
        h.history(chat(1), HistoryDir::Older).await;
    }
    let fake = h.fake.clone();
    h.stop().await;
    cache.barrier().await.unwrap();
    fake
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_serves_history_offline_then_reconciles() {
    let dir = CacheDir::new();
    let key = CacheKey::generate().unwrap();
    session(FakeScript::demo(1, 200), cache_at(dir.path(), &key), 2).await;

    // Second run: the server hangs, so only the cache can answer.
    let cache = cache_at(dir.path(), &key);
    let mut h = harness(FakeScript::demo(1, 200), Some(cache.clone())).await;
    h.fake.inject(Op::History, Fault::Hang);
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    let ev = h.history(chat(1), HistoryDir::Tail).await;
    assert_eq!(history_ids(&ev), (151..=200).collect::<Vec<_>>());
    // A rebuild meanwhile shows the cached tail again rather than nothing.
    h.internal.send(Internal::Wake).await.unwrap();
    let (_, skipped) = h
        .until(|e| {
            matches!(
                e,
                Event::History {
                    dir: HistoryDir::Tail,
                    ..
                }
            )
        })
        .await;
    assert!(
        skipped
            .iter()
            .any(|e| matches!(e, Event::ThreadInvalidated(c) if *c == ThreadKey::whole(chat(1))))
    );
    h.stop().await;

    // Third run: the server deleted message 190; the tail reconcile removes it.
    let mut script = FakeScript::demo(1, 200);
    script
        .history
        .get_mut(&chat(1))
        .unwrap()
        .retain(|m| m.id != MsgId(190));
    let cache = cache_at(dir.path(), &key);
    session(script, cache.clone(), 0).await;
    let tail = cache.tail(ThreadKey::whole(chat(1)), 50).await.unwrap();
    assert!(!tail.iter().any(|m| m.id == MsgId(190)));
    assert_eq!(tail.last().map(|m| m.id), Some(MsgId(200)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contiguous_cache_serves_older_pages_without_the_server() {
    let dir = CacheDir::new();
    let key = CacheKey::generate().unwrap();
    session(FakeScript::demo(1, 300), cache_at(dir.path(), &key), 3).await;

    let cache = cache_at(dir.path(), &key);
    let mut h = harness(FakeScript::demo(1, 300), Some(cache.clone())).await;
    open_settled(&mut h, &cache).await;
    let server_calls = h.fake.calls(Op::History);
    h.send(Command::LoadOlder(ThreadKey::whole(chat(1)))).await;
    let ev = h.history(chat(1), HistoryDir::Older).await;
    assert_eq!(history_ids(&ev), (201..=250).collect::<Vec<_>>());
    assert_eq!(
        h.fake.calls(Op::History),
        server_calls,
        "older page must come from the cache"
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gap_in_cache_is_replaced_by_server_tail() {
    let dir = CacheDir::new();
    let key = CacheKey::generate().unwrap();
    session(FakeScript::demo(1, 100), cache_at(dir.path(), &key), 1).await;

    // 200 more messages arrived while offline: the new tail does not overlap.
    let cache = cache_at(dir.path(), &key);
    session(FakeScript::demo(1, 300), cache.clone(), 0).await;
    let older = cache
        .before(ThreadKey::whole(chat(1)), MsgId(251), 50)
        .await
        .unwrap();
    assert!(
        older.is_empty(),
        "stale pre-gap history must be dropped, got {}",
        older.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn erase_runs_after_queued_writes() {
    let dir = CacheDir::new();
    let key = CacheKey::generate().unwrap();
    let cache = cache_at(dir.path(), &key);
    for i in 1..=100 {
        cache
            .write(crate::cache::CacheCmd::PutMessages(
                ThreadKey::whole(chat(1)),
                vec![text_message(chat(1), i, "x")],
            ))
            .await;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let res = cache
        .erase(
            CacheKey::from_bytes(zeroize::Zeroizing::new(*key.as_bytes())),
            Box::new(move || tx.send(()).map_err(Into::into)),
        )
        .await;
    assert!(res.is_ok(), "erase failed: {res:?}");
    assert!(rx.try_recv().is_ok(), "key deletion ran");
    assert!(
        cache.tail(ThreadKey::whole(chat(1)), 10).await.is_err(),
        "worker stops after erase"
    );
    assert!(std::fs::read_dir(dir.path()).map_or(true, |d| d.count() == 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_chat_list_keeps_every_loaded_page() {
    let dir = CacheDir::new();
    let key = CacheKey::generate().unwrap();
    let cache = cache_at(dir.path(), &key);
    let mut h = harness(FakeScript::demo(150, 1), Some(cache.clone())).await;
    h.until(|e| matches!(e, Event::DialogsReset(_))).await;
    h.send(Command::LoadDialogs).await;
    let (ev, _) = h.until(|e| matches!(e, Event::DialogsAppended(_))).await;
    assert!(matches!(ev, Event::DialogsAppended(v) if v.len() == 50));
    h.stop().await;
    cache.barrier().await.unwrap();
    assert_eq!(cache.chats().await.unwrap().len(), 150);
}
