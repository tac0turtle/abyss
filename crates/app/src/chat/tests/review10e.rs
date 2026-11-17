//! Regression tests for the round 17 re-check of task 10d (plan section 15),
//! the chat shell's half. Each fails on the 10d code.

use std::time::Duration;

use super::*;
use crate::chat::retry::AUTO_RESTART_GAP;

/// Finding 5: a refresh that replaces the window (a new view) keeps late
/// live messages above the request's top. The user, scrolled up, reads one
/// of them (110, 7 px into it); the replacement used to reset the list, so
/// the scroll jumped. Now the reading position survives the new view.
#[gpui::test]
fn a_replaced_view_keeps_the_message_being_read(cx: &mut TestAppContext) {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    for id in 101..=130 {
        shell.apply(live(id));
    }
    let row = |shell: &ChatShell, id| shell.window().unwrap().index_of(MsgId(id)).unwrap();
    shell.rows().scroll_to(ListOffset {
        item_ix: row(&shell, 110),
        offset_in_item: px(7.),
    });
    lay_out(cx, &shell);
    // What core's replacing tail sends: the server's whole history now
    // (1..=30, everything newer deleted) and the late messages on top.
    shell.apply(Event::History {
        thread: THREAD,
        view: ViewGen(2),
        dir: HistoryDir::Tail,
        messages: (1..=30).chain(101..=130).map(msg).collect(),
        removed: Vec::new(),
        has_more: false,
    });
    for _ in 0..2 {
        let top = shell.rows().logical_scroll_top();
        let shown = shell.window().unwrap().get(top.item_ix).map(|m| m.id);
        assert_eq!(shown, Some(MsgId(110)), "still reading 110");
        assert_eq!(top.offset_in_item, px(7.), "at the same offset");
        lay_out(cx, &shell);
    }
    // Following the tail, a new view stays at the tail.
    shell.rows().set_follow_mode(gpui::FollowMode::Tail);
    shell.apply(history(3, HistoryDir::Tail, 101..=130));
    assert!(shell.rows().is_following_tail());
}

/// Attempts of each kind a refusing core saw.
#[derive(Default)]
struct Attempts {
    window: std::cell::Cell<usize>,
    viewing: std::cell::Cell<usize>,
    older: std::cell::Cell<usize>,
}

impl Attempts {
    fn count(&self, cmd: &Command) {
        let counter = match cmd {
            Command::Window(_) => &self.window,
            Command::ViewingTail(_) => &self.viewing,
            Command::LoadOlder(_) => &self.older,
            _ => return,
        };
        counter.set(counter.get() + 1);
    }
}

/// Finding 6: window activation used to reset every backoff, so toggling
/// focus against a full queue asked again on every activation. Now an
/// activation keeps a waiting retry's deadline, and restarts retries that
/// gave up at most once per `AUTO_RESTART_GAP`. Task 10f (round 18,
/// finding 3): a toggle reset the focus backoff whenever no deadline was
/// pending (also after it expired or gave up), so focus retries ran at
/// `FIRST_DELAY` forever. Every kind of attempt is now bounded, and
/// recovery still happens (a lower bound: on the 10e code `Window` was
/// asked about 200 times, and without recovery each kind stops at 5).
#[test]
fn rapid_activation_keeps_retries_bounded() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let top = Some(Seen {
        first_row: 0,
        last_message: Some(49),
    });
    // The live tail is on screen (admitted) before the queue fills, so
    // each deactivation also asks for `ViewingTail(None)`.
    let middle = Some(Seen {
        first_row: 20,
        last_message: Some(49),
    });
    shell.frame_seen(middle, now(), &mut core.admit()).unwrap();
    assert!(viewing(&core.take()).contains(&Some(THREAD)));
    let attempts = Attempts::default();
    let loads = &attempts.older;
    let mut refuse = |cmd: Command| {
        attempts.count(&cmd);
        Err(Refused::Busy)
    };
    let step = Duration::from_millis(100);
    let span = Duration::from_secs(100);
    let mut t = now();
    let mut steps = 0u32;
    while t < now() + span {
        let _ = shell.set_focus(WindowState::Inactive, t, &mut refuse);
        let _ = shell.set_focus(WindowState::Active, t, &mut refuse);
        let _ = shell.frame_seen(top, t, &mut refuse);
        steps += 1;
        if steps == 2 {
            // The first refusal's backoff (`FIRST_DELAY`) still runs.
            assert_eq!(loads.get(), 1, "activation kept the deadline");
        }
        t += step;
    }
    let windows = span.as_secs().div_ceil(AUTO_RESTART_GAP.as_secs()) + 1;
    let bound = usize::from(MAX_FAILURES) * usize::try_from(windows).unwrap();
    // At least two recoveries in 100 s: 3 runs of `MAX_FAILURES`.
    let floor = 3 * usize::from(MAX_FAILURES);
    for (kind, counter) in [
        ("Window", &attempts.window),
        ("ViewingTail", &attempts.viewing),
        ("LoadOlder", &attempts.older),
    ] {
        let n = counter.get();
        assert!(
            (floor..=bound).contains(&n),
            "{n} {kind} attempts in 100 s (between {floor} and {bound})"
        );
    }
    let asked = loads.get();
    // The user's Retry is not limited: it asks at once.
    shell.retry_loads();
    let _ = shell.frame_seen(top, t, &mut refuse);
    assert_eq!(loads.get(), asked + 1);
}
