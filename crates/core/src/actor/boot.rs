//! Bootstrap and light refresh (plan section 2): take snapshots only after the
//! update baseline, buffer live updates meanwhile, then merge the buffer over the
//! snapshots so a stale snapshot never overwrites a newer update.
//!
//! - **Full bootstrap** (after every build): wait for the stream's baseline (at
//!   most [`BASELINE_WAIT`], else rebuild), invalidate every message window, then
//!   fetch the first dialogs page and the open chat's tail.
//! - **Light refresh** (window activation, and every [`REFRESH_EVERY`] while
//!   active): the same snapshots on the live instance, without invalidation; the
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
use crate::api::{Event, HistoryDir, WindowState};
use crate::backend::{Backend, HistoryQuery, StreamItem, Update};
use crate::model::{ChatId, ChatSummary, MsgId, ThreadKey};
use crate::store::MAX_WINDOWS;

/// How long a new instance may take to establish its update baseline.
pub(super) const BASELINE_WAIT: Duration = Duration::from_secs(10);
/// Updates buffered while snapshots are pending; overflow rebuilds.
pub(super) const MAX_BOOT_BUFFER: usize = 4096;
/// Deleted ids remembered while snapshots are pending; overflow rebuilds.
pub(super) const MAX_TOMBSTONES: usize = 4096;
/// Light refresh period while the window is active.
pub(super) const REFRESH_EVERY: Duration = Duration::from_mins(5);
/// Activation does not refresh again within this long of the last refresh.
pub(super) const MIN_REFRESH_GAP: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BootKind {
    Full,
    Refresh,
}

/// Which snapshot finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Snapshot {
    Dialogs,
    /// A tail load; it counts only if it is the thread's current snapshot.
    Tail(Load),
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
    buffer: Vec<Update>,
    tombstones: HashSet<(Option<ChatId>, MsgId)>,
    dialogs_pending: bool,
    /// Pending tail snapshots, by `(thread, request)`: a snapshot completes
    /// (or fails) by its own identity, even after its view went stale, so it
    /// can never be orphaned. At most [`MAX_WINDOWS`] threads.
    tails: HashMap<ThreadKey, ReqId>,
    /// The top message id per chat in the dialogs snapshot (at most one page).
    tops: HashMap<ChatId, MsgId>,
}

impl Boot {
    pub(super) fn full(now: Instant) -> Self {
        Self {
            kind: BootKind::Full,
            baseline_by: Some(now + BASELINE_WAIT),
            buffer: Vec::new(),
            tombstones: HashSet::new(),
            dialogs_pending: false,
            tails: HashMap::new(),
            tops: HashMap::new(),
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
    fn push(&mut self, update: Update) -> Result<(), Overflow> {
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
        self.buffer.push(update);
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

    /// Whether a thread opened now gets its tail as a snapshot of this boot: a
    /// full bootstrap past its baseline, so the open is not stuck behind
    /// `Online` (UI jobs wait for it) and keeps the boot from finishing
    /// before its window is current.
    fn accepts_tail(&self, thread: ThreadKey) -> bool {
        self.kind == BootKind::Full
            && self.baseline_by.is_none()
            && (self.tails.contains_key(&thread) || self.tails.len() < MAX_WINDOWS)
    }

    /// Records what the dialogs snapshot says about each chat's last message.
    pub(super) fn note_dialogs(&mut self, chats: &[ChatSummary]) {
        self.tops
            .extend(chats.iter().filter_map(|c| Some((c.id, c.last_id?))));
    }

    fn snapshots_done(&self) -> bool {
        self.baseline_by.is_none() && !self.dialogs_pending && self.tails.is_empty()
    }
}

/// Where an update being applied comes from.
#[derive(Clone, Copy, Debug)]
pub(super) enum Origin<'a> {
    Live,
    /// Buffered during snapshots and merged over them; carries the dialogs
    /// snapshot's top message id per chat.
    Replay(&'a HashMap<ChatId, MsgId>),
}

impl<B: Backend> Actor<B> {
    pub(super) async fn on_feed(&mut self, item: StreamItem) {
        match item {
            StreamItem::Baseline => self.on_baseline().await,
            StreamItem::Update(update) => {
                // Topic lists stamp an update when it is received, also
                // while it waits here for the windows (`topics`).
                self.topics_on_update(&update).await;
                match self.boot.as_mut() {
                    Some(boot) => {
                        if boot.push(update).is_err() {
                            self.rebuild(Reason::BootOverflow).await;
                        }
                    }
                    None => self.on_update(update, Origin::Live).await,
                }
            }
        }
    }

    async fn on_baseline(&mut self) {
        let Some(boot) = self.boot.as_mut() else {
            return;
        };
        if boot.baseline_by.take().is_none() {
            return; // a second baseline on the same stream: nothing to do
        }
        self.start_snapshots(BootKind::Full).await;
    }

    /// A light refresh. Single-flight; skipped unless the instance is live.
    pub(super) async fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        if self.life.phase != Phase::Live || self.boot.is_some() {
            return;
        }
        self.boot = Some(Boot::refresh());
        self.start_snapshots(BootKind::Refresh).await;
    }

    /// When the next periodic refresh is due, if one is.
    pub(super) fn refresh_due(&self) -> Option<Instant> {
        (self.window == WindowState::Active
            && self.life.phase == Phase::Live
            && self.boot.is_none())
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
            for (thread, _) in self.windows.invalidate_all() {
                self.emit(Event::ThreadInvalidated(thread)).await;
            }
        }
        self.topics_snapshots(kind).await;
        let dialogs = !self.dialogs_busy;
        if dialogs {
            self.dialogs_busy = true;
            self.priority.push_back(Job::Dialogs { cursor: None });
        }
        if let Some(boot) = self.boot.as_mut() {
            boot.dialogs_pending = dialogs;
        }
        // A full bootstrap always refetches the open tail (any older load was
        // for a window that is now invalidated); a refresh never doubles a load.
        let tail = self
            .open
            .filter(|t| kind == BootKind::Full || !self.history_busy.contains_key(t))
            .and_then(|t| Some((t, self.windows.get(t)?.view_gen())));
        if let Some((thread, view)) = tail {
            let load = self.begin_load(thread, view);
            self.claim_tail_snapshot(load);
            // A full bootstrap shows the cached tail again right away (it may
            // be all there is while offline); the server tail is chained after.
            let via = match kind {
                BootKind::Full => TailVia::Cache,
                BootKind::Refresh => TailVia::Server,
            };
            self.start_tail(load, via).await;
        }
        self.maybe_finish_boot().await;
    }

    /// Makes `load` the running boot's tail snapshot of its thread. A
    /// snapshot it replaces (the thread was reopened) leaves the priority queue; if it
    /// is already running, its completion is an ordinary stale load.
    fn claim_tail_snapshot(&mut self, load: Load) {
        let Some(boot) = self.boot.as_mut() else {
            return;
        };
        if boot.tails.insert(load.thread, load.req).is_some() {
            self.priority
                .retain(|j| !matches!(j, Job::History { load: l, .. } if l.thread == load.thread));
        }
    }

    /// Whether a tail opened now for `thread` becomes a boot snapshot (see
    /// [`Boot::accepts_tail`]); claims it if so.
    pub(super) fn claim_if_booting(&mut self, load: Load) {
        if self
            .boot
            .as_ref()
            .is_some_and(|b| b.accepts_tail(load.thread))
        {
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
        let floor = self.tail_floor(load.thread);
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

    /// Records a finished snapshot (successful or not). A tail load that is
    /// no longer its thread's snapshot (replaced by a reopen) changes nothing.
    pub(super) async fn snapshot_done(&mut self, snapshot: Snapshot) {
        let Some(boot) = self.boot.as_mut() else {
            return;
        };
        match snapshot {
            Snapshot::Dialogs => boot.dialogs_pending = false,
            Snapshot::Tail(load) => {
                if boot.awaits_tail(load) {
                    boot.tails.remove(&load.thread);
                }
            }
        }
        self.maybe_finish_boot().await;
    }

    async fn maybe_finish_boot(&mut self) {
        if !self.boot.as_ref().is_some_and(Boot::snapshots_done) {
            return;
        }
        let Some(boot) = self.boot.take() else {
            return;
        };
        for update in boot.buffer {
            self.on_update(update, Origin::Replay(&boot.tops)).await;
        }
        self.last_refresh = Instant::now();
        if boot.kind == BootKind::Full {
            self.go_online().await;
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
        boot.push(Update::Deleted {
            chat: None,
            ids: vec![MsgId(5)],
        })
        .unwrap();
        boot.push(Update::Deleted {
            chat: Some(group),
            ids: vec![MsgId(6)],
        })
        .unwrap();
        assert!(boot.is_dead(user, MsgId(5)));
        assert!(!boot.is_dead(group, MsgId(5)), "channel ids are per chat");
        assert!(boot.is_dead(group, MsgId(6)));
        assert!(!boot.is_dead(user, MsgId(6)));
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
            boot.push(Update::NewMessage(text_message(chat, id, "x")))
                .unwrap();
        }
        assert!(
            boot.push(Update::NewMessage(text_message(chat, 0, "x")))
                .is_err()
        );
    }
}
