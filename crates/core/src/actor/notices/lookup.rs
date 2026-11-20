//! Peer mute evidence and bounded notice candidates waiting for a setting.

use std::collections::{BTreeMap, HashMap, VecDeque};

use super::{Contributor, MAX_NOTICE_MESSAGES};
use crate::model::{ChatId, MsgId, ThreadKey, TopicId};
use crate::store::{BadgeSeq, MAX_DIALOGS, PeerMute};
use tokio::time::Instant;

/// A burst of unknown peers must not fill the job queue or consume all permits.
const MAX_LOOKUP_PEERS: usize = 32;

/// Listed and unlisted dialog records may retain stale mutes across startup,
/// gaps and rebuilds. Confirmations begin empty and are cleared whenever the
/// actor may have missed a settings update.
#[derive(Debug, Default)]
pub(super) struct MuteEvidence {
    cutoff: BadgeSeq,
    verified: HashMap<ChatId, Option<i64>>,
    order: VecDeque<ChatId>,
}

impl MuteEvidence {
    pub(super) fn invalidate(&mut self, cutoff: BadgeSeq) {
        self.cutoff = cutoff;
        self.verified.clear();
        self.order.clear();
    }

    pub(super) fn accepts(&self, stamp: BadgeSeq) -> bool {
        stamp > self.cutoff
    }

    pub(super) fn confirm(&mut self, chat: ChatId, until: Option<i64>) {
        if self.verified.insert(chat, until).is_some() {
            return;
        }
        // Room for listed and unlisted peers. Evicted peers need a fresh
        // read before their next ordinary notice.
        if self.order.len() >= 2 * MAX_DIALOGS
            && let Some(oldest) = self.order.pop_front()
        {
            self.verified.remove(&oldest);
        }
        self.order.push_back(chat);
    }

    pub(super) fn resolve(&self, chat: ChatId, mute: Option<PeerMute>) -> Option<PeerMute> {
        // A dialog record can be dropped independently of its confirmation.
        // Only trust a retained value that matches the evidence.
        mute.filter(|mute| self.verified.get(&chat) == Some(&mute.until()))
    }
}

#[derive(Debug)]
pub(super) struct Deferred {
    /// Arrival, moved forward if a banner for this chat was delivered while
    /// this live candidate waited for its lookup.
    pub(super) at: Instant,
    pub(super) contributor: Contributor,
}

#[derive(Debug, Default)]
pub(super) struct Lookups {
    in_flight: bool,
    peers: VecDeque<(ChatId, BTreeMap<MsgId, Deferred>)>,
    count: usize,
}

impl Lookups {
    /// Overflow drops the ordinary candidate without releasing a notice.
    pub(super) fn push(&mut self, chat: ChatId, id: MsgId, candidate: Deferred) {
        if self.count >= MAX_NOTICE_MESSAGES {
            return;
        }
        if let Some((_, entries)) = self.peers.iter_mut().find(|(peer, _)| *peer == chat) {
            self.count += usize::from(entries.insert(id, candidate).is_none());
        } else if self.peers.len() < MAX_LOOKUP_PEERS {
            self.peers
                .push_back((chat, BTreeMap::from([(id, candidate)])));
            self.count += 1;
        }
    }

    pub(super) fn contains(&self, chat: ChatId, id: MsgId) -> bool {
        self.peers
            .iter()
            .any(|(peer, entries)| *peer == chat && entries.contains_key(&id))
    }

    pub(super) fn newest_contributor(&self, thread: ThreadKey) -> Option<MsgId> {
        let (_, entries) = self.peers.iter().find(|(chat, _)| *chat == thread.chat)?;
        entries
            .iter()
            .rev()
            .find_map(|(&id, entry)| (entry.contributor.topic == thread.topic).then_some(id))
    }

    pub(super) fn start(&mut self) -> Option<ChatId> {
        if self.in_flight {
            return None;
        }
        let (chat, _) = self.peers.front()?;
        self.in_flight = true;
        Some(*chat)
    }

    pub(super) fn retire(&mut self) {
        self.in_flight = false;
    }

    pub(super) fn finish(&mut self, chat: ChatId) -> BTreeMap<MsgId, Deferred> {
        self.in_flight = false;
        let entries = self
            .peers
            .iter()
            .position(|(peer, _)| *peer == chat)
            .and_then(|pos| self.peers.remove(pos))
            .map_or_else(BTreeMap::new, |(_, entries)| entries);
        self.count -= entries.len();
        entries
    }

    pub(super) fn after_delivery(&mut self, chat: ChatId, now: Instant) {
        if let Some((_, entries)) = self.peers.iter_mut().find(|(peer, _)| *peer == chat) {
            for entry in entries.values_mut() {
                entry.at = entry.at.max(now);
            }
        }
    }

    pub(super) fn discard(
        &mut self,
        covers: impl Fn(ChatId) -> bool,
        discard: impl Fn(Option<TopicId>, MsgId) -> bool,
    ) {
        let mut count = self.count;
        self.peers.retain_mut(|(chat, entries)| {
            if covers(*chat) {
                count -= entries.len();
                entries.retain(|&id, entry| !discard(entry.contributor.topic, id));
                count += entries.len();
            }
            !entries.is_empty()
        });
        self.count = count;
    }
}
