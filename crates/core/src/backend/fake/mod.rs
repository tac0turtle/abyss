//! Scripted in-process backend for development and tests (`fake` feature only).
//!
//! Every build yields a fresh instance over one shared fake server. Supports
//! per-call fault injection (error, hang forever, panic, delay), lifecycle faults
//! (failed build, missing baseline, runner exit or panic, a runner that is slow
//! to quit or ignores `quit`), stream faults (recoverable or fatal error, panic),
//! fixed latency, live updates pushed by the test, and counters.
//!
//! Forums (plan section 5, task 3): topic lists in either order, topic
//! history, sends and reads per thread, peer dialogs, and controls that change
//! topics on the server and emit the matching updates (see `topics`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use grammers_session::Session as _;
use tokio::sync::{Notify, mpsc};

use super::{
    AROUND_CONTEXT, Account, AuthError, Backend, BackendError, CheckPassword, Client, CodeRequest,
    DialogsCursor, DialogsPage, HistoryPage, HistoryQuery, Instance, InstanceControl, NewMessage,
    NotifyScope, NotifyTarget, PasswordChallenge, RejectReason, Seed, SentCode, SessionSeed,
    SessionSnapshot, SignIn, StreamError, StreamItem, TopicsById, TopicsCursor, TopicsPage, Update,
    UpdateStream,
};
use crate::api::{ApiCredentials, CodeDelivery, NextDelivery, PasswordBuf};
use crate::credentials::{CredentialSlot, EnvResolution};
use crate::model::{
    Body, ChatFolder, ChatId, ChatKind, ChatSummary, FolderEdit, FolderRules, Message, MsgId,
    PeerFolder, PeerKind, ReadState, ThreadKey, TopicId, TopicOrder, TopicSnapshot,
};
use crate::rich_text::RichText;
use crate::session::AbyssSession;
use crate::store::PostClosed;

mod auth;
mod topics;

pub use auth::{FAKE_SELF_HASH, FAKE_SELF_ID};
pub use topics::{DEMO_FORUM_TOPICS, DemoForum, demo_forum};

/// Maximum faults queued per operation.
const MAX_FAULTS: usize = 1024;
/// Capacity of the pushed-update channel (shared by every instance's stream).
const UPDATE_CAPACITY: usize = 256;
/// A scripted catch-up is bounded like a server difference batch.
const MAX_CATCH_UP_MESSAGES: usize = 4096;
/// Recorded sends, seeds and other logs are capped at this many entries.
const MAX_LOG: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    Dialogs,
    NotifySettings(NotifyScope),
    PeerNotifySettings(ChatId),
    ChatFolders,
    SetPeerFolder,
    EditChatFolder,
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
    /// `messages.getForumTopics`.
    Topics,
    /// `messages.getForumTopicsByID`.
    TopicsById,
    /// `messages.getPeerDialogs`.
    PeerDialogs,
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
    /// Folder edits: the request was sent but sits in transit. The call
    /// never answers (the caller times out), and the write survives that:
    /// it lands when the instance's connection closes (its retirement), or
    /// when the test lands it ([`FakeHandle::land_in_transit`]). Other
    /// calls treat it as `Hang`.
    LandsLate,
    /// Succeeds after the given extra delay. `Dialogs`, `Topics`, `TopicsById`
    /// and `PeerDialogs` answer with the server state *at call time*, so changes
    /// made during the delay cross the response as live updates.
    Delay(Duration),
    /// Login calls (`sign_in`, `check_password`): the request sits in the
    /// sender's queue and the server applies it after the delay, whether or
    /// not the caller still waits (grammers keeps a request queued after its
    /// future is dropped). Retiring the instance first drops it, as quitting
    /// the runner drops the queue. Data calls treat it as `Delay`.
    Queued(Duration),
}

/// A topics call as it reached the fake server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TopicsRequest {
    /// `getForumTopics`; `first` when it had no cursor.
    Page { chat: ChatId, first: bool },
    /// `getForumTopicsByID`.
    ById { chat: ChatId, ids: Vec<TopicId> },
}

/// One entry of [`FakeHandle::topics_calls`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicsCallRecord {
    pub request: TopicsRequest,
    /// When the call reached the server (the runtime's clock).
    pub at: tokio::time::Instant,
}

/// One `peer_dialogs` call as it reached the fake server
/// ([`FakeHandle::peer_calls`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerCallRecord {
    pub chats: Vec<ChatId>,
    /// When the call reached the server (the runtime's clock).
    pub at: tokio::time::Instant,
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
    /// Chat folders, in server order.
    pub folders: Vec<ChatFolder>,
    pub notify_users: Option<i64>,
    pub notify_groups: Option<i64>,
    /// Per chat, ascending by id. Messages carry the topic the way the
    /// server states it: `None` for General and non-forum chats, `Some(t)`
    /// for every other topic (a topic's creation message included).
    pub history: HashMap<ChatId, Vec<Message>>,
    /// Per forum, its topics (a forum is a chat whose dialog kind says so).
    /// The fake serves each topic's activity (`last`, `last_date`,
    /// `last_id`, `top_version`) and `unread` from `history` and
    /// `read_inbox_max`, so the scripted values of those fields are ignored.
    /// Hidden topics (General only) are listed with their visibility.
    pub topics: HashMap<ChatId, Vec<TopicSnapshot>>,
    /// Per forum, the order its topic list is served in (default
    /// [`TopicOrder::LastActivity`]).
    pub topic_order: HashMap<ChatId, TopicOrder>,
    /// Per forum, whether the account may post in closed topics (default
    /// [`PostClosed::Refused`]).
    pub post_closed: HashMap<ChatId, PostClosed>,
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
            folders: Vec::new(),
            notify_users: None,
            notify_groups: None,
            history: HashMap::new(),
            topics: HashMap::new(),
            topic_order: HashMap::new(),
            post_closed: HashMap::new(),
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
        } else if id.kind == PeerKind::Chat {
            ChatKind::Group
        } else {
            ChatKind::Private { contact: false }
        },
        pinned: false,
        read: ReadState::default(),
        unread_mentions: 0,
        last: None,
        last_date: 0,
        last_id: None,
        can_send: true,
        muted_until: None,
        scope_muted_until: 0,
        folder: PeerFolder::Main,
        reading: None,
        topic_unread: None,
    }
}

/// The folder a write sets, made the way the real backend makes it
/// (`grammers::map::folders::edited`): from the folders as they are when
/// the call starts, as a whole folder (`None` deletes) that replaces the
/// one with its id when the write lands. Between the two, other writes can
/// land (`Fault::Delay`), as on the server.
fn fake_edit(
    folders: &[ChatFolder],
    edit: &FolderEdit,
) -> Result<(i32, Option<ChatFolder>), RejectReason> {
    let kind_rules = [
        FolderRules::CONTACTS,
        FolderRules::NON_CONTACTS,
        FolderRules::GROUPS,
        FolderRules::BROADCASTS,
        FolderRules::BOTS,
    ];
    let bounded = |f: ChatFolder| {
        let peers = f.pinned.len() + f.include.len() + f.exclude.len();
        if peers > FAKE_FOLDER_PEERS {
            Err(RejectReason::FolderLimit)
        } else {
            Ok((f.id, Some(f)))
        }
    };
    if let Some((id, tag)) = edit.target() {
        match folders.iter().find(|f| f.id == id) {
            None => return Err(RejectReason::FolderChanged),
            Some(f) if f.shared => return Err(RejectReason::Other),
            Some(f) if f.tag() != *tag => return Err(RejectReason::FolderChanged),
            Some(_) => {}
        }
    }
    let current = |id: i32| {
        folders
            .iter()
            .find(|f| f.id == id)
            .cloned()
            .ok_or(RejectReason::FolderChanged)
    };
    match edit {
        FolderEdit::AddChat { folder, chat, .. } => {
            let mut f = current(*folder)?;
            f.exclude.retain(|c| c != chat);
            if !f.pinned.contains(chat) && !f.include.contains(chat) {
                f.include.push(*chat);
            }
            bounded(f)
        }
        FolderEdit::RemoveChat { folder, chat, .. } => {
            let mut f = current(*folder)?;
            f.pinned.retain(|c| c != chat);
            f.include.retain(|c| c != chat);
            if kind_rules.iter().any(|&r| f.rules.has(r)) && !f.exclude.contains(chat) {
                f.exclude.push(*chat);
            }
            bounded(f)
        }
        FolderEdit::Create { title, chat } => {
            if folders.len() >= FAKE_MAX_FOLDERS {
                return Err(RejectReason::FolderLimit);
            }
            let id = (2..=255)
                .find(|id| !folders.iter().any(|f| f.id == *id))
                .ok_or(RejectReason::FolderLimit)?;
            let f = ChatFolder {
                id,
                title: title.clone(),
                rules: FolderRules::default(),
                pinned: Vec::new(),
                include: vec![*chat],
                exclude: Vec::new(),
                shared: false,
            };
            Ok((id, Some(f)))
        }
        FolderEdit::Delete { folder, .. } => Ok((*folder, None)),
    }
}

/// The real backend's bounds (`grammers::map::folders`), which the fake
/// cannot import (it builds without grammers).
const FAKE_MAX_FOLDERS: usize = 64;
const FAKE_FOLDER_PEERS: usize = 512;

/// Lands every folder write in transit (`Fault::LandsLate`), in order.
fn land_in_transit(s: &mut State) {
    for (id, folder) in std::mem::take(&mut s.in_transit) {
        put_folder(&mut s.folders, id, folder);
    }
}

/// Lands a write made by [`fake_edit`]: replaces, adds or removes the
/// folder with that id.
fn put_folder(folders: &mut Vec<ChatFolder>, id: i32, folder: Option<ChatFolder>) {
    let at = folders.iter().position(|f| f.id == id);
    match (at, folder) {
        (Some(at), Some(f)) => {
            if let Some(slot) = folders.get_mut(at) {
                *slot = f;
            }
        }
        (Some(at), None) => drop(folders.remove(at)),
        (None, Some(f)) => folders.push(f),
        (None, None) => {}
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
    Item(StreamItem),
    Error(StreamError),
    Panic,
}

#[derive(Debug, Default)]
struct State {
    dialogs: Vec<ChatSummary>,
    folders: Vec<ChatFolder>,
    notify_users: Option<i64>,
    notify_groups: Option<i64>,
    /// Folder writes sent but not landed (`Fault::LandsLate`).
    in_transit: Vec<(i32, Option<ChatFolder>)>,
    history: HashMap<ChatId, Vec<Message>>,
    /// History calls that reached the server, in arrival order (capped).
    history_log: Vec<(ThreadKey, HistoryQuery)>,
    faults: HashMap<Op, VecDeque<Fault>>,
    /// Extra waits before the next calls' faults or answers (`lead`).
    leads: HashMap<Op, VecDeque<Duration>>,
    /// Calls held until the test releases them (`hold`): the op, the
    /// call's number (`calls` once counted) and its release.
    holds: Vec<(Op, usize, Arc<Notify>)>,
    #[cfg(test)]
    snapshot_holds: Vec<(Op, Arc<Notify>)>,
    build_faults: VecDeque<BuildFault>,
    calls: HashMap<Op, usize>,
    sent: Vec<(ThreadKey, i64)>,
    /// Server-side dedup: every `random_id` the server accepted, with the
    /// message it created. Telegram deduplicates by `random_id` per sender.
    accepted: HashMap<i64, (ChatId, MsgId)>,
    attempted: Vec<(ThreadKey, i64)>,
    /// The last `Client::retain_peers` set.
    retained: Option<Vec<ChatId>>,
    read: Vec<(ThreadKey, MsgId)>,
    /// Chats whose read pointer a read moved: their count is served from
    /// their history (`topics::current_summary`).
    pointed: HashSet<ChatId>,
    topics: HashMap<ChatId, Vec<TopicSnapshot>>,
    topic_order: HashMap<ChatId, TopicOrder>,
    post_closed: HashMap<ChatId, PostClosed>,
    /// Topics calls in arrival order (capped).
    topics_log: Vec<TopicsCallRecord>,
    /// `peer_dialogs` calls in arrival order (capped).
    peer_log: Vec<PeerCallRecord>,
    /// Forums whose later pages repeat the first page (`repeat_topic_pages`).
    repeat_pages: Vec<ChatId>,
    /// Updates still to be dropped instead of delivered (`drop_next_update`).
    drop_updates: usize,
    #[cfg(test)]
    checkpoint_pts: i32,
    /// The `pts` of each position core recorded for catch-up (capped).
    checkpoints: Vec<i32>,
    /// Per chat, the newest message id and date ever held: new messages are
    /// allocated above it, so deleted ids are never reused.
    high_water: HashMap<ChatId, topics::HighWater>,
    seeds: Vec<Seed>,
    /// The most recently retired instance's session, for late-write tests.
    retired_session: Option<Arc<AbyssSession>>,
    /// The next instance retirement panics (`panic_next_retire`).
    panic_retire: bool,
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
        let high_water = script
            .history
            .iter()
            .map(|(chat, messages)| {
                let mut mark = topics::HighWater::default();
                mark.cover(messages);
                (*chat, mark)
            })
            .collect();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                high_water,
                dialogs: script.dialogs,
                folders: script.folders,
                notify_users: script.notify_users,
                notify_groups: script.notify_groups,
                history: script.history,
                topics: script.topics,
                topic_order: script.topic_order,
                post_closed: script.post_closed,
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
    #[cfg(test)]
    pub(crate) async fn push_stream_item(&self, item: StreamItem) {
        self.stream.send(StreamCmd::Item(item)).await.unwrap();
    }

    /// Holds the next successful snapshot of `op`, after taking its answer.
    #[cfg(test)]
    pub(crate) fn hold_snapshot(&self, op: Op) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        self.shared.with(|s| {
            assert!(s.snapshot_holds.len() < MAX_FAULTS);
            s.snapshot_holds.push((op, Arc::clone(&gate)));
        });
        gate
    }

    /// Queues a one-shot fault for the next call of `op`.
    pub fn inject(&self, op: Op, fault: Fault) {
        self.shared.with(|s| {
            let q = s.faults.entry(op).or_default();
            if q.len() < MAX_FAULTS {
                q.push_back(fault);
            }
        });
    }

    /// Holds the `call`-th data call of `op` (counted as [`Self::calls`]
    /// counts it) before its fault (if any) applies, until the returned
    /// gate is notified (`notify_one`, which a later wait also sees).
    pub fn hold(&self, op: Op, call: usize) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        self.shared.with(|s| {
            if s.holds.len() < MAX_FAULTS {
                s.holds.push((op, call, Arc::clone(&gate)));
            }
        });
        gate
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

    /// Makes the next instance retirement panic once its runner has ended
    /// (`InstanceControl::retire`): what it would hand back is lost.
    pub fn panic_next_retire(&self) {
        self.shared.with(|s| s.panic_retire = true);
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

    /// Delivers a live update on whichever stream is current (or the next
    /// one), unless [`Self::drop_next_update`] asked for it to be lost.
    pub async fn push_update(&self, update: Update) {
        let lost = self.shared.with(|s| {
            // A read on another device moves the server's pointer too.
            if let Update::ReadInbox {
                chat,
                topic: None,
                max_id,
                ..
            } = update
            {
                topics::move_pointer(s, chat, max_id);
            }
            if let Update::NotifySettings { target, until } = &update {
                match target {
                    NotifyTarget::Peer(chat) => {
                        if let Some(row) = s.dialogs.iter_mut().find(|row| row.id == *chat) {
                            row.muted_until = *until;
                        }
                    }
                    NotifyTarget::Scope(NotifyScope::Users) => s.notify_users = *until,
                    NotifyTarget::Scope(NotifyScope::Groups) => s.notify_groups = *until,
                }
            }
            let lost = s.drop_updates > 0;
            s.drop_updates = s.drop_updates.saturating_sub(1);
            lost
        });
        if !lost {
            let _ = self
                .stream
                .send(StreamCmd::Item(StreamItem::Update(update)))
                .await;
        }
    }

    #[cfg(test)]
    pub(crate) async fn push_checkpoint(&self, pts: i32) {
        let state = super::UpdateCheckpoint(grammers_session::types::UpdatesState {
            pts,
            ..Default::default()
        });
        let _ = self
            .stream
            .send(StreamCmd::Item(StreamItem::Checkpoint(state)))
            .await;
    }

    #[cfg(test)]
    pub(crate) fn checkpoint_pts(&self) -> i32 {
        self.shared.with(|s| s.checkpoint_pts)
    }

    /// A difference replay, with the same end marker as a real stream.
    pub async fn catch_up(&self, messages: impl IntoIterator<Item = NewMessage>) {
        for message in messages.into_iter().take(MAX_CATCH_UP_MESSAGES) {
            let item = StreamItem::CatchUp(Update::NewMessage(message));
            if self.stream.send(StreamCmd::Item(item)).await.is_err() {
                return;
            }
        }
        let _ = self
            .stream
            .send(StreamCmd::Item(StreamItem::CatchUpEnded))
            .await;
    }

    /// A "difference too long", then the end marker.
    pub async fn catch_up_gap(&self) {
        for item in [StreamItem::CatchUpGap, StreamItem::CatchUpEnded] {
            if self.stream.send(StreamCmd::Item(item)).await.is_err() {
                return;
            }
        }
    }

    /// The stream's position moves to `pts` (core records it once the updates
    /// before it are applied and their snapshots are current).
    pub async fn checkpoint(&self, pts: i32) {
        let state = grammers_session::types::UpdatesState {
            pts,
            ..Default::default()
        };
        let item = StreamItem::Checkpoint(crate::backend::UpdateCheckpoint(state));
        let _ = self.stream.send(StreamCmd::Item(item)).await;
    }

    /// The `pts` of each position core recorded, oldest first.
    pub fn recorded_checkpoints(&self) -> Vec<i32> {
        self.shared.with(|s| s.checkpoints.clone())
    }

    /// The next update pushed (by the test or by a server control such as
    /// [`Self::close_topic`]) is lost: the server state still changes, but no
    /// update arrives, as with a lost non-PTS update (plan section 2).
    pub fn drop_next_update(&self) {
        self.shared
            .with(|s| s.drop_updates = s.drop_updates.saturating_add(1).min(MAX_FAULTS));
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

    /// `(thread, random_id)` of every send that reached the backend and succeeded.
    pub fn sent(&self) -> Vec<(ThreadKey, i64)> {
        self.shared.with(|s| s.sent.clone())
    }

    /// `(thread, random_id)` of every send call, including failed or hung ones.
    pub fn send_attempts(&self) -> Vec<(ThreadKey, i64)> {
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

    /// `(thread, query)` of every history call that reached the server,
    /// in arrival order, failed or hung ones included.
    pub fn history_calls(&self) -> Vec<(ThreadKey, HistoryQuery)> {
        self.shared.with(|s| s.history_log.clone())
    }

    /// The last peer set core asked an instance to retain.
    pub fn retained_peers(&self) -> Option<Vec<ChatId>> {
        self.shared.with(|s| s.retained.clone())
    }

    /// `(thread, max_id)` of every successful read mark.
    pub fn read_calls(&self) -> Vec<(ThreadKey, MsgId)> {
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

    /// Replaces the server's chat folders (push
    /// [`Update::ChatFoldersChanged`] to have core fetch them).
    pub fn set_folders(&self, folders: Vec<ChatFolder>) {
        self.shared.with(|s| s.folders = folders);
    }

    /// Lands the folder writes in transit (`Fault::LandsLate`) now.
    pub fn land_in_transit(&self) {
        self.shared.with(land_in_transit);
    }

    /// The server's chat folders.
    pub fn folders(&self) -> Vec<ChatFolder> {
        self.shared.with(|s| s.folders.clone())
    }

    /// The server's dialog list.
    pub fn dialogs(&self) -> Vec<ChatSummary> {
        self.shared.with(|s| s.dialogs.clone())
    }

    /// Replaces a chat's server-side history (e.g. simulate deletions).
    pub fn set_history(&self, chat: ChatId, messages: Vec<Message>) {
        self.shared.with(|s| {
            s.high_water.entry(chat).or_default().cover(&messages);
            s.history.insert(chat, messages);
        });
    }
}

impl Backend for FakeBackend {
    fn record_update_state(&self, state: crate::backend::UpdateCheckpoint) {
        self.shared.with(|s| {
            #[cfg(test)]
            {
                s.checkpoint_pts = state.0.pts;
            }
            if s.checkpoints.len() < MAX_FAULTS {
                s.checkpoints.push(state.0.pts);
            }
        });
    }

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
        if self.shared.with(|s| std::mem::take(&mut s.panic_retire)) {
            injected_panic("retirement");
        }
        self.retired.store(true, Ordering::SeqCst);
        // What the closing connection had sent reaches the server now.
        self.shared.with(land_in_transit);
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
            return Ok(StreamItem::Baseline(super::Baseline::Fresh));
        }
        let mut rx = self.stream.lock().await;
        match rx.recv().await {
            Some(StreamCmd::Item(item)) => Ok(item),
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
    /// Counts the call and takes its next injected fault (for calls that
    /// honour no lead or gate).
    fn take_fault(&self, op: Op) -> Option<Fault> {
        self.shared.with(|s| {
            *s.calls.entry(op).or_default() += 1;
            s.faults.get_mut(&op).and_then(VecDeque::pop_front)
        })
    }

    /// Counts the call and takes its fault, lead and gate in one critical
    /// section, so concurrent calls of one op each see their own number.
    fn admit(&self, op: Op) -> (Option<Fault>, Option<Duration>, Option<Arc<Notify>>) {
        self.shared.with(|s| {
            let count = s.calls.entry(op).or_default();
            *count += 1;
            let call = *count;
            let fault = s.faults.get_mut(&op).and_then(VecDeque::pop_front);
            let lead = s.leads.get_mut(&op).and_then(VecDeque::pop_front);
            let held = s
                .holds
                .iter()
                .position(|(o, c, _)| *o == op && *c == call)
                .map(|at| s.holds.swap_remove(at).2);
            (fault, lead, held)
        })
    }

    /// Waits out a call's gate, then its lead.
    async fn wait_admitted(lead: Option<Duration>, held: Option<Arc<Notify>>) {
        if let Some(gate) = held {
            gate.notified().await;
        }
        if let Some(wait) = lead {
            tokio::time::sleep(wait).await;
        }
    }

    /// Counts the call, applies latency, then any injected fault. A revoked
    /// session gets `Unauthorized`.
    async fn enter(&self, op: Op) -> Result<InFlight<'_>, BackendError> {
        let (fault, lead, held) = self.admit(op);
        Self::wait_admitted(lead, held).await;
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
            Some(Fault::Auth(_)) => return Err(BackendError::Rejected(RejectReason::Other)),
            Some(Fault::Hang | Fault::CompleteThenHang | Fault::LandsLate) => {
                std::future::pending::<()>().await;
            }
            Some(Fault::Panic) => injected_panic("call"),
        }
        if self.shared.with(|s| s.auth.authorized) {
            Ok(guard)
        } else {
            Err(BackendError::Unauthorized)
        }
    }

    async fn after_latency(&self, e: BackendError) -> BackendError {
        if !self.shared.latency.is_zero() {
            tokio::time::sleep(self.shared.latency).await;
        }
        e
    }

    /// A read-only call answered with the server state at call time: faults
    /// and leads apply as in [`Self::enter`], but latency and a `Delay` run
    /// after the answer is taken (plan section 11, crossing responses).
    async fn snapshot<T>(
        &self,
        op: Op,
        answer: impl FnOnce(&State) -> Result<T, BackendError>,
    ) -> Result<T, BackendError> {
        let (fault, lead, held) = self.admit(op);
        Self::wait_admitted(lead, held).await;
        let _guard = InFlight::new(&self.shared);
        let delay = match fault {
            None => Duration::ZERO,
            Some(Fault::Delay(d) | Fault::Queued(d)) => d,
            Some(Fault::Error(e)) => return Err(self.after_latency(e).await),
            Some(Fault::Auth(_)) => {
                return Err(self
                    .after_latency(BackendError::Rejected(RejectReason::Other))
                    .await);
            }
            Some(Fault::Hang | Fault::CompleteThenHang | Fault::LandsLate) => {
                std::future::pending().await
            }
            Some(Fault::Panic) => injected_panic("call"),
        };
        let result = self.shared.with(|s| {
            if s.auth.authorized {
                answer(s)
            } else {
                Err(BackendError::Unauthorized)
            }
        });
        #[cfg(test)]
        if result.is_ok() {
            let gate = self.shared.with(|s| {
                let at = s.snapshot_holds.iter().position(|(held, _)| *held == op)?;
                Some(s.snapshot_holds.remove(at).1)
            });
            if let Some(gate) = gate {
                gate.notified().await;
            }
        }
        let wait = self.shared.latency + delay;
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        result
    }
}

/// One page of `all` (ascending). `Around` takes the messages above its
/// anchor oldest first, up to `limit - AROUND_CONTEXT`, and those at or
/// below it newest first, up to `AROUND_CONTEXT`; `has_more` says whether
/// newer history is left above the page. Its bounds are checked before
/// (`super::around`).
fn page(all: &[&Message], query: HistoryQuery, limit: usize) -> HistoryPage {
    let (selected, has_more): (Vec<&Message>, bool) = match query {
        HistoryQuery::Around(anchor) => {
            let above_limit = limit.saturating_sub(to_usize(AROUND_CONTEXT));
            let (below, above): (Vec<&Message>, Vec<&Message>) =
                all.iter().copied().partition(|m| m.id <= anchor);
            let more = above.len() > above_limit;
            let mut page: Vec<&Message> = below
                .into_iter()
                .rev()
                .take(to_usize(AROUND_CONTEXT))
                .collect();
            page.reverse();
            page.extend(above.into_iter().take(above_limit));
            (page, more)
        }
        HistoryQuery::Tail => {
            let start = all.len().saturating_sub(limit);
            (all.iter().skip(start).copied().collect(), start > 0)
        }
        HistoryQuery::Before(id) => {
            let below: Vec<&Message> = all.iter().copied().filter(|m| m.id < id).collect();
            let start = below.len().saturating_sub(limit);
            (below.into_iter().skip(start).collect(), start > 0)
        }
        HistoryQuery::After(id) => {
            let above: Vec<&Message> = all.iter().copied().filter(|m| m.id > id).collect();
            let more = above.len() > limit;
            (above.into_iter().take(limit).collect(), more)
        }
    };
    HistoryPage {
        messages: selected.into_iter().cloned().collect(),
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
        // Answered with the server state at call time, so a change made
        // during a `Delay` crosses the response as a live update.
        let start = cursor.map_or(0, |c| to_usize(c.index));
        let limit = to_usize(limit);
        self.snapshot(Op::Dialogs, |s| {
            let chats: Vec<ChatSummary> = s
                .dialogs
                .iter()
                .skip(start)
                .take(limit)
                .map(|c| topics::current_summary(s, c))
                .collect();
            let end = start + chats.len();
            let next = (end < s.dialogs.len()).then(|| DialogsCursor {
                offset_date: 0,
                offset_id: 0,
                offset_peer: None,
                index: u32::try_from(end).unwrap_or(u32::MAX),
            });
            Ok(DialogsPage { chats, next })
        })
        .await
    }

    async fn notify_settings(&self, target: NotifyTarget) -> Result<Option<i64>, BackendError> {
        let op = match target {
            NotifyTarget::Scope(scope) => Op::NotifySettings(scope),
            NotifyTarget::Peer(chat) => Op::PeerNotifySettings(chat),
        };
        self.snapshot(op, |s| {
            Ok(match target {
                NotifyTarget::Scope(NotifyScope::Users) => s.notify_users,
                NotifyTarget::Scope(NotifyScope::Groups) => s.notify_groups,
                NotifyTarget::Peer(chat) => s
                    .dialogs
                    .iter()
                    .find(|row| row.id == chat)
                    .and_then(|row| row.muted_until),
            })
        })
        .await
    }

    async fn chat_folders(&self) -> Result<Vec<ChatFolder>, BackendError> {
        let _g = self.enter(Op::ChatFolders).await?;
        Ok(self.shared.with(|s| s.folders.clone()))
    }

    async fn set_peer_folder(&self, chat: ChatId, folder: PeerFolder) -> Result<(), BackendError> {
        let _g = self.enter(Op::SetPeerFolder).await?;
        self.shared.with(|s| {
            if let Some(c) = s.dialogs.iter_mut().find(|c| c.id == chat) {
                c.folder = folder;
            }
        });
        Ok(())
    }

    /// Fetch, then write: the edit is made from the folders at call time
    /// and lands after any `Delay`, so writes can interleave. Faults: an
    /// `Error` or `Hang` lands nothing; `CompleteThenHang` lands the write
    /// and never answers (an uncertain outcome).
    async fn edit_chat_folder(&self, edit: FolderEdit) -> Result<(), BackendError> {
        let fault = self.take_fault(Op::EditChatFolder);
        let lead = self.shared.with(|s| {
            s.leads
                .get_mut(&Op::EditChatFolder)
                .and_then(VecDeque::pop_front)
        });
        if let Some(wait) = lead {
            tokio::time::sleep(wait).await;
        }
        let _guard = InFlight::new(&self.shared);
        let made = self.shared.with(|s| {
            if s.auth.authorized {
                fake_edit(&s.folders, &edit).map_err(BackendError::Rejected)
            } else {
                Err(BackendError::Unauthorized)
            }
        });
        match fault {
            None => {}
            Some(Fault::Delay(d) | Fault::Queued(d)) => tokio::time::sleep(d).await,
            Some(Fault::Error(e)) => return Err(e),
            Some(Fault::Auth(_)) => return Err(BackendError::Rejected(RejectReason::Other)),
            Some(Fault::Hang) => std::future::pending().await,
            Some(Fault::LandsLate) => {
                if let Ok(write) = &made {
                    self.shared.with(|s| s.in_transit.push(write.clone()));
                }
                std::future::pending::<()>().await;
            }
            Some(Fault::CompleteThenHang) => {
                if let Ok((id, folder)) = &made {
                    self.shared
                        .with(|s| put_folder(&mut s.folders, *id, folder.clone()));
                }
                std::future::pending::<()>().await;
            }
            Some(Fault::Panic) => injected_panic("call"),
        }
        let (id, folder) = made?;
        self.shared.with(|s| put_folder(&mut s.folders, id, folder));
        Ok(())
    }

    async fn history(
        &self,
        thread: ThreadKey,
        query: HistoryQuery,
        limit: u32,
    ) -> Result<HistoryPage, BackendError> {
        // Refused before the call, as the real backend does.
        if let HistoryQuery::Around(anchor) = query {
            super::around(anchor, limit)?;
        }
        self.shared
            .with(|s| push_capped(&mut s.history_log, (thread, query)));
        let _g = self.enter(Op::History).await?;
        Ok(self
            .shared
            .with(|s| page(&topics::thread_history(s, thread), query, to_usize(limit))))
    }

    async fn send_text(
        &self,
        thread: ThreadKey,
        text: String,
        random_id: i64,
    ) -> Result<MsgId, BackendError> {
        self.shared
            .with(|s| push_capped(&mut s.attempted, (thread, random_id)));
        let fault = self.take_fault(Op::Send);
        let lost = matches!(fault, Some(Fault::CompleteThenHang));
        let _g = self.enter_with(fault.filter(|_| !lost)).await?;
        // Telegram deduplicates by `random_id` (per sender, whatever the
        // peer) and answers a repeat with `RANDOM_ID_DUPLICATE`, which
        // carries no message id: no second message is created.
        if self.shared.with(|s| s.accepted.contains_key(&random_id)) {
            return Err(BackendError::SentUnconfirmed);
        }
        self.shared.with(|s| topics::check_send(s, thread))?;
        if lost {
            // Accepted, but the answer is lost: the server holds the message.
            self.accept(thread, &text, random_id)?;
            return std::future::pending().await;
        }
        // The real server also echoes own sends as updates; the fake does not, so tests
        // control update ordering explicitly with `FakeHandle::push_update`.
        self.accept(thread, &text, random_id)
    }

    async fn read_history(&self, thread: ThreadKey, max_id: MsgId) -> Result<(), BackendError> {
        let _g = self.enter(Op::Read).await?;
        self.shared.with(|s| {
            topics::mark_read(s, thread, max_id)?;
            push_capped(&mut s.read, (thread, max_id));
            Ok(())
        })
    }

    async fn topics(
        &self,
        chat: ChatId,
        cursor: Option<TopicsCursor>,
        limit: u32,
    ) -> Result<TopicsPage, BackendError> {
        let first = cursor.is_none();
        self.log_topics(TopicsRequest::Page { chat, first });
        self.snapshot(Op::Topics, |s| topics::page(s, chat, cursor, limit))
            .await
    }

    async fn topics_by_id(
        &self,
        chat: ChatId,
        ids: &[TopicId],
    ) -> Result<TopicsById, BackendError> {
        let ids_logged = ids.to_vec();
        self.log_topics(TopicsRequest::ById {
            chat,
            ids: ids_logged,
        });
        self.snapshot(Op::TopicsById, |s| topics::by_id(s, chat, ids))
            .await
    }

    async fn peer_dialogs(&self, chats: &[ChatId]) -> Result<Vec<ChatSummary>, BackendError> {
        let record = PeerCallRecord {
            chats: chats.to_vec(),
            at: tokio::time::Instant::now(),
        };
        self.shared.with(|s| push_capped(&mut s.peer_log, record));
        self.snapshot(Op::PeerDialogs, |s| topics::peer_dialogs(s, chats))
            .await
    }

    fn retain_peers(&self, required: &[ChatId]) {
        self.shared.with(|s| s.retained = Some(required.to_vec()));
    }
}

impl FakeClient {
    fn log_topics(&self, request: TopicsRequest) {
        let at = tokio::time::Instant::now();
        self.shared
            .with(|s| push_capped(&mut s.topics_log, TopicsCallRecord { request, at }));
    }

    /// The server accepts a send: one new message, remembered by `random_id`.
    fn accept(&self, thread: ThreadKey, text: &str, random_id: i64) -> Result<MsgId, BackendError> {
        self.shared.with(|s| {
            let mut msg = topics::next_message(s, topics::raw_thread(thread), text)?;
            msg.outgoing = true;
            let id = msg.id;
            push_capped(&mut s.sent, (thread, random_id));
            s.history.entry(thread.chat).or_default().push(msg);
            if s.accepted.len() < MAX_LOG {
                s.accepted.insert(random_id, (thread.chat, id));
            }
            Ok(id)
        })
    }
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Around` takes the newest `AROUND_CONTEXT` messages at or below its
    /// anchor and the oldest above it, up to the rest of the page, at both
    /// ends of history; `has_more` says whether newer history is left.
    #[test]
    fn around_slices_the_context_newest_first_and_the_rest_oldest_first() {
        let chat = ChatId {
            kind: PeerKind::User,
            id: 1,
        };
        let all: Vec<Message> = (1..=100).map(|id| text_message(chat, id, "m")).collect();
        let refs: Vec<&Message> = all.iter().collect();
        let around = |anchor: i32| {
            let p = page(&refs, HistoryQuery::Around(MsgId(anchor)), 15);
            let ids: Vec<i32> = p.messages.iter().map(|m| m.id.0).collect();
            (ids, p.has_more)
        };
        assert_eq!(around(50), ((41..=55).collect(), true));
        assert_eq!(around(3), ((1..=8).collect(), true), "few below");
        assert_eq!(around(97), ((88..=100).collect(), false), "few above");
        assert_eq!(around(0), ((1..=5).collect(), true), "the oldest");
    }
}
