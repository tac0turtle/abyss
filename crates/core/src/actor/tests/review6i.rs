//! Regression tests for review round 32 (re-check of task 6h), task 6i.
//! Each one fails on the code before its fix. Finding 3 (a test hold
//! that expires on its own) is in `secret/clock_gate.rs`.

use std::time::{Duration, Instant};

use super::review6g::{logout_stalled, settled, stay_signed_in};
use super::{Harness, chat, harness};
use crate::actor::sends::{MAX_KEPT_SENDS, MAX_PENDING_SENDS};
use crate::api::{Command, Event, SendState, UserError};
use crate::backend::BackendError;
use crate::backend::fake::{FakeScript, Fault, Op};
use crate::model::{LocalId, ThreadKey};
use crate::secret::SecretItem;

/// Sends `LocalId(id)`; returns the state it settles in (`None`: core
/// refused it and holds nothing, `SendRefused`) and whether `Error(Busy)`
/// came before it.
async fn send_settles(h: &mut Harness, id: u64) -> (Option<SendState>, bool) {
    h.send(Command::SendText {
        thread: ThreadKey::whole(chat(1)),
        text: format!("m{id}"),
        local_id: LocalId(id),
    })
    .await;
    settles(h, id).await
}

/// Retries `LocalId(id)`; as [`send_settles`].
async fn retry_settles(h: &mut Harness, id: u64) -> (Option<SendState>, bool) {
    h.send(Command::RetrySend(LocalId(id))).await;
    settles(h, id).await
}

async fn settles(h: &mut Harness, id: u64) -> (Option<SendState>, bool) {
    let (ev, seen) = h.until(settled(LocalId(id))).await;
    let busy = seen
        .iter()
        .any(|e| matches!(e, Event::Error(UserError::Busy)));
    match ev {
        Event::SendState { state, .. } => (Some(state), busy),
        _ => (None, busy),
    }
}

/// Fills every ordinary slot with a send the server rejected (`Failed`,
/// kept for Retry): ids `1..=MAX_PENDING_SENDS`.
async fn fill_ordinary_with_failures(h: &mut Harness) -> u64 {
    let full = u64::try_from(MAX_PENDING_SENDS).unwrap();
    for _ in 0..full {
        h.fake
            .inject(Op::Send, Fault::Error(BackendError::Rejected));
    }
    for id in 1..=full {
        assert_eq!(send_settles(h, id).await, (Some(SendState::Failed), false));
    }
    full
}

fn sent(state: Option<SendState>) -> bool {
    matches!(state, Some(SendState::Sent { .. }))
}

/// Finding 1, the exact sequence: 256 failed ordinary sends fill the
/// ordinary bound; send 257 (which the UI admits, its cap being 288) is
/// `Busy` then `Failed`; after a Discard, its Retry sends it rather than
/// answering `NotFound`, as the UI cleared its composer.
#[tokio::test(start_paused = true)]
async fn a_send_past_the_ordinary_bound_is_kept_for_retry() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    let next = fill_ordinary_with_failures(&mut h).await + 1;
    assert_eq!(
        send_settles(&mut h, next).await,
        (Some(SendState::Failed), true)
    );
    h.send(Command::DiscardSend(LocalId(1))).await;
    let (state, busy) = retry_settles(&mut h, next).await;
    assert!(sent(state) && !busy, "{state:?}, busy: {busy}");
    h.stop().await;
}

/// Round 31's finding 1, strengthened: ordinary capacity full of failed
/// sends, the kept bound full of refusals behind a stalled logout, so
/// 256 + 32 slots are taken at once. A refusal past the kept bound has no
/// record; after "Stay signed in" a kept Retry is `Busy` and stays kept, a
/// new send is `Busy` with no record, and once a slot frees the kept send
/// is promoted and sent.
#[tokio::test(start_paused = true)]
async fn both_bounds_full_keep_every_admitted_send_retryable() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    let full = fill_ordinary_with_failures(&mut h).await;
    logout_stalled(&mut h).await;
    let kept = u64::try_from(MAX_KEPT_SENDS).unwrap();
    for id in full + 1..=full + kept {
        assert_eq!(
            send_settles(&mut h, id).await,
            (Some(SendState::Failed), false)
        );
    }
    let past = full + kept + 1;
    assert_eq!(send_settles(&mut h, past).await, (None, true));
    stay_signed_in(&mut h).await;
    let first_kept = full + 1;
    // Busy, and still kept: a second Retry is answered `Failed` again,
    // never `NotFound`.
    for _ in 0..2 {
        assert_eq!(
            retry_settles(&mut h, first_kept).await,
            (Some(SendState::Failed), true)
        );
    }
    let fresh = past + 1;
    assert_eq!(send_settles(&mut h, fresh).await, (None, true));
    for unknown in [past, fresh] {
        h.send(Command::RetrySend(LocalId(unknown))).await;
        let (_, seen) = h.until(settled(LocalId(unknown))).await;
        assert!(
            seen.iter()
                .any(|e| matches!(e, Event::Error(UserError::NotFound))),
            "{unknown} has a record: {seen:?}"
        );
    }
    h.send(Command::DiscardSend(LocalId(1))).await;
    let (state, _) = retry_settles(&mut h, first_kept).await;
    assert!(sent(state), "a kept send was not promoted: {state:?}");
    h.stop().await;
}

/// Finding 2: a store operation that never answers keeps the paused clock
/// held, so no slice of the test's clock ends. The wall-clock watchdog
/// still fails the wait within its bound (before: only once the store's
/// own 10 s hold bound let the operation go).
#[tokio::test(start_paused = true)]
async fn a_store_operation_that_never_answers_fails_within_the_watchdog_bound() {
    const BOUND: Duration = Duration::from_secs(1);
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    h.watchdog.set_bound(BOUND);
    let faults = h.faults.clone();
    faults.stall_item(SecretItem::LogoutPending);
    h.send(Command::Logout).await;
    let started = Instant::now();
    let waited = tokio::spawn(async move { h.until(|_| false).await }).await;
    let took = started.elapsed();
    faults.release();
    let panic = waited.unwrap_err().into_panic();
    let message = panic.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(message.contains("watchdog"), "{message}");
    assert!(took < BOUND * 4, "failed after {took:?}");
}
