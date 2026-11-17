//! The four Stage 1 calls over raw TL requests (plan sections 7–9).
//!
//! Raw requests rather than grammers' iterators: `iter_dialogs` builds
//! `Dialog`s, which panic on unknown peers (`peer/dialog.rs:49-66`),
//! `mark_as_read` hardcodes `max_id: 0`, and `send_message` draws a fresh
//! `random_id` per call. Responses go through the pure mappers in `map`.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use grammers_client::InvocationError;
use grammers_client::session::Session as _;
use grammers_client::session::types::{PeerId, PeerInfo};
use grammers_client::tl;

use super::errors::{backend_error, kind};
use super::invoke::{Invoke, RetryClass, call};
use super::map::{self, Directory};
use super::pins::Pins;
use crate::backend::{
    BackendError, DialogsCursor, DialogsPage, HistoryPage, HistoryQuery, RejectReason,
};
use crate::model::{ChatId, MsgId};
use crate::session::{AbyssSession, PinRequest, Pinned};

/// Telegram's page size limit for dialogs and history.
pub(crate) const MAX_PAGE: u32 = 100;

fn page_limit(limit: u32) -> i32 {
    i32::try_from(limit.clamp(1, MAX_PAGE)).unwrap_or(1)
}

fn fail(e: &InvocationError) -> BackendError {
    backend_error(kind(e))
}

/// A response larger than its request allows: a definite failure, logged
/// without any of its content.
fn oversized() -> BackendError {
    tracing::warn!("oversized telegram response rejected");
    BackendError::Rejected(RejectReason::Other)
}

/// Where a later dialogs page starts.
pub(crate) struct DialogsOffset {
    pub(crate) date: i32,
    pub(crate) id: i32,
    pub(crate) peer: tl::enums::InputPeer,
}

/// The `messages.getDialogs` request for one page: the first page without
/// an offset. Pinned dialogs come only with the first page; later pages
/// exclude them, as `iter_dialogs` does
/// (`grammers-client/src/client/dialogs.rs:119`), or they would repeat.
pub(crate) fn dialogs_request(
    offset: Option<DialogsOffset>,
    limit: i32,
) -> tl::functions::messages::GetDialogs {
    let exclude_pinned = offset.is_some();
    let offset = offset.unwrap_or(DialogsOffset {
        date: 0,
        id: 0,
        peer: tl::enums::InputPeer::Empty,
    });
    tl::functions::messages::GetDialogs {
        exclude_pinned,
        folder_id: None,
        offset_date: offset.date,
        offset_id: offset.id,
        offset_peer: offset.peer,
        limit,
        hash: 0,
    }
}

/// The `messages.sendMessage` request for one attempt of a logical send.
/// Every attempt of the same send must carry the same `random_id`.
pub(crate) fn send_request(
    peer: tl::enums::InputPeer,
    text: String,
    random_id: i64,
) -> tl::functions::messages::SendMessage {
    tl::functions::messages::SendMessage {
        no_webpage: true,
        silent: false,
        background: false,
        clear_draft: false,
        noforwards: false,
        update_stickersets_order: false,
        invert_media: false,
        allow_paid_floodskip: false,
        peer,
        reply_to: None,
        message: text,
        random_id,
        reply_markup: None,
        entities: None,
        schedule_date: None,
        schedule_repeat_period: None,
        send_as: None,
        quick_reply_shortcut: None,
        effect: None,
        allow_paid_stars: None,
        suggested_post: None,
        rich_message: None,
    }
}

/// `messages.getHistory` arguments for a query: `(offset_id, add_offset, min_id)`.
pub(crate) fn history_offsets(query: HistoryQuery, limit: i32) -> (i32, i32, i32) {
    match query {
        HistoryQuery::Tail => (0, 0, 0),
        HistoryQuery::Before(id) => (id.0, 0, 0),
        // Newer than `id`: page backwards from it, and let `min_id` cut it off.
        HistoryQuery::After(id) => (id.0, limit.saturating_neg(), id.0),
    }
}

/// Calls of one instance over an [`Invoke`]r, with peers resolved through the
/// instance's session.
pub(crate) struct Calls<I> {
    invoker: I,
    session: Arc<AbyssSession>,
    /// What stays pinned in `session`, so the retired snapshot carries every
    /// access hash core may still need (see [`Pins`]). Held across the
    /// session update (never across an `.await`), so two updates cannot
    /// interleave.
    ///
    /// Lock order: `pins`, then the session's peer map (taken inside
    /// `retain_pinned*`). Nothing takes `pins` while holding the peer map:
    /// response caching (`cache_response`) releases it before [`Self::remember`]
    /// takes `pins`, and the update stream's `cache_peer` never takes `pins`
    /// at all; a desired peer those make addressable is pinned inside the
    /// peer map (`session::PinRequest`), with no other lock taken.
    pins: Mutex<Pins>,
}

impl<I> std::fmt::Debug for Calls<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Calls").finish_non_exhaustive()
    }
}

/// A call in progress on a chat: its peer stays pinned until the guard drops,
/// also when the call's future is cancelled.
pub(crate) struct InFlight<'a, I> {
    calls: &'a Calls<I>,
    id: Option<PeerId>,
}

impl<I> Drop for InFlight<'_, I> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            let mut pins = self.calls.pins();
            pins.end(id);
            self.calls.repin(&mut pins);
        }
    }
}

impl<I> Calls<I> {
    fn pins(&self) -> MutexGuard<'_, Pins> {
        // `Pins` holds no invariant a panic could break halfway.
        self.pins.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Hands the session the selected set when it changed, or when the last
    /// application left listed peers unknown (the session may have learned
    /// them since, so the count can change).
    fn repin(&self, pins: &mut Pins) {
        if pins.changed().is_some() || pins.has_unknown() {
            self.apply(pins, None);
        }
    }

    /// Applies the selected set (core's ids as required, the rest as
    /// transient), merging `resolved` into the session first under the same
    /// peer-map lock.
    fn apply(&self, pins: &mut Pins, resolved: Option<PeerInfo>) {
        let set = pins.selected();
        let (required, transient) = pins.split(&set);
        let request = PinRequest {
            required,
            transient,
        };
        let Pinned { unknown, .. } = self.session.retain_pinned_with(resolved, request);
        if unknown > 0 {
            tracing::debug!(unknown, "listed peers without an access hash left unpinned");
        }
        pins.applied(&set, unknown);
    }

    /// Core's required set (`Client::retain_peers`): pinned exactly, plus
    /// the chats of calls in progress. Never waits on I/O.
    pub(crate) fn retain(&self, required: &[ChatId]) {
        let mut pins = self.pins();
        let invalid = pins.require(required);
        if invalid > 0 {
            tracing::debug!(invalid, "required chats with out-of-range ids ignored");
        }
        self.repin(&mut pins);
    }

    #[cfg(test)]
    pub(crate) fn pin_state(&self) -> (Vec<PeerId>, usize) {
        let pins = self.pins();
        (pins.selected(), pins.unknown())
    }
}

impl<I: Invoke> Calls<I> {
    /// Calls over `session`, whose current pins (the inherited ones) stay as
    /// they are until core's first [`Self::retain`].
    pub(crate) fn new(invoker: I, session: Arc<AbyssSession>) -> Self {
        let pins = Pins::new(session.pinned_ids());
        Self {
            invoker,
            session,
            pins: Mutex::new(pins),
        }
    }

    pub(crate) fn session(&self) -> &Arc<AbyssSession> {
        &self.session
    }

    /// The raw invoker, for the login calls (`grammers::auth`).
    pub(crate) fn invoker(&self) -> &I {
        &self.invoker
    }

    async fn info(&self, chat: ChatId) -> Option<PeerInfo> {
        let id = map::peer_id(chat)?;
        self.session.peer(id).await.ok().flatten()
    }

    /// The `InputPeer` for `chat`. A chat the session has no access hash for
    /// cannot be addressed: that is a definite rejection.
    async fn resolve(&self, chat: ChatId) -> Result<tl::enums::InputPeer, BackendError> {
        let info = self.info(chat).await;
        map::input_peer(chat, info.as_ref()).ok_or(BackendError::Rejected(RejectReason::Other))
    }

    /// Caches every addressable peer of a response (grammers' `auto_cache_peers`
    /// does the same for the responses it handles itself), holding `handoff`
    /// (the chats a dialogs page hands to core, and its cursor peer) until
    /// core pins them: the response's other peers cannot evict them first.
    /// Peers core listed that the session could not pin are retried: the
    /// response may have carried their hashes.
    fn remember(&self, dir: &Directory, handoff: &[ChatId]) {
        let held: Vec<PeerId> = handoff.iter().filter_map(|&c| map::peer_id(c)).collect();
        if self.session.cache_response(&held, dir.infos()).is_err() {
            tracing::warn!("session poisoned; response peers not cached");
        }
        let mut pins = self.pins();
        if pins.has_unknown() {
            self.apply(&mut pins, None);
        }
    }

    /// Resolves a chat about to be addressed and pins it before the request,
    /// until the returned guard drops: a rebuild during the call must still
    /// carry its access hash.
    async fn resolve_used(
        &self,
        chat: ChatId,
    ) -> Result<(tl::enums::InputPeer, InFlight<'_, I>), BackendError> {
        let info = self.info(chat).await;
        let peer = map::input_peer(chat, info.as_ref())
            .ok_or(BackendError::Rejected(RejectReason::Other))?;
        Ok((peer, self.track(chat, info)))
    }

    /// Takes the in-flight guard for `chat`, resolved to `info` before any
    /// lock was held. Another call may have evicted `info` since, so the
    /// pin carries it: under `pins`, the session merges `info` and applies
    /// the selection under one peer-map lock. When the selection is already
    /// applied with nothing unknown, `chat` (if selected) is pinned and
    /// cannot have been evicted.
    pub(crate) fn track(&self, chat: ChatId, info: Option<PeerInfo>) -> InFlight<'_, I> {
        let mut pins = self.pins();
        let id = map::peer_id(chat).filter(|&id| pins.begin(id));
        if id.is_some() && (pins.changed().is_some() || pins.has_unknown()) {
            self.apply(&mut pins, info);
        }
        drop(pins);
        InFlight { calls: self, id }
    }

    pub(crate) async fn dialogs(
        &self,
        cursor: Option<DialogsCursor>,
        limit: u32,
    ) -> Result<DialogsPage, BackendError> {
        let limit = page_limit(limit);
        let offset = match cursor {
            None => None,
            Some(c) => {
                let peer = match c.offset_peer {
                    Some(chat) => self.resolve(chat).await.ok(),
                    None => None,
                };
                Some(DialogsOffset {
                    date: c.offset_date,
                    id: c.offset_id,
                    peer: peer.unwrap_or(tl::enums::InputPeer::Empty),
                })
            }
        };
        let request = dialogs_request(offset, limit);
        let resp = call(&self.invoker, RetryClass::Read, &request)
            .await
            .map_err(|e| fail(&e))?;
        let limit = usize::try_from(limit).unwrap_or(0);
        let parts = map::dialogs_parts(&resp, limit).map_err(|map::Oversized| oversized())?;
        let dir = Directory::from_lists(parts.users, parts.chats);
        let mapped = map::dialogs(&parts, &dir, limit);
        let cursor = mapped.next.as_ref().and_then(|c| c.offset_peer);
        let handoff: Vec<ChatId> = mapped.chats.iter().map(|c| c.id).chain(cursor).collect();
        self.remember(&dir, &handoff);
        if mapped.excluded > 0 {
            tracing::debug!(excluded = mapped.excluded, "broadcast channels left out");
        }
        // No pinning here: core lists the chats it keeps (`retain`); until
        // then the page's peers are held (`remember`).
        Ok(DialogsPage {
            chats: mapped.chats,
            next: mapped.next,
        })
    }

    pub(crate) async fn history(
        &self,
        chat: ChatId,
        query: HistoryQuery,
        limit: u32,
    ) -> Result<HistoryPage, BackendError> {
        let (peer, _in_flight) = self.resolve_used(chat).await?;
        let limit = page_limit(limit);
        let (offset_id, add_offset, min_id) = history_offsets(query, limit);
        let request = tl::functions::messages::GetHistory {
            peer,
            offset_id,
            offset_date: 0,
            add_offset,
            limit,
            max_id: 0,
            min_id,
            hash: 0,
        };
        let resp = call(&self.invoker, RetryClass::Read, &request)
            .await
            .map_err(|e| fail(&e))?;
        let limit = usize::try_from(limit).unwrap_or(0);
        let parts = map::messages_parts(&resp, limit).map_err(|map::Oversized| oversized())?;
        let dir = Directory::from_lists(parts.users, parts.chats);
        self.remember(&dir, &[]);
        Ok(map::history(&parts, &dir, chat, query, limit))
    }

    /// One attempt of a logical send. Never retried here: core retries with
    /// the same `random_id` (plan section 9).
    pub(crate) async fn send_text(
        &self,
        chat: ChatId,
        text: String,
        random_id: i64,
    ) -> Result<MsgId, BackendError> {
        let (peer, _in_flight) = self.resolve_used(chat).await?;
        let request = send_request(peer, text, random_id);
        let resp = call(&self.invoker, RetryClass::Never, &request)
            .await
            .map_err(|e| fail(&e))?;
        // Accepted, but without `updateMessageID` for our id: not an error to
        // retry, and not an id to show (plan section 9, `SentUnconfirmed`).
        map::sent_id(&resp, random_id).ok_or(BackendError::SentUnconfirmed)
    }

    /// Raw `readHistory` with the highest displayed id (grammers'
    /// `mark_as_read` sends `max_id: 0`).
    pub(crate) async fn read_history(
        &self,
        chat: ChatId,
        max_id: MsgId,
    ) -> Result<(), BackendError> {
        let (peer, _in_flight) = self.resolve_used(chat).await?;
        let res = if let Some(channel) = map::input_channel(&peer) {
            let request = tl::functions::channels::ReadHistory {
                channel,
                max_id: max_id.0,
            };
            call(&self.invoker, RetryClass::Read, &request)
                .await
                .map(drop)
        } else {
            let request = tl::functions::messages::ReadHistory {
                peer,
                max_id: max_id.0,
            };
            call(&self.invoker, RetryClass::Read, &request)
                .await
                .map(drop)
        };
        res.map_err(|e| fail(&e))
    }
}
