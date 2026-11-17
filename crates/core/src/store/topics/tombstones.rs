//! Tombstones of removed topics (plan section 6.4): a topic removed by a fresh
//! by-id `deleted` dispatched at `S` is never re-inserted by a result
//! dispatched before `S`.

use super::stamp::FetchSeq;
use crate::model::TopicId;

/// Most tombstones per list.
pub const MAX_TOMBSTONES: usize = 64;

#[derive(Debug, Default)]
pub(super) struct Tombstones(Vec<(TopicId, FetchSeq)>);

impl Tombstones {
    /// Whether a result dispatched at `seq` may not insert `id`.
    pub(super) fn blocks(&self, id: TopicId, seq: FetchSeq) -> bool {
        self.0.iter().any(|&(t, s)| t == id && seq < s)
    }

    /// Records a removal. The caller made room with [`Tombstones::oldest`]
    /// and [`Tombstones::prune`] when [`Tombstones::is_full`].
    pub(super) fn record(&mut self, id: TopicId, seq: FetchSeq) {
        self.0.retain(|&(t, _)| t != id);
        self.0.push((id, seq));
    }

    pub(super) fn is_full(&self) -> bool {
        self.0.len() >= MAX_TOMBSTONES
    }

    pub(super) fn oldest(&self) -> Option<FetchSeq> {
        self.0.iter().map(|&(_, s)| s).min()
    }

    /// Drops every tombstone no in-flight fetch is older than: nothing
    /// left can re-insert its topic.
    pub(super) fn prune(&mut self, oldest_in_flight: Option<FetchSeq>) {
        self.0
            .retain(|&(_, s)| oldest_in_flight.is_some_and(|f| f < s));
    }

    /// A newer result found the topic again: the server is authoritative.
    pub(super) fn forget(&mut self, id: TopicId) {
        self.0.retain(|&(t, _)| t != id);
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.0.len()
    }

    #[cfg(test)]
    pub(super) fn capacity(&self) -> usize {
        self.0.capacity()
    }
}
