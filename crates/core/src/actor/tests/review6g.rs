//! Regression tests for review round 28 (re-check of task 6f), task 6g.
//! Each one fails on the code before its fix. Finding 1 (shutdown during a
//! stalled deferred cache provisioning) is in `core/startup_tests.rs`.

use std::time::Duration;

use super::review6f::flooded;
use super::{Harness, chat, harness};
use crate::api::{AuthState, Command, Event, SendState, UserError};
use crate::backend::Update;
use crate::backend::fake::{FakeScript, Op, text_message};
use crate::model::{LocalId, ThreadKey};
use crate::secret::SecretItem;

/// The next `SendState` of `id` other than `Sending`, or its
/// `SendRefused` (core holds nothing for it, task 6j).
pub(super) fn settled(id: LocalId) -> impl Fn(&Event) -> bool {
    move |e| match e {
        Event::SendState { local_id, state } => *local_id == id && *state != SendState::Sending,
        Event::SendRefused { local_id } => *local_id == id,
        _ => false,
    }
}

/// Logout with the marker write stalled (shown "Logging out"); it fails
/// once the store is released.
pub(super) async fn logout_stalled(h: &mut Harness) {
    h.faults.fail_set_of(SecretItem::LogoutPending);
    h.faults.hold();
    h.send(Command::Logout).await;
    h.auth(|s| *s == AuthState::LogoutStarting).await;
}

/// Releases the stalled marker write (it fails), then "Stay signed in".
pub(super) async fn stay_signed_in(h: &mut Harness) {
    h.faults.release();
    h.auth(|s| matches!(s, AuthState::LogoutNotStarted { .. }))
        .await;
    h.send(Command::Back).await;
    h.auth(AuthState::is_signed_in).await;
}

/// Finding 2: a send refused while "Logging out" is shown is kept
/// `Failed`; after the marker fails and "Stay signed in", Retry sends it.
/// A retry of a send core does not know is answered `SendRefused`.
#[tokio::test(start_paused = true)]
async fn a_send_refused_behind_a_logout_is_retryable_after_stay_signed_in() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    logout_stalled(&mut h).await;
    h.send(Command::SendText {
        thread: ThreadKey::whole(chat(1)),
        text: "later".into(),
        local_id: LocalId(5),
    })
    .await;
    let (ev, _) = h.until(settled(LocalId(5))).await;
    assert!(
        matches!(
            ev,
            Event::SendState {
                state: SendState::Failed,
                ..
            }
        ),
        "{ev:?}"
    );
    stay_signed_in(&mut h).await;
    assert_eq!(h.fake.calls(Op::Send), 0, "sent behind the logout");
    h.send(Command::RetrySend(LocalId(5))).await;
    let (ev, _) = h.until(settled(LocalId(5))).await;
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
    h.send(Command::RetrySend(LocalId(99))).await;
    let (ev, seen) = h.until(settled(LocalId(99))).await;
    assert!(matches!(ev, Event::SendRefused { .. }), "{ev:?}");
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::Error(UserError::NotFound)))
    );
    h.stop().await;
}

/// Finding 3: a flood-waited send falls due while "Logging out" and then
/// "Couldn't start logout" are shown: no backend attempt. "Stay signed in"
/// resends it (its deadline kept, the window still open).
#[tokio::test(start_paused = true)]
async fn a_flood_retry_due_behind_a_logout_waits_for_stay_signed_in() {
    let mut h = flooded(30).await;
    assert_eq!(h.fake.calls(Op::Send), 1);
    logout_stalled(&mut h).await;
    // Due at 30 s; the marker write stalls past it.
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(h.fake.calls(Op::Send), 1, "resent while logging out");
    h.faults.release();
    h.auth(|s| matches!(s, AuthState::LogoutNotStarted { .. }))
        .await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(h.fake.calls(Op::Send), 1, "resent behind the overlay");
    h.send(Command::Back).await;
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
    assert_eq!(h.fake.calls(Op::Send), 2);
    h.stop().await;
}

/// Chat 1's unread count after a new incoming message `id`.
async fn unread_after(h: &mut Harness, id: i32) -> u32 {
    h.fake
        .push_update(Update::NewMessage(text_message(chat(1), id, "in")))
        .await;
    let (ev, _) = h
        .until(|e| matches!(e, Event::ChatBumped(s) if s.id == chat(1)))
        .await;
    match ev {
        Event::ChatBumped(s) => s.unread,
        _ => unreachable!(),
    }
}

/// Finding 4: the open chat's tail is on screen, then a logout hides it. A
/// message that arrives behind "Logging out" is unread, a stale tail
/// report is refused, and after "Stay signed in" messages count as unread
/// until the UI reports the tail on screen again.
#[tokio::test(start_paused = true)]
async fn a_logout_overlay_makes_the_tail_unseen() {
    let mut h = harness(FakeScript::demo(1, 10), None).await;
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h.until(|e| matches!(e, Event::History { .. })).await;
    h.send(Command::ViewingTail(Some(ThreadKey::whole(chat(1)))))
        .await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(unread_after(&mut h, 11).await, 0, "tail on screen");
    logout_stalled(&mut h).await;
    assert_eq!(unread_after(&mut h, 12).await, 1, "behind the overlay");
    // Queued by the UI before it saw the overlay.
    h.send(Command::ViewingTail(Some(ThreadKey::whole(chat(1)))))
        .await;
    assert_eq!(unread_after(&mut h, 13).await, 2, "a stale report");
    stay_signed_in(&mut h).await;
    assert_eq!(unread_after(&mut h, 14).await, 3, "not painted yet");
    h.send(Command::ViewingTail(Some(ThreadKey::whole(chat(1)))))
        .await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(unread_after(&mut h, 15).await, 3, "shown again");
    h.stop().await;
}
