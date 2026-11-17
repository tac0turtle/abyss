use std::time::Instant;

use abyss_core::api::{
    Command, ConnState, HistoryDir, LoadTarget, SendState, UserError, WindowState,
};
use abyss_core::model::{Body, ChatKind, LocalId, PeerKind, ThreadKey};
use abyss_core::rich_text::RichText;
use abyss_core::store::ViewGen;
use gpui::{
    AppContext as _, Context, IntoElement, ListOffset, Render, Styled, TestAppContext, Window, div,
    list, point, px, size,
};

use super::retry::{FIRST_DELAY, MAX_DELAY, MAX_FAILURES};
use super::viewport::Seen;
use super::*;

/// A fixed instant: these tests are about order, not time, unless they
/// say otherwise.
fn now() -> Instant {
    thread_local!(static NOW: Instant = Instant::now());
    NOW.with(|n| *n)
}

const CHAT: ChatId = ChatId {
    kind: PeerKind::User,
    id: 1,
};
const THREAD: ThreadKey = ThreadKey::whole(CHAT);

/// Core's command queue: records what it admits, refuses all while full.
#[derive(Default)]
struct Core {
    sent: Vec<Command>,
    full: bool,
    /// Every command offered, admitted or not.
    attempts: usize,
}

impl Core {
    fn admit(&mut self) -> impl FnMut(Command) -> Result<(), Refused> + '_ {
        move |cmd| {
            self.attempts += 1;
            if self.full {
                return Err(Refused::Busy);
            }
            self.sent.push(cmd);
            Ok(())
        }
    }

    fn take(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.sent)
    }
}

fn summary() -> ChatSummary {
    ChatSummary {
        id: CHAT,
        title: "c".into(),
        kind: ChatKind::Private,
        pinned: false,
        unread: 2,
        unread_mentions: 0,
        last: None,
        last_date: 0,
        last_id: None,
        can_send: true,
    }
}

fn read_only() -> ChatSummary {
    ChatSummary {
        can_send: false,
        ..summary()
    }
}

fn msg(id: i32) -> Message {
    Message {
        id: MsgId(id),
        thread: ThreadKey {
            chat: CHAT,
            topic: None,
        },
        sender: None,
        date: i64::from(id),
        edit_date: None,
        outgoing: false,
        reply_to: None,
        body: Body::Text(RichText::plain("m")),
    }
}

fn history(view: u64, dir: HistoryDir, ids: impl IntoIterator<Item = i32>) -> Event {
    Event::History {
        thread: THREAD,
        view: ViewGen(view),
        dir,
        messages: ids.into_iter().map(msg).collect(),
        removed: Vec::new(),
        has_more: true,
    }
}

/// A live message appended to the attached view 1.
fn live(id: i32) -> Event {
    Event::History {
        thread: THREAD,
        view: ViewGen(1),
        dir: HistoryDir::Newer,
        messages: vec![msg(id)],
        removed: Vec::new(),
        has_more: false,
    }
}

fn reconciled(view: u64, newest: i32) -> Event {
    Event::Reconciled {
        thread: THREAD,
        view: ViewGen(view),
        newest: Some(MsgId(newest)),
    }
}

/// A shell with `CHAT` open and messages 51..=100 loaded.
fn open(core: &mut Core) -> ChatShell {
    let mut shell = ChatShell::default();
    shell.apply(Event::DialogsReset(vec![summary()]));
    shell.select(THREAD, &mut core.admit()).unwrap();
    assert!(matches!(
        core.take().as_slice(),
        [Command::OpenThread(THREAD)]
    ));
    shell.select(THREAD, &mut core.admit()).unwrap();
    assert!(core.take().is_empty(), "already open");
    shell.apply(history(1, HistoryDir::Tail, 51..=100));
    shell
}

/// Submits `text`, and core keeps it (`SendAdmitted`).
fn send(shell: &mut ChatShell, core: &mut Core, text: &str) -> LocalId {
    let chat = shell.send_target().unwrap().unwrap();
    let local_id = shell
        .submit(chat, text, 0, &mut core.admit())
        .unwrap()
        .unwrap();
    shell.apply(Event::SendAdmitted { local_id });
    local_id
}

/// Discards `local_id`, and core answers `SendDiscarded`.
fn discard(shell: &mut ChatShell, core: &mut Core, local_id: LocalId) {
    shell.discard(local_id, &mut core.admit()).unwrap();
    shell.apply(Event::SendDiscarded { local_id });
}

fn set(shell: &mut ChatShell, local_id: LocalId, state: SendState) {
    shell.apply(Event::SendState { local_id, state });
}

fn state_of(shell: &ChatShell, ix: usize) -> Option<SendState> {
    match shell.row(ix)? {
        Row::Echo(e) => Some(e.state),
        Row::Message(_) => None,
    }
}

fn reads(cmds: &[Command]) -> Vec<i32> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::MarkRead { max_id, .. } => Some(max_id.0),
            _ => None,
        })
        .collect()
}

fn viewing(cmds: &[Command]) -> Vec<Option<ThreadKey>> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::ViewingTail(thread) => Some(*thread),
            _ => None,
        })
        .collect()
}

#[test]
fn echo_lifecycle_sent_then_replaced_by_the_server_copy() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let id = send(&mut shell, &mut core, "hello");
    assert!(matches!(
        core.take().as_slice(),
        [Command::SendText { text, local_id, .. }] if text == "hello" && *local_id == id
    ));
    assert_eq!(shell.row_count(), 51);
    assert_eq!(shell.rows().item_count(), 51);
    assert_eq!(state_of(&shell, 50), Some(SendState::Sending));
    set(&mut shell, id, SendState::Sent { id: MsgId(101) });
    assert_eq!(
        state_of(&shell, 50),
        Some(SendState::Sent { id: MsgId(101) })
    );
    shell.apply(history(1, HistoryDir::Newer, [101]));
    assert_eq!(shell.row_count(), 51, "the message replaced the echo");
    assert_eq!(shell.rows().item_count(), 51);
    assert!(matches!(shell.row(50), Some(Row::Message(m)) if m.id == MsgId(101)));
}

/// Finding 1 (HIGH): a full queue must not strand a send.
#[test]
fn full_queue_creates_no_echo_and_changes_nothing() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    core.full = true;
    let refused = shell.submit(THREAD, "draft", 0, &mut core.admit());
    assert_eq!(refused, Err(Refused::Busy));
    assert_eq!(shell.row_count(), 50, "no echo for a send core never took");
    assert_eq!(shell.echoes().count(THREAD), 0);
    core.full = false;
    let id = send(&mut shell, &mut core, "draft");
    assert_eq!(id, LocalId(1), "the refused send used up no id");

    // Retry and Discard change the echo only once core admits them.
    set(&mut shell, id, SendState::Failed);
    core.full = true;
    assert_eq!(shell.retry(id, &mut core.admit()), Err(Refused::Busy));
    assert_eq!(state_of(&shell, 50), Some(SendState::Failed), "still Retry");
    assert_eq!(shell.discard(id, &mut core.admit()), Err(Refused::Busy));
    assert_eq!(shell.row_count(), 51, "a refused discard keeps the echo");
    core.full = false;
    shell.retry(id, &mut core.admit()).unwrap();
    assert_eq!(state_of(&shell, 50), Some(SendState::Sending));

    // Opening another chat: refused, the open chat stays.
    let other = ChatId {
        kind: PeerKind::User,
        id: 2,
    };
    core.full = true;
    assert_eq!(
        shell.select(ThreadKey::whole(other), &mut core.admit()),
        Err(Refused::Busy)
    );
    assert_eq!(shell.selected(), Some(THREAD));
}

#[test]
fn failed_send_retries_then_is_discarded() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let id = send(&mut shell, &mut core, "x");
    core.take();
    shell.discard(id, &mut core.admit()).unwrap();
    assert!(core.take().is_empty(), "in flight: nothing to discard");
    set(&mut shell, id, SendState::Failed);
    shell.retry(id, &mut core.admit()).unwrap();
    assert!(matches!(core.take().as_slice(), [Command::RetrySend(r)] if *r == id));
    assert_eq!(state_of(&shell, 50), Some(SendState::Sending));
    set(&mut shell, id, SendState::Unknown { attempts: 3 });
    discard(&mut shell, &mut core, id);
    assert!(matches!(core.take().as_slice(), [Command::DiscardSend(r)] if *r == id));
    assert_eq!(shell.row_count(), 50);
    assert_eq!(shell.rows().item_count(), 50);
}

/// Finding 7: a flood-waiting send waits; Discard works, Retry is core's.
#[test]
fn flood_waiting_send_can_be_discarded_but_not_retried() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let id = send(&mut shell, &mut core, "x");
    core.take();
    set(&mut shell, id, SendState::FloodWait { seconds: 30 });
    assert!(shell.start_countdown(), "a countdown starts");
    assert!(!shell.start_countdown(), "only one ticker");
    shell.retry(id, &mut core.admit()).unwrap();
    assert!(core.take().is_empty(), "core resends it by itself");
    set(&mut shell, id, SendState::FloodWait { seconds: 5 });
    assert!(shell.countdown_tick(), "still waiting");
    discard(&mut shell, &mut core, id);
    assert!(matches!(core.take().as_slice(), [Command::DiscardSend(r)] if *r == id));
    assert_eq!(shell.row_count(), 50);
    assert!(!shell.countdown_tick(), "nothing waits: the ticker stops");
}

/// 4c: past its flood-wait bounds a send ends `Failed` (with
/// `Error(FloodWait)`); Retry asks core to resend the same send.
#[test]
fn exhausted_flood_wait_fails_with_retry_and_discard() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let id = send(&mut shell, &mut core, "x");
    core.take();
    set(&mut shell, id, SendState::FloodWait { seconds: 30 });
    set(&mut shell, id, SendState::Failed);
    shell.apply(Event::Error(abyss_core::api::UserError::FloodWait {
        seconds: 30,
    }));
    assert_eq!(state_of(&shell, 50), Some(SendState::Failed));
    assert!(!shell.countdown_tick(), "no countdown on a failed send");
    shell.retry(id, &mut core.admit()).unwrap();
    assert!(
        matches!(core.take().as_slice(), [Command::RetrySend(r)] if *r == id),
        "the same send (core keeps its random_id)"
    );
    set(&mut shell, id, SendState::Failed);
    shell.discard(id, &mut core.admit()).unwrap();
    assert!(matches!(core.take().as_slice(), [Command::DiscardSend(r)] if *r == id));
}

/// Task 10c, finding 5: reconciles prove nothing about an unconfirmed
/// send (more than a page of newer messages can put it below both tails);
/// it stays until core reports history covering it.
#[test]
fn unconfirmed_echo_stays_until_core_reports_it_covered() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let id = send(&mut shell, &mut core, "x");
    set(&mut shell, id, SendState::SentUnconfirmed);
    shell.apply(Event::ThreadInvalidated(THREAD));
    shell.apply(history(2, HistoryDir::Tail, 160..=209));
    assert_eq!(shell.row_count(), 51, "a new view keeps the echo");
    for _ in 0..3 {
        shell.apply(reconciled(2, 209));
    }
    assert_eq!(shell.row_count(), 51, "the send is below every tail");
    let other = LocalId(id.0 + 1);
    shell.apply(Event::SendsCovered {
        thread: THREAD,
        sends: vec![other],
    });
    assert_eq!(shell.row_count(), 51, "another send");
    shell.apply(history(2, HistoryDir::Older, 101..=159));
    shell.apply(Event::SendsCovered {
        thread: THREAD,
        sends: vec![id],
    });
    assert_eq!(shell.row_count(), 109, "the server copy took its place");
    assert_eq!(shell.rows().item_count(), 109);
    assert_eq!(shell.echoes().count(THREAD), 0);
}

/// Task 10d, finding 5 (replaces 10c's `a_load_ended_by_reconciled_is_asked_again`,
/// which asserted the duplicate): core defers a load asked while a light
/// refresh runs and starts it after the refresh's `Reconciled`. So
/// `Reconciled` ends nothing: asking again would fetch a page twice. The
/// load ends with its `History` or `LoadEnded`.
#[test]
fn reconciled_does_not_end_a_pending_load() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let top = Some(Seen {
        first_row: 0,
        last_message: None,
    });
    let older = |core: &mut Core| {
        core.take()
            .iter()
            .filter(|c| matches!(c, Command::LoadOlder(THREAD)))
            .count()
    };
    shell.frame_seen(top, now(), &mut core.admit()).unwrap();
    assert_eq!(older(&mut core), 1);
    shell.frame_seen(top, now(), &mut core.admit()).unwrap();
    assert_eq!(older(&mut core), 0, "pending");
    shell.apply(reconciled(1, 100));
    shell.frame_seen(top, now(), &mut core.admit()).unwrap();
    assert_eq!(older(&mut core), 0, "still pending: core runs it next");
    // Its answer ends it; still near the top, the next page is asked.
    shell.apply(history(1, HistoryDir::Older, 41..=50));
    shell.frame_seen(top, now(), &mut core.admit()).unwrap();
    assert_eq!(older(&mut core), 1, "answered by its history");
    // Core had nothing to load: parked until the window changes.
    let ended = LoadTarget::History {
        thread: THREAD,
        dir: HistoryDir::Older,
    };
    shell.load_ended(ended, None, now());
    shell.frame_seen(top, now(), &mut core.admit()).unwrap();
    assert_eq!(older(&mut core), 0, "parked");
    assert_eq!(shell.retry_due(now()), None, "no wake-up for a parked load");
    shell.apply(live(101));
    shell.frame_seen(top, now(), &mut core.admit()).unwrap();
    assert_eq!(older(&mut core), 1, "the window changed");
}

/// Task 10c, finding 2: failed loads retry with a bounded backoff, however
/// many errors and redraws follow, and errors of other commands end none.
#[test]
fn failed_history_loads_retry_a_bounded_number_of_times() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let top = Some(Seen {
        first_row: 0,
        last_message: None,
    });
    let ended = LoadTarget::History {
        thread: THREAD,
        dir: HistoryDir::Older,
    };
    let mut asked = 0;
    let mut t = now();
    for _ in 0..1_000 {
        shell.frame_seen(top, t, &mut core.admit()).unwrap();
        let sent = core.take();
        if sent.iter().any(|c| matches!(c, Command::LoadOlder(THREAD))) {
            asked += 1;
            // A send's error says nothing about the load.
            shell.apply(Event::Error(UserError::Rejected));
            shell.frame_seen(top, t, &mut core.admit()).unwrap();
            assert!(core.take().is_empty(), "an unrelated error ends nothing");
            shell.apply(Event::Error(UserError::Busy));
            shell.load_ended(ended, Some(UserError::Busy), t);
        }
        t += std::time::Duration::from_millis(100);
    }
    assert_eq!(
        asked,
        usize::from(MAX_FAILURES),
        "then it waits for a fresh trigger"
    );
    assert_eq!(shell.retry_due(t), None, "and schedules nothing");
    assert!(shell.history_gave_up(), "Retry is offered");
    shell.apply(reconciled(1, 100));
    shell.frame_seen(top, t, &mut core.admit()).unwrap();
    assert!(core.take().is_empty(), "a refresh is no fresh trigger");
    shell.apply(Event::Connection(ConnState::Online));
    shell.frame_seen(top, t, &mut core.admit()).unwrap();
    assert_eq!(core.take().len(), 1, "coming online is one");
    assert!(!shell.history_gave_up());
}

/// Task 10c, finding 1 (HIGH): a refused `Window(Inactive)` still stops
/// read marks and the live tail at once, and is retried after a backoff.
#[test]
fn a_refused_deactivation_stops_reads_and_is_retried() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let at = |last| {
        Some(Seen {
            first_row: 40,
            last_message: Some(last),
        })
    };
    shell.frame_seen(at(49), now(), &mut core.admit()).unwrap();
    assert_eq!(viewing(&core.take()), [Some(THREAD)]);
    core.full = true;
    let refused = shell.set_focus(WindowState::Inactive, now(), &mut core.admit());
    assert_eq!(refused, Err(Refused::Busy));
    shell.apply(live(101));
    core.full = false;
    shell.frame_seen(at(50), now(), &mut core.admit()).unwrap();
    let cmds = core.take();
    assert!(reads(&cmds).is_empty(), "inactive: 101 is not read");
    assert_eq!(viewing(&cmds), [None], "nor is the tail watched");
    assert!(shell.focus_out_of_sync(), "core still thinks it is active");
    let due = shell.retry_due(now()).expect("a retry waits");
    assert_eq!(due, now() + FIRST_DELAY);
    shell.sync_focus(due, &mut core.admit()).unwrap();
    assert!(matches!(
        core.take().as_slice(),
        [Command::Window(WindowState::Inactive)]
    ));
    assert!(!shell.focus_out_of_sync());
    assert_eq!(shell.retry_due(due), None);
    // A frame retries too, once the backoff allows.
    shell
        .set_focus(WindowState::Active, now(), &mut core.admit())
        .unwrap();
    shell.frame_seen(at(50), now(), &mut core.admit()).unwrap();
    assert_eq!(viewing(&core.take()), [Some(THREAD)]);
    core.full = true;
    assert!(
        shell
            .set_focus(WindowState::Inactive, now(), &mut core.admit())
            .is_err()
    );
    core.full = false;
    shell.frame_seen(at(50), due, &mut core.admit()).unwrap();
    assert!(matches!(
        core.take().as_slice(),
        [
            Command::Window(WindowState::Inactive),
            Command::ViewingTail(None)
        ]
    ));
}

#[test]
fn reconcile_drops_a_sent_echo_whose_message_was_deleted() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let kept = send(&mut shell, &mut core, "a");
    let gone = send(&mut shell, &mut core, "b");
    send(&mut shell, &mut core, "c");
    // 99 was deleted on the server before its id reached us.
    shell.apply(Event::Deleted {
        chat: CHAT,
        ids: vec![MsgId(99)],
    });
    set(&mut shell, kept, SendState::Sent { id: MsgId(150) });
    set(&mut shell, gone, SendState::Sent { id: MsgId(99) });
    assert_eq!(shell.row_count(), 52);
    shell.apply(reconciled(7, 100));
    assert_eq!(shell.row_count(), 52, "another view is not on screen");
    shell.apply(reconciled(1, 100));
    assert_eq!(shell.row_count(), 51);
    assert_eq!(
        state_of(&shell, 49),
        Some(SendState::Sent { id: MsgId(150) })
    );
    assert_eq!(state_of(&shell, 50), Some(SendState::Sending));
}

/// A reopened retained chat arrives as one full `Tail` of a new view, which
/// replaces the rows.
#[test]
fn reopening_a_chat_replaces_the_view_with_its_full_tail() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    shell.jump_to_latest(&mut core.admit()).unwrap();
    assert!(matches!(
        core.take().as_slice(),
        [.., Command::OpenThread(THREAD)]
    ));
    shell.apply(history(3, HistoryDir::Tail, 41..=100));
    assert_eq!(shell.row_count(), 60, "replaced, not appended");
    assert_eq!(shell.rows().item_count(), 60);
    assert!(matches!(shell.row(0), Some(Row::Message(m)) if m.id == MsgId(41)));
}

#[test]
fn read_only_and_empty_submissions_send_nothing() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    assert_eq!(shell.submit(THREAD, "  \n", 0, &mut core.admit()), Ok(None));
    assert!(core.take().is_empty());
    shell.apply(Event::DialogsReset(vec![read_only()]));
    assert_eq!(shell.send_target(), Ok(None));
}

#[test]
fn mark_read_and_live_tail_follow_what_is_on_screen() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let at = |last| Seen {
        first_row: 40,
        last_message: Some(last),
    };
    shell
        .frame_seen(Some(at(45)), now(), &mut core.admit())
        .unwrap();
    let cmds = core.take();
    assert_eq!(reads(&cmds), [96]);
    assert!(viewing(&cmds).is_empty(), "scrolled up: not the tail");
    shell
        .frame_seen(Some(at(45)), now(), &mut core.admit())
        .unwrap();
    shell
        .frame_seen(Some(at(30)), now(), &mut core.admit())
        .unwrap();
    assert!(core.take().is_empty(), "already sent; never backwards");
    shell
        .frame_seen(Some(at(49)), now(), &mut core.admit())
        .unwrap();
    let cmds = core.take();
    assert_eq!(reads(&cmds), [100]);
    assert_eq!(viewing(&cmds), [Some(THREAD)]);
    shell
        .set_focus(WindowState::Inactive, now(), &mut core.admit())
        .unwrap();
    assert!(matches!(
        core.take().as_slice(),
        [
            Command::Window(WindowState::Inactive),
            Command::ViewingTail(None)
        ]
    ));
    shell
        .set_focus(WindowState::Inactive, now(), &mut core.admit())
        .unwrap();
    assert!(core.take().is_empty());
    shell.apply(live(101));
    shell
        .frame_seen(Some(at(50)), now(), &mut core.admit())
        .unwrap();
    assert!(core.take().is_empty(), "not while the window is inactive");
    shell
        .set_focus(WindowState::Active, now(), &mut core.admit())
        .unwrap();
    shell
        .frame_seen(Some(at(50)), now(), &mut core.admit())
        .unwrap();
    let cmds = core.take();
    assert_eq!(reads(&cmds), [101]);
    assert_eq!(viewing(&cmds), [Some(THREAD)]);
    // A refused mark is asked again on a frame after the backoff.
    shell.apply(live(102));
    core.full = true;
    assert!(
        shell
            .frame_seen(Some(at(51)), now(), &mut core.admit())
            .is_err()
    );
    core.full = false;
    shell
        .frame_seen(Some(at(51)), now(), &mut core.admit())
        .unwrap();
    assert!(core.take().is_empty(), "the backoff waits");
    shell
        .frame_seen(Some(at(51)), now() + FIRST_DELAY, &mut core.admit())
        .unwrap();
    assert_eq!(reads(&core.take()), [102]);
}

#[test]
fn scrolling_near_the_top_loads_older_once() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let at = |first_row| {
        Some(Seen {
            first_row,
            last_message: None,
        })
    };
    let older = |core: &mut Core| {
        core.take()
            .into_iter()
            .filter(|c| matches!(c, Command::LoadOlder(THREAD)))
            .count()
    };
    shell.frame_seen(at(20), now(), &mut core.admit()).unwrap();
    assert_eq!(older(&mut core), 0);
    core.full = true;
    assert!(shell.frame_seen(at(2), now(), &mut core.admit()).is_err());
    core.full = false;
    shell.frame_seen(at(2), now(), &mut core.admit()).unwrap();
    assert_eq!(older(&mut core), 0, "a refused load waits its backoff");
    let after = now() + FIRST_DELAY;
    shell.frame_seen(at(2), after, &mut core.admit()).unwrap();
    assert_eq!(older(&mut core), 1, "then is asked again");
    shell.frame_seen(at(0), now(), &mut core.admit()).unwrap();
    assert_eq!(older(&mut core), 0, "in flight");
    shell.apply(history(1, HistoryDir::Older, 1..=50));
    assert_eq!(shell.rows().item_count(), 100);
    shell.frame_seen(at(0), now(), &mut core.admit()).unwrap();
    assert_eq!(older(&mut core), 1);
    let ended = LoadTarget::History {
        thread: THREAD,
        dir: HistoryDir::Older,
    };
    shell.load_ended(ended, Some(UserError::Network), now());
    shell.frame_seen(at(0), now(), &mut core.admit()).unwrap();
    assert_eq!(older(&mut core), 0, "a failed load waits a moment");
    let later = now() + FIRST_DELAY;
    shell.frame_seen(at(0), later, &mut core.admit()).unwrap();
    assert_eq!(older(&mut core), 1);
}

/// Finding 2: paging follows `DialogsPaging`, through the shell.
#[test]
fn dialogs_paging_follows_core() {
    let mut core = Core::default();
    let mut shell = ChatShell::default();
    shell.apply(Event::DialogsReset(Vec::new()));
    shell.request_dialogs(now(), &mut core.admit()).unwrap();
    assert!(core.take().is_empty(), "core has not said more exist");
    shell.apply(Event::DialogsPaging { has_more: true });
    core.full = true;
    assert!(shell.request_dialogs(now(), &mut core.admit()).is_err());
    core.full = false;
    let due = shell.retry_due(now()).expect("a retry waits");
    assert_eq!(due, now() + FIRST_DELAY);
    shell.request_dialogs(due, &mut core.admit()).unwrap();
    assert!(matches!(core.take().as_slice(), [Command::LoadDialogs]));
    shell.request_dialogs(due, &mut core.admit()).unwrap();
    assert!(core.take().is_empty(), "in flight");
    shell.apply(Event::DialogsPaging { has_more: true });
    shell.request_dialogs(due, &mut core.admit()).unwrap();
    assert!(
        matches!(core.take().as_slice(), [Command::LoadDialogs]),
        "an all-filtered page leaves the list empty and asks again"
    );
}

#[test]
fn other_chats_events_do_not_touch_the_open_window() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let other = ChatId {
        kind: PeerKind::User,
        id: 2,
    };
    shell.apply(Event::Deleted {
        chat: other,
        ids: vec![MsgId(60)],
    });
    shell.apply(Event::ThreadInvalidated(ThreadKey::whole(other)));
    assert_eq!(shell.row_count(), 50);
    shell.apply(Event::Deleted {
        chat: CHAT,
        ids: vec![MsgId(60)],
    });
    assert_eq!(shell.rows().item_count(), 49);
}

#[test]
fn switching_chats_keeps_echoes_with_their_chat() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    send(&mut shell, &mut core, "x");
    let other = ChatId {
        kind: PeerKind::User,
        id: 2,
    };
    core.take();
    shell
        .select(ThreadKey::whole(other), &mut core.admit())
        .unwrap();
    assert!(
        matches!(core.take().as_slice(), [Command::OpenThread(t)] if *t == ThreadKey::whole(other))
    );
    assert_eq!(shell.rows().item_count(), 0);
    shell.select(THREAD, &mut core.admit()).unwrap();
    assert_eq!(
        shell.rows().item_count(),
        1,
        "the pending send is still shown"
    );
}

/// Task 10d, finding 2: refused frame commands (loads, read marks, the
/// live tail) wait out their backoff and stop after `MAX_FAILURES`, however
/// many frames are drawn. Focus sync keeps its own backoff meanwhile.
#[test]
fn refused_frame_commands_back_off_and_give_up() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    // The top and the newest message on screen: every frame command is due.
    let seen = Some(Seen {
        first_row: 0,
        last_message: Some(49),
    });
    core.full = true;
    core.attempts = 0;
    let mut t = now();
    for _ in 0..1_000 {
        let _ = shell.frame_seen(seen, t, &mut core.admit());
        t += std::time::Duration::from_millis(100);
    }
    assert_eq!(core.attempts, usize::from(MAX_FAILURES), "bounded");
    assert_eq!(shell.retry_due(t), None, "no wake-up once given up");
    assert!(shell.history_gave_up(), "Retry is offered");
    // A refused deactivation is still retried on its own backoff.
    let _ = shell.set_focus(WindowState::Inactive, t, &mut core.admit());
    core.full = false;
    shell
        .frame_seen(seen, t + FIRST_DELAY, &mut core.admit())
        .unwrap();
    assert!(
        matches!(
            core.take().as_slice(),
            [Command::Window(WindowState::Inactive)]
        ),
        "focus is retried while the frame commands have given up"
    );
    // Activation is a fresh trigger: the frame commands go again.
    shell
        .set_focus(WindowState::Active, t, &mut core.admit())
        .unwrap();
    shell.frame_seen(seen, t, &mut core.admit()).unwrap();
    let cmds = core.take();
    assert!(cmds.iter().any(|c| matches!(c, Command::LoadOlder(THREAD))));
    assert_eq!(reads(&cmds), [100]);
    assert!(!shell.history_gave_up());
}

/// Task 10d, finding 3: one wake-up at a time; a stale one is ignored.
#[test]
fn one_wakeup_at_a_time_and_stale_ones_do_nothing() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let t = now();
    let late = shell.schedule_retry(t + MAX_DELAY).expect("first");
    assert_eq!(
        shell.schedule_retry(t + MAX_DELAY * 2),
        None,
        "one is due sooner"
    );
    let soon = shell
        .schedule_retry(t + FIRST_DELAY)
        .expect("an earlier one replaces it");
    assert_ne!(late, soon);
    assert!(!shell.retry_fired(late), "the replaced wake-up is stale");
    assert!(shell.retry_fired(soon));
    assert!(!shell.retry_fired(soon), "fires once");
    // A current wake-up resends focus only once its backoff allows.
    core.full = true;
    assert!(
        shell
            .set_focus(WindowState::Inactive, t, &mut core.admit())
            .is_err()
    );
    core.full = false;
    shell.retry_wakeup(t, &mut core.admit()).unwrap();
    assert!(core.take().is_empty(), "the focus backoff has not ended");
    shell
        .retry_wakeup(t + FIRST_DELAY, &mut core.admit())
        .unwrap();
    assert!(matches!(
        core.take().as_slice(),
        [Command::Window(WindowState::Inactive)]
    ));
}

/// Task 10d, finding 3: replacing a wake-up cancels its timer task, so
/// only one ever runs.
#[gpui::test]
fn a_replaced_wakeup_task_is_cancelled(cx: &mut TestAppContext) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let fired = Arc::new(AtomicUsize::new(0));
    let mut shell = ChatShell::default();
    let t = cx.executor().now();
    let arm = |shell: &mut ChatShell, wait| {
        let id = shell.schedule_retry(t + wait).unwrap();
        let (fired, timer) = (Arc::clone(&fired), cx.executor().timer(wait));
        let task = cx.executor().spawn(async move {
            timer.await;
            fired.fetch_add(1, Ordering::SeqCst);
        });
        shell.arm_retry(id, task);
    };
    arm(&mut shell, MAX_DELAY);
    arm(&mut shell, FIRST_DELAY);
    cx.executor().advance_clock(MAX_DELAY * 2);
    cx.run_until_parked();
    assert_eq!(
        fired.load(Ordering::SeqCst),
        1,
        "the replaced one never ran"
    );
}

/// Task 10d, finding 4: a detached window whose newer loads failed 5 times
/// (refresh skips detached windows, so no `Reconciled` comes) recovers on
/// the user's Retry, on reconnection and on activation. Never on render.
#[test]
fn exhausted_newer_loads_recover_while_detached() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    shell.apply(Event::History {
        thread: THREAD,
        view: ViewGen(1),
        dir: HistoryDir::Newer,
        messages: vec![msg(101)],
        removed: Vec::new(),
        has_more: true,
    });
    let bottom = Some(Seen {
        first_row: 40,
        last_message: Some(50),
    });
    let ended = LoadTarget::History {
        thread: THREAD,
        dir: HistoryDir::Newer,
    };
    let newer = |core: &mut Core| {
        core.take()
            .iter()
            .filter(|c| matches!(c, Command::LoadNewer(THREAD)))
            .count()
    };
    let mut t = now();
    // Draws frames for 30 s, failing every load: 5 asks (the first on the
    // first frame), then none, and Retry is offered.
    let mut exhaust = |shell: &mut ChatShell, core: &mut Core, why: &str| {
        let mut asked = Vec::new();
        for frame in 0..300 {
            shell.frame_seen(bottom, t, &mut core.admit()).unwrap();
            if newer(core) > 0 {
                asked.push(frame);
                shell.load_ended(ended, Some(UserError::Network), t);
            }
            t += std::time::Duration::from_millis(100);
        }
        assert_eq!(asked.len(), usize::from(MAX_FAILURES), "{why}");
        assert_eq!(asked.first(), Some(&0), "{why}: asked at once");
        assert!(shell.history_gave_up(), "Retry is offered");
        assert!(shell.window().unwrap().has_newer(), "still detached");
        t
    };
    let at = exhaust(&mut shell, &mut core, "first");
    shell.frame_seen(bottom, at, &mut core.admit()).unwrap();
    assert_eq!(newer(&mut core), 0, "rendering is no trigger");
    shell.retry_loads();
    exhaust(&mut shell, &mut core, "the user's Retry");
    shell.apply(Event::Connection(ConnState::Online));
    let at = exhaust(&mut shell, &mut core, "back online");
    shell
        .set_focus(WindowState::Inactive, at, &mut core.admit())
        .unwrap();
    shell
        .set_focus(WindowState::Active, at, &mut core.admit())
        .unwrap();
    core.take();
    exhaust(&mut shell, &mut core, "activation");
}

/// The message list with fixed 20 px rows, as a view (a list needs one),
/// each row tracked as the chat pane tracks it.
struct Rows(ListState, viewport::Painted);

impl Render for Rows {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        let painted = self.1.clone();
        list(self.0.clone(), move |ix, _, _| {
            painted
                .track(ix, div().h(px(20.)))
                .w_full()
                .into_any_element()
        })
        .size_full()
    }
}

/// Lays the message list out in a 200 px viewport with 20 px rows, the way
/// the chat pane does, and reports what it shows.
fn lay_out(cx: &mut TestAppContext, shell: &ChatShell) -> Option<Seen> {
    let rows = shell.rows().clone();
    let painted = shell.painted().clone();
    let window = cx.add_empty_window();
    let view = window.update(|_, cx| cx.new(|_| Rows(rows, painted)));
    window.draw(point(px(0.), px(0.)), size(px(300.), px(200.)), |_, _| {
        view.into_any_element()
    });
    shell.seen()
}

/// Finding 3: what counts as seen comes from the real list layout. Rows
/// above the viewport are not seen: with echoes filling the screen, the
/// newest message is above it and is neither marked read nor the tail.
#[gpui::test]
fn only_rows_inside_the_viewport_count_as_seen(cx: &mut TestAppContext) {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    for n in 0..15 {
        send(&mut shell, &mut core, &format!("e{n}"));
    }
    core.take();
    let seen = lay_out(cx, &shell).unwrap();
    assert_eq!(seen.first_row, 55, "following the tail: the last 10 rows");
    assert_eq!(seen.last_message, None, "only echoes on screen");
    shell
        .frame_seen(Some(seen), now(), &mut core.admit())
        .unwrap();
    assert!(core.take().is_empty(), "message 100 is above the viewport");

    shell.rows().scroll_to(ListOffset {
        item_ix: 40,
        offset_in_item: px(0.),
    });
    let seen = lay_out(cx, &shell).unwrap();
    assert_eq!((seen.first_row, seen.last_message), (40, Some(49)));
    shell
        .frame_seen(Some(seen), now(), &mut core.admit())
        .unwrap();
    let cmds = core.take();
    assert_eq!(reads(&cmds), [100]);
    assert_eq!(viewing(&cmds), [Some(THREAD)]);
    assert_eq!(shell.viewing(), Some(THREAD));

    shell.rows().scroll_to(ListOffset {
        item_ix: 20,
        offset_in_item: px(10.),
    });
    let seen = lay_out(cx, &shell).unwrap();
    assert_eq!((seen.first_row, seen.last_message), (20, Some(30)));
    shell
        .frame_seen(Some(seen), now(), &mut core.admit())
        .unwrap();
    assert_eq!(viewing(&core.take()), [None], "scrolled away from the tail");
}

mod review10e;
mod review10f;
mod review6j;
mod review6k;
