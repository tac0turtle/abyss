//! Bootstrap and light refresh (plan section 2): take snapshots only after the
//! update baseline, buffer live updates meanwhile, then merge the buffer over the
//! snapshots so a stale snapshot never overwrites a newer update.
//!
//! - **Full bootstrap** (after every build): wait for the stream's baseline (at
//!   most [`BASELINE_WAIT`], else rebuild), invalidate every message window, then
//!   fetch the first dialogs page and the open chat's tail.
//! - **Light refresh** (window activation, and every [`REFRESH_EVERY`] while
//!   online): the same snapshots on the live instance, without invalidation; the
//!   open chat's tail is merged into its window rather than replacing it.
//!
//! In both, a deletion seen while snapshots are pending goes into a bounded
//! tombstone set, so a snapshot fetched later cannot resurrect the message.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tokio::time::Instant;

use super::Actor;
use super::jobs::{self, Job, Load, ReqId, TaskKind};
use super::lifecycle::{Phase, Reason};
use super::notices::Difference;
use crate::api::{Event, HistoryDir, WindowState};
use crate::backend::{
    Backend, Baseline, HistoryQuery, NotifyScope, NotifyTarget, StaleHistory, StreamItem, Update,
    UpdateCheckpoint,
};
use crate::model::{ChatId, ChatSummary, MsgId, ThreadKey};
use crate::store::Guarded;
use crate::store::MAX_WINDOWS;

/// How long a new instance may take to establish its update baseline.
pub(super) const BASELINE_WAIT: Duration = Duration::from_secs(10);
/// Updates buffered while snapshots are pending; overflow rebuilds.
pub(super) const MAX_BOOT_BUFFER: usize = 4096;
/// Deleted ids remembered while snapshots are pending; overflow rebuilds.
pub(super) const MAX_TOMBSTONES: usize = 4096;
/// Light refresh period while signed in and online.
pub(super) const REFRESH_EVERY: Duration = Duration::from_mins(5);
/// Activation does not refresh again within this long of the last refresh.
pub(super) const MIN_REFRESH_GAP: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BootKind {
    Full,
    Refresh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Gap {
    Clear,
    Pending,
    /// The cache worker could not commit an invalidation. Retry at baseline.
    PurgeFailed,
}

impl Gap {
    pub(super) fn pending(self) -> bool {
        self != Self::Clear
    }
}

/// Which snapshot finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Snapshot {
    Dialogs {
        req: ReqId,
        applied: bool,
    },
    NotifySettings {
        applied: bool,
    },
    /// A tail load; it counts only if it is the thread's current snapshot.
    Tail {
        load: Load,
        applied: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DialogsSnapshot {
    Waiting,
    Pending(ReqId),
    Done,
}

/// Where a tail load starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TailVia {
    /// The cached tail first (if there is a cache), then the server's.
    Cache,
    Server,
}

/// Too many updates or deletions arrived while snapshots were pending.
#[derive(Debug)]
pub(super) struct Overflow;

#[derive(Debug)]
pub(super) struct Boot {
    pub(super) kind: BootKind,
    /// Deadline for the baseline; `None` once it is established.
    baseline_by: Option<Instant>,
    buffer: Vec<(Update, Source)>,
    suppress_notices: bool,
    tombstones: HashSet<(Option<ChatId>, MsgId)>,
    dialogs: DialogsSnapshot,
    scopes_pending: u8,
    checkpoint: Option<UpdateCheckpoint>,
    /// An update gap arrived while this boot's snapshots were in flight, so
    /// they may predate it. One refresh follows this boot, and the checkpoint
    /// (already past the gap) is only accepted once that refresh is done.
    trailing: bool,
    failed: bool,
    /// A gap refresh waits for a pre-gap history call before taking its tail.
    waiting_tail: Option<ThreadKey>,
    /// Pending tail snapshots, by `(thread, request)`: a snapshot completes
    /// (or fails) by its own identity, even after its view went stale, so it
    /// can never be orphaned. At most [`MAX_WINDOWS`] threads.
    tails: HashMap<ThreadKey, ReqId>,
    /// The top message id per chat in the dialogs snapshot (at most one page).
    tops: Option<HashMap<ChatId, MsgId>>,
}

impl Boot {
    pub(super) fn full(now: Instant) -> Self {
        Self {
            kind: BootKind::Full,
            baseline_by: Some(now + BASELINE_WAIT),
            buffer: Vec::new(),
            suppress_notices: false,
            tombstones: HashSet::new(),
            dialogs: DialogsSnapshot::Waiting,
            scopes_pending: 0,
            checkpoint: None,
            trailing: false,
            failed: false,
            waiting_tail: None,
            tails: HashMap::new(),
            tops: None,
        }
    }

    fn refresh() -> Self {
        Self {
            kind: BootKind::Refresh,
            baseline_by: None,
            ..Self::full(Instant::now())
        }
    }

    pub(super) fn baseline_deadline(&self) -> Option<Instant> {
        self.baseline_by
    }

    /// Buffers `update`, recording deletions as tombstones.
    fn push(&mut self, update: Update, source: Source) -> Result<(), Overflow> {
        if self.buffer.len() >= MAX_BOOT_BUFFER {
            return Err(Overflow);
        }
        if let Update::Deleted { chat, ids } = &update {
            for id in ids {
                if self.tombstones.len() >= MAX_TOMBSTONES {
                    return Err(Overflow);
                }
                self.tombstones.insert((*chat, *id));
            }
        }
        self.buffer.push((update, source));
        Ok(())
    }

    /// Whether `id` in `chat` was deleted while snapshots were pending.
    pub(super) fn is_dead(&self, chat: ChatId, id: MsgId) -> bool {
        self.tombstones.contains(&(Some(chat), id))
            || (chat.kind.has_account_unique_ids() && self.tombstones.contains(&(None, id)))
    }

    /// Whether `load` is this boot's current tail snapshot of its thread.
    pub(super) fn awaits_tail(&self, load: Load) -> bool {
        self.tails.get(&load.thread) == Some(&load.req)
    }

    pub(super) fn awaits_dialogs(&self, req: ReqId) -> bool {
        self.dialogs == DialogsSnapshot::Pending(req)
    }

    /// Whether a thread opened now gets its tail as a snapshot of this boot: a
    /// full bootstrap past its baseline, so the open is not stuck behind
    /// `Online` (UI jobs wait for it) and keeps the boot from finishing
    /// before its window is current.
    fn accepts_tail(&self, thread: ThreadKey) -> bool {
        self.baseline_by.is_none()
            && (self.tails.contains_key(&thread) || self.tails.len() < MAX_WINDOWS)
    }

    /// Records what the dialogs snapshot says about each chat's last message.
    pub(super) fn note_dialogs(&mut self, chats: &[ChatSummary]) {
        self.tops = Some(
            chats
                .iter()
                .filter_map(|c| Some((c.id, c.last_id?)))
                .collect(),
        );
    }

    fn snapshots_done(&self) -> bool {
        self.baseline_by.is_none()
            && self.dialogs == DialogsSnapshot::Done
            && self.scopes_pending == 0
            && self.waiting_tail.is_none()
            && self.tails.is_empty()
    }
}

/// How the stream delivered this update. A snapshot may have buffered
/// either source while it reconciled the dialog list or a thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Source {
    Live,
    CatchUp,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Origin<'a> {
    /// Dialog top ids from an in-flight snapshot that buffered the update.
    pub(super) tops: Option<&'a HashMap<ChatId, MsgId>>,
    pub(super) source: Source,
    pub(super) notify: bool,
}

impl<B: Backend> Actor<B> {
    pub(super) async fn on_feed(&mut self, item: StreamItem) {
        match item {
            StreamItem::Baseline(kind) => self.on_baseline(kind).await,
            StreamItem::Update(update) => self.feed_update(update, Source::Live).await,
            StreamItem::CatchUp(update) => {
                self.begin_catch_up();
                self.notices.difference = Difference::Replaying;
                self.feed_update(update, Source::CatchUp).await;
            }
            StreamItem::CatchUpGap => {
                self.begin_catch_up();
                self.notices.difference = Difference::Reconciling;
                self.invalidate_notice_mutes();
                self.reconcile_gap().await;
            }
            StreamItem::HistoryStale(StaleHistory(chat)) => {
                self.begin_catch_up();
                self.notices.difference = Difference::Replaying;
                self.invalidate_history(chat).await;
            }
            StreamItem::CatchUpEnded => {
                if std::mem::take(&mut self.notices.difference) == Difference::Replaying {
                    self.reconcile_gap().await;
                }
                self.flush_catch_up().await;
            }
            StreamItem::Checkpoint(state) => {
                if let Some(boot) = self.boot.as_mut() {
                    boot.checkpoint = Some(state);
                } else if !self.gap.pending() {
                    self.backend.record_update_state(state);
                }
            }
        }
    }

    fn invalidate_notice_mutes(&mut self) {
        self.notices.invalidate_peer_mutes(self.badges.now());
        if let Some(boot) = self.boot.as_mut() {
            // A lost update may have superseded peer settings buffered
            // before the gap. Subsequent peer settings can confirm them.
            boot.buffer.retain(|(update, _)| {
                !matches!(
                    update,
                    Update::NotifySettings {
                        target: NotifyTarget::Peer(_),
                        ..
                    }
                )
            });
        }
    }

    /// Purges disk history before the checkpoint can pass the gap. Bumping the
    /// affected view generations prevents older cache or RPC reads from
    /// restoring the window or writing their results back to disk.
    async fn invalidate_history(&mut self, chat: Option<ChatId>) {
        self.invalidate_notice_mutes();
        let purged = self.purge_history(chat).await;
        let chat = if purged {
            chat
        } else {
            self.gap = Gap::PurgeFailed;
            None
        };
        // Earlier replay buffered across a snapshot may contain a message
        // whose later edit or deletion was lost at this gap.
        if let Some(boot) = self.boot.as_mut() {
            boot.buffer.retain(|(update, _)| {
                let message = match update {
                    Update::NewMessage(new) => &new.message,
                    Update::Edited(message) => message,
                    _ => return true,
                };
                chat.is_some_and(|chat| message.thread.chat != chat)
            });
            boot.trailing |= self.gap.pending();
        }
        let threads: Vec<_> = self
            .windows
            .threads()
            .filter(|thread| chat.is_none_or(|chat| thread.chat == chat))
            .collect();
        for thread in threads {
            self.windows.invalidate(thread);
            self.forget_owed(thread);
            self.deferred.retain(|d| d.thread != thread);
            self.emit(Event::ThreadInvalidated(thread)).await;
        }
        if !purged {
            self.rebuild(Reason::GapSnapshot).await;
        }
    }

    async fn purge_history(&self, chat: Option<ChatId>) -> bool {
        if let Some(cache) = &self.cache
            && cache.purge_history(chat).await.is_err()
        {
            tracing::warn!("could not invalidate cached history");
            return false;
        }
        true
    }

    /// A known gap needs a snapshot taken after it. Without a boot in flight,
    /// that is a refresh now. With one, its snapshots (some already
    /// requested) may predate the gap, so it is marked to be followed by a
    /// refresh, and it holds its checkpoint back until then.
    async fn reconcile_gap(&mut self) {
        self.gap = Gap::Pending;
        if let Some(boot) = self.boot.as_mut() {
            // Before the baseline no snapshot has been requested yet.
            boot.trailing |= boot.baseline_by.is_none();
            boot.suppress_notices = true;
            return;
        }
        self.refresh().await;
    }

    async fn feed_update(&mut self, update: Update, source: Source) {
        // Topic lists stamp updates on receipt, even while windows reconcile.
        self.topics_on_update(&update).await;
        match self.boot.as_mut() {
            Some(boot) => {
                if boot.push(update, source).is_err() {
                    self.rebuild(Reason::BootOverflow).await;
                }
            }
            None => {
                self.on_update(
                    update,
                    Origin {
                        tops: None,
                        source,
                        notify: source == Source::Live,
                    },
                )
                .await;
            }
        }
    }

    async fn on_baseline(&mut self, kind: Baseline) {
        if self.gap == Gap::PurgeFailed {
            if !self.purge_history(None).await {
                self.rebuild(Reason::GapSnapshot).await;
                return;
            }
            self.gap = Gap::Pending;
        }
        let Some(boot) = self.boot.as_mut() else {
            return;
        };
        if boot.baseline_by.take().is_none() {
            return; // a second baseline on the same stream: nothing to do
        }
        if kind == Baseline::Resumed {
            self.notices.difference = Difference::Reconciling;
        }
        self.start_snapshots(BootKind::Full).await;
    }

    /// A light refresh. Single-flight; skipped unless the instance is live.
    pub(super) async fn refresh(&mut self) {
        self.start_refresh(None).await;
    }

    async fn start_refresh(&mut self, checkpoint: Option<UpdateCheckpoint>) {
        if self.life.phase != Phase::Live || self.boot.is_some() {
            return;
        }
        self.last_refresh = Instant::now();
        let mut boot = Boot::refresh();
        boot.checkpoint = checkpoint;
        if self.gap.pending() {
            boot.suppress_notices = true;
        }
        self.boot = Some(boot);
        self.start_snapshots(BootKind::Refresh).await;
    }

    /// When the next periodic refresh is due, if one is.
    pub(super) fn refresh_due(&self) -> Option<Instant> {
        (self.auth.signed_in && self.life.phase == Phase::Live && self.boot.is_none())
            .then(|| self.last_refresh + REFRESH_EVERY)
    }

    pub(super) async fn on_window(&mut self, state: WindowState) {
        self.window = state;
        if state == WindowState::Active
            && Instant::now().duration_since(self.last_refresh) >= MIN_REFRESH_GAP
        {
            self.refresh().await;
        }
    }

    async fn start_snapshots(&mut self, kind: BootKind) {
        if kind == BootKind::Full {
            // Every window is invalidated below, the open one included (the
            // byte budget never evicts it), which ends the UI's loads.
            self.purge_queued();
            // Deferred loads were for windows that are invalidated now.
            self.deferred.clear();
            self.owed = None;
            // The open thread reloads its tail: an anchor is dropped.
            self.anchored = None;
            self.restoring = None;
            for (thread, _) in self.windows.invalidate_all() {
                self.emit(Event::ThreadInvalidated(thread)).await;
            }
        }
        if kind == BootKind::Full || self.gap.pending() {
            self.start_scope_snapshots();
        }
        if kind == BootKind::Full
            && (self.notices.booted || self.gap.pending())
            && let Some(boot) = self.boot.as_mut()
        {
            boot.suppress_notices = true;
        }
        self.topics_snapshots(kind).await;
        if kind == BootKind::Refresh {
            self.refresh_badges_and_kinds().await;
        }
        if self.dialogs_busy
            && !self.gap.pending()
            && let Some(boot) = self.boot.as_mut()
        {
            boot.dialogs = DialogsSnapshot::Done;
        }
        self.start_snapshot_dialogs();
        if let Some(thread) = self.open.filter(|t| !self.fences.is_fenced(t.chat)) {
            if kind == BootKind::Full || !self.history_busy.contains_key(&thread) {
                self.start_snapshot_tail(thread, kind).await;
            } else if self.gap.pending()
                && let Some(boot) = self.boot.as_mut()
            {
                boot.waiting_tail = Some(thread);
            }
        }
    }

    fn start_snapshot_dialogs(&mut self) {
        if self.dialogs_busy
            || !self
                .boot
                .as_ref()
                .is_some_and(|b| b.baseline_by.is_none() && b.dialogs == DialogsSnapshot::Waiting)
        {
            return;
        }
        let req = self.request_id();
        if let Some(boot) = self.boot.as_mut() {
            boot.dialogs = DialogsSnapshot::Pending(req);
        }
        self.dialogs_busy = true;
        self.priority.push_back(Job::Dialogs {
            req,
            cursor: None,
            stamps: super::jobs::Stamps::default(),
        });
    }

    async fn start_snapshot_tail(&mut self, thread: ThreadKey, kind: BootKind) {
        let Some(view) = self.windows.get(thread).map(crate::store::Window::view_gen) else {
            return;
        };
        let load = self.begin_load(thread, view);
        self.claim_tail_snapshot(load);
        let via = match kind {
            BootKind::Full => TailVia::Cache,
            BootKind::Refresh => TailVia::Server,
        };
        self.start_tail(load, via).await;
    }

    fn start_scope_snapshots(&mut self) {
        let Some(boot) = self.boot.as_mut() else {
            return;
        };
        for scope in [NotifyScope::Users, NotifyScope::Groups] {
            self.priority.push_back(Job::NotifySettings(scope));
            boot.scopes_pending += 1;
        }
    }

    /// The light refresh's share of section 7 and 6.6: the shown forum's
    /// badge when it isn't on the first dialogs page, and a new check of
    /// every suspect chat.
    async fn refresh_badges_and_kinds(&mut self) {
        if let Some(shown) = self.topics.lists.shown() {
            let first_page = self
                .dialogs
                .as_slice()
                .iter()
                .take(usize::try_from(jobs::DIALOGS_PAGE).unwrap_or(usize::MAX))
                .any(|c| c.id == shown);
            if !first_page {
                self.badge_trigger(shown).await;
            }
        }
        self.recheck_suspects().await;
    }

    /// Makes `load` the running boot's tail snapshot of its thread. A
    /// snapshot it replaces (the thread was reopened) leaves the priority queue; if it
    /// is already running, its completion is an ordinary stale load.
    fn claim_tail_snapshot(&mut self, load: Load) {
        let Some(boot) = self.boot.as_mut() else {
            return;
        };
        if boot.waiting_tail == Some(load.thread) {
            boot.waiting_tail = None;
        }
        if boot.tails.insert(load.thread, load.req).is_some() {
            self.priority
                .retain(|j| !matches!(j, Job::History { load: l, .. } if l.thread == load.thread));
        }
    }

    /// Whether a tail opened now for `thread` becomes a boot snapshot (see
    /// [`Boot::accepts_tail`]); claims it if so.
    pub(super) fn claim_if_booting(&mut self, load: Load) {
        if self.boot.as_ref().is_some_and(|b| {
            (b.kind == BootKind::Full || self.gap.pending() && !b.trailing)
                && b.accepts_tail(load.thread)
        }) {
            self.claim_tail_snapshot(load);
        }
    }

    /// Starts the tail load `load`: the cached tail first when `via` says so
    /// and there is a cache (the server tail is chained after it), else the
    /// server tail, as a boot snapshot if `load` is one.
    pub(super) async fn start_tail(&mut self, load: Load, via: TailVia) {
        match self.cache.clone().filter(|_| via == TailVia::Cache) {
            Some(cache) => {
                let cgen = self.cgen;
                let fut = jobs::cache_history(cache, cgen, load, HistoryDir::Tail, None);
                self.spawn_local(TaskKind::History(load, HistoryDir::Tail), fut);
            }
            None if self.is_tail_snapshot(load) => self.push_tail_snapshot(load),
            None => self.request_tail(load).await,
        }
    }

    /// Queues the server tail of `load` as a snapshot of the running boot.
    pub(super) fn push_tail_snapshot(&mut self, load: Load) {
        let floor = self.history_tail_floor(load.thread);
        self.priority.push_back(Job::History {
            load,
            dir: HistoryDir::Tail,
            query: HistoryQuery::Tail,
            floor,
            since: self.windows.continuity_mark(),
        });
    }

    /// Whether `load` is the running boot's tail snapshot of its thread.
    pub(super) fn is_tail_snapshot(&self, load: Load) -> bool {
        self.boot.as_ref().is_some_and(|b| b.awaits_tail(load))
    }

    pub(super) async fn dialogs_finished(&mut self, req: ReqId, guard: Option<Guarded>) {
        self.dialogs_busy = false;
        let applied = matches!(guard, Some(Guarded::Apply | Guarded::ApplyAndRefetch));
        self.snapshot_done(Snapshot::Dialogs { req, applied });
        self.start_snapshot_dialogs();
        if matches!(guard, Some(Guarded::ApplyAndRefetch | Guarded::Discard))
            && !self.boot.as_ref().is_some_and(|b| b.failed)
        {
            self.refetch_first_dialogs().await;
        }
        if std::mem::take(&mut self.dialogs_queued) {
            self.load_dialogs().await;
        }
    }

    /// Records only the request the boot owns. A replaced tail cannot count
    /// for the load that superseded it, and a closed or fenced view has no
    /// tail to reconcile here.
    pub(super) fn snapshot_done(&mut self, snapshot: Snapshot) {
        let required_tail = match snapshot {
            Snapshot::Tail { load, .. } => self.needs_gap_tail(load.thread),
            _ => false,
        };
        let Some(boot) = self.boot.as_mut() else {
            return;
        };
        let required = self.gap.pending() && !boot.trailing;
        match snapshot {
            Snapshot::Dialogs { req, applied } if boot.awaits_dialogs(req) => {
                boot.dialogs = DialogsSnapshot::Done;
                boot.failed |= required && !applied;
            }
            Snapshot::NotifySettings { applied } => {
                boot.scopes_pending = boot.scopes_pending.saturating_sub(1);
                boot.failed |= required && !applied;
            }
            Snapshot::Tail { load, applied } if boot.awaits_tail(load) => {
                boot.tails.remove(&load.thread);
                boot.failed |= required && required_tail && !applied;
            }
            _ => {}
        }
    }

    fn needs_gap_tail(&self, thread: ThreadKey) -> bool {
        self.open == Some(thread)
            && !self.fences.is_fenced(thread.chat)
            && self.windows.get(thread).is_some_and(|w| !w.has_newer())
    }

    /// Run after each actor event, so a call error gets its normal lifecycle
    /// handling before a failed gap snapshot can request a rebuild.
    pub(super) async fn advance_boot(&mut self) {
        let Some(boot) = self.boot.as_ref() else {
            return;
        };
        if boot.failed && !boot.trailing {
            self.rebuild(Reason::GapSnapshot).await;
            return;
        }
        if let Some(thread) = boot.waiting_tail {
            let kind = boot.kind;
            if self.open == Some(thread) && self.history_busy.contains_key(&thread) {
                return;
            }
            if let Some(boot) = self.boot.as_mut() {
                boot.waiting_tail = None;
            }
            if self.needs_gap_tail(thread) {
                self.start_snapshot_tail(thread, kind).await;
            }
        }
        if !self.boot.as_ref().is_some_and(Boot::snapshots_done) {
            return;
        }
        let Some(boot) = self.boot.take() else {
            return;
        };
        self.finish_boot(boot).await;
    }

    async fn finish_boot(&mut self, mut boot: Boot) {
        // Live scope settings received during the read are in the buffer.
        // Apply them over the snapshots before the summary or any due notice.
        for (update, source) in boot.buffer {
            self.on_update(
                update,
                Origin {
                    tops: boot.tops.as_ref(),
                    source,
                    notify: source == Source::Live && !boot.suppress_notices,
                },
            )
            .await;
        }
        // A gap that crossed these snapshots holds the checkpoint past it, and
        // the summary, until the refresh below is done.
        if !boot.trailing {
            self.gap = Gap::Clear;
            self.notices.catch_up_reconciled = boot.tops.is_some();
            self.flush_catch_up().await;
            if let Some(state) = boot.checkpoint.take() {
                self.backend.record_update_state(state);
            }
        }
        self.last_refresh = Instant::now();
        if boot.kind == BootKind::Full {
            self.notices.booted = true;
            self.go_online().await;
        }
        if boot.trailing {
            self.start_refresh(boot.checkpoint).await;
        }
    }
}

#[cfg(all(test, feature = "fake"))]
mod tests {
    use super::*;
    use crate::backend::fake::text_message;
    use crate::model::PeerKind;

    #[test]
    fn tombstones_match_scoped_and_unscoped_deletions() {
        let user = ChatId {
            kind: PeerKind::User,
            id: 1,
        };
        let group = ChatId {
            kind: PeerKind::Channel,
            id: 2,
        };
        let mut boot = Boot::full(Instant::now());
        boot.push(
            Update::Deleted {
                chat: None,
                ids: vec![MsgId(5)],
            },
            Source::Live,
        )
        .unwrap();
        boot.push(
            Update::Deleted {
                chat: Some(group),
                ids: vec![MsgId(6)],
            },
            Source::Live,
        )
        .unwrap();
        assert!(boot.is_dead(user, MsgId(5)));
        assert!(!boot.is_dead(group, MsgId(5)), "channel ids are per chat");
        assert!(boot.is_dead(group, MsgId(6)));
        assert!(!boot.is_dead(user, MsgId(6)));
    }

    #[tokio::test]
    async fn an_invalidation_after_hinted_replay_clears_background_and_buffered_history() {
        use std::sync::Arc;

        use tokio::sync::mpsc;
        use tokio_util::sync::CancellationToken;

        use crate::actor::ActorInit;
        use crate::backend::fake::{FakeBackend, FakeScript, summary};
        use crate::events;
        use crate::secret::MemoryStore;
        use crate::session::PersistHandle;

        let chat = ChatId {
            kind: PeerKind::Channel,
            id: 5,
        };
        let thread = ThreadKey::whole(chat);
        let (backend, _fake) = FakeBackend::new(FakeScript::demo(0, 0));
        let (events, _receiver) = events::channel();
        let (_commands, command_rx) = mpsc::channel(1);
        let (_internal, internal_rx) = mpsc::channel(1);
        let persist = PersistHandle::spawn(MemoryStore::new()).unwrap();
        let (mut actor, _channels) = Actor::new(ActorInit {
            backend: Arc::new(backend),
            events,
            cache: None,
            commands: command_rx,
            internal: internal_rx,
            cancel: CancellationToken::new(),
            account: persist.fresh_session(),
            persist,
            cache_parts: None,
            pending_logout: None,
        });
        actor.dialogs.reset(vec![summary(chat, "supergroup")]);
        for boot_in_flight in [false, true] {
            actor.windows.open(thread);
            actor
                .windows
                .upsert(thread, text_message(chat, 3, "retained"));
            actor.windows.close(thread);
            actor.boot = boot_in_flight.then(Boot::refresh);
            actor
                .on_feed(StreamItem::CatchUp(Update::NewMessage(
                    text_message(chat, 4, "hinted replay").into(),
                )))
                .await;
            if let Some(boot) = &actor.boot {
                assert_eq!(boot.buffer.len(), 1);
            } else {
                assert!(actor.windows.get(thread).unwrap().get(MsgId(4)).is_some());
            }
            actor
                .on_feed(StreamItem::HistoryStale(StaleHistory(Some(chat))))
                .await;
            if let Some(boot) = actor.boot.take() {
                actor.finish_boot(boot).await;
            }
            assert!(actor.windows.get(thread).unwrap().messages().is_empty());
        }
    }

    #[test]
    fn buffer_overflow_is_reported() {
        let chat = ChatId {
            kind: PeerKind::User,
            id: 1,
        };
        let mut boot = Boot::full(Instant::now());
        for i in 0..MAX_BOOT_BUFFER {
            let id = i32::try_from(i).unwrap();
            boot.push(
                Update::NewMessage(text_message(chat, id, "x").into()),
                Source::Live,
            )
            .unwrap();
        }
        assert!(
            boot.push(
                Update::NewMessage(text_message(chat, 0, "x").into()),
                Source::Live
            )
            .is_err()
        );
    }
}
