//! Evidence that server history covers a `SentUnconfirmed` send (plan
//! section 9). Such a send has no known id, and is never matched by text.
//! What core does know:
//!
//! - a lower bound: the send's id is above every id of its thread core knew
//!   when the user sent it (its `floor`), since the server numbers later
//!   messages higher;
//! - an upper bound: a tail requested after the server accepted the send
//!   reaches at least to it (or it was deleted).
//!
//! So server history requested after the acceptance, contiguous from just
//! above `floor` up to such a tail, holds the send's message (unless it was
//! deleted). The evidence counts only what the window actually holds after
//! the page was applied, and so was sent to the UI: messages a merge
//! ignored or a cap trimmed prove nothing, and a window detached from the
//! tail proves nothing. Core tracks that range per thread (a [`Cover`]),
//! reports the sends it covers (`SendsCovered`), and when the tail alone
//! does not reach back far enough (more than a page of newer messages),
//! fetches older pages itself, at most [`MAX_COVER_FETCHES`] per tail.
//!
//! Those fetches are background work. They run only when the thread is idle
//! and no load the user asked for waits (`run_cover_fetch`, after the
//! deferred loads), they insert only what fits without dropping a message
//! (`Windows::prepend_fitting`), and a `LoadOlder` asked while one runs is
//! answered by it rather than queued behind it. Whatever cannot be applied
//! that way leaves the echo unresolved. A page applies only while the
//! window still starts where it was asked from (`on_older`): live messages
//! can trim that end while the fetch runs.
//!
//! Known conservative limitation: coverage may stay unresolved while the
//! window has no room (2,000 messages, or the byte budget). A covering
//! fetch then inserts nothing, so the echo stays until a replacing tail, a
//! reopen or the 256-echo cap.

use std::collections::HashMap;

use super::Actor;
use super::apply::MIN_ID;
use super::jobs::ReqId;
use crate::api::{Event, HistoryDir};
use crate::backend::Backend;
use crate::model::{MsgId, ThreadKey};
use crate::store::{MAX_MESSAGES_PER_CHAT, ViewGen};

/// Older pages core fetches on its own, per covering tail, to reach a send.
pub(super) const MAX_COVER_FETCHES: u8 = 4;

/// Server history applied to `view`, contiguous from `lo` up to the tail
/// requested as `req` (and everything newer that arrived since).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Cover {
    view: ViewGen,
    lo: MsgId,
    req: ReqId,
    fetches: u8,
    /// The covering fetch last begun; it runs while it is the thread's
    /// single-flight load (`ReqId`s are never reused).
    fetching: Option<ReqId>,
}

/// Covers of threads with a `SentUnconfirmed` send awaiting one; an entry
/// goes when its thread has none left.
#[derive(Debug, Default)]
pub(super) struct Covers(HashMap<ThreadKey, Cover>);

impl Covers {
    /// Load `req` of `thread` completed or was dropped, whatever its view:
    /// whether it was the running covering fetch (its page is then applied
    /// with [`crate::store::Windows::prepend_fitting`]). Whether it answers
    /// a user's `LoadOlder` it absorbed is the owed load's business
    /// (`loads.rs`), not the cover's.
    pub(super) fn fetch_ended(&mut self, thread: ThreadKey, req: ReqId) -> bool {
        let Some(c) = self.0.get_mut(&thread).filter(|c| c.fetching == Some(req)) else {
            return false;
        };
        c.fetching = None;
        true
    }
}

impl<B: Backend> Actor<B> {
    /// From where up `view` of `thread` now holds the history a page claimed
    /// from `claimed` up: never below the window's oldest message (unless
    /// the window holds the start of history). `None` when the window is
    /// gone, replaced or detached from the tail, or holds nothing of it.
    fn applied_lo(&self, thread: ThreadKey, view: ViewGen, claimed: MsgId) -> Option<MsgId> {
        let w = self
            .windows
            .get(thread)
            .filter(|w| w.view_gen() == view && !w.has_newer())?;
        let bottom = if w.has_older() {
            w.messages().front()?.id
        } else {
            MIN_ID
        };
        Some(claimed.max(bottom))
    }

    /// A server tail requested as `req` was applied to `view` of `thread`,
    /// authoritative from `claimed` up (`None`: it covered nothing).
    pub(super) async fn tail_covers(
        &mut self,
        thread: ThreadKey,
        view: ViewGen,
        req: ReqId,
        claimed: Option<MsgId>,
    ) {
        let sends = &self.sends;
        self.covers.0.retain(|c, _| sends.awaits_cover(*c));
        let lo = claimed
            .filter(|_| self.sends.awaits_cover(thread))
            .and_then(|claimed| self.applied_lo(thread, view, claimed));
        let Some(lo) = lo else {
            self.covers.0.remove(&thread);
            return;
        };
        let cover = Cover {
            view,
            lo,
            req,
            fetches: 0,
            fetching: None,
        };
        self.covers.0.insert(thread, cover);
        self.settle_cover(thread).await;
    }

    /// A server page of history older than `edge` was applied to `view`,
    /// authoritative from `claimed` (`None`: it covered nothing), while
    /// `edge` was still the window's oldest message (`on_older`). It
    /// extends the thread's cover when it continues it (`edge` is inside it)
    /// and the window still holds it from the tail down.
    pub(super) async fn older_covers(
        &mut self,
        thread: ThreadKey,
        view: ViewGen,
        edge: MsgId,
        claimed: Option<MsgId>,
    ) {
        let lo = claimed.and_then(|claimed| self.applied_lo(thread, view, claimed));
        let Some(cover) = self
            .covers
            .0
            .get_mut(&thread)
            .filter(|c| c.view == view && edge >= c.lo)
        else {
            return;
        };
        match lo {
            Some(lo) => cover.lo = cover.lo.min(lo),
            // Detached by a user's page: the cover no longer reaches the tail.
            None => return,
        }
        self.settle_cover(thread).await;
    }

    /// Reports the sends `thread`'s cover reaches.
    async fn settle_cover(&mut self, thread: ThreadKey) {
        let Some(cover) = self.covers.0.get(&thread).copied() else {
            return;
        };
        let sends = self.sends.covered_by(thread, cover.lo, cover.req);
        if !sends.is_empty() {
            tracing::debug!(count = sends.len(), "unconfirmed sends covered");
            self.emit(Event::SendsCovered { thread, sends }).await;
        }
        if !self.sends.awaits_cover(thread) {
            self.covers.0.remove(&thread);
        }
    }

    /// Background work of the actor loop, after deferred user loads: loads
    /// the page older than the open thread's window when its cover still
    /// waits, the window starts inside the cover (so the page continues
    /// it), is attached and has room, no load runs or waits, and the fetch
    /// budget is not spent.
    pub(super) async fn run_cover_fetch(&mut self) {
        let Some(thread) = self.open else {
            return;
        };
        let Some(cover) = self.covers.0.get(&thread).copied() else {
            return;
        };
        if cover.fetches >= MAX_COVER_FETCHES
            || self.history_busy.contains_key(&thread)
            || self.deferred.iter().any(|d| d.thread == thread)
            || !self.sends.awaits_cover(thread)
        {
            return;
        }
        let Some(edge) = self
            .windows
            .get(thread)
            .filter(|w| w.view_gen() == cover.view && w.has_older() && !w.has_newer())
            .filter(|w| w.messages().len() < MAX_MESSAGES_PER_CHAT)
            .and_then(|w| w.messages().front())
            .map(|m| m.id)
            .filter(|oldest| *oldest >= cover.lo)
        else {
            return;
        };
        let load = self.begin_load(thread, cover.view);
        if let Some(c) = self.covers.0.get_mut(&thread) {
            c.fetches = c.fetches.saturating_add(1);
            c.fetching = Some(load.req);
        }
        self.fetch_more(load, HistoryDir::Older, edge).await;
    }

    /// Whether a `LoadOlder` of `thread` (that already started over
    /// `restarts` times) is answered by the covering fetch running now: its
    /// page is what the user asked for, so the load is not queued behind it
    /// (which would fetch the same page twice). The fetch then owes the
    /// user an answer (`loads::Owed`): its page, a bounded restart when the
    /// page no longer continues the window, or `LoadEnded` when it fails.
    pub(super) fn absorb_older(&mut self, thread: ThreadKey, restarts: u8) -> bool {
        let Some(load) = self.history_busy.get(&thread).copied() else {
            return false;
        };
        let current = self
            .windows
            .get(thread)
            .is_some_and(|w| w.view_gen() == load.view);
        let fetching = self
            .covers
            .0
            .get(&thread)
            .is_some_and(|c| c.fetching == Some(load.req));
        if !(current && fetching) {
            return false;
        }
        self.owe(load, HistoryDir::Older, restarts);
        true
    }
}
