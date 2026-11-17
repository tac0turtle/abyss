//! Regression tests for review round 34 (re-check of task 6j), task 6k,
//! the chat shell's half, and round 35 (task 6l) finding 3. Each fails on
//! the code before its fix. Findings 2 and 3 (a
//! core stop, per-chat drafts) are in `shell/tests.rs`.

use super::*;

/// Finding 1: a Retry of an `Unknown` send is queued; core then sees the
/// late confirmation (`Sent`, or `SentUnconfirmed`) and drops the entry,
/// so the Retry is answered `SendRefused`. That answer is older than the
/// confirmation: the echo keeps its delivered state and is never offered
/// for sending again.
#[test]
fn a_refused_retry_after_a_late_confirmation_never_offers_the_text_again() {
    for confirmed in [
        SendState::Sent { id: MsgId(101) },
        SendState::SentUnconfirmed,
    ] {
        let mut core = Core::default();
        let mut shell = open(&mut core);
        let id = send(&mut shell, &mut core, "once");
        set(&mut shell, id, SendState::Unknown { attempts: 3 });
        shell.retry(id, &mut core.admit()).unwrap();
        assert!(matches!(core.take().as_slice(), [.., Command::RetrySend(r)] if *r == id));
        set(&mut shell, id, confirmed);
        shell.apply(Event::Error(UserError::NotFound));
        shell.apply(Event::SendRefused { local_id: id });
        let echo = shell.echoes().get(id).unwrap();
        assert_eq!(echo.state, confirmed);
        assert!(!echo.is_returned() && !echo.can_retry(), "{confirmed:?}");
        assert_eq!(shell.returned(id), None, "no Put back of sent text");
    }
}

/// Core stopped: an echo the server already has is not stranded (it was
/// delivered); one with a refusal answered stays returned.
#[test]
fn a_core_stop_strands_only_what_waits_on_core() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let sent = send(&mut shell, &mut core, "sent");
    set(&mut shell, sent, SendState::SentUnconfirmed);
    let returned = shell
        .submit(THREAD, "refused", 0, &mut core.admit())
        .unwrap()
        .unwrap();
    shell.apply(Event::SendRefused { local_id: returned });
    let failed = send(&mut shell, &mut core, "failed");
    set(&mut shell, failed, SendState::Failed);
    shell.core_stopped();
    let get = |id| shell.echoes().get(id).unwrap();
    assert!(!get(sent).is_stranded());
    assert!(get(returned).is_returned());
    assert!(get(failed).is_stranded() && !get(failed).can_retry());
    assert_eq!(
        crate::chat::copy::send_label(get(failed), now()),
        crate::chat::copy::STRANDED_UNSENT
    );
    assert_eq!(shell.take_returned(failed).as_deref(), Some("failed"));
}

/// Task 6l, finding 3: a Discard is queued for an `Unknown` send, a late
/// confirmation arrives, then core stops before the discard's answer. No
/// echo stays "Discarding…": a delivered one is kept, shows its delivered
/// state, and offers neither Retry nor Put back.
#[test]
fn a_core_stop_finalizes_a_delivered_echo_left_discarding() {
    for confirmed in [
        SendState::Sent { id: MsgId(101) },
        SendState::SentUnconfirmed,
    ] {
        let mut core = Core::default();
        let mut shell = open(&mut core);
        let id = send(&mut shell, &mut core, "once");
        set(&mut shell, id, SendState::Unknown { attempts: 3 });
        shell.discard(id, &mut core.admit()).unwrap();
        set(&mut shell, id, confirmed);
        shell.core_stopped();
        let echo = shell.echoes().get(id).unwrap();
        assert_eq!(echo.phase, crate::chat::sends::Phase::Kept, "{confirmed:?}");
        assert_eq!(echo.state, confirmed);
        assert!(!echo.can_retry() && !echo.can_discard() && !echo.is_returned());
        assert_ne!(
            crate::chat::copy::send_label(echo, now()),
            "Discarding…",
            "{confirmed:?}"
        );
        assert_eq!(shell.returned(id), None);
    }
}
