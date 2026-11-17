//! The client calls over raw TL requests: the Stage 1 calls (plan sections
//! 7–9) and the forum calls (Stage 2 plan section 5).
//!
//! Raw requests rather than grammers' iterators: `iter_dialogs` builds
//! `Dialog`s, which panic on unknown peers (`peer/dialog.rs:49-66`),
//! `mark_as_read` hardcodes `max_id: 0`, `send_message` draws a fresh
//! `random_id` per call, and grammers has no forum API at all (it sends
//! `top_msg_id: None` everywhere). Responses go through the pure mappers in
//! `map`.
//!
//! Two calls depend on unknowns that only live verification settles (Stage 2
//! plan section 13), each behind one function: [`general_history`] (U1) and
//! [`general_read`] (U2).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use grammers_client::InvocationError;
use grammers_client::session::Session as _;
use grammers_client::session::types::{PeerId, PeerInfo};
use grammers_client::tl;

use super::errors::{backend_error, kind};
use super::invoke::{Invoke, RetryClass, call};
use super::map::{self, Directory, input_channel};
use super::pins::Pins;
use crate::backend::{
    BackendError, DialogsCursor, DialogsPage, HistoryPage, HistoryQuery, RejectReason, TopicsById,
    TopicsCursor, TopicsPage,
};
use crate::model::{
    ChatFolder, ChatId, ChatSummary, FolderEdit, MsgId, PeerFolder, PeerKind, ThreadKey, TopicId,
};
use crate::session::{AbyssSession, PinRequest, Pinned};
use crate::store::{MAX_PEER_DIALOGS, MAX_TOPICS_BY_ID};

/// Telegram's page size limit for dialogs, history and topics.
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

/// A request larger than the trait allows: a caller bug, refused without a
/// request.
fn too_many(call: &'static str, n: usize) -> BackendError {
    tracing::warn!(call, n, "request over its id bound refused");
    BackendError::Rejected(RejectReason::Other)
}

/// Response items dropped by the mappers (malformed topic ids, topics that
/// fail validation, deleted entries in a page), counted at debug.
fn note_skipped(call: &'static str, skipped: usize) {
    if skipped > 0 {
        tracing::debug!(call, skipped, "topic entries skipped");
    }
}

/// Only supergroups can be forums. A topic call for any other chat is
/// answered `NotForum` without a request (bot topics are out of scope).
fn require_channel(chat: ChatId) -> Result<(), BackendError> {
    match chat.kind {
        PeerKind::Channel => Ok(()),
        PeerKind::User | PeerKind::Chat => Err(BackendError::Rejected(RejectReason::NotForum)),
    }
}

/// [`require_channel`] for a thread naming a topic; whole chats need nothing.
fn require_thread(thread: ThreadKey) -> Result<(), BackendError> {
    match thread.topic {
        Some(_) => require_channel(thread.chat),
        None => Ok(()),
    }
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

/// The `reply_to` that posts into `topic` (plan section 5): a plain post
/// into topic `t` "replies" to its root, `t`, without `top_msg_id`. A post
/// into General carries none (Telegram's docs; *live*), nor does one into a
/// chat that is not a forum.
pub(crate) fn topic_reply_to(topic: Option<TopicId>) -> Option<tl::enums::InputReplyTo> {
    let topic = topic.filter(|t| *t != TopicId::GENERAL)?;
    Some(tl::enums::InputReplyTo::Message(
        tl::types::InputReplyToMessage {
            reply_to_msg_id: topic.get(),
            top_msg_id: None,
            reply_to_peer_id: None,
            quote_text: None,
            quote_entities: None,
            quote_offset: None,
            monoforum_peer_id: None,
            todo_item_id: None,
            poll_option: None,
            quote_rich: None,
        },
    ))
}

/// The `messages.sendMessage` request for one attempt of a logical send.
/// Every attempt of the same send must carry the same `random_id`.
pub(crate) fn send_request(
    peer: tl::enums::InputPeer,
    text: String,
    random_id: i64,
    reply_to: Option<tl::enums::InputReplyTo>,
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
        reply_to,
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

/// The request for one page of a thread's history.
pub(crate) enum HistoryRequest {
    /// A whole chat: `messages.getHistory`.
    Chat(tl::functions::messages::GetHistory),
    /// One forum topic: `messages.getReplies` with the topic's root.
    Topic(tl::functions::messages::GetReplies),
}

/// The history request for `thread`'s page. `history_offsets` applies to
/// both calls, which take the same offset arguments (*live*: the `After`
/// semantics for `getReplies`, as for `getHistory` in Stage 1).
pub(crate) fn history_request(
    peer: tl::enums::InputPeer,
    topic: Option<TopicId>,
    query: HistoryQuery,
    limit: i32,
) -> HistoryRequest {
    match topic {
        None => {
            let (offset_id, add_offset, min_id) = history_offsets(query, limit);
            HistoryRequest::Chat(tl::functions::messages::GetHistory {
                peer,
                offset_id,
                offset_date: 0,
                add_offset,
                limit,
                max_id: 0,
                min_id,
                hash: 0,
            })
        }
        Some(TopicId::GENERAL) => general_history(peer, query, limit),
        Some(topic) => HistoryRequest::Topic(replies_request(peer, topic, query, limit)),
    }
}

/// **Unknown U1** (plan section 13): how General's history is fetched. Its
/// messages carry no topic header, so whether `getReplies(msg_id = 1)`
/// returns them is unverified. The fallback is `messages.search` with
/// `top_msg_id = 1`, `q = ""` and an empty filter (same response type).
/// Live verification flips this function only.
fn general_history(peer: tl::enums::InputPeer, query: HistoryQuery, limit: i32) -> HistoryRequest {
    HistoryRequest::Topic(replies_request(peer, TopicId::GENERAL, query, limit))
}

fn replies_request(
    peer: tl::enums::InputPeer,
    topic: TopicId,
    query: HistoryQuery,
    limit: i32,
) -> tl::functions::messages::GetReplies {
    let (offset_id, add_offset, min_id) = history_offsets(query, limit);
    tl::functions::messages::GetReplies {
        peer,
        msg_id: topic.get(),
        offset_id,
        offset_date: 0,
        add_offset,
        limit,
        max_id: 0,
        min_id,
        hash: 0,
    }
}

/// The request marking a thread read up to `max_id`.
pub(crate) enum ReadRequest {
    /// A private chat or basic group: `messages.readHistory`.
    Chat(tl::functions::messages::ReadHistory),
    /// A whole supergroup: `channels.readHistory`.
    Channel(tl::functions::channels::ReadHistory),
    /// One forum topic: `messages.readDiscussion` with the topic's root.
    Topic(tl::functions::messages::ReadDiscussion),
}

/// The read request for `thread`'s `max_id` (plan section 5). A topic is
/// never read with `channels.readHistory`, which has no topic argument.
pub(crate) fn read_request(
    peer: tl::enums::InputPeer,
    topic: Option<TopicId>,
    max_id: MsgId,
) -> ReadRequest {
    match topic {
        None => match input_channel(&peer) {
            Some(channel) => ReadRequest::Channel(tl::functions::channels::ReadHistory {
                channel,
                max_id: max_id.0,
            }),
            None => ReadRequest::Chat(tl::functions::messages::ReadHistory {
                peer,
                max_id: max_id.0,
            }),
        },
        Some(TopicId::GENERAL) => general_read(peer, max_id),
        Some(topic) => ReadRequest::Topic(discussion_request(peer, topic, max_id)),
    }
}

/// **Unknown U2** (plan section 13): how General is marked read. Until live
/// verification shows otherwise it is `readDiscussion(msg_id = 1)`, never
/// `channels.readHistory`, which might clear every topic of the forum.
/// Live verification flips this function only.
fn general_read(peer: tl::enums::InputPeer, max_id: MsgId) -> ReadRequest {
    ReadRequest::Topic(discussion_request(peer, TopicId::GENERAL, max_id))
}

fn discussion_request(
    peer: tl::enums::InputPeer,
    topic: TopicId,
    max_id: MsgId,
) -> tl::functions::messages::ReadDiscussion {
    tl::functions::messages::ReadDiscussion {
        peer,
        msg_id: topic.get(),
        read_max_id: max_id.0,
    }
}

/// The `messages.getForumTopics` request for the page after `cursor` (the
/// first page without one), in the cursor's fields as the page that built
/// it reported them (plan section 5). No search query (`q`).
pub(crate) fn topics_request(
    peer: tl::enums::InputPeer,
    cursor: Option<&TopicsCursor>,
    limit: i32,
) -> tl::functions::messages::GetForumTopics {
    tl::functions::messages::GetForumTopics {
        peer,
        q: None,
        offset_date: cursor.map_or(0, |c| c.offset_date),
        offset_id: cursor.map_or(0, |c| c.offset_id),
        offset_topic: cursor.map_or(0, |c| c.offset_topic),
        limit,
    }
}

/// The `messages.getForumTopicsByID` request for `ids`.
pub(crate) fn topics_by_id_request(
    peer: tl::enums::InputPeer,
    ids: &[TopicId],
) -> tl::functions::messages::GetForumTopicsById {
    tl::functions::messages::GetForumTopicsById {
        peer,
        topics: ids.iter().map(|t| t.get()).collect(),
    }
}

/// The `messages.getPeerDialogs` request for `peers`.
pub(crate) fn peer_dialogs_request(
    peers: Vec<tl::enums::InputPeer>,
) -> tl::functions::messages::GetPeerDialogs {
    tl::functions::messages::GetPeerDialogs {
        peers: peers
            .into_iter()
            .map(|peer| tl::enums::InputDialogPeer::Peer(tl::types::InputDialogPeer { peer }))
            .collect(),
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

    /// `messages.getDialogFilters`, size-checked: the raw folders.
    async fn fetch_raw_folders(&self) -> Result<Vec<tl::enums::DialogFilter>, BackendError> {
        let request = tl::functions::messages::GetDialogFilters {};
        let resp = call(&self.invoker, RetryClass::Read, &request)
            .await
            .map_err(|e| fail(&e))?;
        let raw = map::folders::raw_folders(&resp).map_err(|map::Oversized| oversized())?;
        Ok(raw.to_vec())
    }

    /// `messages.getDialogFilters`. `inputPeerSelf` maps to the signed-in
    /// user, read from the session.
    pub(crate) async fn chat_folders(&self) -> Result<Vec<ChatFolder>, BackendError> {
        let raw = self.fetch_raw_folders().await?;
        let self_id = self.session.snapshot().self_id().ok().flatten();
        Ok(map::folders::chat_folders(&raw, self_id))
    }

    /// `folders.editPeerFolders` for one chat. Idempotent, so retried like
    /// a read.
    pub(crate) async fn set_peer_folder(
        &self,
        chat: ChatId,
        folder: PeerFolder,
    ) -> Result<(), BackendError> {
        let (peer, _in_flight) = self.resolve_used(chat).await?;
        let folder_id = match folder {
            PeerFolder::Main => 0,
            PeerFolder::Archive => map::folders::ARCHIVE,
        };
        let request = tl::functions::folders::EditPeerFolders {
            folder_peers: vec![tl::enums::InputFolderPeer::Peer(
                tl::types::InputFolderPeer { peer, folder_id },
            )],
        };
        call(&self.invoker, RetryClass::Read, &request)
            .await
            .map(drop)
            .map_err(|e| fail(&e))
    }

    /// `messages.updateDialogFilter` made from the raw folders, fetched by
    /// this call right before the write (never a copy shared with other
    /// calls). It sets a whole folder, so it is retried like a read.
    ///
    /// The fresh fetch matters: a copy fetched earlier may miss a change
    /// made since (on another device, or by this account's previous write),
    /// which the whole-folder write would undo, and an instance that never
    /// fetched would pick id 2 for a new folder. Core admits one folder
    /// write at a time, so this account sends no other write between the
    /// fetch and the write; an earlier write with an unknown outcome may
    /// still be applied by the server in between (folders plan, part C,
    /// "Risks").
    pub(crate) async fn edit_chat_folder(&self, edit: FolderEdit) -> Result<(), BackendError> {
        let chat = match &edit {
            FolderEdit::AddChat { chat, .. }
            | FolderEdit::RemoveChat { chat, .. }
            | FolderEdit::Create { chat, .. } => Some(*chat),
            FolderEdit::Delete { .. } => None,
        };
        let (peer, _in_flight) = match chat {
            Some(chat) => {
                let (peer, guard) = self.resolve_used(chat).await?;
                (Some(peer), Some(guard))
            }
            None => (None, None),
        };
        let raw = self.fetch_raw_folders().await?;
        let self_id = self.session.snapshot().self_id().ok().flatten();
        let (id, filter) = map::folders::edited(&raw, &edit, peer, self_id).map_err(|e| {
            tracing::debug!(?e, "folder edit refused");
            BackendError::Rejected(match e {
                map::folders::EditError::Unknown | map::folders::EditError::Changed => {
                    RejectReason::FolderChanged
                }
                map::folders::EditError::Limit => RejectReason::FolderLimit,
                map::folders::EditError::Shared => RejectReason::Other,
            })
        })?;
        let request = tl::functions::messages::UpdateDialogFilter { id, filter };
        call(&self.invoker, RetryClass::Read, &request)
            .await
            .map(drop)
            .map_err(|e| fail(&e))
    }

    /// One page of `thread`'s history. Messages of other threads in the
    /// response are dropped (plan section 5).
    pub(crate) async fn history(
        &self,
        thread: ThreadKey,
        query: HistoryQuery,
        limit: u32,
    ) -> Result<HistoryPage, BackendError> {
        require_thread(thread)?;
        let (peer, _in_flight) = self.resolve_used(thread.chat).await?;
        let limit = page_limit(limit);
        let resp = match history_request(peer, thread.topic, query, limit) {
            HistoryRequest::Chat(r) => call(&self.invoker, RetryClass::Read, &r).await,
            HistoryRequest::Topic(r) => call(&self.invoker, RetryClass::Read, &r).await,
        }
        .map_err(|e| fail(&e))?;
        let limit = usize::try_from(limit).unwrap_or(0);
        let parts = map::messages_parts(&resp, limit).map_err(|map::Oversized| oversized())?;
        let dir = Directory::from_lists(parts.users, parts.chats);
        self.remember(&dir, &[]);
        Ok(map::history(&parts, &dir, thread, query, limit))
    }

    /// One attempt of a logical send. Never retried here: core retries with
    /// the same `random_id` (plan section 9).
    /// `TOPIC_CLOSED` and `TOPIC_DELETED` come back as their
    /// [`RejectReason`]s (`errors::reject_reason`).
    pub(crate) async fn send_text(
        &self,
        thread: ThreadKey,
        text: String,
        random_id: i64,
    ) -> Result<MsgId, BackendError> {
        require_thread(thread)?;
        let (peer, _in_flight) = self.resolve_used(thread.chat).await?;
        let request = send_request(peer, text, random_id, topic_reply_to(thread.topic));
        let resp = call(&self.invoker, RetryClass::Never, &request)
            .await
            .map_err(|e| fail(&e))?;
        // Accepted, but without `updateMessageID` for our id: not an error to
        // retry, and not an id to show (plan section 9, `SentUnconfirmed`).
        map::sent_id(&resp, random_id).ok_or(BackendError::SentUnconfirmed)
    }

    /// Raw `readHistory`/`readDiscussion` with the highest displayed id
    /// (grammers' `mark_as_read` sends `max_id: 0`).
    pub(crate) async fn read_history(
        &self,
        thread: ThreadKey,
        max_id: MsgId,
    ) -> Result<(), BackendError> {
        require_thread(thread)?;
        let (peer, _in_flight) = self.resolve_used(thread.chat).await?;
        let res = match read_request(peer, thread.topic, max_id) {
            ReadRequest::Chat(r) => call(&self.invoker, RetryClass::Read, &r).await.map(drop),
            ReadRequest::Channel(r) => call(&self.invoker, RetryClass::Read, &r).await.map(drop),
            ReadRequest::Topic(r) => call(&self.invoker, RetryClass::Read, &r).await.map(drop),
        };
        res.map_err(|e| fail(&e))
    }

    /// One page of a forum's topics after `cursor`. A chat that is not a
    /// forum is `NotForum` (`CHANNEL_FORUM_MISSING`, or not a supergroup).
    pub(crate) async fn topics(
        &self,
        chat: ChatId,
        cursor: Option<TopicsCursor>,
        limit: u32,
    ) -> Result<TopicsPage, BackendError> {
        require_channel(chat)?;
        let (peer, _in_flight) = self.resolve_used(chat).await?;
        let limit = page_limit(limit);
        let request = topics_request(peer, cursor.as_ref(), limit);
        let resp = call(&self.invoker, RetryClass::Read, &request)
            .await
            .map_err(|e| fail(&e))?;
        let limit = usize::try_from(limit).unwrap_or(0);
        let parts =
            map::topics::topics_parts(&resp, limit).map_err(|map::Oversized| oversized())?;
        let dir = Directory::from_lists(parts.users, parts.chats);
        self.remember(&dir, &[]);
        let (page, skipped) = map::topics::page(&parts, &dir, chat, cursor.as_ref());
        note_skipped("topics", skipped);
        Ok(page)
    }

    /// The given topics of a forum, at most [`MAX_TOPICS_BY_ID`].
    pub(crate) async fn topics_by_id(
        &self,
        chat: ChatId,
        ids: &[TopicId],
    ) -> Result<TopicsById, BackendError> {
        if ids.len() > MAX_TOPICS_BY_ID {
            return Err(too_many("topics_by_id", ids.len()));
        }
        require_channel(chat)?;
        let (peer, _in_flight) = self.resolve_used(chat).await?;
        let request = topics_by_id_request(peer, ids);
        let resp = call(&self.invoker, RetryClass::Read, &request)
            .await
            .map_err(|e| fail(&e))?;
        let parts =
            map::topics::topics_parts(&resp, ids.len()).map_err(|map::Oversized| oversized())?;
        let dir = Directory::from_lists(parts.users, parts.chats);
        self.remember(&dir, &[]);
        let (result, skipped) = map::topics::by_id(&parts, &dir, chat);
        note_skipped("topics_by_id", skipped);
        Ok(result)
    }

    /// The current summaries of `chats`, at most [`MAX_PEER_DIALOGS`].
    /// Chats without an access hash are left out without a request, as are
    /// chats the server does not return and broadcasts.
    pub(crate) async fn peer_dialogs(
        &self,
        chats: &[ChatId],
    ) -> Result<Vec<ChatSummary>, BackendError> {
        if chats.len() > MAX_PEER_DIALOGS {
            return Err(too_many("peer_dialogs", chats.len()));
        }
        let mut asked = Vec::with_capacity(chats.len());
        let mut peers = Vec::with_capacity(chats.len());
        let mut in_flight = Vec::with_capacity(chats.len());
        for &chat in chats {
            if asked.contains(&chat) {
                continue;
            }
            if let Ok((peer, guard)) = self.resolve_used(chat).await {
                asked.push(chat);
                peers.push(peer);
                in_flight.push(guard);
            } else {
                tracing::debug!("peer dialog without an access hash left out");
            }
        }
        if peers.is_empty() {
            return Ok(Vec::new());
        }
        let request = peer_dialogs_request(peers);
        let resp = call(&self.invoker, RetryClass::Read, &request)
            .await
            .map_err(|e| fail(&e))?;
        let parts = map::topics::peer_dialogs_parts(&resp, asked.len())
            .map_err(|map::Oversized| oversized())?;
        let dir = Directory::from_lists(parts.users, parts.chats);
        self.remember(&dir, &asked);
        let mapped = map::topics::peer_dialogs(&parts, &dir, &asked);
        if mapped.excluded > 0 {
            tracing::debug!(excluded = mapped.excluded, "broadcast channels left out");
        }
        drop(in_flight);
        Ok(mapped.chats)
    }
}
