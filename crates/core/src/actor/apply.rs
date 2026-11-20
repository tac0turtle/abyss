//! Applying completions and live updates to the store, cache and UI.

use super::Actor;
use super::FoldersLoad;
use super::anchored::Anchored;
use super::badges::Stated;
use super::boot::{BootKind, Origin, Snapshot, TailVia};
use super::jobs::{
    CallError, CallResult, DialogsStep, Done, Job, Load, Outcome, PAGE, ReqId, Stamps, StreamEnd,
};
use super::lifecycle::Reason;
use super::loads::Owed;
use crate::api::{Event, HistoryDir, LoadTarget, UserError, WindowState};
use crate::backend::{
    Backend, BackendError, DialogsCursor, DialogsPage, HistoryPage, HistoryQuery, RejectReason,
    StreamError, Update,
};
use crate::cache::CacheCmd;
use crate::model::{ChatFolder, ChatId, ChatSummary, Message, MsgId, PeerFolder, ThreadKey};
use crate::store::{
    BadgeSeq, Bump, Change, ContinuityMark, Guarded, Last, Rest, Route, Unread, ViewGen, Window,
};

pub(super) fn user_error(e: CallError) -> UserError {
    match e {
        CallError::Backend(BackendError::Rejected(RejectReason::FolderChanged)) => {
            UserError::FolderChanged
        }
        CallError::Backend(BackendError::Rejected(RejectReason::FolderLimit)) => {
            UserError::FolderLimit
        }
        CallError::Backend(BackendError::Rejected(RejectReason::TopicClosed)) => {
            UserError::TopicClosed
        }
        CallError::Backend(BackendError::Rejected(RejectReason::TopicDeleted)) => {
            UserError::TopicDeleted
        }
        CallError::Backend(BackendError::Rejected(_) | BackendError::Unauthorized) => {
            UserError::Rejected
        }
        CallError::Backend(BackendError::FloodWait { seconds }) => UserError::FloodWait { seconds },
        CallError::Backend(BackendError::Migrated) => UserError::AccountMigrated,
        // `SentUnconfirmed` is a send outcome, handled before errors are mapped.
        CallError::Backend(BackendError::Transient | BackendError::SentUnconfirmed)
        | CallError::Timeout
        | CallError::Dropped => UserError::Network,
    }
}

/// The call error an outcome carries, for stall and migration detection.
fn call_error(outcome: &Outcome) -> Option<CallError> {
    match outcome {
        // Account loss is handled before applying a settings reply.
        Outcome::NotifySettings {
            res: Err(CallError::Timeout),
            ..
        } => Some(CallError::Timeout),
        Outcome::Dialogs { res: Err(e), .. }
        | Outcome::PeerDialogs { res: Err(e), .. }
        | Outcome::History { res: Err(e), .. }
        | Outcome::Send { res: Err(e), .. }
        | Outcome::TopicsPage { res: Err(e), .. }
        | Outcome::TopicsById { res: Err(e), .. }
        | Outcome::Login(Err(e))
        | Outcome::ChatFolders(Err(e))
        | Outcome::FolderWrite { res: Err(e), .. } => Some(*e),
        // A retried read's time-out is not counted again: one read intent
        // can't keep stall detection rebuilding (`reads`).
        Outcome::Read {
            res: Err(e), retry, ..
        } => (*retry == 0 || *e != CallError::Timeout).then_some(*e),
        _ => None,
    }
}

pub(super) const MIN_ID: MsgId = MsgId(i32::MIN);
pub(super) const MAX_ID: MsgId = MsgId(i32::MAX);

/// How a tail page is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TailMode {
    /// Replace the window (open, bootstrap, gaps).
    Replace,
    /// Merge into the window in place (light refresh).
    Merge,
    /// Merge into the window in place, keeping the view and its viewport,
    /// and write the tail through to restore cache continuity (Stage 2 plan
    /// section 17.6.6): the tail a `Newer` page asked for when it attached
    /// a window whose cache isn't contiguous.
    Restore,
}

/// A history response, unpacked.
pub(super) struct HistoryReply {
    pub(super) thread: ThreadKey,
    pub(super) view: ViewGen,
    /// The load that asked for it (its order says what it can cover).
    pub(super) req: ReqId,
    pub(super) dir: HistoryDir,
    pub(super) query: HistoryQuery,
    /// See [`Job::History`](super::jobs::Job::History).
    pub(super) floor: Option<MsgId>,
    /// See [`Job::History`](super::jobs::Job::History).
    pub(super) since: ContinuityMark,
    /// What the server page covered, captured before tombstones filter it.
    pub(super) covered: Option<Coverage>,
    pub(super) page: HistoryPage,
    pub(super) mode: TailMode,
}

/// What a completed history load was, taken from the actor's bookkeeping
/// as it completes.
#[derive(Clone, Copy, Debug)]
pub(super) struct LoadEnd {
    /// It was a covering fetch (`covered.rs`).
    cover_fetch: bool,
    /// It owed the UI its answer (`loads.rs`).
    owed: Option<Owed>,
    /// It was an `OpenThreadAt` load (`anchored.rs`).
    anchored: Option<Anchored>,
}

/// The ids a server page covered: its oldest and newest message as the
/// server sent them. Captured before buffered deletions filter the page, so a
/// page emptied by tombstones still bounds what it is authoritative for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Coverage {
    pub(super) oldest: MsgId,
    pub(super) newest: MsgId,
}

impl Coverage {
    pub(super) fn of(page: &HistoryPage) -> Option<Self> {
        let ids = page.messages.iter().map(|m| m.id);
        Some(Self {
            oldest: ids.clone().min()?,
            newest: ids.max()?,
        })
    }
}

impl HistoryReply {
    /// Up to which id a tail is authoritative: the newest id the server sent,
    /// and the newest seen in the window or awaiting a notice when requested.
    /// Above it, live messages and notice contributors arrived later and are
    /// kept. At or below it, within the tail's coverage, the server decides.
    pub(super) fn authoritative_top(&self) -> Option<MsgId> {
        self.covered.map(|c| c.newest).max(self.floor)
    }

    /// The inclusive id range a tail is authoritative for: from the oldest id
    /// the server sent (or the start of history, when the page is the whole
    /// history) up to [`Self::authoritative_top`]. A partial page that covered
    /// nothing establishes nothing: `None`, so nothing is deleted.
    pub(super) fn authoritative_range(&self) -> Option<(MsgId, MsgId)> {
        let top = self.authoritative_top()?;
        let lo = if self.page.has_more {
            self.covered?.oldest
        } else {
            MIN_ID
        };
        Some((lo, top))
    }
}

/// What a history call asked for.
#[derive(Clone, Copy, Debug)]
struct HistoryReq {
    dir: HistoryDir,
    query: HistoryQuery,
    floor: Option<MsgId>,
    since: ContinuityMark,
}

impl<B: Backend> Actor<B> {
    pub(super) async fn on_done(&mut self, done: Done) {
        let current = done.cgen == self.cgen;
        // Stale completions only arrive from a retirement, right before its
        // parked permits are recycled.
        if current && let Some(permit) = done.parked {
            self.admission.park(permit);
        }
        let err = call_error(&done.outcome).filter(|_| current);
        match done.outcome {
            Outcome::Send { local_id, res } => self.on_send(local_id, res).await,
            Outcome::SendExpired { local_id } => self.send_window_closed(local_id).await,
            // Before the generation check: the intent outlives the instance.
            Outcome::Read {
                thread,
                max_id,
                res,
                ..
            } => self.on_read(done.cgen, thread, max_id, res).await,
            // Before the generation check: a write from a retired instance
            // still frees the account's one write slot (`folder_writes`).
            Outcome::FolderWrite { write, moved, res } => {
                self.on_folder_write(write, done.cgen, moved, res).await;
            }
            _ if !current => {}
            Outcome::NotifySettings {
                target,
                stamps,
                res,
            } => self.on_notify_snapshot(target, stamps.badge, res).await,
            Outcome::Dialogs {
                req,
                step,
                stamps,
                res,
            } => self.on_dialogs(req, step, stamps, res).await,
            Outcome::History {
                load,
                dir,
                query,
                floor,
                since,
                res,
            } => {
                let req = HistoryReq {
                    dir,
                    query,
                    floor,
                    since,
                };
                self.on_history_done(load, req, res).await;
            }
            Outcome::TopicsPage { call, kind, res } => self.on_topics_page(call, kind, res).await,
            Outcome::TopicsById { call, res } => self.on_topics_by_id(call, res).await,
            Outcome::PeerDialogs { chats, stamps, res } => {
                self.on_peer_dialogs(chats, stamps, res).await;
            }
            Outcome::Login(_) | Outcome::UpdatesEnded(StreamEnd::Cancelled) => {}
            Outcome::ChatFolders(res) => self.on_chat_folders(res).await,
            Outcome::CacheChats(chats) => self.on_cache_chats(chats).await,
            Outcome::CacheHistory {
                load,
                dir,
                anchor,
                messages,
            } => self.on_cache_history(load, dir, anchor, messages).await,
            Outcome::UpdatesEnded(StreamEnd::Failed(e)) => self.on_stream_failed(e).await,
        }
        if let Some(e) = err {
            self.inspect_call(e).await;
        }
        // Independent of which time-outs count toward the stall window: once
        // every permit is parked, nothing else can time out to trigger
        // recovery (a retried read's time-outs are not counted, but still
        // park their permits). `rebuild` is a no-op once retiring.
        if current && self.admission.parked() >= super::admission::MAX_CONCURRENT {
            self.rebuild(Reason::Stall).await;
        }
    }

    /// The update stream ended for good: auth loss runs the local logout,
    /// migration the migration policy, anything else a rebuild.
    async fn on_stream_failed(&mut self, e: StreamError) {
        match e {
            StreamError::Unauthorized => self.session_revoked().await,
            StreamError::Migrated => {
                self.error(UserError::AccountMigrated).await;
                self.rebuild(Reason::Migrated).await;
            }
            StreamError::Transient | StreamError::Fatal => {
                self.rebuild(Reason::StreamFailed).await;
            }
        }
    }

    /// A finished history call. A tail that is its thread's boot snapshot
    /// completes the snapshot whether or not its view is still current.
    async fn on_history_done(&mut self, load: Load, req: HistoryReq, res: CallResult<HistoryPage>) {
        let boot = self
            .boot
            .as_ref()
            .filter(|b| req.dir == HistoryDir::Tail && b.awaits_tail(load))
            .map(|b| b.kind);
        self.release_history(load);
        // Whatever the outcome or the view, the load's metadata goes now: a
        // stale reply must not leave a covering fetch, an owed answer or an
        // anchored open behind (task 10f).
        let ended = LoadEnd {
            cover_fetch: self.covers.fetch_ended(load.thread, load.req),
            owed: self.take_owed(load),
            anchored: self.anchored.take_if(|a| a.load == load),
        };
        let restore = self.restoring.take_if(|l| *l == load).is_some();
        let applied = res.is_ok() && self.is_current(load.thread, load.view);
        match res {
            Ok(HistoryPage { messages, has_more }) => {
                // Normalized before anything reads it: coverage, the window
                // and the cache only ever see the load's own thread.
                let page = HistoryPage {
                    messages: self.keep_thread(load.thread, messages),
                    has_more,
                };
                let mode = if boot == Some(BootKind::Refresh) {
                    TailMode::Merge
                } else if restore {
                    TailMode::Restore
                } else {
                    TailMode::Replace
                };
                let reply = HistoryReply {
                    thread: load.thread,
                    view: load.view,
                    req: load.req,
                    dir: req.dir,
                    query: req.query,
                    floor: req.floor,
                    since: req.since,
                    covered: Coverage::of(&page),
                    page,
                    mode,
                };
                self.on_history(reply, ended).await;
            }
            Err(e) => {
                self.error(user_error(e)).await;
                // Only the load the UI waits on ends: never a tail (a load
                // deferred behind it still runs), a background fetch, or a
                // load of a view the UI no longer shows.
                if ended.owed.is_some() {
                    self.history_ended(load.thread, req.dir, Some(user_error(e)))
                        .await;
                }
                // The anchored open's fallback is the UI's (`OpenThread`).
                if let Some(anchored) = ended.anchored {
                    self.anchored_failed(anchored, user_error(e)).await;
                }
            }
        }
        if boot.is_some() {
            self.snapshot_done(Snapshot::Tail { load, applied });
        }
    }

    /// Ends the single-flight history load of its thread if it is `load`.
    pub(super) fn release_history(&mut self, load: Load) {
        if self
            .history_busy
            .get(&load.thread)
            .is_some_and(|l| l.req == load.req)
        {
            self.history_busy.remove(&load.thread);
        }
    }

    pub(super) async fn cache_write(&self, cmd: CacheCmd) {
        if let Some(cache) = &self.cache {
            cache.write(cmd).await;
        }
    }

    pub(super) async fn cache_dialogs(&self) {
        if self.cache.is_some() {
            self.cache_write(CacheCmd::PutChats(self.dialogs.as_slice().to_vec()))
                .await;
        }
    }

    async fn on_dialogs(
        &mut self,
        req: ReqId,
        step: DialogsStep,
        stamps: Stamps,
        res: CallResult<DialogsPage>,
    ) {
        let guard = match res {
            Ok(DialogsPage { mut chats, next }) => {
                // What the page states about kinds, before the guard.
                let stated = self.stated(&chats);
                // Each field keeps a value set after this page was
                // dispatched (a live folder move, a read, a newer
                // `peer_dialogs` result). A folder or mute disagreement
                // asks for the first page again; a page that may predate an
                // evicted unlisted field is dropped and refetched.
                let guarded = self.guard_snapshot(&mut chats, stamps.badge);
                if guarded == Guarded::Discard {
                    tracing::debug!("a dialogs page older than an evicted field update dropped");
                    // Nothing changes; the UI's request is answered, and
                    // the refetch (below) follows.
                    let has_more = !self.dialogs_loaded || self.dialogs_next.is_some();
                    self.emit(Event::DialogsPaging { has_more }).await;
                } else {
                    if let Some(boot) = self.boot.as_mut().filter(|b| b.awaits_dialogs(req)) {
                        boot.note_dialogs(&chats);
                    }
                    self.apply_dialogs_page(step, chats, next).await;
                    // An authoritative page: it settles queued fences it
                    // postdates and applies a known chat's flag change.
                    for s in stated {
                        self.stated_kind(s, stamps.badge).await;
                    }
                }
                Some(guarded)
            }
            Err(e) => {
                self.error(user_error(e)).await;
                self.load_ended(LoadTarget::Dialogs, Some(user_error(e)))
                    .await;
                None
            }
        };
        self.dialogs_finished(req, guard).await;
    }

    /// What a snapshot states about each chat's kind, and what core knew
    /// before applying it.
    pub(super) fn stated(&self, chats: &[ChatSummary]) -> Vec<Stated> {
        chats
            .iter()
            .map(|c| Stated {
                chat: c.id,
                before: self.known_status(c.id),
                kind: c.kind,
            })
            .collect()
    }

    /// Applies a guarded dialogs page: the merge, the cached list and the
    /// events.
    async fn apply_dialogs_page(
        &mut self,
        step: DialogsStep,
        chats: Vec<ChatSummary>,
        next: Option<DialogsCursor>,
    ) {
        let rest = if next.is_some() {
            Rest::More
        } else {
            Rest::Last
        };
        let event = match step {
            DialogsStep::First => {
                // Chats kept from later pages stay provisionally, but
                // pagination restarts from the fresh cursor: a chat that
                // appeared below the first page meanwhile is only found by
                // walking forward again (later pages confirm the kept rows,
                // and admit new ones in server order).
                self.dialogs.merge_first_page(chats, rest);
                self.dialogs_next = next;
                Event::DialogsReset(self.dialogs.as_slice().to_vec())
            }
            DialogsStep::Next => {
                self.dialogs_next = next;
                let page = self.dialogs.append(chats, rest);
                if page.reordered {
                    Event::DialogsReset(self.dialogs.as_slice().to_vec())
                } else {
                    Event::DialogsAppended(page.changed)
                }
            }
        };
        // `PutChats` replaces the cached list, so it always gets the whole list.
        if self.cache.is_some() {
            let all = self.dialogs.as_slice().to_vec();
            self.cache_write(CacheCmd::PutChats(all)).await;
        }
        self.dialogs_loaded = true;
        self.note_kinds();
        self.emit(event).await;
        let has_more = self.dialogs_next.is_some();
        self.emit(Event::DialogsPaging { has_more }).await;
        if step == DialogsStep::First {
            self.load_folders();
        }
    }

    /// Fetches the first dialogs page again (a response crossed a folder
    /// move), unless a dialogs load is already running.
    pub(super) async fn refetch_first_dialogs(&mut self) {
        if self.dialogs_busy {
            return;
        }
        let job = Job::Dialogs {
            req: self.request_id(),
            cursor: None,
            stamps: Stamps::default(),
        };
        if self.enqueue(job).await {
            self.dialogs_busy = true;
        }
    }

    /// The chat folders arrived (or failed to: they stay as they were, and
    /// the next first page asks again; nothing is shown to the user).
    async fn on_chat_folders(&mut self, res: CallResult<Vec<ChatFolder>>) {
        let again = self.folders == FoldersLoad::Again;
        self.folders = FoldersLoad::Idle;
        match res {
            Ok(folders) => self.emit(Event::ChatFolders(folders)).await,
            Err(e) => tracing::debug!(?e, "chat folders not loaded"),
        }
        if again {
            self.load_folders();
        }
    }

    async fn on_cache_chats(&mut self, chats: Option<Vec<ChatSummary>>) {
        let Some(chats) = chats.filter(|c| !c.is_empty()) else {
            return;
        };
        let remembered: Vec<_> = chats
            .iter()
            .filter(|c| c.topic_unread.is_some())
            .cloned()
            .collect();
        if self.dialogs_loaded {
            let changed = self.restore_topic_badges(&remembered);
            if !changed.is_empty() {
                self.cache_dialogs().await;
            }
            for summary in changed {
                self.emit(Event::ChatBumped(summary)).await;
            }
            return; // the server's rows and order stay authoritative
        }
        // The oldest source: nothing changed live since start is reverted.
        let mut chats = chats;
        if self.guard_snapshot(&mut chats, BadgeSeq::default()) == Guarded::Discard {
            return;
        }
        self.dialogs.reset(chats);
        self.restore_topic_badges(&remembered);
        self.note_kinds();
        self.emit(Event::DialogsReset(self.dialogs.as_slice().to_vec()))
            .await;
    }

    /// Emits the messages `change` inserted (ascending) and what it removed.
    /// `has_more` mirrors the window's own flag for `dir`, so the UI and the
    /// store can never disagree about whether more history exists.
    pub(super) async fn emit_history(
        &self,
        thread: ThreadKey,
        view: ViewGen,
        dir: HistoryDir,
        change: Change,
    ) {
        let Some(w) = self.windows.get(thread) else {
            return;
        };
        let has_more = match dir {
            HistoryDir::Tail | HistoryDir::Older => w.has_older(),
            HistoryDir::Newer | HistoryDir::Around => w.has_newer(),
        };
        let mut ids = change.inserted;
        ids.sort_unstable();
        let messages = w
            .messages()
            .iter()
            .filter(|m| ids.binary_search(&m.id).is_ok())
            .cloned()
            .collect();
        self.emit(Event::History {
            thread,
            view,
            dir,
            messages,
            removed: change.removed,
            has_more,
        })
        .await;
    }

    fn is_current(&self, thread: ThreadKey, view: ViewGen) -> bool {
        self.windows
            .get(thread)
            .is_some_and(|w| w.view_gen() == view)
    }

    async fn on_cache_history(
        &mut self,
        load: Load,
        dir: HistoryDir,
        anchor: Option<MsgId>,
        messages: Option<Vec<Message>>,
    ) {
        let messages = self.keep_thread(load.thread, messages.unwrap_or_default());
        if dir == HistoryDir::Tail {
            return self.on_cache_tail(load, messages).await;
        }
        self.release_history(load);
        let owed = self.take_owed(load);
        let Load { thread, view, .. } = load;
        let Some(anchor) = anchor else { return };
        if !self.is_current(thread, view) {
            return;
        }
        // The window's oldest end moved while the cache was read (see
        // `on_older`); a newer read needs no such check, since nothing but a
        // deletion changes a detached window's newest end.
        if dir == HistoryDir::Older && !self.starts_at(thread, anchor) {
            if let Some(owed) = owed {
                self.restart_older(owed).await;
            }
            return;
        }
        // Only the server establishes either end of history: the cache never
        // proves the start, and a short newer page only means the cache ends
        // there (live messages may be missing from it), not that the live
        // tail is reached. So both stay open, and a short newer page
        // continues from the server.
        let short = match dir {
            HistoryDir::Older => messages.is_empty(),
            _ => u32::try_from(messages.len()).map_or(true, |n| n < PAGE),
        };
        let page = HistoryPage {
            messages,
            has_more: true,
        };
        let res = match dir {
            HistoryDir::Older => self.windows.prepend(thread, view, page),
            _ => self.windows.append_newer(thread, view, page),
        };
        let mut owed = owed;
        if let Ok(change) = res
            && !(short && change.inserted.is_empty())
        {
            self.emit_history(thread, view, dir, change).await;
            owed = None; // answered
        }
        if !short {
            return;
        }
        // The cache ran out; continue from the server at the new edge. The
        // continuation answers the UI only if the cache page did not.
        let edge = self.windows.get(thread).and_then(|w| match dir {
            HistoryDir::Older => w.messages().front(),
            _ => w.messages().back(),
        });
        let edge = edge.map_or(anchor, |m| m.id);
        let load = self.begin_load(thread, view);
        if let Some(owed) = owed {
            self.owe(load, dir, owed.restarts);
        }
        self.fetch_more(load, dir, edge).await;
    }

    /// Shows cached history, then asks the server for the tail. The server request
    /// is chained after the cache read so `on_tail` always sees what the cache held.
    async fn on_cache_tail(&mut self, load: Load, messages: Vec<Message>) {
        let snapshot = self.is_tail_snapshot(load);
        let Load { thread, view, .. } = load;
        if !self.is_current(thread, view) {
            self.release_history(load);
            if snapshot {
                self.snapshot_done(Snapshot::Tail {
                    load,
                    applied: false,
                });
            }
            return;
        }
        let page = HistoryPage {
            messages,
            has_more: true,
        };
        if !page.messages.is_empty()
            && let Ok(change) = self.windows.prepend(thread, view, page)
            && !change.inserted.is_empty()
        {
            self.emit_history(thread, view, HistoryDir::Tail, change)
                .await;
        }
        self.start_tail(load, TailVia::Server).await;
    }

    /// A server page; `ended` says what its load was (its bookkeeping is
    /// already cleared, so a stale page leaves nothing behind).
    async fn on_history(&mut self, mut p: HistoryReply, ended: LoadEnd) {
        if self
            .windows
            .get(p.thread)
            .is_none_or(|w| w.view_gen() != p.view)
        {
            return; // stale: closed, evicted or replaced
        }
        if let Some(boot) = &self.boot {
            // A page fetched while a deletion was buffered must not resurrect it.
            p.page
                .messages
                .retain(|m| !boot.is_dead(p.thread.chat, m.id));
        }
        if let Some(anchored) = ended.anchored {
            return self.on_anchored(p, anchored).await;
        }
        match p.dir {
            HistoryDir::Tail if p.mode == TailMode::Replace => self.on_tail(p).await,
            HistoryDir::Tail => self.merge_tail(p).await,
            HistoryDir::Older => self.on_older(p, ended).await,
            // Only an anchored open asks for one.
            HistoryDir::Around => tracing::debug!("an Around page without its open dropped"),
            HistoryDir::Newer => {
                let lo = match p.query {
                    HistoryQuery::After(id) => MsgId(id.0.saturating_add(1)),
                    _ => MIN_ID,
                };
                let hi = match (p.page.has_more, p.covered) {
                    (false, _) => Some(MAX_ID),
                    (true, covered) => covered.map(|c| c.newest),
                };
                if let Some(hi) = hi {
                    self.reconcile(p.thread, lo, hi, &p.page.messages).await;
                }
                if let Ok(change) = self.windows.append_newer(p.thread, p.view, p.page) {
                    self.emit_history(p.thread, p.view, p.dir, change).await;
                }
                self.restore_if_attached(p.thread, p.view).await;
            }
        }
    }

    /// A page of history older than the window, asked as `Before(edge)`
    /// when `edge` was the window's oldest message. It is applied only if
    /// `edge` still is: live messages may have trimmed the window's oldest
    /// end meanwhile (and deletions made room again), and the page would
    /// then leave a hole above it that coverage could span (task 10e). A
    /// page that no longer continues the window is dropped; the user's load
    /// it answered (its own, or one a covering fetch absorbed) starts over
    /// from the window's current edge, a bounded number of times
    /// (`loads::restart_older`).
    async fn on_older(&mut self, p: HistoryReply, ended: LoadEnd) {
        let hi = match p.query {
            HistoryQuery::Before(id) => MsgId(id.0.saturating_sub(1)),
            _ => MAX_ID,
        };
        // A partial page is authoritative from the oldest id the server
        // sent, even when tombstones emptied it.
        let lo = match (p.page.has_more, p.covered) {
            (false, _) => Some(MIN_ID),
            (true, covered) => covered.map(|c| c.oldest),
        };
        if let Some(lo) = lo {
            self.reconcile(p.thread, lo, hi, &p.page.messages).await;
        }
        let edge = match p.query {
            HistoryQuery::Before(edge) => Some(edge),
            _ => None,
        };
        if edge.is_some_and(|edge| !self.starts_at(p.thread, edge)) {
            tracing::debug!("older page no longer continues the window");
            if let Some(owed) = ended.owed {
                self.restart_older(owed).await;
            }
            return;
        }
        // A page core fetched on its own never drops a message.
        let applied = if ended.cover_fetch {
            self.windows.prepend_fitting(p.thread, p.view, p.page)
        } else {
            self.windows.prepend(p.thread, p.view, p.page)
        };
        if let Ok(change) = applied {
            self.emit_history(p.thread, p.view, p.dir, change).await;
        }
        if let Some(edge) = edge {
            self.older_covers(p.thread, p.view, edge, lo).await;
        }
    }

    /// Whether `thread`'s window still starts at `edge`, the oldest message
    /// it held when an older page was asked for.
    fn starts_at(&self, thread: ThreadKey, edge: MsgId) -> bool {
        self.windows
            .get(thread)
            .and_then(|w| w.messages().front())
            .is_some_and(|m| m.id == edge)
    }

    /// Writes a server page over `[lo, hi]` of a contiguous cache.
    pub(super) async fn reconcile(
        &self,
        thread: ThreadKey,
        lo: MsgId,
        hi: MsgId,
        page: &[Message],
    ) {
        if self.windows.is_cache_contiguous(thread) && lo <= hi {
            self.cache_write(CacheCmd::Reconcile {
                thread,
                lo,
                hi,
                messages: page.to_vec(),
            })
            .await;
        }
    }

    /// The server tail replaces the window (which may hold cached history).
    ///
    /// If the page overlaps the cached history, the cache stays and only the
    /// covered range is reconciled. Otherwise there may be a gap, so the thread's
    /// cache is replaced by the page. The cache worker decides, against what the
    /// cache holds: a rebuild may have invalidated the window. Either way the
    /// cache is then contiguous with the live tail for the window's lifetime.
    ///
    /// Window messages up to [`HistoryReply::authoritative_top`] are replaced
    /// by the page, so cached or stale messages the server no longer has go,
    /// including the newest ones and everything when the server's history is
    /// empty. Only messages newer than that (live updates that arrived after
    /// the request) are kept on top.
    pub(super) async fn on_tail(&mut self, p: HistoryReply) {
        let top = p.authoritative_top();
        let range = p.authoritative_range();
        let HistoryReply {
            thread,
            req,
            since,
            mut page,
            ..
        } = p;
        let Some(w) = self.windows.get(thread) else {
            return;
        };
        let deleted = super::merge::tail_deletions(w.messages(), &page, range);
        let late = w
            .messages()
            .iter()
            .filter(|m| top.is_none_or(|t| m.id > t))
            .cloned();
        page.messages.extend(late);
        self.reconcile_notice_tail(thread, range, &page.messages);
        let written = self.write_tail(thread, since, &page, range).await;
        self.apply_tail_deletions(thread.chat, &deleted).await;
        let Some(view) = self.windows.invalidate(thread) else {
            return;
        };
        // Loads deferred for the replaced view are over with it: the UI
        // forgets them on the new view's `Tail` and asks again if needed.
        self.deferred.retain(|d| d.thread != thread);
        self.forget_owed(thread);
        if written {
            self.windows.set_cache_contiguous(thread);
        }
        if let Ok(change) = self.windows.prepend(thread, view, page) {
            self.emit_history(thread, view, HistoryDir::Tail, change)
                .await;
        }
        self.emit_reconciled(thread, view).await;
        self.tail_covers(thread, view, req, range.map(|(lo, _)| lo))
            .await;
    }

    /// Writes a server tail through to `thread`'s cache (`ReconcileTail`):
    /// `page` is the server's page plus the window's messages above its
    /// authoritative top (live messages that arrived after the request).
    /// The cache is reconciled from the page's covered start (not from the
    /// first message tombstones left), or from the start of history when
    /// the page is all of it. A partial page that covered nothing proves
    /// nothing: the cache is left alone and is not contiguous.
    ///
    /// A tail requested before the window last lost continuity may predate
    /// messages that bypassed it: it neither writes the cache nor restores
    /// continuity (only a later tail does). Returns whether it was written:
    /// only then is the cache contiguous with the live tail.
    pub(super) async fn write_tail(
        &self,
        thread: ThreadKey,
        since: ContinuityMark,
        page: &HistoryPage,
        range: Option<(MsgId, MsgId)>,
    ) -> bool {
        let from = match (page.has_more, range) {
            (false, _) => Some(None),
            (true, Some((lo, _))) => Some(Some(lo)),
            (true, None) => None,
        }
        .filter(|_| self.windows.may_restore(thread, since));
        let Some(from) = from.filter(|_| self.cache.is_some()) else {
            return false;
        };
        self.cache_write(CacheCmd::ReconcileTail {
            thread,
            messages: page.messages.clone(),
            from,
        })
        .await;
        true
    }

    /// Tells the UI that `view` of `thread` now matches the server's tail.
    pub(super) async fn emit_reconciled(&self, thread: ThreadKey, view: ViewGen) {
        let newest = self
            .windows
            .get(thread)
            .and_then(|w| w.messages().back())
            .map(|m| m.id);
        self.emit(Event::Reconciled {
            thread,
            view,
            newest,
        })
        .await;
    }

    /// Whether `thread`'s window already holds a newer copy of `m` (a later
    /// edit), which an older copy must not overwrite.
    fn has_newer_copy(&self, thread: ThreadKey, m: &Message) -> bool {
        self.windows
            .get(thread)
            .and_then(|w| w.get(m.id))
            .is_some_and(|cur| cur.version() > m.version())
    }

    pub(super) async fn on_update(&mut self, update: Update, origin: Origin<'_>) {
        match update {
            Update::NewMessage(new) => {
                let flags = super::notices::Flags::from(&new);
                self.on_new_message(new.message, flags, new.chat.as_ref(), origin)
                    .await;
            }
            Update::NotifySettings { target, until } => {
                self.on_notify_settings(target, until).await;
            }
            Update::Edited(m) => self.on_edited(m).await,
            Update::Deleted {
                chat: Some(chat),
                ids,
            } => {
                self.notify_deleted(Some(chat), &ids).await;
                // Deleting an unread message changes the server's counts,
                // and no read update may follow (plan section 6.3).
                self.badge_trigger(chat).await;
                let removed = self.windows.delete(chat, &ids);
                self.cache_write(CacheCmd::Delete(chat, ids)).await;
                if !removed.is_empty() {
                    self.emit(Event::Deleted { chat, ids: removed }).await;
                }
            }
            Update::Deleted { chat: None, ids } => {
                self.notify_deleted(None, &ids).await;
                let removed = self.windows.delete_unscoped(&ids);
                self.cache_write(CacheCmd::DeleteUnscoped(ids)).await;
                for (chat, ids) in removed {
                    self.emit(Event::Deleted { chat, ids }).await;
                }
            }
            Update::ReadInbox {
                chat,
                topic: None,
                still_unread,
                max_id,
            } => {
                self.notify_inbox_read(ThreadKey::whole(chat), max_id).await;
                self.chat_read(chat, still_unread, max_id).await;
            }
            // Topic reads on another device: the topics were triggered when
            // received (`topics::topics_on_update`); the badge refreshes.
            Update::ReadInbox {
                chat,
                topic: Some(topic),
                max_id,
                ..
            }
            | Update::ReadTopicMax {
                chat,
                topic,
                max_id,
            } => {
                self.notify_inbox_read(
                    ThreadKey {
                        chat,
                        topic: Some(topic),
                    },
                    max_id,
                )
                .await;
                self.badge_trigger(chat).await;
            }
            Update::MessageId { random_id, id } => self.on_message_id(random_id, id).await,
            // Topic edits and pins reached the retained topic lists when
            // they were received (`topics::topics_on_update`).
            Update::TopicEdited { .. } | Update::TopicsPinned { .. } => {}
            Update::ChatChanged { chat } => self.fence_chat(chat).await,
            Update::Folders(moves) => self.on_folders(moves).await,
            Update::ChatFoldersChanged => self.load_folders(),
        }
    }

    /// Chats moved into or out of the archive: each listed one changes in
    /// place (`ChatBumped` without a newer message never moves a row), and
    /// the cached list follows, so a restart shows the same split.
    pub(super) async fn on_folders(&mut self, moves: Vec<(ChatId, PeerFolder)>) {
        let mut changed = Vec::new();
        for (chat, folder) in moves {
            let now = self.badges.now();
            if let Some(summary) = self.dialogs.set_folder(chat, folder, now) {
                changed.push(summary.clone());
            }
        }
        if changed.is_empty() {
            return;
        }
        for summary in changed {
            self.emit(Event::ChatBumped(summary)).await;
        }
        if self.cache.is_some() {
            let all = self.dialogs.as_slice().to_vec();
            self.cache_write(CacheCmd::PutChats(all)).await;
        }
    }

    /// Applies an edit unless the window holds a later edit of the message.
    /// An edit only replaces a copy a window holds: found by `(chat, id)`
    /// across the chat's windows, the routed thread's first. The stored
    /// copy's thread wins (plan stage 2, section 6.2).
    pub(super) async fn on_edited(&mut self, m: Message) {
        let (routed, mut m) = match self.route_live(m) {
            (Route::Thread(routed), m) => (routed, m),
            (Route::ForumSuspected(chat), _) => return self.fence_chat(chat).await,
            (Route::ChatOnly(_), _) => return,
        };
        let Some(thread) = self.windows.holder(routed, m.id) else {
            return;
        };
        if thread != routed {
            tracing::debug!(id = m.id.0, "an edit names another thread than its copy");
            m.thread = thread;
        }
        if self.has_newer_copy(thread, &m) || !self.windows.replace(thread, m.clone()) {
            return;
        }
        if self.windows.is_cache_contiguous(thread) {
            self.cache_write(CacheCmd::PutMessages(thread, vec![m.clone()]))
                .await;
        }
        self.emit(Event::Edited(m)).await;
    }

    /// Whether the user is looking at `thread`'s live tail right now: the
    /// window is active, the UI reports this thread's tail on screen, and the
    /// window holds the tail. Only then does a new message not count as unread.
    pub(super) fn tail_visible(&self, thread: ThreadKey) -> bool {
        self.window == WindowState::Active
            && self.viewing == Some(thread)
            && self.open == Some(thread)
            && self.windows.get(thread).is_some_and(|w| !w.has_newer())
    }

    /// A new message bumps its chat; routed to a thread, it also reaches
    /// that thread's window and, under the continuity gate, its cache.
    async fn on_new_message(
        &mut self,
        m: Message,
        flags: super::notices::Flags,
        peer: Option<&(crate::model::ChatKind, String)>,
        origin: Origin<'_>,
    ) {
        let (route, m) = self.route_live(m);
        if origin.notify {
            self.consider_notice(&m, route, flags, peer);
        } else {
            self.remember_replayed_notice(&m, flags);
        }
        let thread = match route {
            Route::Thread(key) => Some(key),
            Route::ForumSuspected(chat) => {
                self.fence_chat(chat).await;
                None
            }
            Route::ChatOnly(_) => None,
        };
        let chat = m.thread.chat;
        let forum = self.is_forum(chat);
        // A forum's badge is never counted locally: an incoming message
        // only makes it due for a refresh (plan section 7).
        if forum && !m.outgoing {
            self.badge_trigger(chat).await;
        }
        let unread = if origin.source == super::boot::Source::CatchUp
            || forum
            || m.outgoing
            || thread.is_some_and(|t| self.tail_visible(t))
        {
            Unread::Unchanged
        } else {
            Unread::Increment
        };
        // A replayed message the dialogs snapshot already reflects (by top
        // message id, not by timestamp) must not bump the chat again and count
        // as unread twice. `Dialogs::bump` applies the same rule live.
        let reflected = origin
            .tops
            .and_then(|tops| tops.get(&chat))
            .is_some_and(|top| *top >= m.id);
        if !reflected && let Bump::Updated(summary) = self.dialogs.bump(chat, Last::of(&m), unread)
        {
            if unread == Unread::Increment {
                // A live change of the count: no older snapshot reverts it.
                self.dialogs.unread_counted(chat, self.badges.now());
            }
            self.emit(Event::ChatBumped(summary)).await;
        }
        let Some(thread) = thread else {
            return; // nothing shows it yet; opening its thread fetches it
        };
        let Some(view) = self.windows.get(thread).map(Window::view_gen) else {
            return;
        };
        if self.has_newer_copy(thread, &m) {
            return;
        }
        // Written through even when a detached window does not take it, so a
        // contiguous cache keeps reaching the live tail.
        if self.windows.is_cache_contiguous(thread) {
            self.cache_write(CacheCmd::PutMessages(thread, vec![m.clone()]))
                .await;
        }
        let id = m.id;
        let change = self.windows.upsert(thread, m);
        if change.inserted.contains(&id) {
            self.emit_history(thread, view, HistoryDir::Newer, change)
                .await;
        }
    }
}
