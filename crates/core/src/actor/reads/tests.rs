//! The read intents against the dialog rows (stage 2 plan, sections
//! 17.6.1 and 17.8): a reference model over random interleavings.

use std::time::Duration;

use proptest::prelude::*;
use tokio::time::Instant;

use super::{Outcome, Reads};
use crate::api::ClientGen;
use crate::model::{ChatId, ChatKind, ChatSummary, MsgId, PeerFolder, PeerKind, ReadState};
use crate::store::{BadgeSeq, Badges, Dialogs, Last, Rest, Unread};

fn chat(id: i64) -> ChatId {
    ChatId {
        kind: PeerKind::User,
        id,
    }
}

/// The chat being read, and one that stays listed beside it.
const READ: i64 = 1;
const OTHER: i64 = 2;

fn thread() -> crate::model::ThreadKey {
    crate::model::ThreadKey::whole(chat(READ))
}

#[derive(Clone, Copy, Debug)]
enum Op {
    /// The user saw the chat up to this many messages below its top.
    Admit(i32),
    Flush,
    /// An attempt in flight ends.
    Complete(usize, Ending),
    /// A new instance: attempts in flight become stale.
    Rebuild,
    /// An incoming message, counted (Stage 1).
    Message,
    /// A read update from the server, up to this many below the top.
    ServerRead(i32),
    /// A snapshot (a dialogs page or `peer_dialogs`) is dispatched.
    Dispatch,
    /// A dispatched snapshot lands with the server state at some point
    /// since, as a first page, a later page or a refresh.
    Land(usize, usize, Install),
    /// The chat leaves the list; a later page admits it again.
    Leave,
}

#[derive(Clone, Copy, Debug)]
enum Ending {
    Landed,
    Unknown,
    FloodWait,
    Refused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Install {
    FirstPage,
    LaterPage,
    Refresh,
}

fn op() -> impl Strategy<Value = Op> {
    let ending = prop_oneof![
        3 => Just(Ending::Landed),
        1 => Just(Ending::Unknown),
        1 => Just(Ending::FloodWait),
        1 => Just(Ending::Refused),
    ];
    let install = prop_oneof![
        Just(Install::FirstPage),
        Just(Install::LaterPage),
        Just(Install::Refresh),
    ];
    prop_oneof![
        4 => (0i32..4).prop_map(Op::Admit),
        3 => Just(Op::Flush),
        4 => (0usize..4, ending).prop_map(|(i, e)| Op::Complete(i, e)),
        1 => Just(Op::Rebuild),
        3 => Just(Op::Message),
        1 => (0i32..4).prop_map(Op::ServerRead),
        2 => Just(Op::Dispatch),
        3 => (0usize..4, 0usize..8, install).prop_map(|(i, p, k)| Op::Land(i, p, k)),
        1 => Just(Op::Leave),
    ]
}

/// The server's side of the read chat.
#[derive(Clone, Debug, Default)]
struct Server {
    /// Incoming message ids, ascending.
    incoming: Vec<i32>,
    top: i32,
    pointer: i32,
}

impl Server {
    fn unread(&self) -> u32 {
        let above = self
            .incoming
            .iter()
            .filter(|id| **id > self.pointer)
            .count();
        u32::try_from(above).unwrap()
    }

    fn summary(&self) -> ChatSummary {
        ChatSummary {
            read: ReadState {
                unread: self.unread(),
                read_inbox_max: MsgId(self.pointer),
            },
            last_id: (self.top > 0).then_some(MsgId(self.top)),
            last_date: i64::from(self.top),
            ..row(READ)
        }
    }
}

fn row(id: i64) -> ChatSummary {
    ChatSummary {
        id: chat(id),
        title: "chat".into(),
        kind: ChatKind::Private { contact: false },
        pinned: false,
        read: ReadState::default(),
        unread_mentions: 0,
        last: None,
        last_date: 0,
        last_id: None,
        can_send: true,
        muted_until: None,
        scope_muted_until: 0,
        folder: PeerFolder::Main,
        reading: None,
        topic_unread: None,
    }
}

/// Where a count came from. A live change outranks a snapshot dispatched
/// at its stamp (that snapshot was sent before it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Origin {
    Snapshot,
    Live,
}

/// The source of a count: a snapshot's dispatch stamp or the clock when a
/// live change landed, then its origin; the later of two live changes at
/// one stamp wins.
type CountSource = (BadgeSeq, Origin, u64);

/// The reference model, from the history of what was applied alone.
#[derive(Debug, Default)]
struct Model {
    listed: bool,
    /// The newest count source applied, and its value.
    count: Option<(CountSource, u32)>,
    /// The highest pointer the server stated or a landed read proved,
    /// since the chat was last forgotten.
    pointer: Option<i32>,
    /// The intent: the highest id admitted and not yet settled.
    intent: Option<MsgId>,
    /// The attempt in flight: its instance and id.
    flight: Option<(ClientGen, MsgId)>,
    order: u64,
}

impl Model {
    fn count(&mut self, stamp: BadgeSeq, origin: Origin, value: u32) {
        self.order += 1;
        let source = (stamp, origin, self.order);
        if self.count.is_none_or(|(newest, _)| source > newest) {
            self.count = Some((source, value));
        }
    }

    fn raise(&mut self, pointer: i32) {
        self.pointer = Some(self.pointer.map_or(pointer, |p| p.max(pointer)));
    }
}

struct Sim {
    dialogs: Dialogs,
    clock: Badges,
    reads: Reads,
    cgen: ClientGen,
    now: Instant,
    /// Attempts in flight, stale ones included.
    flights: Vec<(ClientGen, MsgId)>,
    history: Vec<Server>,
    /// Snapshots in flight: their stamp and the server state they start at.
    snapshots: Vec<(BadgeSeq, usize)>,
    model: Model,
}

impl Sim {
    fn new() -> Self {
        let mut dialogs = Dialogs::default();
        dialogs.reset(vec![row(READ), row(OTHER)]);
        let mut model = Model {
            listed: true,
            ..Model::default()
        };
        model.count(BadgeSeq::default(), Origin::Snapshot, 0);
        model.raise(0);
        Self {
            dialogs,
            clock: Badges::default(),
            reads: Reads::default(),
            cgen: ClientGen(1),
            now: Instant::now(),
            flights: Vec::new(),
            history: vec![Server::default()],
            snapshots: Vec::new(),
            model,
        }
    }

    fn server(&self) -> &Server {
        self.history.last().unwrap()
    }

    fn change(&mut self, f: impl FnOnce(&mut Server)) {
        let mut next = self.server().clone();
        f(&mut next);
        self.history.push(next);
    }

    /// Re-derives the row's overlay, as the actor does after every change
    /// of an intent.
    fn project(&mut self) {
        let reading = self.reads.outstanding(chat(READ));
        self.dialogs.set_reading(chat(READ), reading);
    }

    fn run(&mut self, op: Op) {
        match op {
            Op::Admit(below) => {
                let id = MsgId((self.server().top - below).max(0));
                if self.reads.admit(thread(), id).is_ok() {
                    self.model.intent = Some(self.model.intent.map_or(id, |i| i.max(id)));
                }
                self.project();
            }
            Op::Flush => {
                self.now += Duration::from_secs(60);
                for (thread, max_id, _) in self.reads.due(self.cgen, self.now) {
                    self.reads.queued(thread, self.cgen, max_id);
                    self.flights.push((self.cgen, max_id));
                    self.model.flight = Some((self.cgen, max_id));
                }
            }
            Op::Complete(i, ending) => {
                if self.flights.is_empty() {
                    return;
                }
                let (cgen, max_id) = self.flights.remove(i % self.flights.len());
                self.complete(cgen, max_id, ending);
            }
            Op::Rebuild => self.cgen = ClientGen(self.cgen.0 + 1),
            Op::Message => self.message(),
            Op::ServerRead(below) => self.server_read(below),
            Op::Dispatch => {
                let stamp = self.clock.dispatch();
                self.snapshots.push((stamp, self.history.len() - 1));
            }
            Op::Land(i, pick, install) => {
                if self.snapshots.is_empty() {
                    return;
                }
                let (stamp, from) = self.snapshots.remove(i % self.snapshots.len());
                let at = from + pick % (self.history.len() - from);
                self.land(stamp, at, install);
            }
            // A chat that isn't listed keeps what snapshots said about it.
            Op::Leave if self.model.listed => {
                self.dialogs.reset(vec![row(OTHER)]);
                self.model.listed = false;
                self.model.count = None;
                self.model.pointer = None;
            }
            Op::Leave => {}
        }
    }

    /// An incoming message, counted as Stage 1 counts it.
    fn message(&mut self) {
        let id = self.server().top + 1;
        self.change(|s| {
            s.top = id;
            s.incoming.push(id);
        });
        let last = Last {
            id: MsgId(id),
            date: i64::from(id),
            preview: crate::model::Preview::Service,
        };
        let now = self.clock.now();
        if matches!(
            self.dialogs.bump(chat(READ), last, Unread::Increment),
            crate::store::Bump::Updated(_)
        ) {
            self.dialogs.unread_counted(chat(READ), now);
            let counted = self.model.count.map_or(0, |(_, v)| v).saturating_add(1);
            self.model.count(now, Origin::Live, counted);
        }
    }

    /// A read update from the server, `below` messages under the top.
    fn server_read(&mut self, below: i32) {
        let to = (self.server().top - below).max(0);
        self.change(|s| s.pointer = s.pointer.max(to));
        let (unread, pointer) = (self.server().unread(), self.server().pointer);
        let now = self.clock.now();
        if self
            .dialogs
            .server_read(chat(READ), unread, MsgId(pointer), now)
            .is_some()
        {
            self.model.count(now, Origin::Live, unread);
            self.model.raise(pointer);
        }
    }

    /// An attempt ends, as `Actor::on_read` handles it.
    fn complete(&mut self, cgen: ClientGen, max_id: MsgId, ending: Ending) {
        let outcome = match ending {
            Ending::Landed => Outcome::Landed,
            Ending::Unknown => Outcome::Unknown,
            Ending::FloodWait => Outcome::FloodWait(Duration::from_secs(5)),
            Ending::Refused => Outcome::Refused,
        };
        if outcome == Outcome::Landed {
            self.change(|s| s.pointer = s.pointer.max(max_id.0));
        }
        let ours = self.model.flight == Some((cgen, max_id));
        let ended = self.reads.ended(cgen, thread(), max_id, outcome, self.now);
        assert_eq!(ended, ours, "only the intent's attempt in flight counts");
        if !ended {
            return;
        }
        self.model.flight = None;
        if matches!(outcome, Outcome::Landed | Outcome::Refused)
            && self.model.intent.is_some_and(|i| i <= max_id)
        {
            self.model.intent = None;
        }
        // Listed or not: a chat with a window only remembers it for the
        // page that admits it.
        if outcome == Outcome::Landed && cgen == self.cgen {
            self.dialogs
                .read_landed(chat(READ), max_id, self.clock.now());
            self.model.raise(max_id.0);
        }
        self.project();
    }

    /// A snapshot dispatched at `stamp` lands with the server state at
    /// `at`, guarded and projected as `Actor::guard_snapshot` does. A page
    /// admits the chat when it isn't listed; a refresh only changes a
    /// listed row.
    fn land(&mut self, stamp: BadgeSeq, at: usize, install: Install) {
        let server = self.history[at].clone();
        let mut page = vec![server.summary(), row(OTHER)];
        self.dialogs.guard(&mut page, stamp, |_| None);
        self.reads.project(&mut page);
        let admits = install != Install::Refresh || self.model.listed;
        self.model.count(stamp, Origin::Snapshot, server.unread());
        self.model.raise(server.pointer);
        match install {
            Install::FirstPage => {
                self.dialogs.merge_first_page(page, Rest::More);
            }
            Install::LaterPage => {
                self.dialogs.append(page, Rest::More);
            }
            Install::Refresh => {
                for fresh in page {
                    self.dialogs.refresh(fresh);
                }
            }
        }
        self.model.listed |= admits;
    }

    /// The row against the model, after every step.
    fn check(&self) -> Result<(), TestCaseError> {
        prop_assert_eq!(self.reads.outstanding(chat(READ)), self.model.intent);
        let row = self.dialogs.get(chat(READ));
        prop_assert_eq!(row.is_some(), self.model.listed);
        let Some(row) = row else {
            return Ok(());
        };
        prop_assert_eq!(
            Some(row.read.read_inbox_max.0),
            self.model.pointer,
            "the pointer is the highest evidence"
        );
        prop_assert_eq!(
            Some(row.read.unread),
            self.model.count.map(|(_, v)| v),
            "the count is the newest server value plus counted messages"
        );
        prop_assert_eq!(row.reading, self.model.intent, "the overlay is projected");
        match self.model.intent {
            // Settled: the confirmed count shows again.
            None => prop_assert_eq!(row.displayed_unread(), row.read.unread),
            Some(i) if row.last_id.is_none_or(|last| i >= last) => {
                prop_assert_eq!(row.displayed_unread(), 0, "a covering intent shows 0");
            }
            Some(_) => prop_assert_eq!(row.displayed_unread(), row.read.unread),
        }
        Ok(())
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Admissions (growing intents), flushes, completions (landed, retried,
    /// refused; current and stale instances) in any order, snapshots
    /// installed as first pages, later pages or refreshes, server read
    /// updates, counted messages, and the chat leaving the list (its reads
    /// still landing, as for a window only) and being admitted again,
    /// against a reference model. After every step: the
    /// pointer is the highest server evidence or landed read; the count is
    /// the newest server value under the stamp rule plus counted messages,
    /// so no read writes one; a completion settles only its own id, and an
    /// intent that grew meanwhile stays; the row's overlay is the intent,
    /// whatever replaced the row; and the displayed count is 0 while the
    /// intent covers the top message, and the confirmed count once the
    /// whole intent has settled.
    #[test]
    fn read_marks_settle_against_the_confirmed_read_state(
        ops in proptest::collection::vec(op(), 1..80)
    ) {
        let mut sim = Sim::new();
        for op in ops {
            sim.run(op);
            sim.check()?;
        }
    }
}
