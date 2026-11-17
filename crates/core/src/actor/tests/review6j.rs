//! Regression tests for review round 33 (re-check of task 6i), task 6j:
//! send admission and discards are answered by correlated events, so the
//! UI never changes send capacity before core did. Each one fails on the
//! code before its fix. The UI's side is in `app::chat::tests::review6j`.

use super::review6f::flooded;
use super::review6g::{logout_stalled, settled, stay_signed_in};
use super::{Harness, chat, harness};
use crate::actor::sends::{MAX_KEPT_SENDS, MAX_PENDING_SENDS};
use crate::api::{Command, Event, SendState, UserError};
use crate::backend::BackendError;
use crate::backend::fake::{FakeScript, Fault, Op};
use crate::model::{LocalId, ThreadKey};

/// How core answered a `SendText`.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    /// `SendAdmitted`, then this state.
    Kept(SendState),
    /// `SendRefused`: core holds nothing.
    Refused,
}

/// Sends `LocalId(id)` and returns core's correlated answer.
async fn submit(h: &mut Harness, id: u64) -> Answer {
    let local = LocalId(id);
    h.send(Command::SendText {
        thread: ThreadKey::whole(chat(1)),
        text: format!("m{id}"),
        local_id: local,
    })
    .await;
    let (ev, seen) = h.until(settled(local)).await;
    let admitted = seen
        .iter()
        .any(|e| matches!(e, Event::SendAdmitted { local_id } if *local_id == local));
    match ev {
        Event::SendState { state, .. } if admitted => Answer::Kept(state),
        Event::SendRefused { .. } if !admitted => Answer::Refused,
        other => panic!("uncorrelated answer: {other:?}, seen {seen:?}"),
    }
}

/// Finding 1 (`hold_send` checked only the kept slots): 255 ordinary
/// sends and 32 kept refusals, then a send the UI made before it saw the
/// logout. Its whole table has room, so core keeps it (the UI's cap is the
/// same 288); the next one is refused with `SendRefused`, never left as a
/// `Failed` send core does not know.
#[tokio::test(start_paused = true)]
async fn a_send_behind_a_logout_is_kept_while_the_table_has_room() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    let ordinary = u64::try_from(MAX_PENDING_SENDS).unwrap() - 1;
    for _ in 0..ordinary {
        h.fake
            .inject(Op::Send, Fault::Error(BackendError::Rejected));
    }
    for id in 1..=ordinary {
        assert_eq!(submit(&mut h, id).await, Answer::Kept(SendState::Failed));
    }
    logout_stalled(&mut h).await;
    let kept = u64::try_from(MAX_KEPT_SENDS).unwrap();
    for id in ordinary + 1..=ordinary + kept {
        assert_eq!(submit(&mut h, id).await, Answer::Kept(SendState::Failed));
    }
    let last = ordinary + kept + 1;
    assert_eq!(
        submit(&mut h, last).await,
        Answer::Kept(SendState::Failed),
        "the last free slot was not used"
    );
    assert_eq!(submit(&mut h, last + 1).await, Answer::Refused);
    stay_signed_in(&mut h).await;
    h.send(Command::DiscardSend(LocalId(1))).await;
    h.until(|e| {
        matches!(
            e,
            Event::SendDiscarded {
                local_id: LocalId(1)
            }
        )
    })
    .await;
    h.send(Command::RetrySend(LocalId(last))).await;
    let (ev, _) = h.until(settled(LocalId(last))).await;
    assert!(
        matches!(
            ev,
            Event::SendState {
                state: SendState::Sent { .. },
                ..
            }
        ),
        "{ev:?}"
    );
    h.stop().await;
}

/// Finding 2 (core's side): a Discard that reaches core just after a
/// flood wait's automatic resend went in flight is answered `Busy` and
/// `DiscardRefused` with the state core keeps; the send then completes.
#[tokio::test(start_paused = true)]
async fn a_discard_racing_an_automatic_resend_is_answered_with_the_kept_state() {
    let mut h = flooded(30).await;
    h.fake.inject(Op::Send, Fault::Hang);
    h.until(|e| {
        matches!(
            e,
            Event::SendState {
                local_id: LocalId(1),
                state: SendState::Sending
            }
        )
    })
    .await;
    h.send(Command::DiscardSend(LocalId(1))).await;
    let (ev, seen) = h
        .until(|e| {
            matches!(
                e,
                Event::DiscardRefused {
                    local_id: LocalId(1),
                    ..
                } | Event::SendDiscarded {
                    local_id: LocalId(1)
                }
            )
        })
        .await;
    assert!(
        matches!(
            ev,
            Event::DiscardRefused {
                state: SendState::Sending,
                ..
            }
        ),
        "{ev:?} after {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::Error(UserError::Busy)))
    );
    let (ev, _) = h.until(settled(LocalId(1))).await;
    assert!(
        matches!(
            ev,
            Event::SendState {
                state: SendState::Sent { .. },
                ..
            }
        ),
        "{ev:?}"
    );
    h.stop().await;
}

/// Every discard is answered: `SendDiscarded` for one core removed, and
/// for one it does not know (no `NotFound`: it holds nothing either way).
#[tokio::test(start_paused = true)]
async fn every_discard_is_answered() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    h.fake
        .inject(Op::Send, Fault::Error(BackendError::Rejected));
    assert_eq!(submit(&mut h, 1).await, Answer::Kept(SendState::Failed));
    for id in [1, 77] {
        h.send(Command::DiscardSend(LocalId(id))).await;
        let (_, seen) = h
            .until(|e| matches!(e, Event::SendDiscarded { local_id } if *local_id == LocalId(id)))
            .await;
        assert!(
            !seen.iter().any(|e| matches!(e, Event::Error(_))),
            "{seen:?}"
        );
    }
    h.stop().await;
}
