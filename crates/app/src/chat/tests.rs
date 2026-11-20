use std::time::Instant;

use abyss_core::api::{Command, HistoryDir, LoadTarget, SendState, UserError, WindowState};
use abyss_core::model::{
    Body, ChatKind, LocalId, MsgId, PeerFolder, PeerKind, ReadState, ThreadKey,
};
use abyss_core::rich_text::RichText;
use abyss_core::store::ViewGen;

use super::retry::FIRST_DELAY;
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
        kind: ChatKind::Private { contact: false },
        pinned: false,
        // Nothing unread: it opens at its tail (plan section 17.5).
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
    shell.select(CHAT, &mut core.admit()).unwrap();
    assert!(matches!(
        core.take().as_slice(),
        [Command::OpenThread(THREAD)]
    ));
    shell.select(CHAT, &mut core.admit()).unwrap();
    assert!(core.take().is_empty(), "already open");
    shell.apply(history(1, HistoryDir::Tail, 51..=100));
    shell
}

#[test]
fn a_banner_opens_another_forums_topic_with_one_command_slot() {
    let a = ChatId {
        kind: PeerKind::Channel,
        id: 2,
    };
    let b = ChatId {
        kind: PeerKind::Channel,
        id: 3,
    };
    let topic = TopicId::GENERAL;
    let mut shell = ChatShell::default();
    let mut core = Core::default();
    shell.apply(Event::DialogsReset(
        [a, b]
            .into_iter()
            .map(|id| ChatSummary {
                id,
                kind: ChatKind::Supergroup {
                    forum: true,
                    gigagroup: false,
                },
                ..summary()
            })
            .collect(),
    ));
    shell.select(a, &mut core.admit()).unwrap();
    assert!(matches!(core.take().as_slice(), [Command::OpenForum(chat)] if *chat == a));

    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let mut admit = |cmd| tx.try_send(cmd).map_err(|_| Refused::Busy);
    admit(Command::LoadTopics(a)).unwrap();
    assert_eq!(
        shell.show_forum_topic(b, topic, &mut admit),
        Err(Refused::Busy)
    );
    assert_eq!(
        shell.pane(),
        Pane::Forum {
            chat: a,
            open: None
        }
    );
    assert!(matches!(rx.try_recv(), Ok(Command::LoadTopics(chat)) if chat == a));
    assert!(
        rx.try_recv().is_err(),
        "core received no navigation away from A"
    );

    assert_eq!(shell.show_forum_topic(b, topic, &mut admit), Ok(()));
    assert_eq!(
        shell.pane(),
        Pane::Forum {
            chat: b,
            open: Some(topic)
        }
    );
    assert!(matches!(
        rx.try_recv(),
        Ok(Command::OpenForumTopic { chat, topic: t }) if chat == b && t == topic
    ));
    assert!(rx.try_recv().is_err());
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
    shell.select(other, &mut core.admit()).unwrap();
    assert!(
        matches!(core.take().as_slice(), [Command::OpenThread(t)] if *t == ThreadKey::whole(other))
    );
    assert_eq!(shell.rows().item_count(), 0);
    shell.select(CHAT, &mut core.admit()).unwrap();
    assert_eq!(
        shell.rows().item_count(),
        1,
        "the pending send is still shown"
    );
}

mod covered;
mod first_unread;

/// A shell with `CHAT` open, loaded and empty (a new chat).
fn open_empty(core: &mut Core) -> ChatShell {
    let mut shell = ChatShell::default();
    shell.apply(Event::DialogsReset(vec![summary()]));
    shell.select(CHAT, &mut core.admit()).unwrap();
    shell.apply(history(1, HistoryDir::Tail, 0..0));
    assert_eq!(shell.row_count(), 0, "loaded and empty");
    assert!(shell.announcement().is_none());
    shell
}

/// Review A42: an attached, loaded window can hold no messages (a new chat),
/// and then its first incoming message has no lower bound to beat: it is
/// announced. The user's own message is not.
#[test]
fn the_first_incoming_message_in_an_empty_chat_is_announced() {
    let mut core = Core::default();
    let mut shell = open_empty(&mut core);
    shell.apply(live(2));
    assert_eq!(
        shell.announcement().map(|a| a.text.as_ref()),
        Some("New message from c"),
        "named by the chat when the sender is unknown"
    );

    let mut shell = open_empty(&mut core);
    shell.apply(Event::History {
        thread: THREAD,
        view: ViewGen(1),
        dir: HistoryDir::Newer,
        messages: vec![Message {
            outgoing: true,
            ..msg(1)
        }],
        removed: Vec::new(),
        has_more: false,
    });
    assert!(shell.announcement().is_none(), "the user's own message");
}
