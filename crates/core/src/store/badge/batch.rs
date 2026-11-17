//! The account's `peer_dialogs` scheduler (plan section 7): one call in
//! flight, calls spaced, batches split into reserved slots for kind checks,
//! the shown forum's badge and the badge cursor, unused slots spilling over.

use std::ops::Bound;
use std::time::{Duration, Instant};

use super::{BadgeSeq, Badges};
use crate::model::ChatId;
use crate::store::NotBefore;

/// Most chats per `peer_dialogs` call.
pub const MAX_PEER_DIALOGS: usize = 20;
/// Slots reserved for kind checks (plan section 6.6).
pub const KIND_SLOTS: usize = 10;
/// The slot reserved for the shown forum's badge.
pub const SHOWN_SLOTS: usize = 1;
/// Slots reserved for the badge cursor.
pub const CURSOR_SLOTS: usize = MAX_PEER_DIALOGS - KIND_SLOTS - SHOWN_SLOTS;
/// Least time between `peer_dialogs` calls, account-wide.
pub const PEER_CALL_INTERVAL: Duration = Duration::from_secs(3);
/// Least time between two refreshes of one chat's badge.
pub const PEER_REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// One `peer_dialogs` call reserved: its slots are taken, and it is
/// [`PeerScheduler::dispatched`] when it gets its admission permit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reserved {
    /// How many chats it took from the head of the kind-check FIFO.
    pub kind_checks: usize,
    /// Every chat in the call, without duplicates.
    pub chats: Vec<ChatId>,
}

/// One `peer_dialogs` call reserved and dispatched at once (tests).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerBatch {
    /// Its dispatch stamp on the badge clock.
    pub stamp: BadgeSeq,
    /// How many chats it took from the head of the kind-check FIFO.
    pub kind_checks: usize,
    /// Every chat in the call, without duplicates.
    pub chats: Vec<ChatId>,
}

/// The one call the scheduler has out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flight {
    /// Planned and queued, waiting for admission.
    Reserved,
    /// Admitted at the badge stamp it carries.
    Dispatched(BadgeSeq),
}

#[derive(Debug, Default)]
pub struct PeerScheduler {
    in_flight: Option<Flight>,
    last_call: Option<Instant>,
    not_before: NotBefore,
    /// The last chat the badge cursor took.
    cursor: Option<ChatId>,
}

impl PeerScheduler {
    /// Reserves and dispatches a call at once (what [`Self::reserve`] and
    /// [`Self::dispatched`] do when admission is immediate).
    #[cfg(test)]
    pub fn plan(
        &mut self,
        now: Instant,
        kinds: &[ChatId],
        shown: Option<ChatId>,
        badges: &mut Badges,
    ) -> Option<PeerBatch> {
        let Reserved { kind_checks, chats } = self.reserve(now, kinds, shown, badges)?;
        let stamp = self.dispatched(now, &chats, badges);
        Some(PeerBatch {
            stamp,
            kind_checks,
            chats,
        })
    }

    /// The call to reserve now, if any: none while one is reserved or in
    /// flight, within [`PEER_CALL_INTERVAL`] of the last dispatch, or before
    /// a flood-wait deadline. `kinds` is the kind-check FIFO; the caller pops
    /// `kind_checks` from it. The interval, the badge stamp and the chats'
    /// refresh times count from [`Self::dispatched`], not from here.
    pub fn reserve(
        &mut self,
        now: Instant,
        kinds: &[ChatId],
        shown: Option<ChatId>,
        badges: &Badges,
    ) -> Option<Reserved> {
        let spaced = self
            .last_call
            .is_none_or(|at| now.saturating_duration_since(at) >= PEER_CALL_INTERVAL);
        if self.in_flight.is_some() || !spaced || !self.not_before.allows(now) {
            return None;
        }
        let mut chats = Vec::with_capacity(MAX_PEER_DIALOGS);
        let mut kind_checks = take_kinds(kinds, 0, KIND_SLOTS, &mut chats);
        if let Some(shown) = shown
            && badges.wants(shown, now)
            && !chats.contains(&shown)
        {
            chats.push(shown);
        }
        self.take_cursor(now, shown, CURSOR_SLOTS, badges, &mut chats);
        let room = MAX_PEER_DIALOGS - chats.len();
        kind_checks = take_kinds(kinds, kind_checks, room, &mut chats);
        let room = MAX_PEER_DIALOGS - chats.len();
        self.take_cursor(now, shown, room, badges, &mut chats);
        if chats.is_empty() {
            return None;
        }
        self.in_flight = Some(Flight::Reserved);
        Some(Reserved { kind_checks, chats })
    }

    /// The reserved call got its admission permit at `now`: it is stamped
    /// on the badge clock, the interval counts from here, and its chats
    /// were last sent now.
    pub fn dispatched(&mut self, now: Instant, chats: &[ChatId], badges: &mut Badges) -> BadgeSeq {
        let stamp = badges.dispatch();
        for chat in chats {
            badges.sent(*chat, now);
        }
        self.in_flight = Some(Flight::Dispatched(stamp));
        self.last_call = Some(now);
        stamp
    }

    /// When the next call may be dispatched: `None` while one is in
    /// flight (its completion comes first), else the later of the call
    /// interval's end and the flood-wait deadline (`now` if neither holds
    /// it back).
    pub fn ready_at(&self, now: Instant) -> Option<Instant> {
        if self.in_flight.is_some() {
            return None;
        }
        let spaced = self.last_call.map_or(now, |at| at + PEER_CALL_INTERVAL);
        Some(self.not_before.deadline().map_or(spaced, |d| d.max(spaced)))
    }

    /// The call stamped `stamp` ended (with a result or an error).
    pub fn complete(&mut self, stamp: BadgeSeq) {
        if self.in_flight == Some(Flight::Dispatched(stamp)) {
            self.in_flight = None;
        }
    }

    /// The call reserved or in flight was dropped (a rebuild, a purged
    /// job).
    pub fn drop_in_flight(&mut self) {
        self.in_flight = None;
    }

    /// A `FLOOD_WAIT` until `until`: no call before it. Only ever extends.
    pub fn flood(&mut self, until: Instant) {
        self.not_before.extend(until);
    }

    pub fn not_before(&self) -> NotBefore {
        self.not_before
    }

    /// Up to `room` dirty, eligible chats other than the shown forum, in
    /// ascending order after the cursor, wrapping around.
    fn take_cursor(
        &mut self,
        now: Instant,
        shown: Option<ChatId>,
        room: usize,
        badges: &Badges,
        chats: &mut Vec<ChatId>,
    ) {
        let start = self.cursor.map_or(Bound::Unbounded, Bound::Excluded);
        let after = badges.chats.range((start, Bound::Unbounded));
        let before = self
            .cursor
            .map(|c| badges.chats.range(..=c))
            .into_iter()
            .flatten();
        let mut taken = 0;
        for (chat, _) in after.chain(before) {
            if taken == room {
                break;
            }
            if Some(*chat) != shown && !chats.contains(chat) && badges.wants(*chat, now) {
                chats.push(*chat);
                self.cursor = Some(*chat);
                taken += 1;
            }
        }
    }
}

/// Takes kind checks from `kinds[from..]` into `chats` until `room` are
/// taken. Returns the new count taken from the head.
fn take_kinds(kinds: &[ChatId], from: usize, room: usize, chats: &mut Vec<ChatId>) -> usize {
    let mut taken = from;
    for chat in kinds.iter().skip(from).take(room) {
        if !chats.contains(chat) {
            chats.push(*chat);
        }
        taken += 1;
    }
    taken
}
