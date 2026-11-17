//! Per-field accepted stamps of the dialog rows: one ordering rule for
//! every dialog snapshot (dialogs pages, `peer_dialogs` results, kind
//! checks, cached chats) and every live change, the way a topic list keeps
//! per-group stamps (plan sections 6.4, 6.6 and 7; folders plan, part A).
//!
//! - **The clock** is the badge clock: it advances at every dialogs and
//!   `peer_dialogs` dispatch, so a snapshot's stamp is when it was sent.
//!   Cached chats are the oldest source (stamp zero).
//! - **Snapshots.** A field (kind, folder, unread) takes a snapshot's value
//!   only if the snapshot was dispatched after the field's accepted stamp;
//!   then the stamp moves to the snapshot's. Otherwise the current value
//!   is kept, whatever the source: an older dialogs page can't revert what
//!   a newer `peer_dialogs` result set, nor the reverse.
//! - **Live changes** (a folder move, a read, a counted message) stamp the
//!   field with the clock's value now: a snapshot dispatched before them
//!   can't revert them, one dispatched after them is authoritative.
//! - **Kind while fenced** is held at what core knew before the fence; the
//!   settled kind is committed at its statement's stamp
//!   ([`super::Dialogs::commit_kind`]).
//! - **Chats not listed.** A stamp is only ever kept with the value it
//!   vouches for: for a chat that isn't listed (a live move, a
//!   `peer_dialogs` result about a chat with a window only), the accepted
//!   values are remembered with their stamps until a page admits the chat.
//! - **Bounds.** Stamps are kept for listed rows (pruned with them) and
//!   records for at most [`MAX_UNLISTED`] chats that aren't listed;
//!   evicting one discards every response that could still need it.

use std::collections::{HashMap, HashSet};

use crate::model::{ChatId, ChatKind, ChatSummary, PeerFolder};
use crate::store::{BadgeSeq, ForumStatus, with_status};

/// Live moves of unlisted chats remembered for the page that lists them.
pub(super) const MAX_UNLISTED: usize = super::MAX_DIALOGS;

/// What [`super::Dialogs::guard`] made of a dialog snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Guarded {
    /// Apply it: no folder it states lost to a newer one.
    Apply,
    /// Apply it (newer fields kept over it), then fetch the first page
    /// again: it disagreed with a newer folder, perhaps a live move the
    /// server may not have taken.
    ApplyAndRefetch,
    /// Drop it and fetch the first page again: it was dispatched before an
    /// unlisted chat's live move whose record was evicted.
    Discard,
}

/// The current values of a listed row's guarded fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Current {
    pub(super) kind: ChatKind,
    pub(super) folder: PeerFolder,
    pub(super) unread: u32,
}

/// One chat's accepted stamps.
#[derive(Clone, Copy, Debug, Default)]
struct Accepted {
    kind: BadgeSeq,
    folder: BadgeSeq,
    unread: BadgeSeq,
    /// For a chat that isn't listed: the values its stamps vouch for, until
    /// a page admits it as a row.
    unlisted: Option<Unlisted>,
}

/// The accepted values of a chat that isn't listed (each `None` until a
/// source sets it), kept with their stamps: a stamp is only ever kept with
/// the value it vouches for.
#[derive(Clone, Copy, Debug)]
struct Unlisted {
    kind: Option<ChatKind>,
    folder: Option<PeerFolder>,
    unread: Option<u32>,
    /// For eviction, oldest first.
    order: u64,
}

impl Accepted {
    /// The newest stamp it holds (an eviction's cutoff).
    fn newest(&self) -> BadgeSeq {
        self.kind.max(self.folder).max(self.unread)
    }
}

#[derive(Debug, Default)]
pub(super) struct Fields {
    rows: HashMap<ChatId, Accepted>,
    order: u64,
    /// Snapshots dispatched at or before this may predate an evicted
    /// unlisted record: they are discarded.
    evicted_through: Option<BadgeSeq>,
}

impl Fields {
    /// Guards a snapshot dispatched at `seq` field by field (see the module
    /// docs). `held` names the pre-fence status of a fenced chat, whose kind
    /// no snapshot sets; `current` gives a listed row's values. For a chat
    /// that isn't listed, what the snapshot sets is remembered with its
    /// stamp, and what it can't set is taken from what was remembered.
    pub(super) fn guard(
        &mut self,
        page: &mut [ChatSummary],
        seq: BadgeSeq,
        held: impl Fn(ChatId) -> Option<ForumStatus>,
        current: impl Fn(ChatId) -> Option<Current>,
    ) -> Guarded {
        if self.evicted_through.is_some_and(|e| seq <= e) {
            return Guarded::Discard;
        }
        // Room for every record this snapshot starts, made before any of it
        // is guarded, and never at the cost of a chat it holds: an eviction
        // can't take a record this snapshot still has to be checked against.
        let held_here: Vec<ChatId> = page.iter().map(|c| c.id).collect();
        let starts = page
            .iter()
            .filter(|c| current(c.id).is_none() && !self.has_record(c.id))
            .count();
        self.make_room(starts, &held_here);
        let mut conflict = false;
        for chat in page {
            let now = current(chat.id);
            if now.is_none() {
                self.remember(chat.id);
            }
            let row = self.rows.entry(chat.id).or_default();
            let kept = row.unlisted;
            // What this snapshot sets, for an unlisted chat's record.
            let (mut kind, mut folder, mut unread) = (None, None, None);
            match held(chat.id) {
                Some(before) => chat.kind = with_status(chat.kind, before),
                None if seq > row.kind => {
                    row.kind = seq;
                    kind = Some(chat.kind);
                }
                None => {
                    if let Some(newer) = now.map(|c| c.kind).or(kept.and_then(|u| u.kind)) {
                        chat.kind = newer;
                    }
                }
            }
            if seq > row.folder {
                row.folder = seq;
                folder = Some(chat.folder);
            } else if let Some(newer) = now.map(|c| c.folder).or(kept.and_then(|u| u.folder)) {
                conflict |= chat.folder != newer;
                chat.folder = newer;
            }
            if seq > row.unread {
                row.unread = seq;
                unread = Some(chat.unread);
            } else if let Some(newer) = now.map(|c| c.unread).or(kept.and_then(|u| u.unread)) {
                chat.unread = newer;
            }
            if let Some(kept) = row.unlisted.as_mut() {
                kept.kind = kind.or(kept.kind);
                kept.folder = folder.or(kept.folder);
                kept.unread = unread.or(kept.unread);
            }
        }
        if conflict {
            Guarded::ApplyAndRefetch
        } else {
            Guarded::Apply
        }
    }

    /// A live folder move at clock `now`. `unlisted` carries the folder of
    /// a chat that isn't listed: remembered (bounded) for the page that
    /// lists it.
    pub(super) fn moved(&mut self, chat: ChatId, now: BadgeSeq, unlisted: Option<PeerFolder>) {
        if unlisted.is_some() && !self.has_record(chat) {
            self.make_room(1, &[chat]);
            self.remember(chat);
        }
        let row = self.rows.entry(chat).or_default();
        row.folder = row.folder.max(now);
        if let (Some(kept), Some(folder)) = (row.unlisted.as_mut(), unlisted) {
            kept.folder = Some(folder);
        }
    }

    fn has_record(&self, chat: ChatId) -> bool {
        self.rows.get(&chat).is_some_and(|r| r.unlisted.is_some())
    }

    /// Starts (or keeps) the record of a chat that isn't listed. The caller
    /// made room for it ([`Self::make_room`]).
    fn remember(&mut self, chat: ChatId) {
        if self.has_record(chat) {
            return;
        }
        self.order = self.order.wrapping_add(1);
        let order = self.order;
        self.rows.entry(chat).or_default().unlisted = Some(Unlisted {
            kind: None,
            folder: None,
            unread: None,
            order,
        });
    }

    /// A live change of the unread count at clock `now`.
    pub(super) fn unread_changed(&mut self, chat: ChatId, now: BadgeSeq) {
        let row = self.rows.entry(chat).or_default();
        row.unread = row.unread.max(now);
    }

    /// A settled kind, stated by a source dispatched at `seq`. For a chat
    /// that isn't listed (`unlisted` carries the kind) it is remembered for
    /// the page that lists it.
    pub(super) fn kind_committed(
        &mut self,
        chat: ChatId,
        seq: BadgeSeq,
        unlisted: Option<ChatKind>,
    ) {
        if unlisted.is_some() && !self.has_record(chat) {
            self.make_room(1, &[chat]);
            self.remember(chat);
        }
        let row = self.rows.entry(chat).or_default();
        if seq >= row.kind
            && let (Some(kept), Some(kind)) = (row.unlisted.as_mut(), unlisted)
        {
            kept.kind = Some(kind);
        }
        row.kind = row.kind.max(seq);
    }

    /// Keeps the stamps of listed rows (whose values the rows now hold)
    /// and the records of chats that aren't listed.
    pub(super) fn prune(&mut self, listed: &HashSet<ChatId>) {
        self.rows.retain(|c, r| {
            if listed.contains(c) {
                r.unlisted = None;
                true
            } else {
                r.unlisted.is_some()
            }
        });
        debug_assert!(self.rows.len() <= super::MAX_DIALOGS + MAX_UNLISTED);
    }

    /// Makes room for `n` more unlisted records: past the cap the oldest
    /// go (never one of `keep`), and every response dispatched at or before
    /// an evicted record's newest stamp is discarded.
    fn make_room(&mut self, n: usize, keep: &[ChatId]) {
        let unlisted = self.rows.values().filter(|r| r.unlisted.is_some()).count();
        let excess = (unlisted + n).saturating_sub(MAX_UNLISTED);
        if excess == 0 {
            return;
        }
        let mut oldest: Vec<(u64, ChatId)> = self
            .rows
            .iter()
            .filter(|(c, _)| !keep.contains(c))
            .filter_map(|(c, r)| r.unlisted.map(|u| (u.order, *c)))
            .collect();
        oldest.sort_unstable();
        for (_, chat) in oldest.into_iter().take(excess) {
            if let Some(evicted) = self.rows.remove(&chat) {
                let newest = evicted.newest();
                self.evicted_through = Some(self.evicted_through.map_or(newest, |e| e.max(newest)));
            }
        }
    }

    /// Stamps kept, for the bound test.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }
}
