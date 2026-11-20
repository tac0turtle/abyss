//! A modal overlay over a watched tail owns its `ViewingTail(None)`
//! withdrawal (review round 7, A76): retried on its own, whatever the
//! frame retry (loads, read marks) did before.

use std::time::Duration;

use super::*;
use crate::chat::retry::MAX_DELAY;

fn frame(last: usize) -> Seen {
    Seen {
        first_row: 40,
        last_message: Some(last),
    }
}

/// A shell whose tail is watched and read through 100, with the frame retry
/// then exhausted by five refused read marks. Returns the time of the last
/// refusal.
fn exhausted_frame_retry(core: &mut Core) -> (ChatShell, Instant) {
    let mut shell = open(core);
    shell
        .frame_seen(Some(frame(49)), now(), &mut core.admit())
        .unwrap();
    let cmds = core.take();
    assert_eq!(viewing(&cmds), [Some(THREAD)]);
    shell.apply(live(101));
    core.full = true;
    let mut at = now();
    for _ in 0..5 {
        assert!(
            shell
                .frame_seen(Some(frame(50)), at, &mut core.admit())
                .is_err()
        );
        at += MAX_DELAY;
    }
    assert!(shell.history_gave_up(), "the frame retry gave up");
    core.full = false;
    (shell, at)
}

/// Astra's scenario: the queue frees after the frame retry gave up, then
/// the pane is covered. Core must still be told, a minute later.
#[test]
fn a_cover_after_an_exhausted_frame_retry_still_withdraws_the_tail() {
    let mut core = Core::default();
    let (mut shell, at) = exhausted_frame_retry(&mut core);
    shell.set_covered(Covered::Yes);
    let later = at + Duration::from_secs(60);
    shell
        .frame_seen(Some(frame(50)), later, &mut core.admit())
        .unwrap();
    let cmds = core.take();
    assert_eq!(viewing(&cmds), [None], "{cmds:?}");
    assert!(reads(&cmds).is_empty(), "covered: nothing is read");
    shell
        .frame_seen(Some(frame(50)), later + MAX_DELAY, &mut core.admit())
        .unwrap();
    assert!(core.take().is_empty(), "told once");
}

/// The withdrawal is not a frame command: a refusal leaves the frame
/// retry's Retry pill and counts alone, and waits its own backoff.
#[test]
fn a_refused_withdrawal_backs_off_on_its_own() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    shell
        .frame_seen(Some(frame(49)), now(), &mut core.admit())
        .unwrap();
    core.take();
    shell.set_covered(Covered::Yes);
    core.full = true;
    assert!(
        shell
            .frame_seen(Some(frame(49)), now(), &mut core.admit())
            .is_err()
    );
    assert!(!shell.history_gave_up());
    let attempts = core.attempts;
    shell
        .frame_seen(Some(frame(49)), now(), &mut core.admit())
        .unwrap();
    assert_eq!(core.attempts, attempts, "its backoff waits");
    core.full = false;
    shell
        .frame_seen(Some(frame(49)), now() + FIRST_DELAY, &mut core.admit())
        .unwrap();
    assert_eq!(viewing(&core.take()), [None]);
}

/// Bounded, and it never gives up while the pane stays covered: a full
/// queue sees one attempt per `MAX_DELAY` at most, and the first free slot
/// takes the withdrawal.
#[test]
fn a_withdrawal_is_bounded_but_never_gives_up() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    shell
        .frame_seen(Some(frame(49)), now(), &mut core.admit())
        .unwrap();
    core.take();
    shell.set_covered(Covered::Yes);
    core.full = true;
    let start = core.attempts;
    for ms in (0..100_000).step_by(100) {
        let at = now() + Duration::from_millis(ms);
        let _ = shell.frame_seen(Some(frame(49)), at, &mut core.admit());
    }
    let tried = core.attempts - start;
    assert!((10..=20).contains(&tried), "{tried} attempts in 100 s");
    core.full = false;
    let late = now() + Duration::from_secs(101) + MAX_DELAY;
    shell
        .frame_seen(Some(frame(49)), late, &mut core.admit())
        .unwrap();
    assert_eq!(viewing(&core.take()), [None]);
}

/// No frame has to come: the retry wake-up carries the withdrawal.
#[test]
fn the_wakeup_retries_a_refused_withdrawal() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    shell
        .frame_seen(Some(frame(49)), now(), &mut core.admit())
        .unwrap();
    core.take();
    shell.set_covered(Covered::Yes);
    core.full = true;
    assert!(
        shell
            .frame_seen(Some(frame(49)), now(), &mut core.admit())
            .is_err()
    );
    let due = shell.retry_due(now()).expect("a wake-up is owed");
    assert_eq!(due, now() + FIRST_DELAY);
    core.full = false;
    shell.retry_wakeup(due, &mut core.admit()).unwrap();
    assert_eq!(viewing(&core.take()), [None]);
    assert_eq!(shell.retry_due(due), None, "nothing waits any more");
}

/// A later uncover supersedes the withdrawal: core was never told, so it
/// still has the tail as watched and the frame has nothing to report.
#[test]
fn an_uncover_supersedes_a_pending_withdrawal() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    shell
        .frame_seen(Some(frame(49)), now(), &mut core.admit())
        .unwrap();
    core.take();
    shell.set_covered(Covered::Yes);
    core.full = true;
    assert!(
        shell
            .frame_seen(Some(frame(49)), now(), &mut core.admit())
            .is_err()
    );
    shell.set_covered(Covered::No);
    core.full = false;
    assert_eq!(shell.retry_due(now()), None, "no wake-up for a dead intent");
    let later = now() + MAX_DELAY;
    shell
        .frame_seen(Some(frame(49)), later, &mut core.admit())
        .unwrap();
    shell.retry_wakeup(later, &mut core.admit()).unwrap();
    assert!(core.take().is_empty(), "still watched: nothing to say");
    // Covered again, the withdrawal starts afresh and is not delayed by
    // the failure of the superseded one.
    shell.set_covered(Covered::Yes);
    shell
        .frame_seen(Some(frame(49)), later, &mut core.admit())
        .unwrap();
    assert_eq!(viewing(&core.take()), [None]);
}
