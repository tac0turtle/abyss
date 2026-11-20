//! Badge dirty bits, the `peer_dialogs` slot split, and the dialog rows'
//! per-field ordering across every snapshot source (plan sections 7, 11).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use proptest::prelude::*;

use super::*;
use crate::model::{ChatKind, ChatSummary, MsgId, PeerFolder, PeerKind, ReadState};
use crate::store::Dialogs;

fn chat(id: i64) -> ChatId {
    ChatId {
        kind: PeerKind::Channel,
        id,
    }
}

fn tracked(ids: impl IntoIterator<Item = i64>) -> Badges {
    let mut badges = Badges::default();
    for id in ids {
        badges.track(chat(id));
    }
    badges
}

#[test]
fn a_bit_is_cleared_only_by_a_result_dispatched_after_it() {
    let mut b = tracked([1]);
    let before = b.dispatch();
    b.trigger(chat(1));
    b.settle(chat(1), before);
    assert!(b.is_dirty(chat(1)), "dispatched before the trigger");
    let after = b.dispatch();
    // A trigger crossing the in-flight call: its result can't clear it.
    b.trigger(chat(1));
    b.settle(chat(1), after);
    assert!(b.is_dirty(chat(1)));
    let again = b.dispatch();
    b.settle(chat(1), again);
    assert!(!b.is_dirty(chat(1)));
    // Untracked (not a retained dialog): nothing; leaving drops state.
    b.trigger(chat(2));
    assert!(!b.is_dirty(chat(2)));
    b.trigger(chat(1));
    b.retain(|_| false);
    assert!(b.is_empty() && !b.is_dirty(chat(1)));
}

/// Runs the scheduler at its rate, answering every call at once. Returns,
/// per chat, the call number of its first refresh.
fn drain(
    badges: &mut Badges,
    fifo: &mut VecDeque<ChatId>,
    shown: Option<ChatId>,
    calls: usize,
    mut refill: impl FnMut(&mut Badges, &mut VecDeque<ChatId>),
) -> (BTreeMap<ChatId, usize>, BTreeMap<ChatId, usize>) {
    let mut scheduler = PeerScheduler::default();
    let mut now = Instant::now();
    let (mut badge_at, mut kind_at) = (BTreeMap::new(), BTreeMap::new());
    for call in 1..=calls {
        refill(badges, fifo);
        let kinds: Vec<ChatId> = fifo.iter().copied().collect();
        let Some(batch) = scheduler.plan(now, &kinds, shown, badges) else {
            break;
        };
        assert!(batch.chats.len() <= MAX_PEER_DIALOGS);
        let unique: BTreeSet<ChatId> = batch.chats.iter().copied().collect();
        assert_eq!(unique.len(), batch.chats.len());
        for checked in fifo.drain(..batch.kind_checks) {
            kind_at.entry(checked).or_insert(call);
        }
        for c in &batch.chats {
            if badges.is_dirty(*c) {
                badge_at.entry(*c).or_insert(call);
            }
            badges.settle(*c, batch.stamp);
        }
        assert!(
            scheduler.plan(now, &kinds, shown, badges).is_none(),
            "one in flight"
        );
        scheduler.complete(batch.stamp);
        assert!(
            scheduler
                .plan(
                    now + PEER_CALL_INTERVAL.saturating_sub(Duration::from_millis(1)),
                    &kinds,
                    shown,
                    badges
                )
                .is_none(),
            "calls are spaced"
        );
        now += PEER_CALL_INTERVAL;
    }
    (badge_at, kind_at)
}

/// 40 dirty badges with fenced chats: every kind check within `ceil(m / 10)`
/// calls under badge contention; every badge refreshed.
/// Round 6: kind checks keep the FIFO full on every call and the shown
/// forum is always dirty; an unrelated dirty forum beyond page one is still
/// reached within `ceil(n / 9)` calls through the cursor slots.
#[test]
fn sustained_kind_checks_dont_starve_the_cursor() {
    let n: usize = 999;
    let mut badges = tracked(0..=999);
    let shown = chat(0);
    for id in 1..=999 {
        badges.trigger(chat(id));
    }
    let mut fifo = VecDeque::new();
    let mut next_fenced = 10_000;
    let (badge_at, _) = drain(&mut badges, &mut fifo, Some(shown), 200, |b, fifo| {
        b.trigger(shown);
        while fifo.len() < 2 * KIND_SLOTS {
            next_fenced += 1;
            fifo.push_back(chat(next_fenced));
        }
    });
    let bound = n.div_ceil(CURSOR_SLOTS);
    assert_eq!(bound, 111);
    for id in 1..=999 {
        let at = badge_at.get(&chat(id)).copied();
        assert!(at.is_some_and(|at| at <= bound), "chat {id} at {at:?}");
    }
}

/// Round 5: 50 forums beyond page one each get one trigger, then nothing
/// else happens. All are refreshed by the drain; none is dropped.
#[test]
fn badge_overflow_beyond_page_one_is_drained() {
    let mut badges = tracked(0..1000);
    for id in 900..950 {
        badges.trigger(chat(id));
    }
    let (badge_at, _) = drain(
        &mut badges,
        &mut VecDeque::new(),
        Some(chat(0)),
        20,
        |_, _| {},
    );
    assert_eq!(badge_at.len(), 50);
    assert!(
        badge_at
            .values()
            .all(|at| *at <= 50usize.div_ceil(CURSOR_SLOTS))
    );
    assert_eq!(badges.dirty().count(), 0);
}

#[test]
fn kind_checks_keep_their_slots_under_badge_contention() {
    let mut badges = tracked(0..1000);
    let mut fifo: VecDeque<ChatId> = (5_000..5_040).map(chat).collect();
    let (badge_at, kind_at) = drain(&mut badges, &mut fifo, Some(chat(0)), 200, |b, _| {
        for id in 0..1000 {
            b.trigger(chat(id));
        }
    });
    for id in 5_000..5_040 {
        assert!(
            kind_at[&chat(id)] <= 40usize.div_ceil(KIND_SLOTS),
            "kind check {id}"
        );
    }
    assert!(badge_at.len() >= 999);
    // With no badge dirty, kind checks take every slot.
    let mut idle = tracked(0..10);
    let mut fifo: VecDeque<ChatId> = (5_000..5_040).map(chat).collect();
    let (_, kind_at) = drain(&mut idle, &mut fifo, None, 10, |_, _| {});
    assert!(
        kind_at
            .values()
            .all(|at| *at <= 40usize.div_ceil(MAX_PEER_DIALOGS))
    );
}

#[test]
fn a_flood_wait_pauses_the_scheduler_and_only_extends() {
    let t0 = Instant::now();
    let mut badges = tracked([1]);
    badges.trigger(chat(1));
    let mut s = PeerScheduler::default();
    s.flood(t0 + Duration::from_secs(60));
    s.flood(t0 + Duration::from_secs(10));
    assert!(
        s.plan(t0 + Duration::from_secs(59), &[], None, &mut badges)
            .is_none()
    );
    assert_eq!(
        s.not_before().deadline(),
        Some(t0 + Duration::from_secs(60))
    );
    let batch = s
        .plan(t0 + Duration::from_secs(60), &[], None, &mut badges)
        .unwrap();
    assert_eq!(batch.chats, [chat(1)]);
    // A chat isn't sent again within its refresh interval.
    s.complete(batch.stamp);
    badges.trigger(chat(1));
    assert!(
        s.plan(t0 + Duration::from_secs(62), &[], None, &mut badges)
            .is_none()
    );
}

/// What the server says about one chat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Server {
    unread: u32,
    /// The read pointer: it only rises.
    pointer: i32,
    folder: PeerFolder,
    forum: bool,
}

#[derive(Clone, Copy, Debug)]
enum Op {
    /// The server count changes (a message, a topic read, a deletion): a trigger.
    Change(i64, u32),
    /// A chat-level read update carrying the server's count, raising the
    /// server's pointer.
    ChatRead(i64, u32, i32),
    /// The chat moves in or out of the archive: a live folder update.
    Move(i64, bool),
    /// The forum flag changes, silently: only snapshots tell.
    Kind(i64, bool),
    /// The chat leaves the list (a newer page no longer lists it); a later
    /// page lists it again.
    Leave(i64),
    Deliver(usize),
    DialogsPage,
    Peer,
    Complete(usize, usize),
    Tick(u64),
}

/// Chats `0..LISTED` start listed; the rest start unlisted (a window, a
/// move elsewhere) until a page lists them.
const CHATS: i64 = 5;
const LISTED: i64 = 4;

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0..CHATS, 0u32..9).prop_map(|(c, v)| Op::Change(c, v)),
        1 => (0..CHATS, 0u32..9, 0i32..20).prop_map(|(c, v, p)| Op::ChatRead(c, v, p)),
        1 => (0..CHATS, any::<bool>()).prop_map(|(c, a)| Op::Move(c, a)),
        1 => (0..CHATS, any::<bool>()).prop_map(|(c, f)| Op::Kind(c, f)),
        1 => (0..CHATS).prop_map(Op::Leave),
        3 => (0usize..3).prop_map(Op::Deliver),
        1 => Just(Op::DialogsPage),
        3 => Just(Op::Peer),
        3 => (0usize..3, 0usize..6).prop_map(|(i, p)| Op::Complete(i, p)),
        2 => (0u64..5).prop_map(Op::Tick),
    ]
}

#[derive(Debug)]
enum Event {
    Trigger(ChatId),
    Read(ChatId, u32, MsgId),
    Move(ChatId, PeerFolder),
}

#[derive(Debug)]
struct Flight {
    stamp: BadgeSeq,
    chats: Vec<ChatId>,
    from: usize,
    peer: bool,
}

/// The row as a dialog snapshot of `server` states it.
fn summary(c: ChatId, server: Server) -> ChatSummary {
    ChatSummary {
        read: ReadState {
            unread: server.unread,
            read_inbox_max: MsgId(server.pointer),
        },
        folder: server.folder,
        kind: ChatKind::Supergroup {
            forum: server.forum,
            gigagroup: false,
        },
        id: c,
        title: "chat".into(),
        pinned: false,
        unread_mentions: 0,
        last: None,
        last_date: 0,
        last_id: None,
        can_send: true,
        muted_until: None,
        scope_muted_until: 0,
        reading: None,
        topic_unread: None,
    }
}

/// The field index of the read pointer, which follows the highest source
/// instead of the newest.
const POINTER: usize = 3;

/// A row's guarded fields: unread, archived, forum, and the read pointer.
fn fields(s: &ChatSummary) -> [u64; 4] {
    [
        u64::from(s.read.unread),
        u64::from(s.folder == PeerFolder::Archive),
        u64::from(matches!(s.kind, ChatKind::Supergroup { forum: true, .. })),
        u64::try_from(s.read.read_inbox_max.0).unwrap_or(0),
    ]
}

/// Where a source came from. A live change outranks a snapshot dispatched
/// at the same stamp (that snapshot was sent before it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Origin {
    Snapshot,
    Live,
}

/// One delivered source of a field's value, for the reference model.
#[derive(Clone, Copy, Debug)]
struct Source {
    /// A snapshot's dispatch stamp, or the clock when a live change landed.
    stamp: BadgeSeq,
    origin: Origin,
    /// Delivery order: between live changes at one stamp, the later wins.
    order: u64,
    value: u64,
}

/// The reference model, kept from the delivery history alone: for every
/// chat and field, every source delivered since the chat was last
/// forgotten (it left the list), and which chats are listed. A field
/// should show the value of its newest source; the read pointer, the
/// highest.
#[derive(Debug, Default)]
struct Model {
    log: BTreeMap<(ChatId, usize), Vec<Source>>,
    listed: BTreeSet<ChatId>,
    order: u64,
}

impl Model {
    fn deliver(&mut self, c: ChatId, field: usize, stamp: BadgeSeq, origin: Origin, value: u64) {
        self.order += 1;
        let source = Source {
            stamp,
            origin,
            order: self.order,
            value,
        };
        self.log.entry((c, field)).or_default().push(source);
    }

    fn expected(&self, c: ChatId) -> Option<[u64; 4]> {
        let mut out = [0; 4];
        for (field, slot) in out.iter_mut().enumerate() {
            let log = self.log.get(&(c, field))?;
            *slot = if field == POINTER {
                log.iter().map(|s| s.value).max()?
            } else {
                log.iter()
                    .max_by_key(|s| (s.stamp, s.origin, s.order))?
                    .value
            };
        }
        Some(out)
    }

    /// A chat that leaves the list is forgotten, its sources with it.
    fn forget(&mut self, c: ChatId) {
        self.listed.remove(&c);
        self.log.retain(|(chat, _), _| *chat != c);
    }
}

struct Sim {
    dialogs: Dialogs,
    badges: Badges,
    scheduler: PeerScheduler,
    history: Vec<BTreeMap<ChatId, Server>>,
    events: VecDeque<Event>,
    flights: Vec<Flight>,
    now: Instant,
    model: Model,
}

impl Sim {
    fn new() -> Self {
        let start = Server {
            unread: 0,
            pointer: 0,
            folder: PeerFolder::Main,
            forum: false,
        };
        let server: BTreeMap<ChatId, Server> = (0..CHATS).map(|c| (chat(c), start)).collect();
        let mut dialogs = Dialogs::default();
        let listed: Vec<ChatSummary> = (0..LISTED).map(|c| summary(chat(c), start)).collect();
        let mut model = Model::default();
        for row in &listed {
            model.listed.insert(row.id);
            for (field, value) in fields(row).into_iter().enumerate() {
                model.deliver(row.id, field, BadgeSeq::default(), Origin::Snapshot, value);
            }
        }
        dialogs.reset(listed);
        Self {
            dialogs,
            badges: tracked(0..CHATS),
            scheduler: PeerScheduler::default(),
            history: vec![server],
            events: VecDeque::new(),
            flights: Vec::new(),
            now: Instant::now(),
            model,
        }
    }

    fn set(&mut self, c: ChatId, change: impl FnOnce(&mut Server), event: Option<Event>) {
        let mut state = self.history.last().unwrap().clone();
        change(state.get_mut(&c).unwrap());
        self.history.push(state);
        self.events.extend(event);
    }

    /// Every listed row shows what the model expects, and the store lists
    /// exactly the model's chats.
    fn check(&self) -> Result<(), TestCaseError> {
        let listed: BTreeSet<ChatId> = self.dialogs.as_slice().iter().map(|c| c.id).collect();
        prop_assert_eq!(&listed, &self.model.listed);
        for row in self.dialogs.as_slice() {
            let expected = self.model.expected(row.id);
            prop_assert_eq!(
                Some(fields(row)),
                expected,
                "{:?} shows its newest source",
                row.id
            );
        }
        Ok(())
    }

    fn deliver(&mut self) -> Result<(), TestCaseError> {
        let now = self.badges.now();
        match self.events.pop_front() {
            Some(Event::Trigger(c)) => {
                self.badges.trigger(c);
                prop_assert!(self.badges.is_dirty(c), "every trigger leads to a refresh");
            }
            // A read of a chat that isn't listed has no row to change.
            Some(Event::Read(c, v, max_id)) => {
                if self.dialogs.server_read(c, v, max_id, now).is_some() {
                    self.model.deliver(c, 0, now, Origin::Live, u64::from(v));
                    let pointer = u64::try_from(max_id.0).unwrap_or(0);
                    self.model.deliver(c, POINTER, now, Origin::Live, pointer);
                }
            }
            // A move is remembered, listed or not.
            Some(Event::Move(c, folder)) => {
                self.dialogs.set_folder(c, folder, now);
                let archived = u64::from(folder == PeerFolder::Archive);
                self.model.deliver(c, 1, now, Origin::Live, archived);
            }
            None => {}
        }
        self.check()
    }

    /// A snapshot dispatched at `stamp` lands with the server state at
    /// `at`. A page lists every chat it holds (admitting unlisted ones); a
    /// `peer_dialogs` result only refreshes listed rows.
    fn complete(&mut self, flight: &Flight, at: usize) -> Result<(), TestCaseError> {
        let state = self.history[at].clone();
        let mut page: Vec<ChatSummary> =
            flight.chats.iter().map(|c| summary(*c, state[c])).collect();
        for row in &page {
            for (field, value) in fields(row).into_iter().enumerate() {
                self.model
                    .deliver(row.id, field, flight.stamp, Origin::Snapshot, value);
            }
        }
        prop_assert_ne!(
            self.dialogs.guard(&mut page, flight.stamp, |_| None),
            crate::store::Guarded::Discard
        );
        for fresh in page {
            let c = fresh.id;
            if self.model.listed.contains(&c) {
                self.dialogs.refresh(fresh);
            } else if !flight.peer {
                self.model.listed.insert(c);
                self.dialogs.append(vec![fresh], crate::store::Rest::More);
            }
            self.badges.settle(c, flight.stamp);
        }
        if flight.peer {
            self.scheduler.complete(flight.stamp);
        }
        self.check()
    }

    fn leave(&mut self, c: ChatId) -> Result<(), TestCaseError> {
        if !self.model.listed.contains(&c) || self.model.listed.len() == 1 {
            return Ok(());
        }
        let rest: Vec<ChatSummary> = self
            .dialogs
            .as_slice()
            .iter()
            .filter(|r| r.id != c)
            .cloned()
            .collect();
        self.dialogs.reset(rest);
        self.model.forget(c);
        self.check()
    }

    fn page(&mut self) {
        let stamp = self.badges.dispatch();
        let chats = (0..CHATS).map(chat).collect();
        self.flights.push(Flight {
            stamp,
            chats,
            from: self.history.len() - 1,
            peer: false,
        });
    }

    fn run(&mut self, op: Op) -> Result<(), TestCaseError> {
        match op {
            Op::Change(c, v) => self.set(chat(c), |s| s.unread = v, Some(Event::Trigger(chat(c)))),
            Op::ChatRead(c, v, p) => {
                let mut max_id = MsgId(p);
                let read = |s: &mut Server| {
                    s.unread = v;
                    s.pointer = s.pointer.max(p);
                    max_id = MsgId(s.pointer);
                };
                self.set(chat(c), read, None);
                self.events.push_back(Event::Read(chat(c), v, max_id));
            }
            Op::Move(c, archive) => {
                let folder = if archive {
                    PeerFolder::Archive
                } else {
                    PeerFolder::Main
                };
                self.set(
                    chat(c),
                    |s| s.folder = folder,
                    Some(Event::Move(chat(c), folder)),
                );
            }
            Op::Kind(c, forum) => self.set(chat(c), |s| s.forum = forum, None),
            Op::Leave(c) => self.leave(chat(c))?,
            Op::Deliver(n) => {
                for _ in 0..=n {
                    self.deliver()?;
                }
            }
            Op::DialogsPage => self.page(),
            Op::Peer => {
                if let Some(batch) =
                    self.scheduler
                        .plan(self.now, &[], Some(chat(0)), &mut self.badges)
                {
                    let from = self.history.len() - 1;
                    self.flights.push(Flight {
                        stamp: batch.stamp,
                        chats: batch.chats,
                        from,
                        peer: true,
                    });
                }
            }
            Op::Complete(i, pick) => {
                if !self.flights.is_empty() {
                    let flight = self.flights.remove(i % self.flights.len());
                    let at = flight.from + pick % (self.history.len() - flight.from);
                    self.complete(&flight, at)?;
                }
            }
            Op::Tick(s) => self.now += Duration::from_secs(s),
        }
        Ok(())
    }

    /// Everything delivered and answered, then the drain runs dry and one
    /// last page is fetched: every chat is listed and equals the server's.
    fn settle(&mut self) -> Result<(), TestCaseError> {
        while !self.events.is_empty() {
            self.deliver()?;
        }
        let last = self.history.len() - 1;
        for _ in 0..20 {
            while let Some(flight) = self.flights.pop() {
                self.complete(&flight, last)?;
            }
            self.now += PEER_CALL_INTERVAL;
            let Some(batch) = self
                .scheduler
                .plan(self.now, &[], Some(chat(0)), &mut self.badges)
            else {
                break;
            };
            self.flights.push(Flight {
                stamp: batch.stamp,
                chats: batch.chats,
                from: last,
                peer: true,
            });
        }
        prop_assert_eq!(self.badges.dirty().count(), 0);
        self.page();
        while let Some(flight) = self.flights.pop() {
            self.complete(&flight, last)?;
        }
        prop_assert_eq!(self.dialogs.len(), usize::try_from(CHATS).unwrap());
        for (c, server) in &self.history[last] {
            let row = self.dialogs.get(*c).unwrap();
            prop_assert_eq!(fields(row), fields(&summary(*c, *server)), "converged");
        }
        Ok(())
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Dialog snapshots from every source (dialogs pages, `peer_dialogs`
    /// results) and live changes (counted triggers, chat-level reads,
    /// folder moves; silent kind changes) in random order with crossing
    /// completions, while chats leave the list and come back and unlisted
    /// chats are moved and refreshed. After every step each listed row
    /// shows, field by field (unread, folder, kind), the value of the
    /// newest source the model logged for it, so an older source never
    /// replaces a newer one, also across a chat's admission, and the read
    /// pointer the highest any source stated, so it never moves back;
    /// every trigger is followed by a refresh; and once everything is
    /// answered every row equals the server's.
    #[test]
    fn dialog_rows_follow_the_newest_source(ops in proptest::collection::vec(op(), 1..100)) {
        let mut sim = Sim::new();
        for op in ops {
            sim.run(op)?;
        }
        sim.settle()?;
    }
}
