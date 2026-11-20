//! Client lifecycle (plan section 2): build, supervise, retire, rebuild.
//!
//! The actor owns at most one live client instance. Every failure is handled the
//! same way: stop admission and bump `ClientGen`, retire the instance (cancel and
//! join its tasks, stop its runner and await its completion, revoke its session),
//! and only then recycle parked permits and build again with backoff. Build and
//! retirement run as one spawned "step" at a time, so the actor keeps serving
//! commands, the cache and the UI meanwhile.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::{Id, JoinError, JoinHandle, JoinSet};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::boot::Boot;
use super::jobs::{self, CallError, Done, Outcome, TaskKind};
use super::logout::LocalDone;
use super::{Actor, admission::MAX_CONCURRENT};
use crate::api::{
    ClientGen, ConnState, Event, HistoryDir, LogoutStatus, RemoteLogout, StorageError, UserError,
};
use crate::backend::{
    Backend, BackendError, Instance, InstanceControl, Seed, SessionSeed, SessionSnapshot,
};
use crate::model::ThreadKey;

/// First rebuild delay; doubles per attempt.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// Online this long resets the backoff.
const STABLE_AFTER: Duration = Duration::from_mins(5);
/// A stall: this many deadline misses within [`STALL_WINDOW`].
const STALL_TIMEOUTS: usize = 3;
const STALL_WINDOW: Duration = Duration::from_mins(2);

/// Time limits for retiring an instance.
#[derive(Clone, Copy, Debug)]
pub(super) struct Caps {
    /// For the instance's tasks to finish after cancellation, before abort.
    tasks: Duration,
    /// For the runner to finish after `quit`, before abort.
    runner: Duration,
}

pub(super) const REBUILD_CAPS: Caps = Caps {
    tasks: Duration::from_secs(3),
    runner: Duration::from_secs(5),
};
pub(super) const SHUTDOWN_CAPS: Caps = Caps {
    tasks: Duration::from_secs(1),
    runner: Duration::from_secs(2),
};

/// Why an instance is being replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reason {
    RunnerEnded,
    StreamFailed,
    Stall,
    Wake,
    Migrated,
    NoBaseline,
    BootOverflow,
    GapSnapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    /// A build step is running (after its backoff).
    Building,
    /// An instance exists; the bootstrap has not finished. Only snapshot calls
    /// are admitted.
    Booting,
    Live,
    /// A retirement step is running; then build again, or halt.
    Retiring(After),
    /// The account needs a new sign-in after a repeated migration; no
    /// instance until the user submits a phone number.
    Halted,
    /// An instance exists but the account is not signed in: no update
    /// stream, no bootstrap; only login calls run.
    SignedOut,
    /// A logout step is running (plan section 5).
    LoggingOut(LogoutStage),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LogoutStage {
    /// `Terminate`, `auth.logOut` with the runner alive, then retirement.
    Remote,
    /// Deleting the session item, then crypto-erasing the cache.
    Local,
    /// The local step failed: "Logout incomplete" until a Retry succeeds.
    /// No instance, no login.
    Unfinished,
    /// Startup could not read the logout-pending marker: nothing loaded,
    /// served, built or deleted until `Command::RetryStartup` reads it.
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum After {
    Rebuild,
    Halt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Migration {
    Never,
    /// Already rebuilt once for a migration; the next one halts.
    Once,
}

/// The output of a lifecycle step.
pub(super) enum Step<B: Backend> {
    Built(Result<Instance<B>, BackendError>),
    Retired(Retired),
    /// Logout, remote stage done (the instance, if any, is retired).
    SignedOut {
        remote: RemoteLogout,
        retired: Option<Retired>,
    },
    /// Logout, local stage done.
    Deleted(LocalDone),
}

pub(super) struct Retired {
    pub(super) snapshot: SessionSnapshot,
    /// Completions that must still be applied: send outcomes (which outlive the
    /// instance) and timed-out calls holding a permit.
    pub(super) dones: Vec<Done>,
}

/// Rebuild delays: exponential with jitter, reset after stable operation.
#[derive(Debug, Default)]
struct Backoff {
    attempt: u32,
    stable_since: Option<Instant>,
}

impl Backoff {
    fn next(&mut self, now: Instant) -> (u32, Duration) {
        if self
            .stable_since
            .take()
            .is_some_and(|t| now.duration_since(t) >= STABLE_AFTER)
        {
            self.attempt = 0;
        }
        self.attempt = self.attempt.saturating_add(1);
        let factor = 2u32
            .checked_pow(self.attempt.saturating_sub(1))
            .unwrap_or(u32::MAX);
        let base = BACKOFF_MIN.saturating_mul(factor).min(BACKOFF_MAX);
        (self.attempt, base + jitter(base))
    }
}

/// Up to a fifth of `base`, so clients that failed together retry apart.
fn jitter(base: Duration) -> Duration {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        return Duration::ZERO;
    }
    let span = u64::try_from(base.as_millis() / 5).unwrap_or(0);
    Duration::from_millis(u64::from_le_bytes(bytes) % span.saturating_add(1))
}

/// Lifecycle state owned by the actor.
pub(super) struct Lifecycle<B: Backend> {
    pub(super) phase: Phase,
    step: Option<JoinHandle<Step<B>>>,
    /// Live critical snapshot of the last retired instance; `None` before the
    /// first retirement (the first build loads persisted state).
    session: Option<SessionSnapshot>,
    backoff: Backoff,
    /// Delay before the build that follows the running retirement.
    pending_delay: Duration,
    timeouts: VecDeque<Instant>,
    migration: Migration,
}

impl<B: Backend> Default for Lifecycle<B> {
    fn default() -> Self {
        Self {
            phase: Phase::Building,
            step: None,
            session: None,
            backoff: Backoff::default(),
            pending_delay: Duration::ZERO,
            timeouts: VecDeque::with_capacity(STALL_TIMEOUTS),
            migration: Migration::Never,
        }
    }
}

/// Awaits the running step, if any.
pub(super) async fn next_step<B: Backend>(
    slot: &mut Option<JoinHandle<Step<B>>>,
) -> Result<Step<B>, JoinError> {
    let Some(handle) = slot.as_mut() else {
        return std::future::pending().await;
    };
    let res = handle.await;
    *slot = None;
    res
}

impl<B: Backend> Lifecycle<B> {
    pub(super) fn step_mut(&mut self) -> &mut Option<JoinHandle<Step<B>>> {
        &mut self.step
    }
}

/// One live client instance and everything bound to it.
pub(super) struct Live<B: Backend> {
    pub(super) client: Arc<B::Client>,
    control: B::Control,
    runner: Option<JoinHandle<()>>,
    /// The update stream until the bootstrap starts its consumer (only once
    /// the account is signed in).
    updates: std::sync::Mutex<Option<B::Updates>>,
    pub(super) tasks: JoinSet<Done>,
    pub(super) kinds: HashMap<Id, TaskKind>,
    pub(super) cancel: CancellationToken,
    cgen: ClientGen,
}

pub(super) enum LiveEvent {
    Joined(Result<(Id, Done), JoinError>),
    /// The runner returned or panicked without being asked to quit.
    RunnerEnded,
}

/// Next event of the live instance; pending when there is none.
pub(super) async fn next_live<B: Backend>(live: &mut Option<Live<B>>) -> LiveEvent {
    let Some(l) = live.as_mut() else {
        return std::future::pending().await;
    };
    let runner = async {
        match l.runner.as_mut() {
            Some(h) => {
                let _ = h.await;
            }
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        Some(joined) = l.tasks.join_next_with_id(), if !l.tasks.is_empty() => {
            LiveEvent::Joined(joined)
        }
        () = runner => {
            l.runner = None;
            LiveEvent::RunnerEnded
        }
    }
}

/// A wait capped at a deadline that moves closer once shutdown starts: from
/// then on it is at most `short` away (never later than the original).
struct Cap<'a> {
    at: Instant,
    short: Duration,
    shutdown: &'a CancellationToken,
}

impl<'a> Cap<'a> {
    fn new(cap: Duration, short: Duration, shutdown: &'a CancellationToken) -> Self {
        let now = Instant::now();
        let at = if shutdown.is_cancelled() {
            now + cap.min(short)
        } else {
            now + cap
        };
        Self {
            at,
            short,
            shutdown,
        }
    }

    /// `None` when the cap expired first.
    async fn run<F: Future>(&mut self, fut: F) -> Option<F::Output> {
        let mut fut = pin!(fut);
        if !self.shutdown.is_cancelled() {
            tokio::select! {
                res = tokio::time::timeout_at(self.at, &mut fut) => return res.ok(),
                () = self.shutdown.cancelled() => {}
            }
        }
        self.at = self.at.min(Instant::now() + self.short);
        tokio::time::timeout_at(self.at, fut).await.ok()
    }
}

/// Retires an instance (plan section 2, rebuild steps 2-6). Returns once its
/// runner has completed or its cancellation has completed, and its session
/// is revoked. `shutdown` shortens the remaining caps to [`SHUTDOWN_CAPS`], so
/// a retirement running when shutdown starts still finishes all of its steps,
/// promptly, and never detaches a runner.
pub(super) async fn retire<B: Backend>(
    live: Live<B>,
    caps: Caps,
    shutdown: &CancellationToken,
) -> Retired {
    let Live {
        client,
        control,
        runner,
        updates,
        mut tasks,
        mut kinds,
        cancel,
        cgen,
    } = live;
    cancel.cancel();
    drop(updates);
    let mut dones = Vec::with_capacity(MAX_CONCURRENT);
    let mut cap = Cap::new(caps.tasks, SHUTDOWN_CAPS.tasks, shutdown);
    while let Some(Some(joined)) = cap.run(tasks.join_next_with_id()).await {
        keep(joined, &mut kinds, cgen, &mut dones);
    }
    tasks.abort_all();
    while let Some(joined) = tasks.join_next_with_id().await {
        keep(joined, &mut kinds, cgen, &mut dones);
    }
    control.quit();
    if let Some(mut handle) = runner {
        let mut cap = Cap::new(caps.runner, SHUTDOWN_CAPS.runner, shutdown);
        if cap.run(&mut handle).await.is_none() {
            // `abort` only schedules cancellation: await it to completion.
            handle.abort();
            let _ = handle.await;
        }
    }
    let snapshot = control.retire();
    drop(client);
    Retired { snapshot, dones }
}

/// Keeps the completions retirement must hand back; a send (or folder write)
/// whose task was aborted or panicked has an unknown outcome. Folder writes
/// are handed back so the account's write slot is freed (`folder_writes`).
fn keep(
    joined: Result<(Id, Done), JoinError>,
    kinds: &mut HashMap<Id, TaskKind>,
    cgen: ClientGen,
    dones: &mut Vec<Done>,
) {
    match joined {
        Ok((id, done)) => {
            kinds.remove(&id);
            if done.parked.is_some()
                || matches!(
                    done.outcome,
                    Outcome::Send { .. }
                        | Outcome::SendExpired { .. }
                        | Outcome::FolderWrite { .. }
                )
            {
                dones.push(done);
            }
        }
        Err(e) => {
            let outcome = match kinds.remove(&e.id()) {
                Some(TaskKind::Send(local_id)) => Outcome::Send {
                    local_id,
                    res: Err(CallError::Dropped),
                },
                Some(TaskKind::FolderWrite(write)) => Outcome::FolderWrite {
                    write,
                    moved: None,
                    res: Err(CallError::Dropped),
                },
                _ => return,
            };
            dones.push(Done {
                cgen,
                parked: None,
                outcome,
            });
        }
    }
}

impl<B: Backend> Actor<B> {
    /// Schedules a build after `delay`.
    pub(super) fn start_build(&mut self, delay: Duration) {
        let session = self
            .life
            .session
            .clone()
            .map_or(SessionSeed::Stored, SessionSeed::Live);
        let seed = Seed { session };
        let backend = Arc::clone(&self.backend);
        self.life.phase = Phase::Building;
        self.life.step = Some(tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            Step::Built(backend.build(seed).await)
        }));
    }

    pub(super) async fn on_step(&mut self, res: Result<Step<B>, JoinError>) {
        match (res, self.life.phase) {
            (Ok(Step::Built(Ok(instance))), _) => {
                self.go_live(instance);
                self.after_live().await;
            }
            (Ok(Step::Built(Err(_))) | Err(_), Phase::Building) => self.build_failed().await,
            (Ok(Step::Retired(retired)), Phase::Retiring(after)) => {
                self.finish_retire(retired, after).await;
            }
            // A retirement panicked: the instance is gone with it, so carry on
            // with the last known snapshot.
            (Err(_), Phase::Retiring(after)) => {
                self.admission.release_parked();
                self.folder_writes_lost_with_retirement().await;
                self.folder_transport_retired().await;
                self.after_retire(after).await;
            }
            (Ok(Step::SignedOut { remote, retired }), _) => {
                self.after_remote_logout(remote, retired).await;
            }
            (Err(_), Phase::LoggingOut(LogoutStage::Remote)) => {
                self.after_remote_logout(RemoteLogout::Failed(UserError::Internal), None)
                    .await;
            }
            (Ok(Step::Deleted(done)), _) => self.finish_logout(done).await,
            (Err(_), Phase::LoggingOut(LogoutStage::Local)) => {
                self.finish_logout(LocalDone::failed(StorageError::Internal))
                    .await;
            }
            _ => {}
        }
    }

    async fn build_failed(&mut self) {
        self.emit(Event::Connection(ConnState::Offline)).await;
        let (_, delay) = self.life.backoff.next(Instant::now());
        self.start_build(delay);
    }

    /// Starts the instance's runner. A signed-in account bootstraps at once;
    /// otherwise the instance only serves login calls.
    fn go_live(&mut self, instance: Instance<B>) {
        let Instance {
            client,
            updates,
            runner,
            control,
        } = instance;
        self.live = Some(Live {
            client: Arc::new(client),
            control,
            runner: Some(tokio::spawn(runner)),
            updates: std::sync::Mutex::new(Some(updates)),
            tasks: JoinSet::new(),
            kinds: HashMap::new(),
            cancel: CancellationToken::new(),
            cgen: self.cgen,
        });
        if self.auth.signed_in {
            self.start_bootstrap();
        } else {
            self.life.phase = Phase::SignedOut;
        }
    }

    /// Starts the update consumer, then the bootstrap (plan section 2). The
    /// stream's first `next` establishes the baseline, so this runs only once
    /// the account is signed in.
    pub(super) fn start_bootstrap(&mut self) {
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let updates = live
            .updates
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(updates) = updates {
            let consumer = jobs::consume_updates(
                updates,
                self.cgen,
                self.updates_tx.clone(),
                live.cancel.clone(),
            );
            let handle = live.tasks.spawn(consumer);
            live.kinds.insert(handle.id(), TaskKind::Updates);
        }
        self.life.phase = Phase::Booting;
        self.boot = Some(Boot::full(Instant::now()));
    }

    /// Replaces the instance. Coalesces: a trigger while no instance is up is
    /// ignored, so each failure causes exactly one rebuild.
    pub(super) async fn rebuild(&mut self, reason: Reason) {
        if !matches!(
            self.life.phase,
            Phase::Booting | Phase::Live | Phase::SignedOut
        ) {
            return;
        }
        let Some(live) = self.live.take() else {
            return;
        };
        let after = self.after_for(reason);
        if after == After::Rebuild {
            self.begin_catch_up();
        }
        for (thread, dir) in self.begin_retire() {
            // The UI keeps its window across a rebuild: tell it the loads it
            // waits on are gone, so its backoff and Retry take over (rounds
            // 20 and 22).
            self.history_ended(thread, dir, Some(UserError::Busy)).await;
        }
        self.life.phase = Phase::Retiring(after);
        if after == After::Rebuild {
            let (attempt, delay) = self.life.backoff.next(Instant::now());
            self.life.pending_delay = delay;
            self.emit(Event::Connection(ConnState::Rebuilding { attempt }))
                .await;
        }
        let shutdown = self.cancel.clone();
        self.life.step = Some(tokio::spawn(async move {
            Step::Retired(retire(live, REBUILD_CAPS, &shutdown).await)
        }));
    }

    /// An authenticated migration rebuilds once; a second one halts. (The
    /// failed call already reported `AccountMigrated`.)
    fn after_for(&mut self, reason: Reason) -> After {
        if reason != Reason::Migrated {
            return After::Rebuild;
        }
        match self.life.migration {
            Migration::Never => {
                self.life.migration = Migration::Once;
                After::Rebuild
            }
            Migration::Once => After::Halt,
        }
    }

    /// Rebuild step 1: stop admission and make every completion of the old
    /// instance stale. Parked permits stay parked until retirement completes.
    /// Returns the loads the UI waits on, which retirement abandons: the
    /// owed one, queued or running (its late result is stale), and the
    /// deferred ones, which could only queue for an instance that may never
    /// come (round 22), and an anchored open's load. Each is taken here, so
    /// nothing answers or starts it again (task 10g). Distinct, at most
    /// `2 + MAX_DEFERRED`.
    #[must_use]
    pub(super) fn begin_retire(&mut self) -> Vec<(ThreadKey, HistoryDir)> {
        self.cgen = ClientGen(self.cgen.0 + 1);
        self.boot = None;
        self.notices.retire(self.badges.now());
        self.life.timeouts.clear();
        self.history_busy.clear();
        self.dialogs_busy = false;
        self.dialogs_queued = false;
        self.purge_queued();
        // The `peer_dialogs` call in flight is stale now: its kind checks go
        // back to the head of the FIFO, unspent (plan section 6.6).
        self.peer.drop_in_flight();
        self.fences.return_flight();
        let mut waiting = Vec::with_capacity(self.deferred.len().saturating_add(2));
        let owed = self.owed.take().map(|o| (o.load.thread, o.dir));
        // An anchored open's load ends too: the UI falls back to
        // `OpenThread` and its cached tail (best effort, `anchored`).
        let anchored = self.anchored.take().map(|a| (a.load.thread, a.dir()));
        self.restoring = None;
        let deferred = self.deferred.drain(..).map(|d| (d.thread, d.dir));
        for target in owed.into_iter().chain(anchored).chain(deferred) {
            if !waiting.contains(&target) {
                waiting.push(target);
            }
        }
        waiting
    }

    async fn finish_retire(&mut self, retired: Retired, after: After) {
        let Retired { snapshot, dones } = retired;
        self.life.session = Some(snapshot);
        for done in dones {
            self.on_done(done).await;
        }
        // The runner has completed: grammers' queues are gone with it.
        self.admission.release_parked();
        self.folder_transport_retired().await;
        self.after_retire(after).await;
    }

    async fn after_retire(&mut self, after: After) {
        match after {
            After::Rebuild => self.start_build(self.life.pending_delay),
            After::Halt => {
                self.life.phase = Phase::Halted;
                self.emit(Event::Connection(ConnState::Offline)).await;
                self.abandon_account().await;
            }
        }
    }

    /// Clears a migration halt: the next repeated migration rebuilds again.
    pub(super) fn clear_halt(&mut self) {
        self.life.migration = Migration::Never;
    }

    /// Forgets the retired instance's snapshot, so the next build seeds from
    /// the account (a new generation after logout).
    pub(super) fn forget_instance_session(&mut self) {
        self.life.session = None;
    }

    /// Bootstrap finished: resume admission and replay unresolved sends.
    pub(super) async fn go_online(&mut self) {
        self.life.phase = Phase::Live;
        self.life.backoff.stable_since = Some(Instant::now());
        self.emit(Event::Connection(ConnState::Online)).await;
        self.replay_sends().await;
    }

    /// Stall and migration detection on a current-generation call result.
    pub(super) async fn inspect_call(&mut self, err: CallError) {
        match err {
            CallError::Backend(BackendError::Migrated) => self.rebuild(Reason::Migrated).await,
            // AUTH_KEY_UNREGISTERED / SESSION_REVOKED: local logout, then sign in.
            CallError::Backend(BackendError::Unauthorized) => self.session_revoked().await,
            CallError::Timeout => {
                let now = Instant::now();
                let t = &mut self.life.timeouts;
                while t
                    .front()
                    .is_some_and(|f| now.duration_since(*f) > STALL_WINDOW)
                    || t.len() >= STALL_TIMEOUTS
                {
                    t.pop_front();
                }
                t.push_back(now);
                if t.len() >= STALL_TIMEOUTS || self.admission.parked() >= MAX_CONCURRENT {
                    self.rebuild(Reason::Stall).await;
                }
            }
            CallError::Backend(_) | CallError::Dropped => {}
        }
    }

    /// Shutdown (plan section 5, steps 3 and 5): retires whatever instance
    /// exists and finishes a running logout. A running retirement is never
    /// aborted: it owns the runner and the session's write token, and it
    /// shortens its own caps once shutdown starts, so it is awaited to the end
    /// (abort, join, revoke). Finishing after `deadline` makes the save
    /// unconfirmed.
    pub(super) async fn shut_down_lifecycle(&mut self, deadline: Instant) -> LifeAtShutdown {
        let mut out = LifeAtShutdown {
            retired: true,
            logout: None,
        };
        if let Some(mut step) = self.life.step.take() {
            match self.life.phase {
                // Nothing is running yet: an unspawned instance just drops.
                Phase::Building => {
                    step.abort();
                    let _ = step.await;
                }
                Phase::LoggingOut(LogoutStage::Local) => {
                    let res = tokio::time::timeout_at(deadline, &mut step).await;
                    let done = if let Ok(Ok(Step::Deleted(done))) = res {
                        Some(done.local)
                    } else {
                        step.abort();
                        let _ = step.await;
                        None
                    };
                    out.logout = Some(self.logout_status(done));
                }
                _ => {
                    let res = step.await;
                    out.retired = res.is_ok() && Instant::now() <= deadline;
                    if let Ok(Step::SignedOut { remote, .. }) = res {
                        self.note_remote_logout(remote);
                        let local = self.delete_local_until(deadline).await;
                        out.logout = Some(self.logout_status(local));
                    }
                }
            }
        }
        if let Some(live) = self.live.take() {
            let shutdown = self.cancel.clone();
            retire(live, SHUTDOWN_CAPS, &shutdown).await;
            out.retired &= Instant::now() <= deadline;
        }
        if out.logout.is_none() {
            out.logout = self.unfinished_logout_status();
        }
        out
    }
}

/// What shutdown observed of the lifecycle.
#[derive(Debug)]
pub(super) struct LifeAtShutdown {
    /// Every retirement completed (runner joined, session revoked) within the
    /// deadline.
    pub(super) retired: bool,
    pub(super) logout: Option<LogoutStatus>,
}

impl<B: Backend> Actor<B> {
    /// Retires the signed-out instance: one built for API credentials no
    /// longer in use (Back, Forget, new or rejected credentials; task 6b), or
    /// one an ambiguous sign-in may still be queued on, before the probe
    /// (task 6d). Like a rebuild, without backoff or a `Rebuilding` event:
    /// the next build waits for the credential slot to hold a pair.
    pub(super) fn retire_signed_out(&mut self) {
        if self.life.phase != Phase::SignedOut {
            return;
        }
        let Some(live) = self.live.take() else {
            return;
        };
        // Signed out: no chat is open, so no load is waited on.
        let _ = self.begin_retire();
        self.life.phase = Phase::Retiring(After::Rebuild);
        self.life.pending_delay = Duration::ZERO;
        let shutdown = self.cancel.clone();
        self.life.step = Some(tokio::spawn(async move {
            Step::Retired(retire(live, REBUILD_CAPS, &shutdown).await)
        }));
    }
}
