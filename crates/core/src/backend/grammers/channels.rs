//! Bounds the channels grammers' message box tracks (stage 3 review A15).
//!
//! `MessageBoxes` adds an entry for every distinct channel that sends an
//! update and never removes one, and its only view of them,
//! `session_state()`, collects them all. The feed bounds both from outside
//! (grammers stays unpatched):
//!
//! - [`Recency`] remembers when each channel last produced an update;
//! - past [`MAX_TRACKED_CHANNELS`], the least recently active channels are
//!   forgotten, through the one removal grammers exposes: a channel
//!   difference that ends as `Banned` drops the entry.
//!
//! **Policy for an evicted channel.** It is treated as if it were missing
//! from the persisted checkpoint, which has been the rule since the cap was
//! introduced: its next socket update starts a fresh entry (applied without
//! gap detection) and anything missed while untracked is repaired by the
//! next dialogs snapshot (every full bootstrap, every light refresh). A
//! pending out-of-order buffer is dropped with it, so the feed invalidates
//! the victim when it has evidence of an unresolved gap.

use std::collections::{BTreeSet, HashMap};

use grammers_client::tl;
use grammers_session::types::ChannelState;
use grammers_session::updates::{MessageBox, MessageBoxes, PrematureEndReason, State, UpdatesLike};

use super::map::{message::raw_key, peers::channel_chat};
use crate::model::PeerKind;

/// Channels the message box keeps. Also the capacity of every checkpoint.
pub(super) const MAX_TRACKED_CHANNELS: usize = crate::store::MAX_DIALOGS;

/// The highest unaccounted-for socket pts in each tracked channel.
#[derive(Default)]
pub(super) struct PossibleGaps(HashMap<i64, i32>);

impl PossibleGaps {
    pub(super) fn record(
        &mut self,
        mut offered: HashMap<i64, i32>,
        applied: &[(tl::enums::Update, State)],
        channels: &[ChannelState],
    ) {
        for (_, state) in applied {
            if let Some(MessageBox::Channel { channel_id, pts }) = state.message_box
                && offered
                    .get(&channel_id)
                    .is_some_and(|&target| pts >= target)
            {
                offered.remove(&channel_id);
            }
        }
        for channel in channels {
            if let Some(&target) = offered.get(&channel.id)
                && target > channel.pts
            {
                record_max(&mut self.0, channel.id, target);
            }
        }
    }

    pub(super) fn retain(&mut self, channels: &[ChannelState]) {
        if !self.0.is_empty() {
            self.0 = channels
                .iter()
                .filter_map(|c| {
                    let &target = self.0.get(&c.id)?;
                    (target > c.pts).then_some((c.id, target))
                })
                .collect();
        }
    }

    pub(super) fn remove(&mut self, id: i64) -> bool {
        self.0.remove(&id).is_some()
    }
}

pub(super) fn offered_pts(updates: &[tl::enums::Update]) -> HashMap<i64, i32> {
    let mut offered = HashMap::new();
    for update in updates {
        if let Some((id, pts)) = ordered_pts(update) {
            record_max(&mut offered, id, pts);
        }
    }
    offered
}

fn record_max(points: &mut HashMap<i64, i32>, id: i64, pts: i32) {
    let point = points.entry(id).or_insert(pts);
    *point = (*point).max(pts);
}

/// The socket constructors with ordered channel pts in the pinned grammers.
fn ordered_pts(update: &tl::enums::Update) -> Option<(i64, i32)> {
    use tl::enums::Update;
    let message_pts = |message, pts| channel_id(message).map(|id| (id, pts));
    match update {
        Update::NewChannelMessage(u) => message_pts(&u.message, u.pts),
        Update::EditChannelMessage(u) => message_pts(&u.message, u.pts),
        Update::DeleteChannelMessages(u) => Some((u.channel_id, u.pts)),
        Update::ReadChannelInbox(u) => Some((u.channel_id, u.pts)),
        Update::ChannelWebPage(u) => Some((u.channel_id, u.pts)),
        Update::PinnedChannelMessages(u) => Some((u.channel_id, u.pts)),
        _ => None,
    }
    .filter(|&(id, _)| channel_chat(id).is_some())
}

/// When each channel last produced an update, by a counter of batches.
pub(super) struct Recency {
    seen: HashMap<i64, u64>,
    tick: u64,
}

impl Recency {
    pub(super) fn new() -> Self {
        Self {
            seen: HashMap::with_capacity(MAX_TRACKED_CHANNELS),
            tick: 0,
        }
    }

    /// Notes the channels a processed batch touched. The map is pruned to the
    /// tracked set at batch boundaries, so it holds at most the tracked
    /// channels plus one batch's.
    pub(super) fn touch(&mut self, batch: &[(tl::enums::Update, State)]) {
        self.tick = self.tick.saturating_add(1);
        for (_, state) in batch {
            if let Some(MessageBox::Channel { channel_id, .. }) = state.message_box {
                self.seen.insert(channel_id, self.tick);
            }
        }
    }

    /// Forgets the least recently active channels of `channels` beyond the
    /// cap, if grammers is idle (no difference pending, so removing an entry
    /// cannot meet a request in flight).
    pub(super) fn trim(&self, boxes: &mut MessageBoxes, channels: &[ChannelState]) -> Vec<i64> {
        if boxes.get_difference().is_some() {
            return Vec::new();
        }
        forget_idle(boxes, self.victims(channels))
    }

    /// Drops what is remembered of channels that are no longer tracked.
    pub(super) fn retain(&mut self, channels: &[ChannelState]) {
        if self.seen.len() <= MAX_TRACKED_CHANNELS {
            return;
        }
        let mut tracked: Vec<i64> = channels.iter().map(|c| c.id).collect();
        tracked.sort_unstable();
        self.seen.retain(|id, _| tracked.binary_search(id).is_ok());
    }

    /// The channels beyond the cap, least recently active first. A channel
    /// never touched here (loaded from the checkpoint) counts as the oldest.
    fn victims(&self, channels: &[ChannelState]) -> Vec<i64> {
        let excess = channels.len().saturating_sub(MAX_TRACKED_CHANNELS);
        if excess == 0 {
            return Vec::new();
        }
        let mut by_age: Vec<(u64, i64)> = channels
            .iter()
            .map(|c| (self.seen.get(&c.id).copied().unwrap_or(0), c.id))
            .collect();
        by_age.sort_unstable();
        by_age.into_iter().take(excess).map(|(_, id)| id).collect()
    }
}

/// Retire pending broadcast differences in grammers' order. Remaining
/// broadcasts can be forgotten once no supported channel is getting a reply.
pub(super) fn forget_broadcasts(boxes: &mut MessageBoxes, mut ids: BTreeSet<i64>) -> BTreeSet<i64> {
    while let Some(request) = boxes.get_channel_difference()
        && let tl::enums::InputChannel::Channel(channel) = request.channel
        && ids.remove(&channel.channel_id)
    {
        boxes.end_channel_difference(PrematureEndReason::Banned);
    }
    for id in forget_idle(boxes, ids.iter().copied()) {
        ids.remove(&id);
    }
    ids
}

/// Forgets the requested entries when no channel difference has priority.
fn forget_idle(boxes: &mut MessageBoxes, channels: impl IntoIterator<Item = i64>) -> Vec<i64> {
    if boxes.get_channel_difference().is_some() {
        return Vec::new();
    }
    channels
        .into_iter()
        .take_while(|&id| forget(boxes, id))
        .collect()
}

pub(super) fn broadcast_chat(raw: &tl::enums::Chat) -> Option<i64> {
    match raw {
        tl::enums::Chat::Channel(c) if c.broadcast => Some(c.id),
        tl::enums::Chat::ChannelForbidden(c) if c.broadcast => Some(c.id),
        _ => None,
    }
}

/// grammers identifies `post` as a broadcast-channel message, even when
/// the envelope does not include its peer in the directory.
pub(super) fn broadcast_post(update: &tl::enums::Update) -> Option<i64> {
    let raw = match update {
        tl::enums::Update::NewChannelMessage(u) => &u.message,
        tl::enums::Update::EditChannelMessage(u) => &u.message,
        _ => return None,
    };
    match raw {
        tl::enums::Message::Message(m) if m.post => channel_id(raw),
        tl::enums::Message::Service(m) if m.post => channel_id(raw),
        _ => None,
    }
}

fn channel_id(raw: &tl::enums::Message) -> Option<i64> {
    let (chat, _) = raw_key(raw)?;
    (chat.kind == PeerKind::Channel).then_some(chat.id)
}

/// Removes `id`'s entry: `updateChannelTooLong` without a pts starts its
/// difference, and ending that difference as banned pops the entry
/// (`MessageBoxes::end_channel_difference`). Nothing is requested. Only valid
/// while no difference is pending.
fn forget(boxes: &mut MessageBoxes, id: i64) -> bool {
    let gap = tl::types::UpdateChannelTooLong {
        channel_id: id,
        pts: None,
    };
    let updates = tl::types::Updates {
        updates: vec![gap.into()],
        users: Vec::new(),
        chats: Vec::new(),
        date: 0,
        seq: 0,
    };
    if boxes
        .process_updates(UpdatesLike::Updates(updates.into()))
        .is_err()
    {
        return false;
    }
    let ours = boxes.get_channel_difference().is_some_and(|request| {
        matches!(&request.channel, tl::enums::InputChannel::Channel(c) if c.channel_id == id)
    });
    if ours {
        boxes.end_channel_difference(PrematureEndReason::Banned);
    }
    ours
}
