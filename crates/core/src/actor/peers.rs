//! The peers core still needs (`Client::retain_peers`). Core owns this set,
//! so the backend never has to guess which access hashes to keep pinned:
//! pending sends first, then the windowed chats (most recent first), then the
//! retained dialogs in list order; deduplicated and bounded by
//! [`MAX_PINNED_PEERS`].
//!
//! Coalesced: recomputed at most once per actor loop iteration, only when the
//! sends, windows or dialogs changed membership (their revisions) or the
//! instance changed, and handed to the instance only when the set differs.

use std::collections::HashSet;

use super::Actor;
use crate::api::ClientGen;
use crate::backend::{Backend, Client};
use crate::model::ChatId;
use crate::session::MAX_PINNED_PEERS;

/// What the last computed set was derived from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Inputs {
    cgen: ClientGen,
    sends: u64,
    windows: u64,
    dialogs: u64,
}

#[derive(Debug, Default)]
pub(super) struct PeerSync {
    inputs: Option<Inputs>,
    /// The set last handed to instance `inputs.cgen`.
    last: Vec<ChatId>,
}

/// Orders and bounds the required set.
fn required(
    sends: Vec<ChatId>,
    windows: impl Iterator<Item = ChatId>,
    dialogs: impl Iterator<Item = ChatId>,
) -> Vec<ChatId> {
    let mut seen = HashSet::with_capacity(MAX_PINNED_PEERS);
    let mut out = Vec::with_capacity(MAX_PINNED_PEERS);
    for chat in sends.into_iter().chain(windows).chain(dialogs) {
        if out.len() >= MAX_PINNED_PEERS {
            break;
        }
        if seen.insert(chat) {
            out.push(chat);
        }
    }
    out
}

impl<B: Backend> Actor<B> {
    /// Hands the live instance the current required peer set if it changed.
    pub(super) fn sync_peers(&mut self) {
        let Some(live) = self.live.as_ref() else {
            return;
        };
        let inputs = Inputs {
            cgen: self.cgen,
            sends: self.sends.rev(),
            windows: self.windows.rev(),
            dialogs: self.dialogs.rev(),
        };
        if self.peers.inputs == Some(inputs) {
            return;
        }
        let new_instance = self.peers.inputs.is_none_or(|i| i.cgen != inputs.cgen);
        self.peers.inputs = Some(inputs);
        let mut windows: Vec<ChatId> = self.windows.threads().map(|t| t.chat).collect();
        windows.reverse();
        let set = required(
            self.sends.chats(),
            windows.into_iter(),
            self.dialogs.as_slice().iter().map(|c| c.id),
        );
        if new_instance || set != self.peers.last {
            live.client.retain_peers(&set);
            self.peers.last = set;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PeerKind;

    fn c(id: i64) -> ChatId {
        ChatId {
            kind: PeerKind::User,
            id,
        }
    }

    #[test]
    fn sends_then_windows_then_dialogs_deduplicated_and_bounded() {
        let dialogs = (1..=2000).map(c);
        let set = required(vec![c(5000), c(3)], [c(4000), c(3)].into_iter(), dialogs);
        assert_eq!(set.len(), MAX_PINNED_PEERS);
        assert_eq!(&set[..4], &[c(5000), c(3), c(4000), c(1)]);
        assert_eq!(set.iter().collect::<HashSet<_>>().len(), set.len());
    }
}
