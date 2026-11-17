//! First-unread positioning, the read gate and the divider (plan section
//! 17.5, Stage 2 task 18).

use proptest::prelude::*;

use super::*;
use crate::chat::first_unread::open_command;

/// `CHAT`, with incoming messages unread above `pointer`.
fn unread_at(pointer: i32) -> ChatSummary {
    ChatSummary {
        read: ReadState {
            unread: 5,
            read_inbox_max: MsgId(pointer),
        },
        ..summary()
    }
}

/// A page of `CHAT` in `view`, its messages `outgoing` the user's own.
fn page(view: u64, dir: HistoryDir, ids: impl IntoIterator<Item = i32>, outgoing: &[i32]) -> Event {
    let messages = ids
        .into_iter()
        .map(|id| Message {
            outgoing: outgoing.contains(&id),
            ..msg(id)
        })
        .collect();
    Event::History {
        thread: THREAD,
        view: ViewGen(view),
        dir,
        messages,
        removed: Vec::new(),
        has_more: true,
    }
}

/// Message `id` arrives at the attached tail of view `view`.
fn arrives(view: u64, id: i32, side: Side) -> Event {
    Event::History {
        thread: THREAD,
        view: ViewGen(view),
        dir: HistoryDir::Newer,
        messages: vec![Message {
            outgoing: side == Side::Own,
            ..msg(id)
        }],
        removed: Vec::new(),
        has_more: false,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Own,
    Theirs,
}

/// A frame that painted rows `first..=last`.
fn painted(first: usize, last: usize) -> Option<Seen> {
    Some(Seen {
        first_row: first,
        last_message: Some(last),
    })
}

/// The rows that carry the divider.
fn divider_rows(shell: &ChatShell) -> Vec<usize> {
    (0..shell.row_count())
        .filter(|ix| shell.divider_before(*ix))
        .collect()
}

/// `CHAT` read up to `pointer`, selected: it opens anchored there.
fn open_unread(core: &mut Core, pointer: i32) -> ChatShell {
    let mut shell = ChatShell::default();
    shell.apply(Event::DialogsReset(vec![unread_at(pointer)]));
    shell.select(CHAT, &mut core.admit()).unwrap();
    assert!(matches!(
        core.take().as_slice(),
        [Command::OpenThreadAt { thread: THREAD, after }] if *after == MsgId(pointer)
    ));
    shell
}

/// The `Around` page lands at the divider, right after the read position
/// and before the first newer message, the user's own included; reads wait
/// for the first paint and start above the read position.
#[test]
fn opening_with_unread_lands_at_the_divider() {
    let mut core = Core::default();
    let mut shell = open_unread(&mut core, 60);
    shell.apply(page(1, HistoryDir::Around, 51..=100, &[61, 62]));
    assert_eq!(
        divider_rows(&shell),
        [10],
        "before 61, though it is outgoing"
    );
    let top = shell.rows().logical_scroll_top();
    assert_eq!((top.item_ix, top.offset_in_item), (10, px(0.)));
    assert!(!shell.rows().is_following_tail());
    assert!(shell.window().is_some_and(MessageWindow::has_newer));
    assert!(core.take().is_empty(), "nothing is read before the paint");

    // The first paint shows only the read context: no read mark.
    shell
        .frame_seen(painted(0, 9), now(), &mut core.admit())
        .unwrap();
    assert!(reads(&core.take()).is_empty());
    shell
        .frame_seen(painted(5, 30), now(), &mut core.admit())
        .unwrap();
    let cmds = core.take();
    assert_eq!(reads(&cmds), [81], "a partial read");
    assert!(viewing(&cmds).is_empty(), "detached: not the live tail");

    // Nothing read yet: the page holds only newer messages, its first the
    // user's own, and the server's split proves it first.
    let mut shell = open_unread(&mut core, 0);
    shell.apply(page(1, HistoryDir::Around, 1..=40, &[1]));
    assert_eq!(divider_rows(&shell), [0]);
    assert_eq!(shell.rows().logical_scroll_top().item_ix, 0);
    assert!(!shell.rows().is_following_tail());
}

/// The anchored open's load fails: the thread opens at its tail, which
/// doesn't hold the read position, so it stays at the tail with no divider
/// and reaches the read gate's `Ready` like any open.
#[test]
fn an_anchored_open_falls_back_to_the_tail_when_its_load_ends() {
    let mut core = Core::default();
    let mut shell = open_unread(&mut core, 20);
    let ended = |dir| LoadTarget::History {
        thread: THREAD,
        dir,
    };
    shell.load_ended(ended(HistoryDir::Tail), Some(UserError::Network), now());
    core.full = true;
    assert!(shell.frame_seen(None, now(), &mut core.admit()).is_err());
    core.full = false;
    let later = now() + FIRST_DELAY;
    shell.frame_seen(None, later, &mut core.admit()).unwrap();
    assert!(matches!(
        core.take().as_slice(),
        [Command::OpenThread(THREAD), Command::ViewingTail(None)] | [Command::OpenThread(THREAD)]
    ));
    // Its own failure doesn't fall back again.
    shell.load_ended(ended(HistoryDir::Tail), Some(UserError::Network), now());
    shell.frame_seen(None, later, &mut core.admit()).unwrap();
    assert!(core.take().is_empty());

    shell.apply(history(2, HistoryDir::Tail, 51..=100));
    assert!(divider_rows(&shell).is_empty());
    assert!(shell.rows().is_following_tail());
    shell
        .frame_seen(painted(40, 49), later, &mut core.admit())
        .unwrap();
    let cmds = core.take();
    assert_eq!(reads(&cmds), [100]);
    assert_eq!(viewing(&cmds), [Some(THREAD)]);
}

/// The divider holds still while the thread is shown: reads here or
/// elsewhere and new messages don't move it, and a rebuild keeps it where
/// its window holds it. A send removes it for good, and leaving drops it.
#[test]
fn the_divider_stays_until_a_send_or_leaving() {
    let mut core = Core::default();
    let mut shell = open_unread(&mut core, 60);
    shell.apply(history(1, HistoryDir::Tail, 31..=100));
    assert_eq!(divider_rows(&shell), [30]);
    shell
        .frame_seen(painted(30, 69), now(), &mut core.admit())
        .unwrap();
    assert_eq!(reads(&core.take()), [100]);
    let read = ChatSummary {
        read: ReadState {
            unread: 0,
            read_inbox_max: MsgId(100),
        },
        ..summary()
    };
    shell.apply(Event::ChatBumped(read));
    shell.apply(live(101));
    assert_eq!(divider_rows(&shell), [30]);

    shell.apply(Event::ThreadInvalidated(THREAD));
    shell.apply(history(2, HistoryDir::Tail, 51..=110));
    assert_eq!(divider_rows(&shell), [10], "still after 60");

    send(&mut shell, &mut core, "hi");
    assert!(divider_rows(&shell).is_empty());
    shell
        .set_focus(WindowState::Inactive, now(), &mut core.admit())
        .unwrap();
    shell.apply(arrives(2, 111, Side::Theirs));
    assert!(divider_rows(&shell).is_empty(), "none comes back");

    // Back with the chat read: no divider, and an ordinary open.
    let other = ChatId {
        kind: PeerKind::User,
        id: 2,
    };
    shell.select(other, &mut core.admit()).unwrap();
    assert!(divider_rows(&shell).is_empty());
    core.take();
    shell.select(CHAT, &mut core.admit()).unwrap();
    assert!(matches!(
        core.take().as_slice(),
        [Command::OpenThread(THREAD)]
    ));
    shell.apply(history(3, HistoryDir::Tail, 51..=111));
    assert!(divider_rows(&shell).is_empty());
}

/// Opened with nothing unread, the thread gets a divider once: for an
/// incoming message that arrives while its tail isn't viewed.
#[test]
fn a_late_divider_marks_what_arrives_unseen() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    shell
        .frame_seen(painted(40, 49), now(), &mut core.admit())
        .unwrap();
    shell.apply(live(101));
    assert!(divider_rows(&shell).is_empty(), "the tail is viewed");
    // Inactive, though core wasn't told the tail is gone: it isn't viewed.
    core.full = true;
    let told = shell.set_focus(WindowState::Inactive, now(), &mut core.admit());
    assert!(told.is_err());
    shell.apply(arrives(1, 102, Side::Own));
    assert!(divider_rows(&shell).is_empty(), "the user's own");
    shell.apply(live(103));
    assert_eq!(divider_rows(&shell), [52], "after 102");
    core.full = false;
    shell.apply(live(104));
    shell
        .set_focus(WindowState::Active, now(), &mut core.admit())
        .unwrap();
    shell
        .frame_seen(painted(45, 54), now(), &mut core.admit())
        .unwrap();
    shell
        .set_focus(WindowState::Inactive, now(), &mut core.admit())
        .unwrap();
    shell.apply(live(105));
    assert_eq!(divider_rows(&shell), [52], "one per open");
}

/// The other chat of the property, never unread.
const OTHER: ChatId = ChatId {
    kind: PeerKind::User,
    id: 2,
};

/// What the property does to the shell.
#[derive(Clone, Copy, Debug)]
enum Op {
    /// Selects `CHAT` (unread above 50, or read) or `OTHER`: a switch
    /// closes the open thread.
    Open {
        other: bool,
        unread: bool,
    },
    /// A history event of the open thread (or of the other one).
    History {
        other: bool,
        view: ViewPick,
        dir: HistoryDir,
        first: i32,
        len: i32,
    },
    Invalidate,
    LoadEnded(HistoryDir),
    /// A painted frame showing up to row `last` (as a share of the rows),
    /// or nothing.
    Paint(Option<u8>),
    Focus(WindowState),
}

#[derive(Clone, Copy, Debug)]
enum ViewPick {
    New,
    Same,
    Stale,
}

fn op() -> impl Strategy<Value = Op> {
    let dir = prop_oneof![
        Just(HistoryDir::Tail),
        Just(HistoryDir::Around),
        Just(HistoryDir::Older),
        Just(HistoryDir::Newer),
    ];
    let view = prop_oneof![
        Just(ViewPick::New),
        Just(ViewPick::Same),
        Just(ViewPick::Stale)
    ];
    prop_oneof![
        (any::<bool>(), any::<bool>()).prop_map(|(other, unread)| Op::Open { other, unread }),
        (any::<bool>(), view, dir.clone(), 30..90i32, 0..20i32).prop_map(
            |(other, view, dir, first, len)| Op::History {
                other: other && first % 4 == 0,
                view,
                dir,
                first,
                len,
            }
        ),
        Just(Op::Invalidate),
        dir.prop_map(Op::LoadEnded),
        proptest::option::of(any::<u8>()).prop_map(Op::Paint),
        prop_oneof![Just(WindowState::Active), Just(WindowState::Inactive)].prop_map(Op::Focus),
    ]
}

/// Where the open view stands for reads, in the reference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ViewRead {
    /// No history of the view since the open, the last invalidation or
    /// the last new view.
    #[default]
    NoHistory,
    /// Its first history arrived; no frame was painted since.
    Unpainted,
    Painted,
}

/// The anchored open's first answer, in the reference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Answer {
    /// A plain open, or the anchored one was answered.
    #[default]
    Settled,
    Anchored,
    /// Its load ended: `OpenThread` goes with the next frame.
    FallbackDue,
}

/// The reference: what may be read, kept apart from the shell's own
/// state. A view's reads wait for its first history and then a painted
/// frame; a switch or a new view starts over.
#[derive(Debug)]
struct Model {
    open: Option<ThreadKey>,
    /// The window's view, as core's events set it.
    view: Option<u64>,
    state: ViewRead,
    answer: Answer,
    /// The highest id read, starting at the read position.
    read: Option<MsgId>,
    active: WindowState,
    next_view: u64,
}

impl Model {
    fn new() -> Self {
        Self {
            open: None,
            view: None,
            state: ViewRead::NoHistory,
            answer: Answer::Settled,
            read: None,
            active: WindowState::Active,
            next_view: 0,
        }
    }

    /// The command the open sends, if it opens anything.
    fn open(&mut self, thread: ThreadKey, after: Option<MsgId>) -> Option<Command> {
        if self.open == Some(thread) {
            return None;
        }
        *self = Self {
            open: Some(thread),
            answer: after.map_or(Answer::Settled, |_| Answer::Anchored),
            read: after,
            active: self.active,
            next_view: self.next_view,
            ..Self::new()
        };
        Some(open_command(thread, after))
    }

    /// History of the open thread in `view`, which the window takes when
    /// it starts its first view or isn't stale.
    fn history(&mut self, view: u64, dir: HistoryDir) {
        let accepted = match self.view {
            None => matches!(dir, HistoryDir::Tail | HistoryDir::Around),
            Some(cur) => view >= cur,
        };
        if accepted && self.view != Some(view) {
            self.view = Some(view);
            self.state = ViewRead::Unpainted;
            self.answer = Answer::Settled;
        }
    }

    /// The view `pick` names.
    fn pick(&mut self, pick: ViewPick) -> u64 {
        match pick {
            ViewPick::New => {
                self.next_view += 1;
                self.next_view
            }
            ViewPick::Same => self.view.unwrap_or(0),
            ViewPick::Stale => self.view.unwrap_or(0).saturating_sub(1),
        }
    }
}

/// What the reference expects the shell to send for one step.
#[derive(Debug, Default)]
struct Expect {
    open: Option<Command>,
    read: Option<MsgId>,
    /// Whether the step was a frame, the only place reads go.
    frame: bool,
}

fn select(
    shell: &mut ChatShell,
    core: &mut Core,
    model: &mut Model,
    chat: ChatId,
    after: Option<MsgId>,
) -> Expect {
    let summary = match after {
        Some(after) => unread_at(after.0),
        None => ChatSummary {
            id: chat,
            ..summary()
        },
    };
    shell.apply(Event::ChatBumped(summary));
    let open = model.open(ThreadKey::whole(chat), after);
    shell.select(chat, &mut core.admit()).unwrap();
    Expect {
        open,
        ..Expect::default()
    }
}

fn frame(shell: &mut ChatShell, core: &mut Core, model: &mut Model, share: Option<u8>) -> Expect {
    let rows = shell.window().map_or(0, MessageWindow::len);
    let seen = share.filter(|_| rows > 0).map(|s| Seen {
        first_row: 0,
        last_message: Some(usize::from(s) * rows / 256),
    });
    let open = match model.answer {
        Answer::FallbackDue => {
            model.answer = Answer::Settled;
            model.open.map(Command::OpenThread)
        }
        Answer::Settled | Answer::Anchored => None,
    };
    if model.state == ViewRead::Unpainted && seen.is_some() {
        model.state = ViewRead::Painted;
    }
    let shown = seen
        .and_then(|s| s.last_message)
        .and_then(|ix| shell.window()?.get(ix))
        .map(|m| m.id);
    let read = shown.filter(|id| {
        model.state == ViewRead::Painted
            && model.active == WindowState::Active
            && model.read.is_none_or(|r| *id > r)
    });
    model.read = read.or(model.read);
    shell.frame_seen(seen, now(), &mut core.admit()).unwrap();
    Expect {
        open,
        read,
        frame: true,
    }
}

/// Applies `op` to the shell and the model, and checks what the shell
/// sent against the model.
fn step(
    shell: &mut ChatShell,
    core: &mut Core,
    model: &mut Model,
    op: Op,
) -> Result<(), TestCaseError> {
    let expect = match (op, model.open) {
        (Op::Open { other: true, .. }, _) => select(shell, core, model, OTHER, None),
        (
            Op::Open {
                other: false,
                unread,
            },
            _,
        ) => select(shell, core, model, CHAT, unread.then_some(MsgId(50))),
        (Op::Paint(share), _) => frame(shell, core, model, share),
        (Op::Focus(state), _) => {
            model.active = state;
            shell.set_focus(state, now(), &mut core.admit()).unwrap();
            Expect::default()
        }
        (_, None) => Expect::default(),
        (
            Op::History {
                other,
                view,
                dir,
                first,
                len,
            },
            Some(open),
        ) => {
            let view = model.pick(view);
            let thread = match (other, open.chat == CHAT) {
                (false, _) => open,
                (true, true) => ThreadKey::whole(OTHER),
                (true, false) => THREAD,
            };
            if thread == open {
                model.history(view, dir);
            }
            shell.apply(Event::History {
                thread,
                view: ViewGen(view),
                dir,
                messages: (first..first + len).map(msg).collect(),
                removed: Vec::new(),
                has_more: true,
            });
            Expect::default()
        }
        (Op::Invalidate, Some(open)) => {
            model.state = ViewRead::NoHistory;
            shell.apply(Event::ThreadInvalidated(open));
            Expect::default()
        }
        (Op::LoadEnded(dir), Some(thread)) => {
            if model.answer == Answer::Anchored
                && matches!(dir, HistoryDir::Tail | HistoryDir::Around)
            {
                model.answer = Answer::FallbackDue;
            }
            let target = LoadTarget::History { thread, dir };
            shell.load_ended(target, Some(UserError::Network), now());
            Expect::default()
        }
    };
    check(&core.take(), model, &expect)
}

/// Checks the opens and read marks `cmds` holds against `expect`.
fn check(cmds: &[Command], model: &Model, expect: &Expect) -> Result<(), TestCaseError> {
    let debug = |c: &Command| format!("{c:?}");
    let opens: Vec<String> = cmds
        .iter()
        .filter(|c| matches!(c, Command::OpenThread(_) | Command::OpenThreadAt { .. }))
        .map(debug)
        .collect();
    let expected: Vec<String> = expect.open.iter().map(debug).collect();
    prop_assert_eq!(opens, expected);
    let marks: Vec<(ThreadKey, MsgId)> = cmds
        .iter()
        .filter_map(|c| match c {
            Command::MarkRead { thread, max_id } => Some((*thread, *max_id)),
            _ => None,
        })
        .collect();
    prop_assert!(expect.frame || marks.is_empty(), "a read outside a frame");
    let want: Vec<(ThreadKey, MsgId)> = expect
        .read
        .into_iter()
        .filter_map(|id| Some((model.open?, id)))
        .collect();
    prop_assert_eq!(marks, want);
    Ok(())
}

proptest::proptest! {
    /// `MarkRead` goes only in `Ready` for the current view: after the
    /// view's first history and a painted frame, for the open thread, never
    /// after a switch, an invalidation or a new view before that paint, and
    /// never at or below the read position. Every path (anchored, tail,
    /// fallback) reaches `Ready`: a painted frame of an arrived view that
    /// shows something new while active always reads it.
    #[test]
    fn read_marks_wait_for_the_gate(ops in proptest::collection::vec(op(), 1..60)) {
        let mut core = Core::default();
        let mut shell = ChatShell::default();
        let other = ChatSummary { id: OTHER, ..summary() };
        shell.apply(Event::DialogsReset(vec![summary(), other]));
        let mut model = Model::new();
        for op in ops {
            step(&mut shell, &mut core, &mut model, op)?;
        }
    }
}
