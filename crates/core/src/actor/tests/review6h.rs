//! Regression tests for review round 31 (re-check of task 6g), task 6h.
//! Each one fails on the code before its fix. Finding 2 (paused clock vs.
//! the persistence thread) is proven by the whole suite under
//! `ABYSS_TEST_STORE_DELAY_MS=50` (`secret::ClockGate`); finding 4 is in
//! `core/startup_tests.rs`.

use super::review6g::{logout_stalled, settled, stay_signed_in};
use super::{Harness, chat, harness};
use crate::actor::sends::{MAX_KEPT_SENDS, MAX_PENDING_SENDS};
use crate::api::{AuthState, Command, Event, SendState, UserError};
use crate::backend::fake::FakeScript;
use crate::model::{LocalId, ThreadKey};

/// Sends `LocalId(id)` and returns the state it settles in.
async fn send_settles(h: &mut Harness, id: u64) -> SendState {
    h.send(Command::SendText {
        thread: ThreadKey::whole(chat(1)),
        text: format!("m{id}"),
        local_id: LocalId(id),
    })
    .await;
    match h.until(settled(LocalId(id))).await.0 {
        Event::SendState { state, .. } => state,
        _ => unreachable!(),
    }
}

/// Finding 1: more sends are refused behind a stalled logout than the
/// kept slots hold. Since round 33 they take ordinary slots once the kept
/// ones are full (core keeps a send while its table has room, the UI's
/// one capacity rule), and the UI's cap stops it there. After "Stay signed
/// in" an ordinary send still has a slot, and every refusal is retried with
/// its payload: one in a kept slot and one past it.
#[tokio::test(start_paused = true)]
async fn refusals_behind_a_logout_are_kept_while_the_table_has_room() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    logout_stalled(&mut h).await;
    let refused = u64::try_from(MAX_PENDING_SENDS).unwrap();
    for id in 1..=refused {
        assert_eq!(send_settles(&mut h, id).await, SendState::Failed);
    }
    stay_signed_in(&mut h).await;
    let fresh = refused + 1;
    assert!(
        matches!(send_settles(&mut h, fresh).await, SendState::Sent { .. }),
        "an ordinary send found no slot"
    );
    h.send(Command::RetrySend(LocalId(1))).await;
    let (ev, _) = h.until(settled(LocalId(1))).await;
    assert!(
        matches!(
            ev,
            Event::SendState {
                state: SendState::Sent { .. },
                ..
            }
        ),
        "a held send was not kept: {ev:?}"
    );
    // Past the kept bound: in an ordinary slot, retried with its payload.
    let past = u64::try_from(MAX_KEPT_SENDS).unwrap() + 1;
    h.send(Command::RetrySend(LocalId(past))).await;
    let (ev, _) = h.until(settled(LocalId(past))).await;
    assert!(
        matches!(
            ev,
            Event::SendState {
                state: SendState::Sent { .. },
                ..
            }
        ),
        "a refusal past the kept bound was not kept: {ev:?}"
    );
    h.stop().await;
}

/// Answers a Retry of a send core does not know: `Error(NotFound)`, then
/// `SendRefused`.
async fn unknown_retry_is_not_found(h: &mut Harness, id: u64) {
    h.send(Command::RetrySend(LocalId(id))).await;
    let (ev, seen) = h.until(settled(LocalId(id))).await;
    assert!(matches!(ev, Event::SendRefused { .. }), "{ev:?}");
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::Error(UserError::NotFound))),
        "no NotFound before SendRefused: {seen:?}"
    );
}

/// Finding 3: a Retry of an unknown send behind the overlay is answered as
/// it is elsewhere, while "Logging out" and while "Couldn't start logout"
/// is shown.
#[tokio::test(start_paused = true)]
async fn an_unknown_retry_behind_the_overlay_is_not_found() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    logout_stalled(&mut h).await;
    unknown_retry_is_not_found(&mut h, 77).await;
    h.faults.release();
    h.auth(|s| matches!(s, AuthState::LogoutNotStarted { .. }))
        .await;
    unknown_retry_is_not_found(&mut h, 78).await;
    h.stop().await;
}
