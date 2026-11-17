//! Scripted in-process backend for development and tests (`fake` feature only).
//!
//! Every build yields a fresh instance over one shared fake server. Supports
//! per-call fault injection (error, hang forever, panic, delay), lifecycle faults
//! (failed build, missing baseline, runner exit or panic, a runner that is slow
//! to quit or ignores `quit`), stream faults (recoverable or fatal error, panic),
//! fixed latency, live updates pushed by the test, and counters.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use grammers_session::Session as _;
use tokio::sync::{Notify, mpsc};

use super::{
    Account, AuthError, Backend, BackendError, CheckPassword, Client, CodeRequest, DialogsCursor,
    DialogsPage, HistoryPage, HistoryQuery, Instance, InstanceControl, PasswordChallenge, Seed,
    SentCode, SessionSeed, SessionSnapshot, SignIn, StreamError, StreamItem, Update, UpdateStream,
};
use crate::api::{ApiCredentials, CodeDelivery, NextDelivery, PasswordBuf};
use crate::credentials::{CredentialSlot, EnvResolution};
use crate::model::{Body, ChatId, ChatKind, ChatSummary, Message, MsgId, PeerKind, ThreadKey};
use crate::rich_text::RichText;
use crate::session::AbyssSession;

mod auth;

pub use auth::{FAKE_SELF_HASH, FAKE_SELF_ID};

/// Maximum faults queued per operation.
const MAX_FAULTS: usize = 1024;
/// Capacity of the pushed-update channel (shared by every instance's stream).
const UPDATE_CAPACITY: usize = 256;
/// Recorded sends, seeds and other logs are capped at this many entries.
const MAX_LOG: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    Dialogs,
    History,
    Send,
    Read,
    IsAuthorized,
    RequestCode,
    /// `auth.resendCode` (task 6b).
    ResendCode,
    SignIn,
    CheckPassword,
    PasswordChallenge,
    FetchSelf,
    SignOut,
}

#[derive(Clone, Copy, Debug)]
pub enum Fault {
    Error(BackendError),
    /// A login-specific error (data calls see it as `Rejected`).
    Auth(AuthError),
    /// Never completes (a stalled transport).
    Hang,
    /// The server applies the call, but the answer never arrives (an
    /// ambiguous completion). Sends are accepted (a message exists on the
    /// server) and then hang; other data calls just hang.
    CompleteThenHang,
    /// Panics inside the call (a bug in the Telegram library).
    Panic,
    /// Succeeds after the given extra delay.
    Delay(Duration),
    /// Login calls (`sign_in`, `check_password`): the request sits in the
    /// sender's queue and the server applies it after the delay, whether or
    /// not the caller still waits (grammers keeps a request queued after its
    /// future is dropped). Retiring the instance first drops it, as quitting
    /// the runner drops the queue. Data calls treat it as `Delay`.
    Queued(Duration),
}

/// A fault applied to the next build.
#[derive(Clone, Copy, Debug)]
pub enum BuildFault {
    /// The build itself fails.
    Fail(BackendError),
    /// The stream never reports a baseline (`getState` failed).
    NoBaseline,
}

/// How the current runner reacts to `quit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuitBehavior {
    Prompt,
    /// Stops after the given delay.
    Slow(Duration),
    /// Never stops by itself; only abort ends it.
    Ignore,
}

/// Initial data for a fake account.
#[derive(Clone, Debug)]
pub struct FakeScript {
    pub dialogs: Vec<ChatSummary>,
    /// Per chat, ascending by id.
    pub history: HashMap<ChatId, Vec<Message>>,
    pub latency: Duration,
    pub login: FakeLogin,
    /// What the environment says about credentials. The default is a valid
    /// env pair ([`fake_credentials`]), so the fake starts at the phone
    /// screen; `Absent` starts onboarding (`--fake --onboarding`).
    pub env: EnvResolution,
}

impl Default for FakeScript {
    fn default() -> Self {
        Self {
            dialogs: Vec::new(),
            history: HashMap::new(),
            latency: Duration::ZERO,
            login: FakeLogin::default(),
            env: EnvResolution::Pair(fake_credentials()),
        }
    }
}

/// A valid-looking pair the fake server accepts.
pub fn fake_credentials() -> ApiCredentials {
    ApiCredentials::fake(FAKE_API_ID)
}

/// The `api_id` of [`fake_credentials`].
pub const FAKE_API_ID: i32 = 1_000_000;
/// An `api_id` the fake server answers with `API_ID_INVALID`.
pub const FAKE_INVALID_API_ID: i32 = 1;
/// An `api_id` the fake server answers with `API_ID_PUBLISHED_FLOOD`.
pub const FAKE_PUBLISHED_API_ID: i32 = 2;

/// How the fake server answers a login.
#[derive(Clone, Debug)]
pub struct FakeLogin {
    /// The code every `request_login_code` "sends".
    pub code: String,
    /// The 2FA password, if the account has one.
    pub password: Option<String>,
    pub hint: Option<String>,
    /// The number has no account: `sign_in` answers `SignUpRequired`.
    pub sign_up_required: bool,
    pub display_name: String,
    /// The self user a login caches (the account the session belongs to).
    pub self_id: i64,
    /// Where a first code request says the code went. `None` scripts an
    /// unsupported flow (email setup): `OfficialAppRequired`.
    pub sent: Option<SentCode>,
}

impl Default for FakeLogin {
    fn default() -> Self {
        Self {
            code: FAKE_CODE.into(),
            password: None,
            hint: None,
            sign_up_required: false,
            display_name: "Fake User".into(),
            self_id: auth::FAKE_SELF_ID,
            sent: Some(SentCode {
                delivery: CodeDelivery::App,
                length: Some(5),
                next: Some(NextDelivery::Sms),
                timeout: Some(Duration::from_secs(60)),
            }),
        }
    }
}

/// The fake's login code.
pub const FAKE_CODE: &str = "12345";
/// The demo account's 2FA password (`FakeScript::demo`).
pub const DEMO_PASSWORD: &str = "abyss";

impl FakeScript {
    /// A small demo account for running the app without credentials. Its
    /// login code is [`FAKE_CODE`] and its 2FA password [`DEMO_PASSWORD`].
    pub fn demo(chats: u32, messages_per_chat: u32) -> Self {
        let mut script = FakeScript {
            login: FakeLogin {
                password: Some(DEMO_PASSWORD.into()),
                hint: Some(format!("demo: {DEMO_PASSWORD}")),
                display_name: "Demo User".into(),
                ..FakeLogin::default()
            },
            ..FakeScript::default()
        };
        for i in 1..=i64::from(chats) {
            let id = ChatId {
                kind: if i % 3 == 0 {
                    PeerKind::Channel
                } else {
                    PeerKind::User
                },
                id: i,
            };
            script.dialogs.push(summary(id, &format!("Chat {i}")));
            let count = i32::try_from(messages_per_chat).unwrap_or(i32::MAX);
            let msgs = (1..=count)
                .map(|m| text_message(id, m, &format!("Message {m} in chat {i}")))
                .collect();
            script.history.insert(id, msgs);
        }
        script
    }
}

pub fn summary(id: ChatId, title: &str) -> ChatSummary {
    ChatSummary {
        id,
        title: crate::sanitize::display_name(title),
        kind: if id.kind == PeerKind::Channel {
            ChatKind::Supergroup {
                forum: false,
                gigagroup: false,
            }
        } else {
            ChatKind::Private
        },
        pinned: false,
        unread: 0,
        unread_mentions: 0,
        last: None,
        last_date: 0,
        last_id: None,
        can_send: true,
    }
}

pub fn text_message(chat: ChatId, id: i32, text: &str) -> Message {
    Message {
        id: MsgId(id),
        thread: ThreadKey { chat, topic: None },
        sender: None,
        date: i64::from(id),
        edit_date: None,
        outgoing: false,
        reply_to: None,
        body: Body::Text(RichText::plain(text)),
    }
}

/// What the test asks the current runner to do.
#[derive(Clone, Copy, Debug)]
enum RunnerCmd {
    Exit,
    Panic,
}

/// What the shared stream channel carries.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "test-only channel; updates are the common item"
)]
enum StreamCmd {
    Update(Update),
    Error(StreamError),
    Panic,
}

#[derive(Debug, Default)]
struct State {
    dialogs: Vec<ChatSummary>,
    history: HashMap<ChatId, Vec<Message>>,
    faults: HashMap<Op, VecDeque<Fault>>,
    /// Extra waits before the next calls' faults or answers (`lead`).
    leads: HashMap<Op, VecDeque<Duration>>,
    build_faults: VecDeque<BuildFault>,
    calls: HashMap<Op, usize>,
    sent: Vec<(ChatId, i64)>,
    /// Server-side dedup: every `random_id` the server accepted, with the
    /// message it created. Telegram deduplicates by `random_id` per sender.
    accepted: HashMap<i64, (ChatId, MsgId)>,
    attempted: Vec<(ChatId, i64)>,
    /// The last `Client::retain_peers` set.
    retained: Option<Vec<ChatId>>,
    read: Vec<(ChatId, MsgId)>,
    seeds: Vec<Seed>,
    /// The most recently retired instance's session, for late-write tests.
    retired_session: Option<Arc<AbyssSession>>,
    quit: Option<QuitBehavior>,
    runner: Option<mpsc::Sender<RunnerCmd>>,
    /// The account's critical state, from `Backend::use_account`.
    account: Option<crate::session::SessionSnapshot>,
    auth: auth::ServerAuth,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    latency: Duration,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    runners_finished: AtomicUsize,
    retired: AtomicUsize,
    revoked: AtomicUsize,
}

/// Test-side control of a running [`FakeBackend`].
#[derive(Clone, Debug)]
pub struct FakeHandle {
    shared: Arc<Shared>,
    stream: mpsc::Sender<StreamCmd>,
}

#[derive(Debug)]
pub struct FakeBackend {
    shared: Arc<Shared>,
    stream: Arc<tokio::sync::Mutex<mpsc::Receiver<StreamCmd>>>,
    /// The pair in use; a build waits for one (plan section 17.2a).
    slot: CredentialSlot,
}

impl FakeBackend {
    pub fn new(script: FakeScript) -> (FakeBackend, FakeHandle) {
        let (tx, rx) = mpsc::channel(UPDATE_CAPACITY);
        let slot = CredentialSlot::new(script.env.clone());
        // Direct builds (the contract suite) need no actor to resolve an env pair.
        if let EnvResolution::Pair(creds) = script.env {
            slot.set(Some(creds));
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                dialogs: script.dialogs,
                history: script.history,
                auth: auth::ServerAuth::new(script.login),
                ..State::default()
            }),
            latency: script.latency,
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            runners_finished: AtomicUsize::new(0),
            retired: AtomicUsize::new(0),
            revoked: AtomicUsize::new(0),
        });
        let handle = FakeHandle {
            shared: Arc::clone(&shared),
            stream: tx,
        };
        let backend = FakeBackend {
            shared,
            stream: Arc::new(tokio::sync::Mutex::new(rx)),
            slot,
        };
        (backend, handle)
    }
}

#[expect(
    clippy::panic,
    reason = "deliberate fault injection for panic-containment tests"
)]
fn injected_panic(what: &str) -> ! {
    panic!("injected fake backend panic in {what}")
}

impl Shared {
    fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut guard)
    }
}

fn push_capped<T>(log: &mut Vec<T>, item: T) {
    if log.len() < MAX_LOG {
        log.push(item);
    }
}

/// Tracks concurrent calls for admission tests.
struct InFlight<'a>(&'a Shared);

impl<'a> InFlight<'a> {
    fn new(shared: &'a Shared) -> Self {
        let now = shared.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        shared.max_in_flight.fetch_max(now, Ordering::SeqCst);
        Self(shared)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Counts runner completions, including cancellation (the future is dropped).
struct RunnerGuard(Arc<Shared>);

impl Drop for RunnerGuard {
    fn drop(&mut self) {
        self.0.runners_finished.fetch_add(1, Ordering::SeqCst);
    }
}

impl FakeHandle {
    /// Queues a one-shot fault for the next call of `op`.
    pub fn inject(&self, op: Op, fault: Fault) {
        self.shared.with(|s| {
            let q = s.faults.entry(op).or_default();
            if q.len() < MAX_FAULTS {
                q.push_back(fault);
            }
        });
    }

    /// Queues a one-shot wait for the next data call of `op`, before its
    /// fault (if any) applies: with `inject`, a failure or panic that comes
    /// late.
    pub fn lead(&self, op: Op, wait: Duration) {
        self.shared.with(|s| {
            let q = s.leads.entry(op).or_default();
            if q.len() < MAX_FAULTS {
                q.push_back(wait);
            }
        });
    }

    /// Queues a fault for the next build.
    pub fn on_next_build(&self, fault: BuildFault) {
        self.shared.with(|s| {
            if s.build_faults.len() < MAX_FAULTS {
                s.build_faults.push_back(fault);
            }
        });
    }

    /// Sets how runners react to `quit` from now on.
    pub fn set_quit(&self, behavior: QuitBehavior) {
        self.shared.with(|s| s.quit = Some(behavior));
    }

    /// Makes the current runner return (the connection driver died).
    pub fn runner_exit(&self) {
        self.runner_cmd(RunnerCmd::Exit);
    }

    /// Makes the current runner panic (a connection-task panic re-raised).
    pub fn runner_panic(&self) {
        self.runner_cmd(RunnerCmd::Panic);
    }

    fn runner_cmd(&self, cmd: RunnerCmd) {
        if let Some(tx) = self.shared.with(|s| s.runner.clone()) {
            let _ = tx.try_send(cmd);
        }
    }

    /// Delivers a live update on whichever stream is current (or the next one).
    pub async fn push_update(&self, update: Update) {
        let _ = self.stream.send(StreamCmd::Update(update)).await;
    }

    /// The current stream's next `next` call fails with `e`.
    pub async fn stream_error(&self, e: StreamError) {
        let _ = self.stream.send(StreamCmd::Error(e)).await;
    }

    /// The current stream's next `next` call panics.
    pub async fn stream_panic(&self) {
        let _ = self.stream.send(StreamCmd::Panic).await;
    }

    pub fn calls(&self, op: Op) -> usize {
        self.shared.with(|s| s.calls.get(&op).copied().unwrap_or(0))
    }

    pub fn in_flight(&self) -> usize {
        self.shared.in_flight.load(Ordering::SeqCst)
    }

    pub fn max_in_flight(&self) -> usize {
        self.shared.max_in_flight.load(Ordering::SeqCst)
    }

    /// Number of builds attempted (including failed ones).
    pub fn builds(&self) -> usize {
        self.shared.with(|s| s.seeds.len())
    }

    /// The seed of every build, in order.
    pub fn seeds(&self) -> Vec<Seed> {
        self.shared.with(|s| s.seeds.clone())
    }

    /// Builds that loaded persisted state (process startup only).
    pub fn stored_loads(&self) -> usize {
        self.shared.with(|s| {
            s.seeds
                .iter()
                .filter(|seed| seed.session.is_stored())
                .count()
        })
    }

    /// Runners that completed or were cancelled.
    pub fn runners_finished(&self) -> usize {
        self.shared.runners_finished.load(Ordering::SeqCst)
    }

    /// Instances whose control was retired.
    pub fn retired(&self) -> usize {
        self.shared.retired.load(Ordering::SeqCst)
    }

    /// Instances whose session write token was revoked at retirement.
    pub fn revoked(&self) -> usize {
        self.shared.revoked.load(Ordering::SeqCst)
    }

    /// `(chat, random_id)` of every send that reached the backend and succeeded.
    pub fn sent(&self) -> Vec<(ChatId, i64)> {
        self.shared.with(|s| s.sent.clone())
    }

    /// `(chat, random_id)` of every send call, including failed or hung ones.
    pub fn send_attempts(&self) -> Vec<(ChatId, i64)> {
        self.shared.with(|s| s.attempted.clone())
    }

    /// The server message created for `random_id`, if the server accepted it
    /// (including sends whose answer was lost).
    pub fn accepted(&self, random_id: i64) -> Option<(ChatId, MsgId)> {
        self.shared.with(|s| s.accepted.get(&random_id).copied())
    }

    /// Server messages in `chat` (own sends included).
    pub fn server_history(&self, chat: ChatId) -> Vec<Message> {
        self.shared
            .with(|s| s.history.get(&chat).cloned().unwrap_or_default())
    }

    /// The last peer set core asked an instance to retain.
    pub fn retained_peers(&self) -> Option<Vec<ChatId>> {
        self.shared.with(|s| s.retained.clone())
    }

    pub fn read_calls(&self) -> Vec<(ChatId, MsgId)> {
        self.shared.with(|s| s.read.clone())
    }

    /// A critical write (a new home DC) arriving on the most recently retired
    /// instance's session after retirement, as a late runner write would.
    /// Returns `false` if no instance was retired yet.
    pub async fn late_critical_write(&self) -> bool {
        let Some(session) = self.shared.with(|s| s.retired_session.clone()) else {
            return false;
        };
        let next = next_home_dc(&session);
        session.set_home_dc_id(next).await.is_ok()
    }

    /// Edits the server's dialog list (server order).
    pub fn edit_dialogs(&self, f: impl FnOnce(&mut Vec<ChatSummary>)) {
        self.shared.with(|s| f(&mut s.dialogs));
    }

    /// Replaces a chat's server-side history (e.g. simulate deletions).
    pub fn set_history(&self, chat: ChatId, messages: Vec<Message>) {
        self.shared.with(|s| {
            s.history.insert(chat, messages);
        });
    }
}

impl Backend for FakeBackend {
    type Client = FakeClient;
    type Updates = FakeUpdates;
    type Control = FakeControl;

    fn use_account(&self, account: SessionSnapshot) {
        self.shared.with(|s| s.account = Some(account));
    }

    fn credentials(&self) -> &CredentialSlot {
        &self.slot
    }

    async fn build(&self, seed: Seed) -> Result<Instance<Self>, BackendError> {
        let creds = self.slot.wait().await;
        // Like the real backend: a fresh session object over the account's
        // critical cell (or the retired instance's), taking its write token.
        let account = self.shared.with(|s| s.account.clone());
        let snapshot = match &seed.session {
            SessionSeed::Stored => account.unwrap_or_else(SessionSnapshot::detached),
            SessionSeed::Live(snapshot) => snapshot.clone(),
        };
        let (runner_tx, runner_rx) = mpsc::channel(4);
        let fault = self.shared.with(|s| {
            push_capped(&mut s.seeds, seed);
            s.runner = Some(runner_tx);
            s.build_faults.pop_front()
        });
        let baseline = match fault {
            Some(BuildFault::Fail(e)) => return Err(e),
            Some(BuildFault::NoBaseline) => false,
            None => true,
        };
        let session =
            Arc::new(AbyssSession::from_snapshot(snapshot).map_err(|_| BackendError::Transient)?);
        // A critical write during the instance's life, as the runner makes
        // when it connects (grammers `set_dc_option`): advances `rev`.
        let next = next_home_dc(&session);
        session
            .set_home_dc_id(next)
            .await
            .map_err(|_| BackendError::Transient)?;
        let updates = FakeUpdates {
            stream: Arc::clone(&self.stream),
            baseline,
        };
        Ok(self.instance(session, updates, runner_rx, creds))
    }
}

/// A home DC different from the session's current one, so each call is a change.
fn next_home_dc(session: &AbyssSession) -> i32 {
    session.home_dc_id().map_or(1, |dc| dc.rem_euclid(5) + 1)
}

impl FakeBackend {
    fn instance(
        &self,
        session: Arc<AbyssSession>,
        updates: FakeUpdates,
        runner_rx: mpsc::Receiver<RunnerCmd>,
        creds: Arc<ApiCredentials>,
    ) -> Instance<Self> {
        let quit = Arc::new(Notify::new());
        let retired = Arc::new(AtomicBool::new(false));
        Instance {
            client: FakeClient {
                shared: Arc::clone(&self.shared),
                session: Arc::clone(&session),
                creds,
                retired: Arc::clone(&retired),
            },
            updates,
            runner: Box::pin(run_runner(
                Arc::clone(&self.shared),
                Arc::clone(&quit),
                runner_rx,
            )),
            control: FakeControl {
                shared: Arc::clone(&self.shared),
                quit,
                session,
                retired,
            },
        }
    }
}

async fn run_runner(shared: Arc<Shared>, quit: Arc<Notify>, mut cmds: mpsc::Receiver<RunnerCmd>) {
    let _guard = RunnerGuard(Arc::clone(&shared));
    tokio::select! {
        () = quit.notified() => {
            match shared.with(|s| s.quit).unwrap_or(QuitBehavior::Prompt) {
                QuitBehavior::Prompt => {}
                QuitBehavior::Slow(d) => tokio::time::sleep(d).await,
                QuitBehavior::Ignore => std::future::pending().await,
            }
        }
        cmd = cmds.recv() => match cmd {
            Some(RunnerCmd::Exit) => {}
            Some(RunnerCmd::Panic) => injected_panic("runner"),
            // Replaced by a newer build; keep running until quit.
            None => quit.notified().await,
        },
    }
}

/// Retirement hooks of one fake instance.
#[derive(Debug)]
pub struct FakeControl {
    shared: Arc<Shared>,
    quit: Arc<Notify>,
    session: Arc<AbyssSession>,
    /// Shared with the instance's client: its queued requests are dropped.
    retired: Arc<AtomicBool>,
}

impl InstanceControl for FakeControl {
    fn quit(&self) {
        self.quit.notify_one();
    }

    fn retire(self) -> SessionSnapshot {
        self.retired.store(true, Ordering::SeqCst);
        self.shared.retired.fetch_add(1, Ordering::SeqCst);
        // Revoke first: a late write through this instance must not reach the
        // snapshot the next instance is built from.
        if self.session.revoke().is_ok() {
            self.shared.revoked.fetch_add(1, Ordering::SeqCst);
        }
        let snapshot = self.session.snapshot();
        self.shared
            .with(|s| s.retired_session = Some(Arc::clone(&self.session)));
        snapshot
    }
}

/// The update stream of one fake instance.
#[derive(Debug)]
pub struct FakeUpdates {
    stream: Arc<tokio::sync::Mutex<mpsc::Receiver<StreamCmd>>>,
    /// The baseline is still to be reported.
    baseline: bool,
}

impl UpdateStream for FakeUpdates {
    async fn next(&mut self) -> Result<StreamItem, StreamError> {
        if std::mem::take(&mut self.baseline) {
            return Ok(StreamItem::Baseline);
        }
        let mut rx = self.stream.lock().await;
        match rx.recv().await {
            Some(StreamCmd::Update(u)) => Ok(StreamItem::Update(u)),
            Some(StreamCmd::Error(e)) => Err(e),
            Some(StreamCmd::Panic) => injected_panic("update stream"),
            None => Err(StreamError::Fatal),
        }
    }
}

/// Calls of one fake instance, served by the shared fake server.
#[derive(Debug)]
pub struct FakeClient {
    shared: Arc<Shared>,
    /// This instance's session.
    session: Arc<AbyssSession>,
    /// The pair this instance was built with.
    creds: Arc<ApiCredentials>,
    /// Set when the instance is retired: queued requests are dropped.
    retired: Arc<AtomicBool>,
}

impl FakeClient {
    /// Counts the call and takes its next injected fault.
    fn take_fault(&self, op: Op) -> Option<Fault> {
        self.shared.with(|s| {
            *s.calls.entry(op).or_default() += 1;
            s.faults.get_mut(&op).and_then(VecDeque::pop_front)
        })
    }

    /// Counts the call, applies latency, then any injected fault. A revoked
    /// session gets `Unauthorized`.
    async fn enter(&self, op: Op) -> Result<InFlight<'_>, BackendError> {
        let fault = self.take_fault(op);
        let lead = self
            .shared
            .with(|s| s.leads.get_mut(&op).and_then(VecDeque::pop_front));
        if let Some(wait) = lead {
            tokio::time::sleep(wait).await;
        }
        self.enter_with(fault).await
    }

    /// [`Self::enter`] with the fault already taken.
    async fn enter_with(&self, fault: Option<Fault>) -> Result<InFlight<'_>, BackendError> {
        let guard = InFlight::new(&self.shared);
        if !self.shared.latency.is_zero() {
            tokio::time::sleep(self.shared.latency).await;
        }
        match fault {
            None => {}
            Some(Fault::Delay(d) | Fault::Queued(d)) => tokio::time::sleep(d).await,
            Some(Fault::Error(e)) => return Err(e),
            Some(Fault::Auth(_)) => return Err(BackendError::Rejected),
            Some(Fault::Hang | Fault::CompleteThenHang) => std::future::pending().await,
            Some(Fault::Panic) => injected_panic("call"),
        }
        if self.shared.with(|s| s.auth.authorized) {
            Ok(guard)
        } else {
            Err(BackendError::Unauthorized)
        }
    }
}

fn page(all: &[Message], query: HistoryQuery, limit: usize) -> HistoryPage {
    let (selected, has_more): (Vec<Message>, bool) = match query {
        HistoryQuery::Tail => {
            let start = all.len().saturating_sub(limit);
            (all.iter().skip(start).cloned().collect(), start > 0)
        }
        HistoryQuery::Before(id) => {
            let below: Vec<&Message> = all.iter().filter(|m| m.id < id).collect();
            let start = below.len().saturating_sub(limit);
            (below.into_iter().skip(start).cloned().collect(), start > 0)
        }
        HistoryQuery::After(id) => {
            let above: Vec<&Message> = all.iter().filter(|m| m.id > id).collect();
            let more = above.len() > limit;
            (above.into_iter().take(limit).cloned().collect(), more)
        }
    };
    HistoryPage {
        messages: selected,
        has_more,
    }
}

impl Client for FakeClient {
    type LoginToken = auth::FakeLoginToken;
    type PasswordToken = auth::FakePasswordToken;

    async fn is_authorized(&self) -> Result<bool, BackendError> {
        self.auth_is_authorized().await
    }

    async fn request_login_code(
        &self,
        phone: &str,
    ) -> Result<CodeRequest<Self::LoginToken>, AuthError> {
        self.auth_request_code(phone).await
    }

    async fn resend_login_code(
        &self,
        token: &Self::LoginToken,
    ) -> Result<CodeRequest<Self::LoginToken>, AuthError> {
        self.auth_resend_code(token).await
    }

    async fn sign_in(
        &self,
        token: &Self::LoginToken,
        code: &str,
    ) -> Result<SignIn<Self::PasswordToken>, AuthError> {
        self.auth_sign_in(token, code).await
    }

    async fn check_password(
        &self,
        token: Self::PasswordToken,
        password: PasswordBuf,
    ) -> Result<CheckPassword<Self::PasswordToken>, AuthError> {
        self.auth_check_password(token, password).await
    }

    async fn password_challenge(
        &self,
    ) -> Result<PasswordChallenge<Self::PasswordToken>, AuthError> {
        self.auth_challenge().await
    }

    async fn fetch_self(&self) -> Result<Account, BackendError> {
        self.auth_fetch_self().await
    }

    async fn sign_out(&self) -> Result<(), BackendError> {
        self.auth_sign_out().await
    }

    async fn dialogs(
        &self,
        cursor: Option<DialogsCursor>,
        limit: u32,
    ) -> Result<DialogsPage, BackendError> {
        let _g = self.enter(Op::Dialogs).await?;
        let start = cursor.map_or(0, |c| to_usize(c.index));
        let limit = to_usize(limit);
        Ok(self.shared.with(|s| {
            let chats: Vec<ChatSummary> =
                s.dialogs.iter().skip(start).take(limit).cloned().collect();
            let end = start + chats.len();
            let next = (end < s.dialogs.len()).then(|| DialogsCursor {
                offset_date: 0,
                offset_id: 0,
                offset_peer: None,
                index: u32::try_from(end).unwrap_or(u32::MAX),
            });
            DialogsPage { chats, next }
        }))
    }

    async fn history(
        &self,
        thread: ThreadKey,
        query: HistoryQuery,
        limit: u32,
    ) -> Result<HistoryPage, BackendError> {
        let _g = self.enter(Op::History).await?;
        Ok(self.shared.with(|s| {
            let all = s
                .history
                .get(&thread.chat)
                .map(Vec::as_slice)
                .unwrap_or_default();
            page(all, query, to_usize(limit))
        }))
    }

    async fn send_text(
        &self,
        thread: ThreadKey,
        text: String,
        random_id: i64,
    ) -> Result<MsgId, BackendError> {
        let chat = thread.chat;
        self.shared
            .with(|s| push_capped(&mut s.attempted, (chat, random_id)));
        let fault = self.take_fault(Op::Send);
        let lost = matches!(fault, Some(Fault::CompleteThenHang));
        let _g = self.enter_with(fault.filter(|_| !lost)).await?;
        // Telegram deduplicates by `random_id` (per sender, whatever the
        // peer) and answers a repeat with `RANDOM_ID_DUPLICATE`, which
        // carries no message id: no second message is created.
        if self.shared.with(|s| s.accepted.contains_key(&random_id)) {
            return Err(BackendError::SentUnconfirmed);
        }
        if lost {
            // Accepted, but the answer is lost: the server holds the message.
            self.accept(chat, &text, random_id);
            return std::future::pending().await;
        }
        // The real server also echoes own sends as updates; the fake does not, so tests
        // control update ordering explicitly with `FakeHandle::push_update`.
        Ok(self.accept(chat, &text, random_id))
    }

    async fn read_history(&self, thread: ThreadKey, max_id: MsgId) -> Result<(), BackendError> {
        let _g = self.enter(Op::Read).await?;
        self.shared
            .with(|s| push_capped(&mut s.read, (thread.chat, max_id)));
        Ok(())
    }

    fn retain_peers(&self, required: &[ChatId]) {
        self.shared.with(|s| s.retained = Some(required.to_vec()));
    }
}

impl FakeClient {
    /// The server accepts a send: one new message, remembered by `random_id`.
    fn accept(&self, chat: ChatId, text: &str, random_id: i64) -> MsgId {
        self.shared.with(|s| {
            push_capped(&mut s.sent, (chat, random_id));
            let history = s.history.entry(chat).or_default();
            let id = MsgId(history.last().map_or(1, |m| m.id.0.saturating_add(1)));
            let mut msg = text_message(chat, id.0, text);
            msg.outgoing = true;
            history.push(msg);
            if s.accepted.len() < MAX_LOG {
                s.accepted.insert(random_id, (chat, id));
            }
            id
        })
    }
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}
