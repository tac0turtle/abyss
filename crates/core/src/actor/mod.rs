//! The core actor: owns the store and the client lifecycle, admits backend calls,
//! applies completions and live updates, and emits events. Runs on the core
//! runtime; never blocks.

mod admission;
mod apply;
mod auth;
mod auth_outcome;
mod badges;
mod binding;
mod boot;
mod covered;
mod fences;
mod folder_writes;
mod jobs;
mod lifecycle;
mod loads;
mod logout;
mod marker;
mod merge;
mod onboarding;
mod paused;
mod peers;
mod reads;
mod sends;
#[cfg(all(test, feature = "fake"))]
mod tests;
mod threads;
mod topics;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::{Id, JoinError, JoinSet};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use admission::Admission;
pub use admission::MAX_CONCURRENT;
use auth::AuthMachine;
pub(crate) use binding::{Identity, provision_cache};
use boot::Boot;
use jobs::{CallCtx, Done, FolderJob, Job, Load, ReqId, TaskKind};
use lifecycle::{Lifecycle, Live, LiveEvent, Phase, Reason};
use loads::Deferred;
pub(crate) use logout::CacheParts;
pub(crate) use marker::{PendingLogout, read_pending_logout};

use crate::api::{
    AuthState, ClientGen, Command, ConnState, Event, HistoryDir, LoadTarget, RemoteLogout,
    SaveStatus, UserError, WindowState,
};
use crate::backend::{Backend, DialogsCursor, HistoryQuery, StreamItem};
use crate::cache::CacheHandle;
use crate::core::ShutdownReport;
use crate::events::EventSender;
use crate::model::{ChatId, FolderEdit, MsgId, ThreadKey};
use crate::session::{PersistHandle, SessionSnapshot};
use crate::store::{Badges, Dialogs, KindChecks, PeerScheduler, ViewGen, Window, Windows};

/// Shutdown's deadline for retirement and the persistence seal, counted from
/// its start (plan section 5).
pub const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(3);
/// How long shutdown waits for queued cache writes (best effort; the cache
/// is disposable and does not affect the save status).
const CACHE_FLUSH: Duration = Duration::from_secs(3);

/// Per-call deadline for backend calls.
pub const CALL_DEADLINE: Duration = Duration::from_secs(30);
/// Coalescing interval for read marks.
pub const READ_FLUSH: Duration = Duration::from_millis(500);
/// Admitted-call backlog waiting for a permit.
const MAX_QUEUED: usize = 256;
/// Stream items buffered between the update consumer and the actor.
const UPDATE_QUEUE: usize = 256;

/// Whether `cmd` acts on the signed-in account's data.
fn account_scoped(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::LoadDialogs
            | Command::OpenThread(_)
            | Command::CloseThread(_)
            | Command::LoadOlder(_)
            | Command::LoadNewer(_)
            | Command::SendText { .. }
            | Command::RetrySend(_)
            | Command::DiscardSend(_)
            | Command::MarkRead { .. }
            | Command::ViewingTail(_)
            | Command::OpenForum(_)
            | Command::CloseForum(_)
            | Command::LoadTopics(_)
            | Command::SetPeerFolder { .. }
            | Command::EditChatFolder(_)
    )
}

/// A UI command and the account epoch the UI had seen when it sent it
/// (see [`crate::events`]).
#[derive(Debug)]
pub(crate) struct Stamped {
    pub epoch: u64,
    pub cmd: Command,
}

/// Control-plane messages, never queued behind UI commands.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Internal {
    /// The system woke from sleep: rebuild rather than trust old sockets.
    Wake,
}

/// The chat folders load (`Job::ChatFolders`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FoldersLoad {
    Idle,
    /// Queued or running.
    Running,
    /// Queued or running, and another load is wanted after it.
    Again,
}

pub(crate) struct Actor<B: Backend> {
    backend: Arc<B>,
    events: EventSender,
    /// Shutdown. Also interrupts an `emit` blocked on a full event queue.
    cancel: CancellationToken,
    cache: Option<CacheHandle>,
    dialogs: Dialogs,
    windows: Windows,
    cgen: ClientGen,
    admission: Admission,
    /// UI-initiated calls; admitted only while live.
    queue: VecDeque<Job>,
    /// Bootstrap and refresh snapshots; admitted whenever an instance exists.
    priority: VecDeque<Job>,
    /// Cache reads, which outlive client instances.
    tasks: JoinSet<Done>,
    kinds: HashMap<Id, TaskKind>,
    live: Option<Live<B>>,
    life: Lifecycle<B>,
    boot: Option<Boot>,
    window: WindowState,
    last_refresh: Instant,
    /// Threads with a history load (cache or server) in flight, and that
    /// load. At most one per window thread.
    history_busy: HashMap<ThreadKey, Load>,
    /// `LoadOlder`/`LoadNewer` asked while their thread's load ran; started
    /// when it ends (at most [`loads::MAX_DEFERRED`]).
    deferred: Vec<loads::Deferred>,
    /// The history load in flight that owes the UI its answer (task 10f).
    owed: Option<loads::Owed>,
    /// Server history known to cover threads with `SentUnconfirmed` sends.
    covers: covered::Covers,
    /// Source of [`jobs::ReqId`]s.
    next_req: u64,
    dialogs_busy: bool,
    dialogs_queued: bool,
    folders: FoldersLoad,
    /// Archive moves and folder edits: one at a time (`folder_writes`).
    folder_writes: folder_writes::FolderWrites,
    dialogs_loaded: bool,
    dialogs_next: Option<DialogsCursor>,
    open: Option<ThreadKey>,
    /// The thread whose live tail the UI reports on screen
    /// (`Command::ViewingTail`).
    viewing: Option<ThreadKey>,
    /// The peer set last handed to the instance (`Client::retain_peers`).
    peers: peers::PeerSync,
    /// Forum topic lists and their account-level limits (`topics`).
    topics: topics::TopicLists,
    /// The kind last stated for each chat with a retained view (`threads`).
    forum_kinds: threads::KnownKinds,
    /// Forum badge stamps and dirty bits, one per retained dialog (`badges`).
    badges: Badges,
    /// The account's `peer_dialogs` scheduler (`badges`).
    peer: PeerScheduler,
    /// The kind-check FIFO and the fenced chats (`fences`).
    fences: KindChecks,
    /// The revisions the last membership pass saw (`fences`).
    members: Option<fences::Members>,
    /// Read intents per thread, flushed every `READ_FLUSH` until they
    /// land (at most `reads::MAX_READS`).
    reads: HashMap<ThreadKey, reads::ReadMark>,
    sends: sends::PendingSends,
    deadline: Duration,
    updates_tx: mpsc::Sender<(ClientGen, StreamItem)>,
    persist: PersistHandle,
    /// The account's live critical state; a new generation after logout.
    account: SessionSnapshot,
    /// The cache's directory and key, for crypto-erase on logout.
    cache_parts: Option<CacheParts>,
    auth: AuthMachine<B::Client>,
    /// The remote half of a running logout, until it is reported.
    logout_remote: Option<RemoteLogout>,
    /// A logout whose local deletion failed, until a Retry succeeds.
    unfinished_logout: Option<(RemoteLogout, crate::api::LocalDeletion)>,
    /// Startup found the logout-pending marker (see `marker`).
    pending_logout: Option<PendingLogout>,
    /// A logout waiting on its marker, or one that could not start.
    logout_gate: marker::LogoutGate,
}

pub(crate) struct ActorChannels {
    pub commands: mpsc::Receiver<Stamped>,
    pub internal: mpsc::Receiver<Internal>,
    pub updates: mpsc::Receiver<(ClientGen, StreamItem)>,
}

/// Inputs of [`Actor::new`].
pub(crate) struct ActorInit<B> {
    pub backend: Arc<B>,
    pub events: EventSender,
    pub cache: Option<CacheHandle>,
    pub commands: mpsc::Receiver<Stamped>,
    pub internal: mpsc::Receiver<Internal>,
    pub cancel: CancellationToken,
    pub persist: PersistHandle,
    /// Loaded at startup (`PersistHandle::load_session`).
    pub account: SessionSnapshot,
    pub cache_parts: Option<CacheParts>,
    /// Read before the session (`read_pending_logout`); when set, `account`
    /// is a fresh one and the cache is quarantined.
    pub pending_logout: Option<PendingLogout>,
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

impl<B: Backend> Actor<B> {
    pub(crate) fn new(init: ActorInit<B>) -> (Self, ActorChannels) {
        let (updates_tx, updates) = mpsc::channel(UPDATE_QUEUE);
        init.backend.use_account(init.account.clone());
        let auth = AuthMachine::new(&init.account);
        let actor = Actor {
            backend: init.backend,
            events: init.events,
            cancel: init.cancel,
            cache: init.cache,
            dialogs: Dialogs::default(),
            windows: Windows::default(),
            cgen: ClientGen(1),
            admission: Admission::default(),
            queue: VecDeque::new(),
            priority: VecDeque::new(),
            tasks: JoinSet::new(),
            kinds: HashMap::new(),
            live: None,
            life: Lifecycle::default(),
            boot: None,
            window: WindowState::Active,
            last_refresh: Instant::now(),
            history_busy: HashMap::new(),
            deferred: Vec::with_capacity(loads::MAX_DEFERRED),
            owed: None,
            covers: covered::Covers::default(),
            next_req: 0,
            dialogs_busy: false,
            dialogs_queued: false,
            folders: FoldersLoad::Idle,
            folder_writes: folder_writes::FolderWrites::default(),
            dialogs_loaded: false,
            dialogs_next: None,
            open: None,
            viewing: None,
            peers: peers::PeerSync::default(),
            topics: topics::TopicLists::default(),
            forum_kinds: threads::KnownKinds::default(),
            badges: Badges::default(),
            peer: PeerScheduler::default(),
            fences: KindChecks::default(),
            members: None,
            reads: HashMap::new(),
            sends: sends::PendingSends::default(),
            deadline: CALL_DEADLINE,
            updates_tx,
            persist: init.persist,
            account: init.account,
            cache_parts: init.cache_parts,
            auth,
            logout_remote: None,
            unfinished_logout: None,
            pending_logout: init.pending_logout,
            logout_gate: marker::LogoutGate::default(),
        };
        let channels = ActorChannels {
            commands: init.commands,
            internal: init.internal,
            updates,
        };
        (actor, channels)
    }

    /// Runs until cancelled or the command channel closes, then shuts down
    /// (plan section 5) and reports what it observed.
    pub(crate) async fn run(mut self, mut ch: ActorChannels) -> ShutdownReport {
        let cancel = self.cancel.clone();
        self.start().await;
        let mut flush = tokio::time::interval(READ_FLUSH);
        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let baseline_by = self.boot.as_ref().and_then(Boot::baseline_deadline);
            let refresh_at = self.refresh_due();
            let flood_over = self.auth.flood_deadline();
            let topics_at = self.topics_wake();
            let peer_at = self.peer_wake();
            // Paused while a logout hides the account (`paused`).
            let send_due = self
                .sends
                .next_due()
                .filter(|_| !self.logout_gate.hides_account());
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                Some(msg) = ch.internal.recv() => self.on_internal(msg).await,
                step = lifecycle::next_step(self.life.step_mut()) => self.on_step(step).await,
                Some(msg) = self.auth.rx.recv() => self.on_auth(msg).await,
                ev = lifecycle::next_live(&mut self.live) => self.on_live(ev).await,
                Some(joined) = self.tasks.join_next_with_id(), if !self.tasks.is_empty() => {
                    self.on_joined(joined).await;
                }
                Some((cgen, item)) = ch.updates.recv() => {
                    if cgen == self.cgen {
                        self.on_feed(item).await;
                    }
                }
                cmd = ch.commands.recv() => match cmd {
                    Some(stamped) => self.on_stamped(stamped).await,
                    None => break,
                },
                () = sleep_until(baseline_by) => self.rebuild(Reason::NoBaseline).await,
                () = sleep_until(refresh_at) => self.refresh().await,
                () = sleep_until(flood_over) => self.flood_over().await,
                () = sleep_until(send_due) => self.resume_flooded_sends().await,
                // `pump_topics` below dispatches what became due.
                () = sleep_until(topics_at) => {}
                // So does `pump_peer_dialogs`.
                () = sleep_until(peer_at) => {}
                _ = flush.tick() => self.flush_reads().await,
            }
            // User loads first; background coverage only when idle.
            self.run_deferred_loads().await;
            self.run_cover_fetch().await;
            debug_assert!(self.owed_is_current(), "owed load of an old view");
            debug_assert!(self.open_is_retained(), "open chat without its window");
            self.pump_topics().await;
            self.sync_members().await;
            #[cfg(test)]
            debug_assert!(
                self.kind_checks_within_required(),
                "kind checks outside the required peers"
            );
            self.pump_peer_dialogs();
            self.pump();
            self.sync_peers();
        }
        self.shut_down().await
    }

    /// Plan section 5, steps 2-6: end the actor's tasks, retire the instance
    /// (finishing a running retirement or logout), then seal persistence with
    /// a barrier on the current revision. Never claims a save it did not see.
    async fn shut_down(&mut self) -> ShutdownReport {
        let deadline = Instant::now() + SHUTDOWN_DEADLINE;
        // Nothing opens core's cache from here on; what is open drains
        // (round 23). Reopened when the actor is done, for a later core.
        let _admission = self
            .cache_parts
            .as_ref()
            .map(|parts| crate::cache::close_admission(&parts.dir));
        self.auth.abort_task();
        self.auth.creds.abort();
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
        // The producers are stopped: close the caches alongside retirement,
        // not after it. A logout owns its cache until its local step has
        // erased it, so then they close afterwards.
        let early = (!matches!(self.life.phase, Phase::LoggingOut(_))).then(|| self.close_caches());
        let closing = async move {
            if let Some(closing) = early {
                closing.await;
            }
        };
        let (life, ()) = tokio::join!(self.shut_down_lifecycle(deadline), closing);
        // After a logout there is nothing to save; its status says the rest.
        let sealed = life.logout.is_some()
            || matches!(
                tokio::time::timeout_at(deadline, self.account.barrier()).await,
                Ok(Ok(()))
            );
        // Whatever is left: a logout's caches, or a test's (flushed only).
        self.close_caches().await;
        let save = if life.retired && sealed {
            SaveStatus::Confirmed
        } else {
            SaveStatus::Unconfirmed
        };
        ShutdownReport {
            save,
            logout: life.logout,
        }
    }

    async fn start(&mut self) {
        self.set_auth(AuthState::Connecting).await;
        self.emit(Event::Connection(ConnState::Connecting)).await;
        self.load_cached_chats();
        self.resolve_credentials().await;
        // An interrupted logout finishes before any client is built.
        if let Some(pending) = self.pending_logout.take() {
            return self.resume_logout(pending).await;
        }
        self.start_build(Duration::ZERO);
    }

    /// Shows the served cache's chats until the server answers.
    fn load_cached_chats(&mut self) {
        let Some(cache) = self.cache.clone() else {
            return;
        };
        let cgen = self.cgen;
        self.spawn_local(TaskKind::CacheChats, async move {
            let chats = cache.chats().await.ok();
            Done {
                cgen,
                parked: None,
                outcome: jobs::Outcome::CacheChats(chats),
            }
        });
    }

    /// Spawns a task that is not bound to a client instance (cache reads).
    fn spawn_local(
        &mut self,
        kind: TaskKind,
        fut: impl std::future::Future<Output = Done> + Send + 'static,
    ) {
        let handle = self.tasks.spawn(fut);
        self.kinds.insert(handle.id(), kind);
    }

    /// Emits an event, waiting for queue space; gives up on shutdown so a UI
    /// that stopped draining cannot block cancellation.
    async fn emit(&self, event: Event) {
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => {}
            // A closed channel means the UI is gone; shutdown follows via cancellation.
            _ = self.events.send(event) => {}
        }
    }

    async fn error(&self, e: UserError) {
        self.emit(Event::Error(e)).await;
    }

    /// Queues a backend call; it is spawned when admitted and a permit is free.
    async fn enqueue(&mut self, job: Job) -> bool {
        if self.queue.len() >= MAX_QUEUED {
            self.error(UserError::Busy).await;
            return false;
        }
        self.queue.push_back(job);
        true
    }

    /// Spawns queued jobs on the live instance while permits are available.
    /// Snapshot jobs go first; UI jobs wait until the bootstrap has finished.
    fn pump(&mut self) {
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let user = self.life.phase == Phase::Live;
        // A topics flood wait is checked at admission too: a topics job
        // queued before it began stays queued (in order, the queue still
        // bounded) while other jobs pass it.
        let held = self.topics.flooded();
        // Work of a fenced chat waits too (its fence purged what was queued
        // when it started; this holds anything since, without blocking
        // other jobs).
        // Usually empty; at most the required peers.
        let fenced: Vec<ChatId> = self.fences.fenced().collect();
        let admissible = |job: &Job| {
            !(held && job.is_topics()) && job.fenced_chat().is_none_or(|c| !fenced.contains(&c))
        };
        loop {
            let first = |q: &VecDeque<Job>| q.iter().position(admissible);
            let (queue, at) = match (first(&self.priority), first(&self.queue)) {
                (Some(at), _) => (&mut self.priority, at),
                (None, Some(at)) if user => (&mut self.queue, at),
                _ => return,
            };
            let Some(permit) = self.admission.try_acquire() else {
                return;
            };
            let Some(mut job) = queue.remove(at) else {
                return;
            };
            match &mut job {
                Job::Dialogs { stamps, .. } => stamps.badge = self.badges.dispatch(),
                // Its dispatch: the badge stamp and the call interval count
                // from admission, not from planning.
                Job::PeerDialogs { chats, stamps } => {
                    let now = Instant::now().into_std();
                    stamps.badge = self.peer.dispatched(now, chats, &mut self.badges);
                    self.fences.stamp_flight(stamps.badge);
                }
                _ => {}
            }
            let ctx = CallCtx {
                client: Arc::clone(&live.client),
                cgen: self.cgen,
                deadline: self.deadline,
                cancel: live.cancel.clone(),
            };
            let (kind, fut) = jobs::job_future(ctx, job, permit);
            let handle = live.tasks.spawn(fut);
            live.kinds.insert(handle.id(), kind);
        }
    }

    /// Drops queued snapshot and history loads (their views are going away) and
    /// dialogs loads; sends stay queued for the next instance, and read marks
    /// are sent again from the read intents (`reads`).
    /// What a purged load owed the UI is left to the caller: retirement
    /// answers it, and a full bootstrap invalidates its view (task 10g).
    fn purge_queued(&mut self) {
        // Sends and folder changes are the user's intent: they run on the
        // next instance.
        let keep = |job: &Job| {
            matches!(
                job,
                Job::Send { .. }
                    | Job::Folders(FolderJob::SetPeerFolder { .. } | FolderJob::Edit { .. })
            )
        };
        self.folders = FoldersLoad::Idle;
        let (kept, purged): (Vec<Job>, Vec<Job>) = self.queue.drain(..).partition(keep);
        self.queue = kept.into();
        let purged: Vec<Job> = self.priority.drain(..).chain(purged).collect();
        for job in purged {
            match job {
                Job::Dialogs { .. } => self.dialogs_busy = false,
                Job::History { load, .. } => {
                    self.release_history(load);
                    self.covers.fetch_ended(load.thread, load.req);
                }
                Job::TopicsPage { call, .. } | Job::TopicsById { call, .. } => {
                    self.topics_job_purged(call);
                }
                Job::PeerDialogs { .. } => self.peer_job_dropped(),
                Job::Read { thread, max_id, .. } => self.read_job_dropped(thread, max_id),
                Job::Send { .. } | Job::Folders(_) => {}
            }
        }
    }

    async fn on_internal(&mut self, msg: Internal) {
        match msg {
            Internal::Wake => self.rebuild(Reason::Wake).await,
        }
    }

    async fn on_live(&mut self, ev: LiveEvent) {
        match ev {
            LiveEvent::Joined(Ok((id, done))) => {
                if let Some(live) = self.live.as_mut() {
                    live.kinds.remove(&id);
                }
                self.on_done(done).await;
            }
            LiveEvent::Joined(Err(e)) => {
                let kind = self.live.as_mut().and_then(|l| l.kinds.remove(&e.id()));
                if e.is_panic() {
                    self.on_panic(kind).await;
                }
            }
            LiveEvent::RunnerEnded => self.rebuild(Reason::RunnerEnded).await,
        }
    }

    async fn on_joined(&mut self, joined: Result<(Id, Done), JoinError>) {
        match joined {
            Ok((id, done)) => {
                self.kinds.remove(&id);
                self.on_done(done).await;
            }
            Err(e) => {
                let kind = self.kinds.remove(&e.id());
                if e.is_panic() {
                    self.on_panic(kind).await;
                }
            }
        }
    }

    /// A task panicked (for example inside the Telegram library). Its permit was
    /// released by unwinding; clear the state it held and keep running.
    async fn on_panic(&mut self, kind: Option<TaskKind>) {
        match kind {
            Some(TaskKind::ChatFolders) => {
                // A load wanted after this one still runs.
                let again = self.folders == FoldersLoad::Again;
                self.folders = FoldersLoad::Idle;
                if again {
                    self.load_folders();
                }
            }
            Some(TaskKind::FolderWrite(write)) => self.folder_write_panicked(write).await,
            Some(TaskKind::Dialogs) => {
                self.dialogs_busy = false;
                self.load_ended(LoadTarget::Dialogs, Some(UserError::Internal))
                    .await;
                self.snapshot_done(boot::Snapshot::Dialogs).await;
            }
            Some(TaskKind::History(load, dir)) => {
                self.release_history(load);
                // Only the load the UI waits on ends: a load deferred behind
                // it still runs (and answers), and a load of an old view or a
                // background load answers nothing (task 10f).
                self.covers.fetch_ended(load.thread, load.req);
                if self.take_owed(load).is_some() {
                    self.history_ended(load.thread, dir, Some(UserError::Internal))
                        .await;
                }
                self.snapshot_done(boot::Snapshot::Tail(load)).await;
            }
            Some(TaskKind::Send(local_id)) => {
                if let Some(state) = self.sends.mark_unknown(local_id) {
                    self.emit(Event::SendState { local_id, state }).await;
                }
            }
            Some(TaskKind::TopicsPage(call)) => {
                self.topics_call_panicked(call, topics::Fetch::Page).await;
            }
            Some(TaskKind::TopicsById(call)) => {
                self.topics_call_panicked(call, topics::Fetch::ById).await;
            }
            Some(TaskKind::PeerDialogs(stamp)) => self.peer_call_panicked(stamp).await,
            Some(TaskKind::Updates) => {
                self.error(UserError::Internal).await;
                return self.rebuild(Reason::StreamFailed).await;
            }
            Some(TaskKind::Auth(tag)) => return self.on_auth_panic(tag).await,
            _ => {}
        }
        self.error(UserError::Internal).await;
    }

    /// Admits a UI command. An account-scoped one is refused while no
    /// account is signed in, during a logout (a send or retry already while
    /// its marker is written, `LogoutGate::refuses`), or when the UI made it
    /// in another account epoch: a command queued behind a logout, or made
    /// for the old account, never acts for the next one (review round 15,
    /// C). A refusal in the UI's epoch is answered (`acknowledge_refused`).
    async fn on_stamped(&mut self, stamped: Stamped) {
        let Stamped { epoch, cmd } = stamped;
        if !account_scoped(&cmd) {
            return self.on_command(cmd).await;
        }
        let current = epoch == self.events.epoch();
        let signed_in = self.auth.signed_in && !matches!(self.life.phase, Phase::LoggingOut(_));
        if current && signed_in {
            if self.logout_gate.refuses(&cmd) {
                return self.refused_behind_logout(cmd).await;
            }
            return self.on_command(cmd).await;
        }
        tracing::debug!(current, signed_in, "account command refused");
        // A command made for an earlier epoch is not the UI's anymore.
        if current {
            self.acknowledge_refused(cmd).await;
        }
    }

    async fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::LoadDialogs => self.load_dialogs().await,
            Command::OpenThread(thread) => self.open_thread(thread).await,
            Command::CloseThread(thread) => self.close_thread(thread).await,
            Command::OpenForum(chat) => self.open_forum(chat).await,
            Command::CloseForum(chat) => self.close_forum(chat),
            Command::LoadTopics(chat) => self.load_topics(chat).await,
            Command::LoadOlder(thread) => {
                self.load_more(Deferred::fresh(thread, HistoryDir::Older))
                    .await;
            }
            Command::LoadNewer(thread) => {
                self.load_more(Deferred::fresh(thread, HistoryDir::Newer))
                    .await;
            }
            Command::SendText {
                thread,
                text,
                local_id,
            } => self.send_text(thread, text, local_id).await,
            Command::RetrySend(local_id) => self.retry_send(local_id).await,
            Command::DiscardSend(local_id) => self.discard_send(local_id).await,
            Command::Window(state) => self.on_window(state).await,
            Command::MarkRead { thread, max_id } => self.mark_read(thread, max_id).await,
            Command::ViewingTail(thread) => self.viewing = thread,
            Command::SetPeerFolder { chat, folder } => {
                self.folder_write(folder_writes::FolderWrite::Move { chat, folder })
                    .await;
            }
            Command::EditChatFolder(edit) => self.edit_chat_folder(edit).await,
            Command::SubmitPhone(phone) => {
                let phone = zeroize::Zeroizing::new(phone);
                self.submit_phone(&phone).await;
            }
            Command::SubmitCode(code) => self.submit_code(zeroize::Zeroizing::new(code)).await,
            Command::SubmitPassword(password) => self.submit_password(password).await,
            Command::ResendCode => self.resend_code().await,
            Command::RetryPasswordParams => self.retry_password_params().await,
            Command::Logout => self.on_logout_command().await,
            Command::SubmitCredentials { api_id, api_hash } => {
                self.submit_credentials(api_id, api_hash).await;
            }
            Command::ForgetStoredCredentials => self.forget_credentials().await,
            Command::ReloadCredentials => self.reload_credentials().await,
            Command::RetryCredentialSave => self.retry_credential_save().await,
            Command::Back => self.back().await,
            Command::RetryStartup => self.retry_startup().await,
        }
    }

    /// Loads the chat folders in the background: after each first dialogs
    /// page and when they change. At most one load runs, with at most one
    /// more after it; a full queue drops it quietly (the next first page
    /// asks again).
    fn load_folders(&mut self) {
        match self.folders {
            FoldersLoad::Running | FoldersLoad::Again => self.folders = FoldersLoad::Again,
            FoldersLoad::Idle if self.queue.len() < MAX_QUEUED => {
                self.queue.push_back(Job::Folders(FolderJob::Load));
                self.folders = FoldersLoad::Running;
            }
            FoldersLoad::Idle => {}
        }
    }

    /// Takes a folder change (it waits its turn, `folder_writes`); a new
    /// folder's title must be one Telegram takes.
    async fn edit_chat_folder(&mut self, edit: FolderEdit) {
        let edit = match edit {
            FolderEdit::Create { title, chat } => match crate::model::folder_title(&title) {
                Some(title) => FolderEdit::Create { title, chat },
                None => return self.error(UserError::Rejected).await,
            },
            other => other,
        };
        self.folder_write(folder_writes::FolderWrite::Edit(edit))
            .await;
    }

    async fn load_dialogs(&mut self) {
        if self.dialogs_busy {
            self.dialogs_queued = true;
            return;
        }
        if self.dialogs_loaded && self.dialogs_next.is_none() {
            // Every page is loaded.
            return self.load_ended(LoadTarget::Dialogs, None).await;
        }
        let cursor = if self.dialogs_loaded {
            self.dialogs_next.clone()
        } else {
            None
        };
        let stamps = jobs::Stamps::default();
        if self.enqueue(Job::Dialogs { cursor, stamps }).await {
            self.dialogs_busy = true;
        } else {
            self.load_ended(LoadTarget::Dialogs, Some(UserError::Busy))
                .await;
        }
    }

    /// Opens `thread`: the retained window (if attached to the live tail) is
    /// shown at once as a complete `Tail` snapshot of the new view, then the
    /// cached and server tails follow. During a full bootstrap the tail is a
    /// boot snapshot, replacing any earlier one of the same thread.
    async fn open_thread(&mut self, thread: ThreadKey) {
        if self.refuse_open(thread).await {
            return;
        }
        if let Some(prev) = self.open.filter(|p| *p != thread) {
            self.windows.close(prev);
            self.forget_owed(prev);
        }
        self.forget_owed(thread);
        // A detached window is not what the user asked for: start from the tail.
        if self.windows.get(thread).is_some_and(Window::has_newer) {
            self.windows.invalidate(thread);
        }
        let view = self.windows.open(thread);
        self.note_kinds();
        let prev = self.open.replace(thread);
        self.follow_open_thread(prev).await;
        self.seed_view(thread, view).await;
        if self.is_fenced(thread.chat) {
            // The fence's lift loads the open thread's tail (`fences`).
            return;
        }
        let load = self.begin_load(thread, view);
        self.claim_if_booting(load);
        if let Some(cache) = &self.cache {
            cache
                .write(crate::cache::CacheCmd::Touch(thread.chat))
                .await;
        }
        // The server tail is requested once the cache tail is shown.
        self.start_tail(load, boot::TailVia::Cache).await;
    }

    /// Emits the window `thread` retains as a complete `Tail` of `view`, so a
    /// newly opened view never starts empty while its tail loads.
    async fn seed_view(&self, thread: ThreadKey, view: ViewGen) {
        let Some(w) = self
            .windows
            .get(thread)
            .filter(|w| !w.messages().is_empty())
        else {
            return;
        };
        self.emit(Event::History {
            thread,
            view,
            dir: HistoryDir::Tail,
            messages: w.messages().iter().cloned().collect(),
            removed: Vec::new(),
            has_more: w.has_older(),
        })
        .await;
    }

    /// Starts a history load for `thread`'s `view`, making it the thread's
    /// single-flight load.
    fn begin_load(&mut self, thread: ThreadKey, view: ViewGen) -> Load {
        self.next_req = self.next_req.wrapping_add(1);
        let load = Load {
            thread,
            view,
            req: ReqId(self.next_req),
        };
        self.history_busy.insert(thread, load);
        load
    }

    /// The newest id `thread`'s window holds now: what a tail requested now
    /// is authoritative up to.
    fn tail_floor(&self, thread: ThreadKey) -> Option<MsgId> {
        self.windows
            .get(thread)
            .and_then(|w| w.messages().back())
            .map(|m| m.id)
    }

    async fn request_tail(&mut self, load: Load) {
        let job = Job::History {
            load,
            dir: HistoryDir::Tail,
            query: HistoryQuery::Tail,
            floor: self.tail_floor(load.thread),
            since: self.windows.continuity_mark(),
        };
        if !self.enqueue(job).await {
            self.release_history(load);
        }
    }

    async fn close_thread(&mut self, thread: ThreadKey) {
        self.windows.close(thread);
        self.forget_owed(thread);
        if self.open == Some(thread) {
            self.open = None;
            self.follow_open_thread(Some(thread)).await;
        }
    }

    /// Scrolls the open window. Serves from the disk cache when it is known to be
    /// contiguous, else asks the server. Single-flight per thread: asked while
    /// a load runs, it waits for that one, except that a covering fetch
    /// (`covered.rs`) already loads the older page. Answered by `History` in
    /// `dir`, or by `LoadEnded` (see `loads.rs`).
    async fn load_more(&mut self, asked: Deferred) {
        let Deferred {
            thread,
            dir,
            restarts,
        } = asked;
        if self.open != Some(thread) {
            return self.history_ended(thread, dir, None).await;
        }
        if self.is_fenced(thread.chat) {
            return self.defer_fenced(asked).await;
        }
        if dir == HistoryDir::Older && self.absorb_older(thread, restarts) {
            return; // the covering fetch running answers it
        }
        if self.history_busy.contains_key(&thread) {
            return self.defer_load(asked);
        }
        let Some(w) = self.windows.get(thread) else {
            return self.history_ended(thread, dir, None).await;
        };
        let view = w.view_gen();
        let (edge, more) = match dir {
            HistoryDir::Older => (w.messages().front(), w.has_older()),
            _ => (w.messages().back(), w.has_newer()),
        };
        let Some(edge) = edge.filter(|_| more).map(|m| m.id) else {
            return self.history_ended(thread, dir, None).await;
        };
        let load = self.begin_load(thread, view);
        self.owe(load, dir, restarts);
        if let Some(cache) = self
            .cache
            .clone()
            .filter(|_| self.windows.is_cache_contiguous(thread))
        {
            let cgen = self.cgen;
            self.spawn_local(
                TaskKind::History(load, dir),
                jobs::cache_history(cache, cgen, load, dir, Some(edge)),
            );
            return;
        }
        self.fetch_more(load, dir, edge).await;
    }

    async fn fetch_more(&mut self, load: Load, dir: HistoryDir, edge: MsgId) {
        let query = if dir == HistoryDir::Older {
            HistoryQuery::Before(edge)
        } else {
            HistoryQuery::After(edge)
        };
        let job = Job::History {
            load,
            dir,
            query,
            floor: None,
            since: self.windows.continuity_mark(),
        };
        if !self.enqueue(job).await {
            self.release_history(load);
            // A covering fetch that cannot be queued answers nobody.
            self.covers.fetch_ended(load.thread, load.req);
            if self.take_owed(load).is_some() {
                self.history_ended(load.thread, dir, Some(UserError::Busy))
                    .await;
            }
        }
    }
}
