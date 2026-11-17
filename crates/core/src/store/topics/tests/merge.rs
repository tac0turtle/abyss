//! The merge policy against a simulated server (plan sections 6.4, 11).
//!
//! The server changes topics and queues the matching live events; fetches
//! complete in any order with snapshots of the server taken anywhere between
//! their dispatch and their completion, so a snapshot sees none, a prefix or
//! all of the crossing changes, and events may arrive after a snapshot that
//! already contains them.
//!
//! An independent oracle re-implements the section 6.4 rules on its own
//! state (its own dispatch counter, per-group stamps, tombstones and the
//! unlisted-change mark) and the list must agree with it after every step.
//! Once quiet, the list must also equal the server.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use proptest::prelude::*;

use super::*;
use crate::rich_text::RichText;
use crate::store::topics::entry::TopicEntry;
use crate::store::topics::stamp::FetchSeq;
use crate::store::topics::{Tail, TopicEdit, UnreadTrigger};

const TOPICS: i32 = 5;
const UNEDITED: i64 = i64::MIN;

#[derive(Clone, Debug)]
struct ServerTopic {
    /// Message ids (the root, the topic's id, included) and edit versions.
    msgs: BTreeMap<i32, i64>,
    read_max: i32,
    closed: bool,
    hidden: bool,
}

/// One server state: absent topics were deleted.
type State = BTreeMap<i32, ServerTopic>;

fn preview(version: i64) -> Preview {
    if version == UNEDITED {
        Preview::Service
    } else {
        Preview::Text(format!("v{version}"))
    }
}

fn snapshot_of(t: i32, s: &ServerTopic) -> TopicSnapshot {
    let unread = s
        .msgs
        .keys()
        .filter(|m| **m > s.read_max && **m != t)
        .count();
    let top = s.msgs.last_key_value();
    TopicSnapshot {
        topic: Topic {
            closed: s.closed,
            unread: u32::try_from(unread).unwrap(),
            read_inbox_max: MsgId(s.read_max),
            last_id: top.map(|(m, _)| MsgId(*m)),
            last: Some(preview(top.map_or(UNEDITED, |(_, v)| *v))),
            last_date: 0,
            ..topic(t)
        },
        visibility: if s.hidden {
            Visibility::Hidden
        } else {
            Visibility::Shown
        },
        top_version: top.map_or(UNEDITED, |(_, v)| *v),
    }
}

#[derive(Clone, Debug)]
enum Event {
    New(i32, i32),
    Read(i32),
    Edit(i32, TopicEdit),
    MessageEdit(i32, i32, i64),
    Deleted(Vec<i32>),
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Msg(i32),
    Read(i32),
    Close(i32, bool),
    Hide(bool),
    EditMessage(i32, usize),
    DeleteMessage(i32, usize),
    DeleteTopic(i32),
    Deliver(usize),
    ById,
    Page,
    Complete(usize, usize),
    Fail(usize),
    Tick(u64),
}

fn op() -> impl Strategy<Value = Op> {
    let t = 1..=TOPICS;
    prop_oneof![
        4 => t.clone().prop_map(Op::Msg),
        2 => t.clone().prop_map(Op::Read),
        1 => (t.clone(), any::<bool>()).prop_map(|(t, c)| Op::Close(t, c)),
        1 => any::<bool>().prop_map(Op::Hide),
        2 => (t.clone(), 0usize..3).prop_map(|(t, k)| Op::EditMessage(t, k)),
        2 => (t, 0usize..4).prop_map(|(t, k)| Op::DeleteMessage(t, k)),
        1 => (2..=TOPICS).prop_map(Op::DeleteTopic),
        4 => (0usize..4).prop_map(Op::Deliver),
        3 => Just(Op::ById),
        1 => Just(Op::Page),
        4 => (0usize..4, 0usize..8).prop_map(|(i, p)| Op::Complete(i, p)),
        1 => (0usize..4).prop_map(Op::Fail),
        2 => (0u64..12).prop_map(Op::Tick),
    ]
}

#[derive(Debug)]
enum Fetch {
    Page,
    ById(Vec<TopicId>),
}

#[derive(Debug)]
struct Flight {
    seq: FetchSeq,
    /// The oracle's own number for this dispatch.
    n: u64,
    fetch: Fetch,
    /// The server history index at dispatch.
    from: usize,
}

/// One group's stamps in the oracle: the dispatch that last set it, and the
/// oracle's dispatch count at its last live change.
#[derive(Clone, Copy, Debug, Default)]
struct Stamps {
    seen: u64,
    changed: u64,
}

impl Stamps {
    fn applies(self, n: u64) -> bool {
        n > self.seen.max(self.changed)
    }
}

/// What the client should hold for a topic.
#[derive(Clone, Debug, PartialEq)]
struct Expected {
    closed: bool,
    visibility: Visibility,
    unread: u32,
    last_id: Option<MsgId>,
    last: Option<Preview>,
    last_version: i64,
}

#[derive(Clone, Debug)]
struct ModelTopic {
    want: Expected,
    meta: Stamps,
    unread: Stamps,
    activity: Stamps,
}

/// The section 6.4 rules, written against the oracle's own state.
#[derive(Debug, Default)]
struct Oracle {
    dispatched: u64,
    topics: BTreeMap<TopicId, ModelTopic>,
    removed_at: BTreeMap<TopicId, u64>,
    unlisted: u64,
}

impl Oracle {
    fn dispatch(&mut self) -> u64 {
        self.dispatched += 1;
        self.dispatched
    }

    fn live(&mut self, event: &Event) {
        let now = self.dispatched;
        let touch = |o: &mut Self, t: i32, f: &mut dyn FnMut(&mut ModelTopic)| match o
            .topics
            .get_mut(&tid(t))
        {
            Some(m) => f(m),
            None => o.unlisted = now,
        };
        match event {
            Event::New(t, id) => touch(self, *t, &mut |m| {
                if m.want.last_id.is_none_or(|l| MsgId(*id) > l) {
                    m.want.last_id = Some(MsgId(*id));
                    m.want.last = Some(Preview::Service);
                    m.want.last_version = UNEDITED;
                    m.activity.changed = now;
                }
                m.unread.changed = now;
            }),
            Event::Read(t) => touch(self, *t, &mut |m| m.unread.changed = now),
            Event::Edit(t, edit) => touch(self, *t, &mut |m| {
                if let Some(c) = edit.closed {
                    m.want.closed = c;
                }
                if let Some(v) = edit.visibility {
                    m.want.visibility = v;
                }
                m.meta.changed = now;
            }),
            Event::MessageEdit(t, id, version) => touch(self, *t, &mut |m| {
                let is_top = m.want.last_id == Some(MsgId(*id));
                let unsettled = m.activity.changed >= m.activity.seen;
                if is_top && *version > m.want.last_version {
                    m.want.last = Some(preview(*version));
                    m.want.last_version = *version;
                    m.activity.changed = now;
                } else if !is_top && unsettled {
                    // The real top isn't known yet: this may be it.
                    m.activity.changed = now;
                }
            }),
            Event::Deleted(ids) => {
                self.unlisted = now;
                for (t, m) in &mut self.topics {
                    m.unread.changed = now;
                    if m.want.last_id.is_some_and(|l| ids.contains(&l.0)) {
                        m.activity.changed = now;
                    }
                    if ids.contains(&t.get()) {
                        m.meta.changed = now;
                    }
                }
            }
        }
    }

    fn found(&mut self, n: u64, s: &TopicSnapshot) {
        let id = s.topic.id;
        let fresh = Expected {
            closed: s.topic.closed,
            visibility: s.visibility,
            unread: s.topic.unread,
            last_id: s.topic.last_id,
            last: s.topic.last.clone(),
            last_version: s.top_version,
        };
        match self.topics.get_mut(&id) {
            Some(m) => {
                if m.meta.applies(n) {
                    (m.want.closed, m.want.visibility, m.meta.seen) =
                        (fresh.closed, fresh.visibility, n);
                }
                if m.unread.applies(n) {
                    (m.want.unread, m.unread.seen) = (fresh.unread, n);
                }
                if m.activity.applies(n) {
                    m.want.last_id = fresh.last_id;
                    m.want.last = fresh.last;
                    m.want.last_version = fresh.last_version;
                    m.activity.seen = n;
                }
            }
            None if self.removed_at.get(&id).is_some_and(|r| n < *r) => {}
            None => {
                let changed = if n <= self.unlisted { self.unlisted } else { 0 };
                let stamps = Stamps { seen: n, changed };
                let m = ModelTopic {
                    want: fresh,
                    meta: stamps,
                    unread: stamps,
                    activity: stamps,
                };
                self.topics.insert(id, m);
            }
        }
    }

    fn deleted(&mut self, n: u64, id: TopicId) {
        match self.topics.get(&id) {
            Some(m) if m.meta.applies(n) => {
                self.topics.remove(&id);
                self.removed_at.insert(id, n);
            }
            Some(_) => {}
            None => {
                let at = self.removed_at.entry(id).or_insert(n);
                *at = (*at).max(n);
            }
        }
    }
}

struct Sim {
    list: TopicList,
    oracle: Oracle,
    history: Vec<State>,
    events: VecDeque<Event>,
    flights: Vec<Flight>,
    next_msg: i32,
    next_version: i64,
    now: Instant,
    /// Every unread value a snapshot sent, per topic.
    sent_unread: BTreeMap<TopicId, BTreeSet<u32>>,
}

impl Sim {
    fn new() -> Self {
        let state: State = (1..=TOPICS)
            .map(|t| {
                let s = ServerTopic {
                    msgs: [(t, UNEDITED), (100 + t, UNEDITED)].into(),
                    read_max: 0,
                    closed: false,
                    hidden: false,
                };
                (t, s)
            })
            .collect();
        let mut sim = Self {
            list: list(),
            oracle: Oracle::default(),
            history: vec![state],
            events: VecDeque::new(),
            flights: Vec::new(),
            next_msg: 200,
            next_version: 1,
            now: Instant::now(),
            sent_unread: BTreeMap::new(),
        };
        sim.list.request_page(PageKind::First);
        let call = sim.list.dispatch_page().expect("dispatched");
        let n = sim.oracle.dispatch();
        sim.flights.push(Flight {
            seq: call.seq,
            n,
            fetch: Fetch::Page,
            from: 0,
        });
        sim
    }

    fn server(&self) -> &State {
        self.history.last().expect("history is never empty")
    }

    /// Applies a server change and queues its event.
    fn change(&mut self, f: impl FnOnce(&mut State, &mut i32, &mut i64) -> Option<Event>) {
        let mut state = self.server().clone();
        if let Some(event) = f(&mut state, &mut self.next_msg, &mut self.next_version) {
            self.history.push(state);
            self.events.push_back(event);
        }
    }

    fn change_server(&mut self, op: Op) {
        match op {
            Op::Msg(t) => self.change(|s, next, _| {
                let topic = s.get_mut(&t)?;
                *next += 1;
                topic.msgs.insert(*next, UNEDITED);
                Some(Event::New(t, *next))
            }),
            Op::Read(t) => self.change(|s, _, _| {
                let topic = s.get_mut(&t)?;
                topic.read_max = *topic.msgs.last_key_value()?.0;
                Some(Event::Read(t))
            }),
            Op::Close(t, closed) => self.change(|s, _, _| {
                s.get_mut(&t)?.closed = closed;
                Some(Event::Edit(
                    t,
                    TopicEdit {
                        closed: Some(closed),
                        ..TopicEdit::default()
                    },
                ))
            }),
            Op::Hide(hidden) => self.change(|s, _, _| {
                s.get_mut(&1)?.hidden = hidden;
                let visibility = Some(if hidden {
                    Visibility::Hidden
                } else {
                    Visibility::Shown
                });
                Some(Event::Edit(
                    1,
                    TopicEdit {
                        visibility,
                        ..TopicEdit::default()
                    },
                ))
            }),
            Op::EditMessage(t, k) => self.change(|s, _, version| {
                let topic = s.get_mut(&t)?;
                let id = *topic.msgs.keys().filter(|m| **m != t).rev().nth(k)?;
                *version += 1;
                topic.msgs.insert(id, *version);
                Some(Event::MessageEdit(t, id, *version))
            }),
            Op::DeleteMessage(t, k) => self.change(|s, _, _| {
                let topic = s.get_mut(&t)?;
                let id = *topic.msgs.keys().filter(|m| **m != t).rev().nth(k)?;
                topic.msgs.remove(&id);
                Some(Event::Deleted(vec![id]))
            }),
            Op::DeleteTopic(t) => self.change(|s, _, _| {
                let topic = s.remove(&t)?;
                Some(Event::Deleted(topic.msgs.into_keys().collect()))
            }),
            _ => {}
        }
    }

    fn run(&mut self, op: Op) -> Result<(), TestCaseError> {
        match op {
            Op::Deliver(n) => {
                for _ in 0..=n {
                    self.deliver_one()?;
                }
            }
            Op::ById => self.dispatch_by_id(),
            Op::Page => {
                self.list.request_page(PageKind::First);
                if let Some(call) = self.list.dispatch_page() {
                    let n = self.oracle.dispatch();
                    let from = self.history.len() - 1;
                    self.flights.push(Flight {
                        seq: call.seq,
                        n,
                        fetch: Fetch::Page,
                        from,
                    });
                }
            }
            Op::Complete(i, pick) => {
                if !self.flights.is_empty() {
                    let flight = self.flights.remove(i % self.flights.len());
                    let span = self.history.len() - flight.from;
                    self.complete(&flight, flight.from + pick % span)?;
                }
            }
            Op::Fail(i) => {
                if !self.flights.is_empty() {
                    let flight = self.flights.remove(i % self.flights.len());
                    prop_assert!(self.list.fail(flight.seq, self.now).is_ok());
                }
            }
            Op::Tick(s) => self.now += Duration::from_secs(s),
            _ => self.change_server(op),
        }
        check_invariants(&self.list).map_err(TestCaseError::fail)?;
        self.agree()
    }

    fn dispatch_by_id(&mut self) {
        if let Some(call) = self.list.dispatch_by_id(self.now) {
            let n = self.oracle.dispatch();
            let from = self.history.len() - 1;
            self.flights.push(Flight {
                seq: call.seq,
                n,
                fetch: Fetch::ById(call.ids),
                from,
            });
        }
    }

    fn deliver_one(&mut self) -> Result<(), TestCaseError> {
        let Some(event) = self.events.pop_front() else {
            return Ok(());
        };
        let now = self.now;
        let l = &mut self.list;
        let listed: BTreeSet<TopicId> = l.entries.iter().map(TopicEntry::id).collect();
        let due: Vec<TopicId> = match &event {
            Event::New(t, id) => {
                l.on_new_message(&message(tid(*t), *id), Tail::Hidden, now);
                vec![tid(*t)]
            }
            Event::Read(t) => {
                l.trigger_unread(tid(*t), UnreadTrigger::ReadRemotely, now);
                vec![tid(*t)]
            }
            Event::Edit(t, edit) => {
                l.on_topic_edited(tid(*t), edit.clone(), now);
                vec![tid(*t)]
            }
            Event::MessageEdit(t, id, version) => {
                let msg = Message {
                    body: Body::Text(RichText::plain(&format!("v{version}"))),
                    edit_date: Some(*version),
                    ..message(tid(*t), *id)
                };
                l.on_edit(&msg, now);
                vec![]
            }
            Event::Deleted(ids) => {
                let ids: Vec<MsgId> = ids.iter().copied().map(MsgId).collect();
                l.on_deleted(&ids, &[], now);
                listed.iter().copied().collect()
            }
        };
        self.oracle.live(&event);
        for id in due.into_iter().filter(|id| listed.contains(id)) {
            prop_assert!(self.list.is_due(id), "a live change leaves {:?} due", id);
        }
        Ok(())
    }

    fn complete(&mut self, flight: &Flight, at: usize) -> Result<(), TestCaseError> {
        let state = self.history[at].clone();
        let (found, deleted): (Vec<TopicSnapshot>, Vec<TopicId>) = match &flight.fetch {
            Fetch::Page => (
                state
                    .iter()
                    .rev()
                    .map(|(t, s)| snapshot_of(*t, s))
                    .collect(),
                vec![],
            ),
            Fetch::ById(ids) => {
                let found = ids
                    .iter()
                    .filter_map(|id| state.get(&id.get()).map(|s| snapshot_of(id.get(), s)))
                    .collect();
                let deleted = ids
                    .iter()
                    .copied()
                    .filter(|id| !state.contains_key(&id.get()))
                    .collect();
                (found, deleted)
            }
        };
        for s in &found {
            self.sent_unread
                .entry(s.topic.id)
                .or_default()
                .insert(s.topic.unread);
        }
        let result = match &flight.fetch {
            Fetch::Page => {
                let p = page(TopicOrder::LastActivity, found.clone(), Rest::Last);
                self.list.apply_page(flight.seq, p, self.now)
            }
            Fetch::ById(_) => {
                self.list
                    .apply_by_id(flight.seq, by_id(found.clone(), deleted.clone()), self.now)
            }
        };
        prop_assert!(result.is_ok(), "no fetch is invalidated with five topics");
        // Every accepted deletion is told as one, however the rows change.
        if let Ok(changes) = &result {
            for id in deleted.iter().filter(|id| self.list.entry(**id).is_none()) {
                let told = ListChange::Removed(*id, TopicGone::Deleted);
                prop_assert!(changes.contains(&told), "{:?}: {:?}", id, changes);
            }
        }
        for s in &found {
            self.oracle.found(flight.n, s);
        }
        for id in deleted {
            self.oracle.deleted(flight.n, id);
        }
        Ok(())
    }

    /// The list holds exactly what the oracle expects, and only counts some
    /// snapshot sent.
    fn agree(&self) -> Result<(), TestCaseError> {
        let listed: BTreeSet<TopicId> = self.list.entries.iter().map(TopicEntry::id).collect();
        let expected: BTreeSet<TopicId> = self.oracle.topics.keys().copied().collect();
        prop_assert_eq!(listed, expected, "membership");
        for e in &self.list.entries {
            let got = Expected {
                closed: e.topic.closed,
                visibility: e.visibility,
                unread: e.topic.unread,
                last_id: e.topic.last_id,
                last: e.topic.last.clone(),
                last_version: e.last_version,
            };
            let model = &self.oracle.topics[&e.id()];
            prop_assert_eq!(&got, &model.want, "{:?}", e.id());
            let unsettled = [model.meta, model.unread, model.activity]
                .iter()
                .any(|s| s.changed >= s.seen);
            prop_assert!(
                !unsettled || self.list.is_due(e.id()),
                "{:?} has unsettled groups but isn't due",
                e.id()
            );
            let sent = self
                .sent_unread
                .get(&e.id())
                .is_some_and(|v| v.contains(&e.topic.unread));
            prop_assert!(sent, "{:?} holds an unread count no snapshot sent", e.id());
        }
        Ok(())
    }

    /// Delivers everything, then drains with fresh snapshots until nothing
    /// is due. The list must then equal the server.
    fn settle(&mut self) -> Result<(), TestCaseError> {
        while !self.events.is_empty() {
            self.deliver_one()?;
        }
        let last = self.history.len() - 1;
        for _ in 0..80 {
            while let Some(flight) = self.flights.pop() {
                self.complete(&flight, last)?;
            }
            self.agree()?;
            self.now += Duration::from_secs(31);
            self.dispatch_by_id();
            if self.flights.is_empty() {
                break;
            }
        }
        prop_assert_eq!(self.list.due_len(), 0, "everything settled");
        let server = self.server().clone();
        let listed: BTreeSet<i32> = self.list.entries.iter().map(|e| e.id().get()).collect();
        prop_assert_eq!(listed, server.keys().copied().collect::<BTreeSet<_>>());
        for (t, s) in &server {
            let want = snapshot_of(*t, s);
            let entry = self.list.entry(tid(*t)).expect("listed");
            prop_assert_eq!(entry.topic.unread, want.topic.unread, "unread of {}", t);
            prop_assert_eq!(entry.topic.last_id, want.topic.last_id, "last of {}", t);
            prop_assert_eq!(&entry.topic.last, &want.topic.last, "preview of {}", t);
            prop_assert_eq!(entry.topic.closed, want.topic.closed, "closed of {}", t);
            prop_assert_eq!(entry.visibility, want.visibility, "visibility of {}", t);
        }
        Ok(())
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Random interleavings of server changes (messages, reads, topic and
    /// message edits, deletions), delayed live events, dispatches and
    /// completions in any order: the list agrees with the independent oracle
    /// at every step (each group applied iff the result's dispatch is newer
    /// than its last snapshot and last live change, removal only on a fresh
    /// `deleted`, tombstones and the unlisted-change mark respected), counts
    /// only hold values a snapshot sent, live changes leave topics due, and
    /// once quiet the list equals the server.
    #[test]
    fn merge_policy(ops in proptest::collection::vec(op(), 1..120)) {
        let mut sim = Sim::new();
        let first = sim.flights.pop().expect("the first page");
        sim.complete(&first, 0)?;
        sim.agree()?;
        for op in ops {
            sim.run(op)?;
        }
        sim.settle()?;
    }
}
