//! Regression tests for the round 18 re-check of task 10e (plan section 15),
//! the chat shell's half. Each fails on the 10e code. (Finding 3 is
//! covered by the strengthened `review10e::rapid_activation_keeps_retries_bounded`.)

use std::time::Duration;

use super::*;
use crate::chat::retry::AUTO_RESTART_GAP;

const OTHER: ChatId = ChatId {
    kind: PeerKind::User,
    id: 2,
};
const OTHER_THREAD: ThreadKey = ThreadKey::whole(OTHER);

fn in_other(m: Message) -> Message {
    Message {
        thread: ThreadKey {
            chat: OTHER,
            topic: None,
        },
        ..m
    }
}

/// Refuses every frame command but admits focus changes, counting
/// `LoadOlder` attempts per thread.
fn frames_refused(
    sent: &std::cell::RefCell<Vec<ThreadKey>>,
) -> impl FnMut(Command) -> Result<(), Refused> + '_ {
    move |cmd| match cmd {
        Command::Window(_) | Command::ViewingTail(_) | Command::OpenThread(_) => Ok(()),
        Command::LoadOlder(thread) => {
            sent.borrow_mut().push(thread);
            Err(Refused::Busy)
        }
        _ => Err(Refused::Busy),
    }
}

/// Draws a frame showing the top every 100 ms from `from` for `span`.
fn frames(shell: &mut ChatShell, admit: Admit<'_>, from: Instant, span: Duration) -> Instant {
    let top = Some(Seen {
        first_row: 0,
        last_message: Some(49),
    });
    let mut t = from;
    while t < from + span {
        let _ = shell.frame_seen(top, t, admit);
        t += Duration::from_millis(100);
    }
    t
}

fn toggle(shell: &mut ChatShell, admit: Admit<'_>, t: Instant) {
    let _ = shell.set_focus(WindowState::Inactive, t, admit);
    let _ = shell.set_focus(WindowState::Active, t, admit);
}

/// Finding 4: chat A's retries gave up and an activation recovered them,
/// using the shell-wide allowance. Chat B's retries then gave up too, and
/// its activation came within `AUTO_RESTART_GAP`: the recovery was held
/// back, and nothing woke the shell when the gap ended, so chat B stayed
/// stuck until the next activation or Retry. Now the held recovery is
/// scheduled for the gap's end and runs from the retry wake-up.
#[test]
fn a_held_back_recovery_runs_when_the_gap_ends() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let sent = std::cell::RefCell::new(Vec::new());
    let mut admit = frames_refused(&sent);
    // Chat A gives up, then an activation recovers it at `t0`.
    let t0 = frames(&mut shell, &mut admit, now(), Duration::from_secs(20));
    assert!(shell.history_gave_up());
    toggle(&mut shell, &mut admit, t0);
    assert!(!shell.history_gave_up(), "recovered");
    // Chat B gives up as well.
    shell.select(OTHER_THREAD, &mut admit).unwrap();
    shell.apply(Event::History {
        thread: OTHER_THREAD,
        view: ViewGen(2),
        dir: HistoryDir::Tail,
        messages: (51..=100).map(|id| in_other(msg(id))).collect(),
        removed: Vec::new(),
        has_more: true,
    });
    let t1 = frames(&mut shell, &mut admit, t0, Duration::from_secs(20));
    assert!(shell.history_gave_up());
    let asked = sent.borrow().iter().filter(|c| **c == OTHER_THREAD).count();
    assert_eq!(asked, usize::from(MAX_FAILURES));
    // Activation within the gap: held back, but a wake-up is due at its end.
    toggle(&mut shell, &mut admit, t1);
    assert!(shell.history_gave_up(), "the gap holds");
    let end = t0 + AUTO_RESTART_GAP;
    assert_eq!(shell.retry_due(t1), Some(end), "woken when the gap ends");
    // The wake-up recovers chat B, and the next frame asks again.
    shell.retry_wakeup(end, &mut admit).unwrap();
    assert!(!shell.history_gave_up(), "recovered at the gap's end");
    frames(&mut shell, &mut admit, end, Duration::from_millis(100));
    let asked = sent.borrow().iter().filter(|c| **c == OTHER_THREAD).count();
    assert_eq!(asked, usize::from(MAX_FAILURES) + 1);
    assert_eq!(
        shell.retry_due(end),
        Some(end + FIRST_DELAY),
        "bounded again"
    );
}

/// Optional item 5: the message being read (110) was deleted by the time
/// a replacing view arrives. The reading position cannot be kept, so the
/// list falls back to the new view's default position (the newest
/// messages, as for any reset) rather than a stale row or offset.
#[gpui::test]
fn a_replaced_view_without_the_anchor_falls_back_to_the_tail(cx: &mut TestAppContext) {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    for id in 101..=130 {
        shell.apply(live(id));
    }
    let len = |shell: &ChatShell| shell.window().unwrap().len();
    let ix = shell.window().unwrap().index_of(MsgId(110)).unwrap();
    shell.rows().scroll_to(ListOffset {
        item_ix: ix,
        offset_in_item: px(7.),
    });
    lay_out(cx, &shell);
    shell.apply(Event::History {
        thread: THREAD,
        view: ViewGen(2),
        dir: HistoryDir::Tail,
        messages: (1..=30)
            .chain(101..=109)
            .chain(111..=130)
            .map(msg)
            .collect(),
        removed: Vec::new(),
        has_more: false,
    });
    assert_eq!(shell.window().unwrap().index_of(MsgId(110)), None);
    for _ in 0..2 {
        let seen = lay_out(cx, &shell).expect("rows are laid out");
        assert_eq!(seen.last_message, Some(len(&shell) - 1), "at the tail");
        assert!(seen.first_row < len(&shell), "a valid row on screen");
    }
}

/// Finding 4: a held-back recovery is not run while the window is
/// inactive; activation after the gap recovers as before.
#[test]
fn a_held_back_recovery_waits_for_an_active_window() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let sent = std::cell::RefCell::new(Vec::new());
    let mut admit = frames_refused(&sent);
    let t0 = frames(&mut shell, &mut admit, now(), Duration::from_secs(20));
    toggle(&mut shell, &mut admit, t0);
    let t1 = frames(&mut shell, &mut admit, t0, Duration::from_secs(20));
    toggle(&mut shell, &mut admit, t1);
    let _ = shell.set_focus(WindowState::Inactive, t1, &mut admit);
    let end = t0 + AUTO_RESTART_GAP;
    shell.retry_wakeup(end, &mut admit).unwrap();
    assert!(shell.history_gave_up(), "not while inactive");
    assert_eq!(shell.retry_due(end), None, "and no wake-up loop");
    let _ = shell.set_focus(WindowState::Active, end, &mut admit);
    assert!(
        !shell.history_gave_up(),
        "activation after the gap recovers"
    );
}
