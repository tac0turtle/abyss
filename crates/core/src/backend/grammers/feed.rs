//! Retain the source and end of each difference. grammers' public
//! `next_raw` flattens socket updates and difference slices into one stream.
//! Use its message box directly so the same ordering and gap recovery apply
//! while core can distinguish replay from live traffic.

mod history;

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use grammers_client::InvocationError;
use grammers_client::tl;
use grammers_session::Session as _;
use grammers_session::types::{ChannelKind, PeerInfo, UpdateState, UpdatesState};
use grammers_session::updates::{MessageBoxes, PrematureEndReason, UpdateAndPeers, UpdatesLike};
use tokio::sync::mpsc;

use super::{
    channels::{
        MAX_TRACKED_CHANNELS, PossibleGaps, Recency, broadcast_chat, broadcast_post,
        forget_broadcasts, offered_pts,
    },
    invoke::Invoke,
    map,
    stream::{Pull, Source},
};
use crate::backend::{Baseline, NotifyTarget, StaleHistory, StreamItem, Update, UpdateCheckpoint};
use crate::model::{ChatId, PeerKind};
use crate::session::AbyssSession;
use history::HistoryUpdates;

const MAX_BATCH: usize = 4096;
const CHANNEL_DIFF_LIMIT: i32 = 100;
const ACCOUNT_DIFF_LIMIT: i32 = 1000;
/// How long the feed waits for one of its own requests (`getState`,
/// `getDifference`, `getChannelDifference`). On expiry the instance is
/// retired: the request may still sit in the runner, and a feed that waits on
/// it would stop all live updates (stage 3 review A9).
pub(super) const FEED_RPC_DEADLINE: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum CatchUp {
    #[default]
    Idle,
    Replay,
    Reconcile,
}

impl CatchUp {
    fn start(&mut self) {
        if *self == Self::Idle {
            *self = Self::Replay;
        }
    }
}

pub(super) struct Feed<I = grammers_client::Client> {
    client: I,
    session: Arc<AbyssSession>,
    socket: mpsc::Receiver<UpdatesLike>,
    boxes: MessageBoxes,
    recency: Recency,
    possible_gaps: PossibleGaps,
    common_gap: HistoryUpdates,
    rejected_history: HistoryUpdates,
    broadcasts: BTreeSet<i64>,
    tracked_broadcasts: BTreeSet<i64>,
    stale_scopes: BTreeSet<Option<ChatId>>,
    baseline_pending: bool,
    catch_up: CatchUp,
    ready: VecDeque<StreamItem>,
}

impl<I: Invoke> Feed<I> {
    pub(super) fn new(
        client: I,
        session: Arc<AbyssSession>,
        socket: mpsc::Receiver<UpdatesLike>,
        resume: Option<UpdatesState>,
    ) -> Self {
        Self {
            client,
            session,
            socket,
            boxes: resume
                .filter(|s| s.pts > 0)
                .map_or_else(MessageBoxes::new, MessageBoxes::load),
            recency: Recency::new(),
            possible_gaps: PossibleGaps::default(),
            common_gap: HistoryUpdates::default(),
            rejected_history: HistoryUpdates::default(),
            broadcasts: BTreeSet::new(),
            tracked_broadcasts: BTreeSet::new(),
            stale_scopes: BTreeSet::new(),
            baseline_pending: true,
            catch_up: CatchUp::Idle,
            ready: VecDeque::new(),
        }
    }

    /// A carried checkpoint makes grammers begin with a difference.
    pub(super) fn baseline_kind(&self) -> Baseline {
        if self.boxes.is_empty() {
            Baseline::Fresh
        } else {
            Baseline::Resumed
        }
    }

    async fn baseline(&mut self) -> Result<(), InvocationError> {
        if self.boxes.is_empty() {
            self.fresh_state().await?;
            self.checkpoint(self.boxes.session_state());
        }
        self.session
            .set_update_state(UpdateState::All(self.boxes.session_state()))
            .await
            .map_err(|e| InvocationError::Session(Box::new(e)))?;
        self.baseline_pending = false;
        Ok(())
    }

    async fn fresh_state(&mut self) -> Result<(), InvocationError> {
        let tl::enums::updates::State::State(state) =
            self.invoke(&tl::functions::updates::GetState {}).await?;
        self.boxes.set_state(state);
        Ok(())
    }

    /// One of the feed's own requests, under [`FEED_RPC_DEADLINE`]. Expiry is
    /// `Dropped`, which the stream reports as fatal: core rebuilds the
    /// instance, so the abandoned request cannot pile up in its runner.
    async fn invoke<R>(&self, request: &R) -> Result<R::Return, InvocationError>
    where
        R: tl::RemoteCall + Send + Sync,
        R::Return: Send,
    {
        match tokio::time::timeout(FEED_RPC_DEADLINE, self.client.invoke(request)).await {
            Ok(reply) => reply,
            Err(_) => Err(InvocationError::Dropped("update request timed out")),
        }
    }

    pub(super) async fn next(&mut self) -> Result<StreamItem, InvocationError> {
        if self.baseline_pending {
            self.baseline().await?;
        }
        loop {
            if let Some(item) = self.ready.pop_front() {
                return Ok(item);
            }
            if self.boxes.is_empty() {
                self.fresh_state().await?;
            }
            let deadline = self.boxes.check_deadlines();
            if let Some(request) = self.boxes.get_difference() {
                self.account_difference(request).await?;
            } else if let Some(request) = self.boxes.get_channel_difference() {
                self.channel_difference(request).await?;
            } else if self.catch_up != CatchUp::Idle {
                self.finish_catch_up();
            } else {
                match tokio::time::timeout_at(deadline.into(), self.socket.recv()).await {
                    Ok(Some(updates)) => Box::pin(self.process_socket(updates)).await?,
                    Ok(None) => return Err(InvocationError::Dropped("updates channel closed")),
                    Err(_) => {}
                }
            }
        }
    }

    async fn process_socket(&mut self, updates: UpdatesLike) -> Result<(), InvocationError> {
        let (raw, users, chats, seq_start) = socket_updates(&updates).unwrap_or_default();
        check_batch(raw.len(), 0)?;
        check_peers(users.len(), chats.len())?;
        let mut sequence_gap = false;
        if seq_start != 0 {
            let state = self.boxes.session_state();
            // Let grammers skip the envelope and return its peers without
            // observing signals or history from an already applied sequence.
            if seq_start <= state.seq {
                if let Ok((_, users, chats)) = self.boxes.process_updates(updates) {
                    self.directory(&users, &chats)?;
                    self.checkpoint(state);
                }
                return Ok(());
            }
            sequence_gap = seq_start > state.seq.saturating_add(1);
        }
        let offered = socket_channel_pts(raw, &updates)?;
        self.observe_broadcasts(raw, chats, &offered).await?;
        let Some(hinted) = self.observe_signals(raw) else {
            return Ok(());
        };
        let mut common = HistoryUpdates::offered_common(raw, &updates);
        let mut rejected = if sequence_gap {
            HistoryUpdates::from_raw(raw)
        } else {
            HistoryUpdates::default()
        };
        let Ok(batch) = self.boxes.process_updates(updates) else {
            self.catch_up = CatchUp::Reconcile;
            rejected.exclude_broadcasts(&self.broadcasts);
            self.rejected_history.merge(rejected);
            self.emit_stale(hinted);
            return Ok(());
        };
        if let Some(state) = self.map_batch(batch, offered, hinted)? {
            // No account difference is in flight on the socket path. Common
            // advancing here also accounts for updates grammers reordered.
            common.retain_above(state.pts);
            self.common_gap.retain_above(state.pts);
            self.common_gap.merge(common);
            if self.catch_up == CatchUp::Idle {
                self.checkpoint(state);
            }
        }
        Ok(())
    }

    /// Mark hinted batches as replay before mapping. Emit the invalidations
    /// after their items, so the actor discards any history from that batch.
    fn observe_signals(
        &mut self,
        updates: &[tl::enums::Update],
    ) -> Option<BTreeSet<Option<ChatId>>> {
        if updates
            .iter()
            .any(|u| matches!(u, tl::enums::Update::PtsChanged))
        {
            self.abandon_difference();
            self.keep_acks(updates);
            return None;
        }
        let mut stale = BTreeSet::new();
        for id in updates.iter().filter_map(channel_hint) {
            self.possible_gaps.remove(id);
            if let Some(chat) = map::peers::channel_chat(id)
                && !self.is_ignored(chat)
            {
                stale.insert(Some(chat));
            }
        }
        Some(stale)
    }

    fn finish_catch_up(&mut self) {
        let rejected = std::mem::take(&mut self.rejected_history);
        self.emit_stale(rejected.scopes());
        if std::mem::take(&mut self.catch_up) == CatchUp::Reconcile {
            self.ready.push_back(StreamItem::CatchUpGap);
        }
        self.stale_scopes.clear();
        self.ready.push_back(StreamItem::CatchUpEnded);
        let mut state = self.boxes.session_state();
        state.channels.retain(|c| !self.broadcasts.contains(&c.id));
        self.checkpoint(state);
    }

    async fn account_difference(
        &mut self,
        mut request: tl::functions::updates::GetDifference,
    ) -> Result<(), InvocationError> {
        use tl::enums::updates::Difference;
        request.pts_limit = Some(ACCOUNT_DIFF_LIMIT);
        request.qts_limit = Some(ACCOUNT_DIFF_LIMIT);
        let reply = self.invoke(&request).await?;
        self.catch_up.start();
        let (messages, raw, users, chats, final_reply) = match &reply {
            Difference::Difference(d) => (
                d.new_messages
                    .len()
                    .saturating_add(d.new_encrypted_messages.len()),
                d.other_updates.as_slice(),
                d.users.as_slice(),
                d.chats.as_slice(),
                true,
            ),
            Difference::Slice(d) => (
                d.new_messages
                    .len()
                    .saturating_add(d.new_encrypted_messages.len()),
                d.other_updates.as_slice(),
                d.users.as_slice(),
                d.chats.as_slice(),
                false,
            ),
            Difference::Empty(_) | Difference::TooLong(_) => (0, &[][..], &[][..], &[][..], true),
        };
        check_batch(messages, raw.len())?;
        check_peers(users.len(), chats.len())?;
        let offered = offered_pts(raw);
        self.observe_broadcasts(raw, chats, &offered).await?;
        let Some(mut stale) = self.observe_signals(raw) else {
            return Ok(());
        };
        if matches!(reply, Difference::TooLong(_)) {
            stale.insert(None);
        }
        let mut history = HistoryUpdates::from_raw(raw);
        let batch = self.boxes.apply_difference(reply);
        check_batch(batch.0.len(), 0)?;
        history.account_for(&batch.0);
        stale.extend(history.scopes());
        self.rejected_history.account_for(&batch.0);
        self.common_gap.account_for(&batch.0);
        if final_reply {
            stale.extend(std::mem::take(&mut self.common_gap).scopes());
        }
        self.map_batch(batch, offered, stale)?;
        Ok(())
    }

    async fn channel_difference(
        &mut self,
        mut request: tl::functions::updates::GetChannelDifference,
    ) -> Result<(), InvocationError> {
        let tl::enums::InputChannel::Channel(channel) = &mut request.channel else {
            return Err(InvocationError::Dropped("invalid difference peer"));
        };
        let chat = map::peers::channel_chat(channel.channel_id);
        let info = self.channel_peer(channel.channel_id).await?;
        if self.broadcasts.contains(&channel.channel_id) || is_broadcast(info.as_ref()) {
            self.boxes
                .end_channel_difference(PrematureEndReason::Banned);
            self.possible_gaps.remove(channel.channel_id);
            self.tracked_broadcasts.remove(&channel.channel_id);
            return Ok(());
        }
        let Some(PeerInfo::Channel {
            auth: Some(auth), ..
        }) = info
        else {
            self.end_channel_difference(chat, PrematureEndReason::Banned);
            return Ok(());
        };
        channel.access_hash = auth.hash();
        request.limit = CHANNEL_DIFF_LIMIT;
        match self.invoke(&request).await {
            Ok(reply) => self.apply_channel_difference(chat, reply).await,
            Err(e) if e.is("CHANNEL_PRIVATE") => {
                self.end_channel_difference(chat, PrematureEndReason::Banned);
                Ok(())
            }
            Err(e)
                if e.is("PERSISTENT_TIMESTAMP_OUTDATED")
                    || matches!(&e, InvocationError::Rpc(rpc) if rpc.code == 500) =>
            {
                self.end_channel_difference(chat, PrematureEndReason::TemporaryServerIssues);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn end_channel_difference(&mut self, chat: Option<ChatId>, reason: PrematureEndReason) {
        self.boxes.end_channel_difference(reason);
        if let Some(chat) = chat {
            self.possible_gaps.remove(chat.id);
            self.emit_stale(BTreeSet::from([Some(chat)]));
        }
    }

    async fn apply_channel_difference(
        &mut self,
        chat: Option<ChatId>,
        reply: tl::enums::updates::ChannelDifference,
    ) -> Result<(), InvocationError> {
        use tl::enums::updates::ChannelDifference;
        let (raw, chats, incomplete) = match &reply {
            ChannelDifference::Difference(d) => {
                check_batch(d.new_messages.len(), d.other_updates.len())?;
                check_peers(d.users.len(), d.chats.len())?;
                // grammers may filter ordered history updates from a final body.
                let incomplete = d.r#final
                    && d.other_updates.iter().any(|u| {
                        channel_history_mutation(u).is_some()
                            || matches!(u, tl::enums::Update::NewChannelMessage(_))
                    });
                (d.other_updates.as_slice(), d.chats.as_slice(), incomplete)
            }
            ChannelDifference::TooLong(d) => {
                check_batch(d.messages.len(), 0)?;
                check_peers(d.users.len(), d.chats.len())?;
                // grammers discards the messages and panics without dialog pts.
                if !matches!(&d.dialog, tl::enums::Dialog::Dialog(dialog) if dialog.pts.is_some()) {
                    return Err(InvocationError::Dropped("malformed channel difference"));
                }
                (&[][..], d.chats.as_slice(), true)
            }
            ChannelDifference::Empty(_) => (&[][..], &[][..], false),
        };
        let offered = offered_pts(raw);
        self.observe_broadcasts(raw, chats, &offered).await?;
        let Some(mut stale) = self.observe_signals(raw) else {
            return Ok(());
        };
        self.catch_up.start();
        let lost_gap = chat.is_some_and(|chat| self.possible_gaps.remove(chat.id));
        if incomplete || lost_gap {
            stale.extend(chat.map(Some));
        }
        let batch = self.boxes.apply_channel_difference(reply);
        self.rejected_history.account_for(&batch.0);
        self.common_gap.account_for(&batch.0);
        if let Some(state) = self.map_batch(batch, offered, stale)? {
            self.common_gap.retain_above(state.pts);
        }
        Ok(())
    }

    fn emit_stale(&mut self, scopes: BTreeSet<Option<ChatId>>) {
        for scope in scopes {
            if scope.is_some_and(|chat| self.is_ignored(chat)) || self.stale_scopes.contains(&None)
            {
                continue;
            }
            let scope = if self.stale_scopes.len() == MAX_BATCH {
                None
            } else {
                scope
            };
            if scope.is_none() {
                self.stale_scopes.clear();
            }
            if self.stale_scopes.insert(scope) {
                self.catch_up = CatchUp::Reconcile;
                self.ready
                    .push_back(StreamItem::HistoryStale(StaleHistory(scope)));
            }
        }
    }

    fn is_ignored(&self, chat: ChatId) -> bool {
        chat.kind == PeerKind::Channel && self.broadcasts.contains(&chat.id)
    }

    async fn channel_peer(&self, id: i64) -> Result<Option<PeerInfo>, InvocationError> {
        let Some(peer) = map::peers::channel_chat(id).and_then(map::peer_id) else {
            return Ok(None);
        };
        self.session
            .peer(peer)
            .await
            .map_err(|e| InvocationError::Session(Box::new(e)))
    }

    async fn observe_broadcasts(
        &mut self,
        raw: &[tl::enums::Update],
        chats: &[tl::enums::Chat],
        offered: &HashMap<i64, i32>,
    ) -> Result<(), InvocationError> {
        // Retain classifications that are still in the boxes. During a
        // difference grammers may hold other channels ahead of a broadcast.
        self.broadcasts
            .retain(|id| self.tracked_broadcasts.contains(id));
        let broadcasts = chats
            .iter()
            .filter_map(broadcast_chat)
            .chain(raw.iter().filter_map(broadcast_post))
            .filter_map(map::peers::channel_chat);
        // Keep the classification even for min peers that carry no hash.
        let infos = broadcasts
            .filter_map(|chat| {
                self.broadcasts
                    .insert(chat.id)
                    .then_some(PeerInfo::Channel {
                        id: chat.id,
                        auth: None,
                        kind: Some(ChannelKind::Broadcast),
                    })
            })
            .collect::<Vec<_>>();
        if !infos.is_empty() && self.session.cache_response(&[], infos).is_err() {
            tracing::warn!("update broadcast classification cache write failed");
        }
        let candidates: BTreeSet<_> = offered
            .keys()
            .copied()
            .chain(raw.iter().filter_map(channel_hint))
            .collect();
        for id in candidates {
            if !self.broadcasts.contains(&id) && is_broadcast(self.channel_peer(id).await?.as_ref())
            {
                self.broadcasts.insert(id);
            }
        }
        self.rejected_history.exclude_broadcasts(&self.broadcasts);
        Ok(())
    }

    /// The peer directory of a batch, cached in the session.
    fn directory(
        &self,
        users: &[tl::enums::User],
        chats: &[tl::enums::Chat],
    ) -> Result<map::Directory, InvocationError> {
        check_peers(users.len(), chats.len())?;
        let dir = map::Directory::from_lists(users, chats);
        if self.session.cache_response(&[], dir.infos()).is_err() {
            tracing::warn!("update peer cache write failed");
        }
        Ok(dir)
    }

    fn map_batch(
        &mut self,
        (updates, users, chats): UpdateAndPeers,
        mut offered: HashMap<i64, i32>,
        mut invalidations: BTreeSet<Option<ChatId>>,
    ) -> Result<Option<UpdatesState>, InvocationError> {
        check_batch(updates.len(), 0)?;
        let dir = self.directory(&users, &chats)?;
        self.recency.touch(&updates);
        let state = self.boxes.session_state();
        self.possible_gaps.retain(&state.channels);
        offered.retain(|id, _| !self.broadcasts.contains(id));
        if !offered.is_empty() {
            self.possible_gaps
                .record(offered, &updates, &state.channels);
        }
        let Some(state) = self.bounded_state(state, &mut invalidations)? else {
            self.keep_acks(updates.iter().map(|(raw, _)| raw));
            return Ok(None);
        };
        invalidations.retain(|scope| scope.is_none_or(|chat| !self.is_ignored(chat)));
        if !invalidations.is_empty() {
            self.catch_up = CatchUp::Reconcile;
        }
        let item = if self.catch_up == CatchUp::Idle {
            StreamItem::Update
        } else {
            StreamItem::CatchUp
        };
        for update in updates.iter().filter_map(|(raw, _)| map::update(raw, &dir)) {
            if self.keeps_update(&update) {
                self.ready.push_back(item(update));
            }
        }
        self.emit_stale(invalidations);
        Ok(Some(state))
    }

    fn keeps_update(&self, update: &Update) -> bool {
        if update_chat(update).is_some_and(|chat| self.is_ignored(chat)) {
            return false;
        }
        // A later slice of an incomplete replay cannot seed a contiguous
        // window. Keep other effects, including send acknowledgments.
        let message = match update {
            Update::NewMessage(new) => &new.message,
            Update::Edited(message) => message,
            _ => return true,
        };
        !self.stale_scopes.contains(&None)
            && !self.stale_scopes.contains(&Some(message.thread.chat))
    }

    /// A snapshot has no `random_id` with which to correlate an unresolved send.
    fn keep_acks<'a>(&mut self, updates: impl IntoIterator<Item = &'a tl::enums::Update>) {
        let dir = map::Directory::default();
        self.ready.extend(
            updates
                .into_iter()
                .filter(|raw| matches!(raw, tl::enums::Update::MessageId(_)))
                .filter_map(|raw| map::update(raw, &dir))
                .map(StreamItem::CatchUp),
        );
    }

    /// Enforce the cumulative budget before mapping this batch or fetching
    /// another slice. grammers cannot evict channels during a difference.
    /// In that case abandon it and reconcile from a fresh baseline.
    fn bounded_state(
        &mut self,
        mut state: UpdatesState,
        invalidations: &mut BTreeSet<Option<ChatId>>,
    ) -> Result<Option<UpdatesState>, InvocationError> {
        let broadcasts: BTreeSet<_> = state
            .channels
            .iter()
            .filter(|c| self.broadcasts.contains(&c.id))
            .map(|c| c.id)
            .collect();
        self.tracked_broadcasts = forget_broadcasts(&mut self.boxes, broadcasts);
        if self.tracked_broadcasts.len() > MAX_BATCH {
            return Err(InvocationError::Dropped("oversized pending broadcast set"));
        }
        state.channels.retain(|c| !self.broadcasts.contains(&c.id));
        if state.channels.len() > MAX_TRACKED_CHANNELS {
            let victims = self.recency.trim(&mut self.boxes, &state.channels);
            for id in victims {
                if self.possible_gaps.remove(id) {
                    invalidations.extend(map::peers::channel_chat(id).map(Some));
                }
            }
            state = self.boxes.session_state();
            if state.channels.len() > MAX_TRACKED_CHANNELS {
                self.abandon_difference();
                return Ok(None);
            }
        }
        self.recency.retain(&state.channels);
        self.possible_gaps.retain(&state.channels);
        Ok(Some(state))
    }

    fn abandon_difference(&mut self) {
        self.boxes = MessageBoxes::new();
        self.recency = Recency::new();
        self.possible_gaps = PossibleGaps::default();
        self.common_gap = HistoryUpdates::default();
        self.rejected_history = HistoryUpdates::default();
        self.broadcasts.clear();
        self.tracked_broadcasts.clear();
        self.emit_stale(BTreeSet::from([None]));
    }

    /// The channels the message box tracks now.
    #[cfg(test)]
    pub(super) fn tracked_channels(&self) -> usize {
        self.boxes.session_state().channels.len()
    }

    /// The position after the preceding batch. The channel vector is bounded
    /// by `bounded_state` and shrunk to its length before it waits for core.
    fn checkpoint(&mut self, mut state: UpdatesState) {
        state.channels.shrink_to_fit();
        self.ready
            .push_back(StreamItem::Checkpoint(UpdateCheckpoint(state)));
    }
}

fn is_broadcast(info: Option<&PeerInfo>) -> bool {
    matches!(
        info,
        Some(PeerInfo::Channel {
            kind: Some(ChannelKind::Broadcast),
            ..
        })
    )
}

fn channel_hint(update: &tl::enums::Update) -> Option<i64> {
    match update {
        tl::enums::Update::ChannelTooLong(hint) => Some(hint.channel_id),
        _ => None,
    }
}

fn socket_channel_pts(
    raw: &[tl::enums::Update],
    updates: &UpdatesLike,
) -> Result<HashMap<i64, i32>, InvocationError> {
    let UpdatesLike::AffectedChannelMessages {
        message_ids,
        affected,
        channel_id,
    } = updates
    else {
        return Ok(offered_pts(raw));
    };
    check_batch(message_ids.len(), 0)?;
    Ok(map::peers::channel_chat(*channel_id)
        .map(|_| (*channel_id, affected.pts))
        .into_iter()
        .collect())
}

fn update_chat(update: &Update) -> Option<ChatId> {
    match update {
        Update::NewMessage(new) => Some(new.message.thread.chat),
        Update::Edited(message) => Some(message.thread.chat),
        Update::Deleted { chat, .. } => *chat,
        Update::ReadInbox { chat, .. }
        | Update::ReadTopicMax { chat, .. }
        | Update::TopicEdited { chat, .. }
        | Update::TopicsPinned { chat }
        | Update::ChatChanged { chat }
        | Update::NotifySettings {
            target: NotifyTarget::Peer(chat),
            ..
        } => Some(*chat),
        _ => None,
    }
}

fn channel_history_mutation(update: &tl::enums::Update) -> Option<ChatId> {
    match update {
        tl::enums::Update::DeleteChannelMessages(u) => map::peers::channel_chat(u.channel_id),
        tl::enums::Update::EditChannelMessage(u) => map::message::raw_key(&u.message)
            .map(|(chat, _)| chat)
            .filter(|chat| chat.kind == PeerKind::Channel),
        _ => None,
    }
}

fn check_batch(messages: usize, updates: usize) -> Result<(), InvocationError> {
    if messages.saturating_add(updates) > MAX_BATCH {
        Err(InvocationError::Dropped("oversized updates batch"))
    } else {
        Ok(())
    }
}

fn check_peers(users: usize, chats: usize) -> Result<(), InvocationError> {
    if users.saturating_add(chats) > map::peers::MAX_DIRECTORY {
        Err(InvocationError::Dropped("oversized update peer directory"))
    } else {
        Ok(())
    }
}

type SocketBatch<'a> = (
    &'a [tl::enums::Update],
    &'a [tl::enums::User],
    &'a [tl::enums::Chat],
    i32,
);

fn socket_updates(updates: &UpdatesLike) -> Option<SocketBatch<'_>> {
    use tl::enums::Updates;
    let raw = match updates {
        UpdatesLike::Updates(raw) => raw,
        UpdatesLike::InvitedUsers(tl::types::messages::InvitedUsers { updates, .. })
        | UpdatesLike::ChatInviteJoinResult(tl::types::messages::ChatInviteJoinResultOk {
            updates,
            ..
        }) => updates,
        _ => return None,
    };
    Some(match raw {
        Updates::Updates(raw) => (&raw.updates, &raw.users, &raw.chats, raw.seq),
        Updates::Combined(raw) => (&raw.updates, &raw.users, &raw.chats, raw.seq_start),
        Updates::UpdateShort(raw) => (std::slice::from_ref(&raw.update), &[], &[], 0),
        _ => return None,
    })
}

impl<I: Invoke> Source for Feed<I> {
    type Raw = StreamItem;

    fn pull(mut self) -> Pull<Self> {
        Box::pin(async move {
            let result = self.next().await;
            (self, result)
        })
    }

    fn map(raw: Self::Raw) -> Option<StreamItem> {
        Some(raw)
    }
}
