//! Starting core: a dedicated `abyss-core` OS thread running a small multi-thread
//! tokio runtime, plus the handle the UI talks to.
//!
//! Startup (task 6): spawn the persistence worker (the Keychain in
//! production, an in-memory store under the fake), load the session, provision
//! the cache key and open the cache, then run the actor. Storage problems at
//! startup are reported as events, never as a failed start.

use std::path::PathBuf;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::actor::{
    Actor, ActorChannels, ActorInit, CacheParts, Identity, Internal, PendingLogout, Stamped,
    provision_cache, read_pending_logout,
};

use crate::api::{Command, Event, LogoutStatus, SaveStatus, UserError};
use crate::backend::Backend;
#[cfg(target_os = "macos")]
use crate::backend::grammers::GrammersBackend;
use crate::events::{self, EpochObserver, EventReceiver, EventSender};
use crate::secret::SecretStore;
use crate::session::PersistHandle;

/// Queued UI commands.
const COMMAND_CAPACITY: usize = 256;
/// Queued supervisor messages (rebuilds are rare and coalesce).
const INTERNAL_CAPACITY: usize = 8;
/// Runtime worker threads.
const WORKERS: usize = 2;
/// How long shutdown waits for spawned tasks after the actor stops.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(1);
/// How long the core thread then waits for its cache workers to close.
const CACHE_CLOSE_WAIT: Duration = Duration::from_secs(2);

/// Where the encrypted message cache lives. Core provisions its key.
#[derive(Debug)]
pub struct CacheConfig {
    pub dir: PathBuf,
}

#[derive(Debug, Default)]
pub struct CoreConfig {
    /// Required for the real backend. Local storage unless explicitly opted in.
    pub secret_storage: Option<crate::secret::SecretStorageConfig>,
    /// `None` runs without a disk cache.
    pub cache: Option<CacheConfig>,
    /// Require the UI to acknowledge platform notification cleanup on logout.
    pub notification_cleanup: events::NotificationCleanupPolicy,
    /// Faults for manual testing of the fake (`ABYSS_FAKE_FAULTS`).
    #[cfg(feature = "fake")]
    pub fake_faults: FakeFaults,
}

/// Environment variable naming fake faults, parsed only in `fake` builds.
#[cfg(feature = "fake")]
pub const FAKE_FAULTS_VAR: &str = "ABYSS_FAKE_FAULTS";

/// Store faults for `--fake` runs: `store-stall` holds every Keychain call
/// after startup (each for at most 10 s), `store-fail` fails every write.
/// Comma-separated; unknown names are ignored.
#[cfg(feature = "fake")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FakeFaults {
    pub store_stall: bool,
    pub store_fail: bool,
}

#[cfg(feature = "fake")]
impl FakeFaults {
    pub fn parse(spec: &str) -> Self {
        let mut faults = Self::default();
        for name in spec.split(',').map(str::trim) {
            match name {
                "store-stall" => faults.store_stall = true,
                "store-fail" => faults.store_fail = true,
                _ => {}
            }
        }
        faults
    }
}

#[derive(Debug)]
pub enum BackendChoice {
    /// The real backend (task 11). The env pair, if any, is resolved first;
    /// then the Keychain, the embedded pair, or onboarding (plan 17.1).
    Grammers(crate::credentials::EnvResolution),
    #[cfg(feature = "fake")]
    Fake(crate::backend::fake::FakeBackend),
}

#[derive(Debug)]
pub enum StartError {
    /// The private secret storage location or preference is unavailable.
    SecretStorage,
    Runtime(std::io::Error),
    Thread(std::io::Error),
    /// The persistence worker thread could not start.
    Persist(std::io::Error),
    /// The real backend needs the macOS Keychain. Other hosts build and
    /// test against the fake backend only.
    #[cfg(not(target_os = "macos"))]
    UnsupportedPlatform,
}

/// Why a command was not queued. Carries the command back.
#[derive(Debug)]
pub enum SendError {
    /// The command queue is full; core is busy.
    Full(Command),
    /// Core has stopped.
    Closed(Command),
}

/// What core observed while stopping (plan section 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShutdownReport {
    /// `Confirmed` only if the instance was retired (runner joined, session
    /// revoked) and the persistence barrier succeeded within the deadline.
    pub save: SaveStatus,
    /// Present when a logout was running or finished during shutdown.
    pub logout: Option<LogoutStatus>,
}

impl ShutdownReport {
    pub const UNCONFIRMED: Self = Self {
        save: SaveStatus::Unconfirmed,
        logout: None,
    };
}

/// The UI's side of core's stop requests (round 30). Core cancels when a
/// request arrives, or when every handle half holding one is dropped (the
/// channel closes): see [`stop_on_request`]. Asking never takes the
/// cancellation token's lock on the UI thread.
type StopRequests = mpsc::Sender<()>;

/// The command half of a [`CoreHandle`].
#[derive(Debug)]
pub struct CoreSender {
    cmd: mpsc::Sender<Stamped>,
    /// The account epoch the UI has seen (stamped on each command).
    epoch: EpochObserver,
    stop: StopRequests,
    done: Option<oneshot::Receiver<ShutdownReport>>,
    /// Control plane: never queued behind UI commands.
    internal: mpsc::Sender<Internal>,
}

/// The event half of a [`CoreHandle`].
#[derive(Debug)]
pub struct CoreEvents {
    events: EventReceiver,
    _stop: StopRequests,
}

/// The UI's connection to core. Dropping it (both halves, once split) stops
/// core.
#[derive(Debug)]
pub struct CoreHandle {
    sender: CoreSender,
    events: CoreEvents,
}

impl CoreSender {
    /// Tells core the system woke from sleep (`NSWorkspaceDidWakeNotification`).
    /// Core rebuilds the client rather than trust sockets across sleep. Never
    /// blocks; wakes that arrive while one is pending coalesce.
    pub fn notify_wake(&self) {
        let _ = self.internal.try_send(Internal::Wake);
    }

    /// Queues a command without waiting.
    pub fn send(&self, cmd: Command) -> Result<(), SendError> {
        let stamped = Stamped {
            epoch: self.epoch.get(),
            cmd,
        };
        self.cmd.try_send(stamped).map_err(|e| match e {
            mpsc::error::TrySendError::Full(s) => SendError::Full(s.cmd),
            mpsc::error::TrySendError::Closed(s) => SendError::Closed(s.cmd),
        })
    }

    /// Asks core to stop. The receiver resolves when the core thread is done
    /// (`None` if called twice). Never blocks on core: see [`Self::cancel`].
    pub fn begin_shutdown(&mut self) -> Option<oneshot::Receiver<ShutdownReport>> {
        self.cancel();
        self.done.take()
    }

    /// Asks core to stop, without waiting for its report. Safe on the UI
    /// thread during quit (round 30): a `try_send` into a one-slot channel,
    /// so it never waits for capacity or for core. A request already queued,
    /// or a core already gone, makes it a no-op. The token is cancelled on
    /// a core runtime thread. Not lock-free: waking that thread takes
    /// tokio's scheduler locks for a queue push and an unpark, held by
    /// other threads only for the same O(1) steps.
    pub fn cancel(&self) {
        let _ = self.stop.try_send(());
    }
}

impl CoreEvents {
    /// The account epoch carried by the last event this receiver returned.
    /// Read it when taking a notice, before taking another event.
    pub fn account_epoch(&self) -> u64 {
        self.events.observer().get()
    }

    /// Next event; `None` once core has stopped.
    pub async fn recv(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    /// Polls for the next event; `Ready(None)` once core has stopped.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        self.events.poll_recv(cx)
    }

    pub fn try_recv(&mut self) -> Option<Event> {
        self.events.try_recv()
    }
}

impl CoreHandle {
    /// Splits into the command and event halves, so the UI's event pump can
    /// own the receiver. Core stops once both halves are dropped.
    pub fn split(self) -> (CoreSender, CoreEvents) {
        (self.sender, self.events)
    }

    pub fn notify_wake(&self) {
        self.sender.notify_wake();
    }

    pub fn send(&self, cmd: Command) -> Result<(), SendError> {
        self.sender.send(cmd)
    }

    pub async fn recv(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        self.events.poll_recv(cx)
    }

    pub fn try_recv(&mut self) -> Option<Event> {
        self.events.try_recv()
    }

    pub fn begin_shutdown(&mut self) -> Option<oneshot::Receiver<ShutdownReport>> {
        self.sender.begin_shutdown()
    }
}

/// Starts core on its own thread.
#[cfg_attr(
    not(any(target_os = "macos", feature = "fake")),
    expect(
        clippy::needless_pass_by_value,
        reason = "off macOS without the fake, every arm refuses and consumes nothing"
    )
)]
pub fn start(config: CoreConfig, backend: BackendChoice) -> Result<CoreHandle, StartError> {
    match backend {
        #[cfg(not(target_os = "macos"))]
        BackendChoice::Grammers(_) => {
            let _ = config;
            Err(StartError::UnsupportedPlatform)
        }
        #[cfg(target_os = "macos")]
        BackendChoice::Grammers(env) => {
            let storage = config
                .secret_storage
                .as_ref()
                .ok_or(StartError::SecretStorage)?;
            let store = crate::secret::SwitchingStore::new(
                storage.clone(),
                crate::secret::KeychainStore::with_namespace(
                    storage.keychain_namespace.as_str(),
                    crate::secret::KeychainLocation::File,
                ),
            );
            let slot = crate::credentials::CredentialSlot::new(env);
            let make = move |persist| GrammersBackend::new(slot, persist);
            start_with(config, make, store, || {})
        }
        #[cfg(feature = "fake")]
        BackendChoice::Fake(fake) => start_fake(config, fake),
    }
}

#[cfg(feature = "fake")]
fn start_fake(
    config: CoreConfig,
    fake: crate::backend::fake::FakeBackend,
) -> Result<CoreHandle, StartError> {
    use crate::secret::{FaultyStore, MemoryStore, StoreOp};
    let (store, control) = FaultyStore::new(MemoryStore::new());
    let faults = config.fake_faults;
    if faults.store_fail {
        control.fail(StoreOp::Set);
    }
    // Stall only after the startup reads, so the app still comes up.
    let after_load = move || {
        if faults.store_stall {
            control.hold();
        }
    };
    start_with(config, move |_| fake, store, after_load)
}

/// Everything the core thread needs to bring the actor up.
struct Boot<B> {
    backend: B,
    persist: PersistHandle,
    cache: Option<CacheConfig>,
    events: EventSender,
    commands: mpsc::Receiver<Stamped>,
    internal: mpsc::Receiver<Internal>,
    cancel: CancellationToken,
}

/// Before the shutdown ack: waits for every cache worker on core's cache
/// directory to close its database. The actor closes the caches it holds,
/// but a worker can outlive it: a handle dropped without `close`, a close
/// past `CACHE_FLUSH`, or an open still running on a blocking thread that
/// `shutdown_timeout` abandoned (it took its census entry before it was
/// scheduled). The UI quits on the ack, and SQLCipher's exit-time teardown
/// must not race a close (exit SIGSEGV, section 13). This wait is best
/// effort; the exit hook is the hard bound (`cache::census`).
fn wait_caches_closed(dir: Option<&std::path::Path>) {
    if let Some(dir) = dir
        && !crate::cache::wait_closed(Some(dir), CACHE_CLOSE_WAIT)
    {
        tracing::warn!("a cache worker was still open at shutdown");
    }
}

/// Starts the persistence worker over `store`, builds the backend over it
/// (`make`), and runs the actor on the core thread.
fn start_with<B: Backend>(
    config: CoreConfig,
    make: impl FnOnce(PersistHandle) -> B,
    store: impl SecretStore,
    after_load: impl FnOnce() + Send + 'static,
) -> Result<CoreHandle, StartError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKERS)
        .thread_name("abyss-rt")
        .enable_all()
        .build()
        .map_err(StartError::Runtime)?;
    if config.cache.is_some() {
        // Before anything can open a cache, and on the caller's thread, so
        // no quit can race it: SQLCipher's exit handler, then ours (round 23,
        // finding 1). A failure is logged by `init`; the run continues
        // without a cache, since every open is then refused.
        let _ = crate::cache::init();
    }
    let persist = PersistHandle::spawn(store).map_err(StartError::Persist)?;
    let backend = make(persist.clone());
    let (cmd_tx, commands) = mpsc::channel(COMMAND_CAPACITY);
    let (internal_tx, internal) = mpsc::channel(INTERNAL_CAPACITY);
    let (events, events_rx) = events::channel_with_notifications(config.notification_cleanup);
    let (done_tx, done_rx) = oneshot::channel();
    let cancel = CancellationToken::new();
    let (stop, stop_rx) = mpsc::channel(1);
    let stopper = stop_on_request(stop_rx, cancel.clone());
    let boot = Boot {
        backend,
        persist,
        cache: config.cache,
        events,
        commands,
        internal,
        cancel,
    };
    std::thread::Builder::new()
        .name("abyss-core".into())
        .spawn(move || {
            let cache_dir = boot.cache.as_ref().map(|c| c.dir.clone());
            let report = runtime.block_on(async move {
                // First, so a stop asked for before anything else runs
                // still cancels startup.
                drop(tokio::spawn(stopper));
                // Shutdown during startup (a stalled Keychain read or
                // prompt): nothing was confirmed. The persistence worker is
                // its own thread, so its stalled call stays off this path.
                let Some((actor, channels)) = bring_up(boot, after_load).await else {
                    return ShutdownReport::UNCONFIRMED;
                };
                // Spawned so a panic in the actor itself is contained and reported.
                tokio::spawn(actor.run(channels))
                    .await
                    .unwrap_or(ShutdownReport::UNCONFIRMED)
            });
            runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
            wait_caches_closed(cache_dir.as_deref());
            let _ = done_tx.send(report);
        })
        .map_err(StartError::Thread)?;
    let epoch = events_rx.observer();
    Ok(CoreHandle {
        sender: CoreSender {
            cmd: cmd_tx,
            epoch,
            stop: stop.clone(),
            done: Some(done_rx),
            internal: internal_tx,
        },
        events: CoreEvents {
            events: events_rx,
            _stop: stop,
        },
    })
}

/// Cancels core on the first stop request, or once every [`StopRequests`]
/// sender is dropped (the UI dropped its handle). Runs on core's runtime,
/// so the token's lock and the wakeups of everything waiting on it stay off
/// the UI thread (round 30).
async fn stop_on_request(mut requests: mpsc::Receiver<()>, cancel: CancellationToken) {
    let _ = requests.recv().await;
    cancel.cancel();
}

/// [`prepare`], abandoned when shutdown starts first (review round 8): the
/// startup waits must never hold up the shutdown acknowledgement.
async fn bring_up<B: Backend>(
    boot: Boot<B>,
    after_load: impl FnOnce(),
) -> Option<(Actor<B>, ActorChannels)> {
    let cancel = boot.cancel.clone();
    tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        up = prepare(boot, after_load) => Some(up),
    }
}

/// Loads the session and opens the cache (task 5 wiring, steps 1-2). A
/// Keychain that cannot be read counts as logged out, with a warning. The
/// cache is bound to the loaded account, or quarantined (plan section 16).
async fn prepare<B: Backend>(
    boot: Boot<B>,
    after_load: impl FnOnce(),
) -> (Actor<B>, ActorChannels) {
    let Boot {
        backend,
        persist,
        cache,
        events,
        commands,
        internal,
        cancel,
    } = boot;
    // Before the session: an interrupted logout is finished first, and the
    // old account's session is never loaded (review round 15, A).
    let pending_logout = read_pending_logout(&persist, &events).await;
    let blocked = pending_logout.is_some_and(PendingLogout::blocks_session);
    let (account, identity) = if blocked {
        (persist.fresh_session(), Identity::Unknown)
    } else {
        load_account(&persist, &events).await
    };
    let (handle, cache_parts) = match (cache, blocked) {
        // Opening may destroy or recreate the cache. Leave that to the
        // logout's ordered local step, or to startup once the marker is resolved.
        (Some(CacheConfig { dir }), true) => (None, Some(CacheParts::closed(dir))),
        (Some(CacheConfig { dir }), _) => {
            let provisioned = provision_cache(&persist, dir, identity).await;
            (provisioned.served, Some(provisioned.parts))
        }
        (None, _) => (None, None),
    };
    after_load();
    Actor::new(ActorInit {
        backend: Arc::new(backend),
        events,
        cache: handle,
        commands,
        internal,
        cancel,
        persist,
        account,
        cache_parts,
        pending_logout,
    })
}

/// Loads the stored session. A Keychain that cannot be read leaves the
/// account unknown, with a warning.
pub(crate) async fn load_account(
    persist: &PersistHandle,
    events: &EventSender,
) -> (crate::session::SessionSnapshot, Identity) {
    match persist.load_session().await {
        Ok(loaded) => {
            let identity = match loaded.snapshot.self_id() {
                Ok(Some(id)) => Identity::Verified(id),
                _ => Identity::SignedOut,
            };
            (loaded.snapshot, identity)
        }
        Err(e) => {
            let _ = events.send(Event::Error(UserError::from(&e))).await;
            (persist.fresh_session(), Identity::Unknown)
        }
    }
}

#[cfg(all(test, feature = "fake"))]
mod startup_tests;

#[cfg(all(test, feature = "fake"))]
mod tests {
    use super::*;
    use crate::api::AuthState;
    use crate::backend::fake::{FAKE_CODE, FakeBackend, FakeScript};

    /// The next event matching `pred`; `None` if core stopped or 10 s passed.
    async fn until(core: &mut CoreHandle, pred: impl Fn(&Event) -> bool) -> Option<Event> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match core.recv().await {
                    Some(e) if pred(&e) => return Some(e),
                    Some(_) => {}
                    None => return None,
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    #[tokio::test]
    async fn logs_in_loads_dialogs_and_shuts_down_cleanly() {
        let mut script = FakeScript::demo(3, 5);
        script.login.password = None;
        let (fake, _h) = FakeBackend::new(script);
        let mut core = start(CoreConfig::default(), BackendChoice::Fake(fake)).unwrap();
        until(&mut core, |e| {
            matches!(e, Event::Auth(AuthState::Phone { .. }))
        })
        .await;
        core.send(Command::SubmitPhone("+15550100".into())).unwrap();
        until(&mut core, |e| {
            matches!(e, Event::Auth(AuthState::Code { .. }))
        })
        .await;
        core.send(Command::SubmitCode(FAKE_CODE.into())).unwrap();
        let signed_in = until(
            &mut core,
            |e| matches!(e, Event::Auth(s) if s.is_signed_in()),
        )
        .await;
        assert!(matches!(
            signed_in,
            Some(Event::Auth(AuthState::SignedIn { .. }))
        ));
        let reset = until(
            &mut core,
            |e| matches!(e, Event::DialogsReset(v) if !v.is_empty()),
        )
        .await;
        assert!(matches!(reset, Some(Event::DialogsReset(v)) if v.len() == 3));
        let done = core.begin_shutdown().unwrap();
        let report = done.await.unwrap();
        assert_eq!(report.save, SaveStatus::Confirmed);
        assert_eq!(report.logout, None);
        assert!(core.begin_shutdown().is_none());
    }

    #[tokio::test]
    async fn split_halves_and_poll_recv() {
        let (fake, _h) = FakeBackend::new(FakeScript::default());
        let core = start(CoreConfig::default(), BackendChoice::Fake(fake)).unwrap();
        let (mut sender, mut events) = core.split();
        let first = std::future::poll_fn(|cx| events.poll_recv(cx)).await;
        assert!(matches!(first, Some(Event::Auth(AuthState::Connecting))));
        let report = sender.begin_shutdown().unwrap().await.unwrap();
        assert_eq!(report.save, SaveStatus::Confirmed);
    }

    #[tokio::test]
    async fn store_fail_fault_makes_the_login_unsaved() {
        let (fake, _h) = FakeBackend::new(FakeScript::default());
        let config = CoreConfig {
            fake_faults: FakeFaults::parse("store-fail, unknown"),
            ..CoreConfig::default()
        };
        let mut core = start(config, BackendChoice::Fake(fake)).unwrap();
        until(&mut core, |e| {
            matches!(e, Event::Auth(AuthState::Phone { .. }))
        })
        .await;
        core.send(Command::SubmitPhone("+15550100".into())).unwrap();
        until(&mut core, |e| {
            matches!(e, Event::Auth(AuthState::Code { .. }))
        })
        .await;
        core.send(Command::SubmitCode(FAKE_CODE.into())).unwrap();
        let ev = until(
            &mut core,
            |e| matches!(e, Event::Auth(s) if s.is_signed_in()),
        )
        .await;
        assert!(matches!(
            ev,
            Some(Event::Auth(AuthState::SignedInUnsaved { .. }))
        ));
        let report = core.begin_shutdown().unwrap().await.unwrap();
        assert_eq!(report.save, SaveStatus::Unconfirmed);
    }

    /// `--fake --onboarding`: no env pair and nothing stored, so core asks
    /// for credentials and builds nothing until a pair is entered.
    #[tokio::test]
    async fn onboarding_starts_from_need_credentials() {
        let script = FakeScript {
            env: crate::credentials::EnvResolution::Absent,
            ..FakeScript::default()
        };
        let (fake, h) = FakeBackend::new(script);
        let mut core = start(CoreConfig::default(), BackendChoice::Fake(fake)).unwrap();
        let ev = until(&mut core, |e| {
            matches!(e, Event::Auth(AuthState::NeedCredentials { .. }))
        })
        .await;
        assert!(matches!(
            ev,
            Some(Event::Auth(AuthState::NeedCredentials {
                error: None,
                source: None
            }))
        ));
        assert_eq!(h.builds(), 0);
        core.send(Command::SubmitCredentials {
            api_id: 12345,
            api_hash: crate::api::ApiHash::parse(&"a".repeat(32)).unwrap(),
        })
        .unwrap();
        until(&mut core, |e| {
            matches!(e, Event::Auth(AuthState::Phone { .. }))
        })
        .await;
        assert_eq!(h.builds(), 1);
        let report = core.begin_shutdown().unwrap().await.unwrap();
        assert_eq!(report.save, SaveStatus::Confirmed);
    }
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SecretStorage => {
                f.write_str("could not read the secret storage preference or locate its directory")
            }
            Self::Runtime(_) => f.write_str("could not start the async runtime"),
            Self::Thread(_) => f.write_str("could not start the core thread"),
            Self::Persist(_) => f.write_str("could not start the session store"),
            #[cfg(not(target_os = "macos"))]
            Self::UnsupportedPlatform => f.write_str("the real backend needs macOS"),
        }
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SecretStorage => None,
            Self::Runtime(e) | Self::Thread(e) | Self::Persist(e) => Some(e),
            #[cfg(not(target_os = "macos"))]
            Self::UnsupportedPlatform => None,
        }
    }
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full(_) => f.write_str("core is busy; command not queued"),
            Self::Closed(_) => f.write_str("core has stopped; command not queued"),
        }
    }
}

impl std::error::Error for SendError {}
