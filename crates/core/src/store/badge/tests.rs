//! Badge stamps, dirty bits and the `peer_dialogs` slot split (plan sections
//! 7, 11).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use proptest::prelude::*;

use super::*;
use crate::model::PeerKind;

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
    assert_eq!(b.offer(chat(1), BadgeSource::Fetched(before)), Offer::Apply);
    assert!(b.is_dirty(chat(1)), "dispatched before the trigger");
    let after = b.dispatch();
    // A trigger crossing the in-flight call: its result can't clear it.
    b.trigger(chat(1));
    b.offer(chat(1), BadgeSource::Fetched(after));
    assert!(b.is_dirty(chat(1)));
    let again = b.dispatch();
    b.offer(chat(1), BadgeSource::Fetched(again));
    assert!(!b.is_dirty(chat(1)));
}

#[test]
fn sources_apply_in_dispatch_order() {
    let mut b = tracked([1]);
    let early = b.dispatch();
    let late = b.dispatch();
    assert_eq!(b.offer(chat(1), BadgeSource::Fetched(late)), Offer::Apply);
    assert_eq!(b.offer(chat(1), BadgeSource::Fetched(early)), Offer::Ignore);
    // A chat-level read is newer than every dispatch before it.
    assert_eq!(b.offer(chat(1), BadgeSource::ChatRead), Offer::Apply);
    assert_eq!(
        b.offer(chat(1), BadgeSource::Fetched(late)),
        Offer::Ignore,
        "a peer_dialogs dispatched before the read"
    );
    // A chat-level read doesn't clear a dirty bit.
    b.trigger(chat(1));
    b.offer(chat(1), BadgeSource::ChatRead);
    assert!(b.is_dirty(chat(1)));
    // Untracked (not a retained dialog): ignored, and leaving drops state.
    assert_eq!(b.offer(chat(2), BadgeSource::ChatRead), Offer::Ignore);
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
            badges.offer(*c, BadgeSource::Fetched(batch.stamp));
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

#[derive(Clone, Copy, Debug)]
enum Op {
    /// The server count changes (a message, a topic read, a deletion): a trigger.
    Change(i64, u32),
    /// A chat-level read update carrying the server's count.
    ChatRead(i64, u32),
    Deliver(usize),
    DialogsPage,
    Peer,
    Complete(usize, usize),
    Tick(u64),
}

const CHATS: i64 = 4;

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0..CHATS, 0u32..9).prop_map(|(c, v)| Op::Change(c, v)),
        1 => (0..CHATS, 0u32..9).prop_map(|(c, v)| Op::ChatRead(c, v)),
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
    Read(ChatId, u32),
}

#[derive(Debug)]
struct Flight {
    stamp: BadgeSeq,
    chats: Vec<ChatId>,
    from: usize,
    peer: bool,
}

struct Sim {
    badges: Badges,
    scheduler: PeerScheduler,
    history: Vec<BTreeMap<ChatId, u32>>,
    events: VecDeque<Event>,
    flights: Vec<Flight>,
    now: Instant,
    shown: BTreeMap<ChatId, u32>,
    sent: BTreeMap<ChatId, BTreeSet<u32>>,
}

impl Sim {
    fn new() -> Self {
        let server: BTreeMap<ChatId, u32> = (0..CHATS).map(|c| (chat(c), 0)).collect();
        Self {
            badges: tracked(0..CHATS),
            scheduler: PeerScheduler::default(),
            shown: server.clone(),
            sent: server.keys().map(|c| (*c, BTreeSet::from([0]))).collect(),
            history: vec![server],
            events: VecDeque::new(),
            flights: Vec::new(),
            now: Instant::now(),
        }
    }

    fn set(&mut self, c: ChatId, v: u32, event: Event) {
        let mut state = self.history.last().unwrap().clone();
        state.insert(c, v);
        self.history.push(state);
        self.events.push_back(event);
    }

    fn offer(&mut self, c: ChatId, source: BadgeSource, value: u32) -> Result<(), TestCaseError> {
        let shown_stamp = self.badges.chats[&c].shown;
        let offer = self.badges.offer(c, source);
        if let BadgeSource::Fetched(stamp) = source {
            prop_assert_eq!(
                offer == Offer::Apply,
                stamp > shown_stamp,
                "dispatch order decides"
            );
        }
        self.sent.get_mut(&c).unwrap().insert(value);
        if offer == Offer::Apply {
            self.shown.insert(c, value);
        }
        prop_assert!(
            self.sent[&c].contains(&self.shown[&c]),
            "only server values"
        );
        Ok(())
    }

    fn deliver(&mut self) -> Result<(), TestCaseError> {
        match self.events.pop_front() {
            Some(Event::Trigger(c)) => {
                self.badges.trigger(c);
                prop_assert!(self.badges.is_dirty(c), "every trigger leads to a refresh");
            }
            Some(Event::Read(c, v)) => self.offer(c, BadgeSource::ChatRead, v)?,
            None => {}
        }
        Ok(())
    }

    fn complete(&mut self, flight: &Flight, at: usize) -> Result<(), TestCaseError> {
        let state = self.history[at].clone();
        for c in &flight.chats {
            self.offer(*c, BadgeSource::Fetched(flight.stamp), state[c])?;
        }
        if flight.peer {
            self.scheduler.complete(flight.stamp);
        }
        Ok(())
    }

    fn run(&mut self, op: Op) -> Result<(), TestCaseError> {
        match op {
            Op::Change(c, v) => self.set(chat(c), v, Event::Trigger(chat(c))),
            Op::ChatRead(c, v) => self.set(chat(c), v, Event::Read(chat(c), v)),
            Op::Deliver(n) => {
                for _ in 0..=n {
                    self.deliver()?;
                }
            }
            Op::DialogsPage => {
                let stamp = self.badges.dispatch();
                let chats = (0..CHATS).map(chat).collect();
                self.flights.push(Flight {
                    stamp,
                    chats,
                    from: self.history.len() - 1,
                    peer: false,
                });
            }
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
        prop_assert_eq!(
            &self.shown,
            &self.history[last],
            "the badge equals the server's"
        );
        Ok(())
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Messages, reads, chat-level reads, dialogs pages and `peer_dialogs`
    /// results in random order with crossing completions: the badge only
    /// holds server values, an earlier-dispatched source never replaces a
    /// later one, every trigger is followed by a refresh, and after the last
    /// refresh the badge equals the server's value.
    #[test]
    fn forum_badge(ops in proptest::collection::vec(op(), 1..100)) {
        let mut sim = Sim::new();
        for op in ops {
            sim.run(op)?;
        }
        sim.settle()?;
    }
}
