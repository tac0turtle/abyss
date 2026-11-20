//! Opening a thread at its read position, and returning to the live tail
//! (Stage 2 plan sections 17.6.4–17.6.6).
//!
//! - **`OpenThreadAt { thread, after }`** shows nothing retained or cached:
//!   only a server page fetched for this open proves where the first message
//!   above `after` is. Its load is first a server tail. A tail that reaches
//!   `after` (or holds the whole history) is applied as an open's tail;
//!   otherwise it is dropped unapplied and the same view loads
//!   `Around(after)`, a detached window ([`crate::store::Windows::anchor`])
//!   whose history never populates or reconciles the cached message pages
//!   (metadata such as `Touch`, and authoritative deletions, still reach
//!   the cache as for any window). An `Around` page with nothing above
//!   `after` falls back to a tail applied whatever it covers, and so does an
//!   anchor with no successor (`i32::MAX`). The open's load ends in
//!   `LoadEnded` when it fails, offline included, so the UI can fall back
//!   to `OpenThread` (best effort).
//! - **Restoring continuity.** A `Newer` page that attaches the open
//!   window while its cache isn't contiguous asks for one server tail
//!   ([`TailMode::Restore`](super::apply::TailMode::Restore), applied by
//!   `merge_tail`): a merge that keeps the view, writes the tail through
//!   (`ReconcileTail`) and restores continuity only under
//!   `Windows::may_restore`. When that tail fails or is refused, a later
//!   light refresh of the attached window restores it the same way, under
//!   its own mark (`merge_tail`). Besides these, only a replacing tail
//!   makes the cache contiguous again.
//!
//! The anchored load and the restoring tail are the thread's single-flight
//! history load, so user loads asked meanwhile are deferred (`loads`).

use super::Actor;
use super::apply::HistoryReply;
use super::jobs::{Job, Load};
use super::lifecycle::Phase;
use crate::api::{HistoryDir, LoadTarget, UserError};
use crate::backend::{Backend, HistoryQuery};
use crate::model::{MsgId, ThreadKey};
use crate::store::ViewGen;

/// What the page an anchored open's load fetches must show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Proof {
    /// A server tail applied only if it reaches the anchor; else `Around`
    /// follows.
    Tail,
    /// The `Around(after)` page.
    Around,
    /// A server tail applied whatever it covers.
    AnyTail,
}

/// The load of an `OpenThreadAt` in flight: its thread's single-flight
/// load, until its page is applied or it fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Anchored {
    pub(super) load: Load,
    /// The read position the open shows.
    pub(super) after: MsgId,
    pub(super) proof: Proof,
}

impl Anchored {
    /// The direction its `History` (or `LoadEnded`) answers in.
    pub(super) fn dir(self) -> HistoryDir {
        match self.proof {
            Proof::Around => HistoryDir::Around,
            Proof::Tail | Proof::AnyTail => HistoryDir::Tail,
        }
    }

    fn target(self) -> LoadTarget {
        LoadTarget::History {
            thread: self.load.thread,
            dir: self.dir(),
        }
    }
}

/// Whether a server tail proves where the first message above `after` is:
/// its covered range reaches `after`, or it is the whole history.
fn proves(p: &HistoryReply, after: MsgId) -> bool {
    !p.page.has_more || p.covered.is_some_and(|c| c.oldest <= after)
}

impl<B: Backend> Actor<B> {
    /// `OpenThreadAt` (see the module docs). Refused like `OpenThread`,
    /// and for a negative `after`. A fenced chat's open waits for the lift,
    /// which loads the tail (the anchor is dropped).
    pub(super) async fn open_thread_at(&mut self, thread: ThreadKey, after: MsgId) {
        if after.0 < 0 {
            tracing::debug!("a negative read position refused");
            return self.error(UserError::Rejected).await;
        }
        if self.refuse_open(thread).await {
            return;
        }
        // No shortcut: whatever the window retains goes unseen.
        self.windows.invalidate(thread);
        let view = self.show_thread(thread).await;
        if self.is_fenced(thread.chat) {
            return;
        }
        if !matches!(self.life.phase, Phase::Booting | Phase::Live) {
            // Offline: best effort. The UI falls back to `OpenThread`,
            // which shows the cached tail.
            let target = LoadTarget::History {
                thread,
                dir: HistoryDir::Tail,
            };
            return self.load_ended(target, Some(UserError::Network)).await;
        }
        let load = self.begin_load(thread, view);
        let proof = if after.0.checked_add(1).is_some() {
            Proof::Tail
        } else {
            Proof::AnyTail
        };
        self.claim_if_booting(load);
        self.touch_cache(thread).await;
        self.load_anchored(Anchored { load, after, proof }).await;
    }

    /// Requests `anchored`'s page: a server tail (as a boot snapshot when
    /// its load is one) or the `Around` page.
    async fn load_anchored(&mut self, anchored: Anchored) {
        let Anchored { load, after, proof } = anchored;
        self.anchored = Some(anchored);
        if proof != Proof::Around && self.is_tail_snapshot(load) {
            return self.push_tail_snapshot(load);
        }
        let (dir, query) = match proof {
            Proof::Around => (HistoryDir::Around, HistoryQuery::Around(after)),
            Proof::Tail | Proof::AnyTail => (HistoryDir::Tail, HistoryQuery::Tail),
        };
        let job = Job::History {
            load,
            dir,
            query,
            floor: self.history_tail_floor(load.thread),
            since: self.windows.continuity_mark(),
        };
        if !self.enqueue(job).await {
            self.release_history(load);
            self.anchored = None;
            self.load_ended(anchored.target(), Some(UserError::Busy))
                .await;
        }
    }

    /// The anchored open's load failed (an error, a panic, or a rebuild
    /// abandoned it): the UI falls back to `OpenThread`.
    pub(super) async fn anchored_failed(&self, anchored: Anchored, error: UserError) {
        self.load_ended(anchored.target(), Some(error)).await;
    }

    /// A current page of the anchored open's load (see the module docs).
    pub(super) async fn on_anchored(&mut self, p: HistoryReply, anchored: Anchored) {
        let Anchored { after, proof, .. } = anchored;
        match proof {
            Proof::Tail if !proves(&p, after) => {
                tracing::debug!("the tail doesn't reach the read position: Around follows");
                self.reload_anchored(p.thread, p.view, after, Proof::Around)
                    .await;
            }
            Proof::Tail | Proof::AnyTail => self.on_tail(p).await,
            Proof::Around => self.on_around(p, after).await,
        }
    }

    /// The same view's next anchored load, the page just received dropped
    /// unapplied.
    async fn reload_anchored(
        &mut self,
        thread: ThreadKey,
        view: ViewGen,
        after: MsgId,
        proof: Proof,
    ) {
        let load = self.begin_load(thread, view);
        self.load_anchored(Anchored { load, after, proof }).await;
    }

    /// The `Around(after)` page fills the view's window, detached, and is
    /// emitted as `History { dir: Around }`. Its history is neither read
    /// from nor written to the cached message pages: the window isn't
    /// contiguous, which keeps those cache paths shut until it returns to
    /// the tail (deletions and metadata still apply as for any window). A
    /// page with nothing above `after` is dropped for the tail.
    async fn on_around(&mut self, p: HistoryReply, after: MsgId) {
        let HistoryReply {
            thread, view, page, ..
        } = p;
        if !page.messages.iter().any(|m| m.id > after) {
            tracing::debug!("nothing above the read position: the tail instead");
            return self
                .reload_anchored(thread, view, after, Proof::AnyTail)
                .await;
        }
        debug_assert!(!self.windows.is_cache_contiguous(thread));
        if let Ok(change) = self.windows.anchor(thread, view, page) {
            self.emit_history(thread, view, HistoryDir::Around, change)
                .await;
        }
    }

    /// A `Newer` page was applied to `view` of `thread`: when it attached
    /// the open window to the live tail while its cache isn't contiguous,
    /// one server tail follows to restore continuity (`TailMode::Restore`).
    /// The page's load has released the slot already; a load running or a
    /// fence (whose lift reloads the tail) leaves it alone, and so does a
    /// window without a cache to restore.
    pub(super) async fn restore_if_attached(&mut self, thread: ThreadKey, view: ViewGen) {
        let attached = self
            .windows
            .get(thread)
            .is_some_and(|w| w.view_gen() == view && !w.has_newer() && !w.cache_contiguous());
        if !attached
            || self.cache.is_none()
            || self.open != Some(thread)
            || self.history_busy.contains_key(&thread)
            || self.is_fenced(thread.chat)
        {
            return;
        }
        let load = self.begin_load(thread, view);
        self.restoring = Some(load);
        self.request_tail(load).await;
    }

    /// Invariant, checked once per actor loop turn: the anchored load
    /// belongs to the open thread's current view.
    pub(super) fn anchored_is_current(&self) -> bool {
        self.anchored.is_none_or(|a| {
            Some(a.load.thread) == self.open
                && self
                    .windows
                    .get(a.load.thread)
                    .is_some_and(|w| w.view_gen() == a.load.view)
        })
    }
}
