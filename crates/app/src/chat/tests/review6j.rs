//! Regression tests for review round 33 (re-check of task 6i), task 6j,
//! the chat shell's half: the UI changes send capacity only on core's
//! correlated answer. Each fails on the 6i code. Core's half is in
//! `abyss_core`'s `actor/tests/review6j.rs`.

use super::*;
use crate::chat::copy::{RETURNED, send_label};
use crate::chat::sends::MAX_ECHOES;

/// Fills the echo table with sends core keeps, `Failed`; returns their ids.
fn fill(shell: &mut ChatShell, core: &mut Core) -> Vec<LocalId> {
    let ids: Vec<LocalId> = (0..MAX_ECHOES)
        .map(|n| send(shell, core, &format!("m{n}")))
        .collect();
    for id in &ids {
        set(shell, *id, SendState::Failed);
    }
    core.take();
    ids
}

/// Whether a new submission is refused for room, before and inside
/// `submit` (the composer keeps its text).
fn no_room(shell: &mut ChatShell, core: &mut Core) -> bool {
    let target = shell.send_target();
    let submitted = shell.submit(THREAD, "one more", 0, &mut core.admit());
    let refused = target == Err(UserError::Busy) && submitted == Ok(None);
    assert!(core.take().is_empty() || !refused, "sent while full");
    refused
}

/// Finding 2: a Discard of a flood-waiting send races its automatic
/// resend. The echo keeps its slot until core answers; core refuses (the
/// resend went in flight), so the UI applies core's state and still has no
/// room. Before: the echo went when the Discard was queued, and the UI
/// admitted a 289th send while core was full.
#[test]
fn a_discard_keeps_its_slot_until_core_answers() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let ids = fill(&mut shell, &mut core);
    let racing = *ids.first().unwrap();
    set(&mut shell, racing, SendState::FloodWait { seconds: 30 });
    shell.discard(racing, &mut core.admit()).unwrap();
    assert!(matches!(core.take().as_slice(), [Command::DiscardSend(r)] if *r == racing));
    assert_eq!(
        shell.echoes().count(THREAD),
        MAX_ECHOES,
        "discarding still counts"
    );
    assert!(
        no_room(&mut shell, &mut core),
        "admitted while core is full"
    );
    // The resend went in flight first: core keeps the send.
    shell.apply(Event::DiscardRefused {
        local_id: racing,
        state: SendState::Sending,
    });
    let echo = shell.echoes().get(racing).unwrap();
    assert_eq!(echo.state, SendState::Sending);
    assert!(!echo.can_discard() && !echo.can_retry());
    assert!(no_room(&mut shell, &mut core));
    // A discard core accepts frees the slot only on its answer.
    let other = *ids.last().unwrap();
    shell.discard(other, &mut core.admit()).unwrap();
    core.take();
    assert!(no_room(&mut shell, &mut core));
    shell.apply(Event::SendDiscarded { local_id: other });
    assert!(shell.echoes().get(other).is_none());
    assert_eq!(shell.send_target(), Ok(Some(THREAD)));
}

/// Finding 1, the UI's side: a send core refused and holds nothing for
/// (`SendRefused`, e.g. its table filled behind a logout) keeps its text
/// as a returned echo: no Retry (it could only be `NotFound`), Put back or
/// Dismiss, and its slot counts until the user takes it.
#[test]
fn a_refused_send_keeps_its_text_for_the_box() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let local_id = shell
        .submit(THREAD, "keep me", 0, &mut core.admit())
        .unwrap()
        .unwrap();
    core.take();
    shell.apply(Event::Error(UserError::Busy));
    shell.apply(Event::SendRefused { local_id });
    let echo = shell.echoes().get(local_id).unwrap();
    assert!(echo.is_returned() && !echo.can_retry() && !echo.can_discard());
    assert_eq!(send_label(echo, now()), RETURNED);
    assert_eq!(shell.echoes().count(THREAD), 1, "still counted");
    // Late state events cannot revive it.
    set(&mut shell, local_id, SendState::Failed);
    assert!(shell.echoes().get(local_id).unwrap().is_returned());
    assert_eq!(shell.returned(local_id), Some((THREAD, "keep me")));
    assert_eq!(shell.take_returned(local_id).as_deref(), Some("keep me"));
    assert_eq!(shell.echoes().count(THREAD), 0);
    assert_eq!(shell.take_returned(local_id), None);
}

/// A Retry core cannot run because it holds no such send is answered
/// `SendRefused`: the echo becomes returned text, never a `Failed` echo
/// whose Retry is `NotFound` again.
#[test]
fn a_retry_core_does_not_know_returns_the_text() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let id = send(&mut shell, &mut core, "lost?");
    set(&mut shell, id, SendState::Failed);
    shell.retry(id, &mut core.admit()).unwrap();
    assert_eq!(shell.echoes().get(id).unwrap().state, SendState::Sending);
    shell.apply(Event::Error(UserError::NotFound));
    shell.apply(Event::SendRefused { local_id: id });
    let echo = shell.echoes().get(id).unwrap();
    assert!(echo.is_returned() && !echo.can_retry());
}

/// One capacity rule: submissions core has not answered count toward the
/// cap, and so do returned texts.
#[test]
fn unanswered_submissions_count_toward_the_cap() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    for n in 0..MAX_ECHOES {
        let local_id = shell
            .submit(THREAD, &format!("m{n}"), 0, &mut core.admit())
            .unwrap()
            .unwrap();
        if n == 0 {
            shell.apply(Event::SendRefused { local_id });
        }
    }
    core.take();
    assert!(no_room(&mut shell, &mut core));
}
