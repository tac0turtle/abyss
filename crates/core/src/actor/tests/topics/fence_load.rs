//! The `peer_dialogs` scheduler under load (plan sections 6.6, 7 and 11):
//! kind checks and badge refreshes share calls through reserved slots, so
//! neither starves the other; fenced chats are checked in FIFO order with
//! bounded rechecks, re-dispatched after a rebuild, and retired when they
//! leave the required-peer set.

use std::collections::BTreeSet;
use std::time::Duration;

use super::*;
use crate::api::{SendState, UserError, WindowState};
use crate::backend::fake::{Fault, Op};
use crate::backend::{BackendError, Update};
use crate::model::LocalId;
use crate::store::{CURSOR_SLOTS, MAX_PEER_DIALOGS, PEER_CALL_INTERVAL};

fn whole(chat: ChatId) -> ThreadKey {
    ThreadKey::whole(chat)
}

async fn opened(h: &mut Harness, thread: ThreadKey) {
    h.send(Command::OpenThread(thread)).await;
    reconciled(h, thread).await;
}

/// Calls from record `from` on, as chat sets.
fn calls_from(h: &Harness, from: usize) -> Vec<BTreeSet<ChatId>> {
    peer_calls(h)
        .into_iter()
        .skip(from)
        .map(|r| r.chats.into_iter().collect())
        .collect()
}

/// The first call (from record `from`) that asked for `chat`, 1-based.
fn first_call(calls: &[BTreeSet<ChatId>], chat: ChatId) -> Option<usize> {
    calls.iter().position(|c| c.contains(&chat)).map(|n| n + 1)
}

/// 40 dirty badges beyond page one and 3 fenced chats: the 3 kind checks
/// complete, every fenced chat's waiting command is answered (a deferred
/// load, held sends), and every badge is refreshed within `ceil(40 / 9)`
/// calls of the cursor's slots.
#[tokio::test(start_paused = true)]
async fn forty_dirty_badges_with_three_fences() {
    let mut script = FakeScript::demo(100, 1);
    let forums: Vec<ChatId> = (0..40).map(|n| forum(1000 + n)).collect();
    for f in &forums {
        add_unread_forum(&mut script, *f);
    }
    let groups = [group(2000), group(2001), group(2002)];
    for g in groups {
        add_group(&mut script, g, 60);
    }
    let mut h = online(script).await;
    load_all_dialogs(&mut h).await;
    opened(&mut h, whole(groups[0])).await;
    let from = peer_calls(&h).len();
    for f in &forums {
        h.fake
            .read_remotely(topic_key(*f, 2), MsgId(10))
            .await
            .unwrap();
    }
    for g in groups {
        h.fake.push_update(Update::ChatChanged { chat: g }).await;
    }
    h.send(Command::LoadOlder(whole(groups[0]))).await;
    for (n, g) in groups[1..].iter().enumerate() {
        h.send(Command::SendText {
            thread: whole(*g),
            text: "held".into(),
            local_id: LocalId(u64::try_from(n).unwrap() + 1),
        })
        .await;
    }
    let seen = events_for(&mut h, PEER_CALL_INTERVAL * 8).await;
    let calls = calls_from(&h, from);
    for g in groups {
        assert_eq!(calls.iter().filter(|c| c.contains(&g)).count(), 1, "{g:?}");
    }
    // The first call took one forum (the first trigger); the drain the rest.
    for f in &forums {
        let at = first_call(&calls, *f).unwrap_or_else(|| panic!("{f:?} never refreshed"));
        assert!(at <= 1 + 40usize.div_ceil(CURSOR_SLOTS), "{f:?} at {at}");
        assert_eq!(
            last_bump(&seen, *f).map(|c| c.read.unread),
            Some(0),
            "{f:?}"
        );
    }
    assert!(seen.iter().any(|e| matches!(
        e,
        Event::LoadEnded { target: LoadTarget::History { thread, .. }, error: None } if *thread == whole(groups[0])
    )));
    for id in [1, 2] {
        assert!(seen.iter().any(|e| matches!(
            e,
            Event::SendState { local_id, state: SendState::Sent { .. } } if *local_id == LocalId(id)
        )));
    }
    assert_spaced(&h);
    h.stop().await;
}

/// 40 chats beyond page one are fenced at once, five are
/// triggered again while their check is out, and a rebuild drops a call in
/// the middle. With no badge dirty every chat gets its first attempt within
/// `ceil(40 / 20)` calls; the re-triggered chats are checked exactly once
/// more, the dropped call's chats are re-dispatched first after the
/// baseline, no reconcile of its own runs, and every waiting command is
/// answered (one beyond the deferred list's room at once, with `Busy`).
#[tokio::test(start_paused = true)]
async fn forty_chats_fenced_at_once_across_a_rebuild() {
    let mut script = FakeScript::demo(100, 1);
    let groups: Vec<ChatId> = (0..40).map(|n| group(2000 + n)).collect();
    for g in &groups {
        add_group(&mut script, *g, 60);
    }
    let mut h = online(script).await;
    load_all_dialogs(&mut h).await;
    let open = whole(groups[0]);
    opened(&mut h, open).await;
    // A flood wait on the first call holds dispatch until every chat is
    // fenced; then a slow call, crossed by new triggers, and a call that
    // hangs until the rebuild drops it.
    for fault in [
        Fault::Error(BackendError::FloodWait { seconds: 10 }),
        Fault::Delay(Duration::from_secs(5)),
        Fault::Hang,
    ] {
        h.fake.inject(Op::PeerDialogs, fault);
    }
    let from = peer_calls(&h).len();
    h.fake
        .push_update(Update::ChatChanged { chat: groups[0] })
        .await;
    peer_calls_reached(&mut h, from + 1).await;
    for g in &groups {
        h.fake.push_update(Update::ChatChanged { chat: *g }).await;
    }
    h.send(Command::LoadOlder(open)).await;
    h.send(Command::LoadNewer(open)).await;
    h.send(Command::LoadOlder(open)).await;
    h.until(|e| {
        matches!(
            e,
            Event::LoadEnded {
                target: LoadTarget::History { .. },
                error: Some(UserError::Busy)
            }
        )
    })
    .await;
    peer_calls_reached(&mut h, from + 2).await;
    for g in &groups[..5] {
        h.fake.push_update(Update::ChatChanged { chat: *g }).await;
    }
    peer_calls_reached(&mut h, from + 3).await;
    let calls = calls_from(&h, from + 1);
    for g in &groups {
        assert!(
            first_call(&calls, *g).is_some_and(|n| n <= 40usize.div_ceil(MAX_PEER_DIALOGS)),
            "{g:?}"
        );
    }
    h.internal.send(crate::actor::Internal::Wake).await.unwrap();
    let (_, rebuilt) = h
        .until(|e| matches!(e, Event::Connection(ConnState::Online)))
        .await;
    peer_calls_reached(&mut h, from + 5).await;
    events_for(&mut h, Duration::from_secs(20)).await;
    let calls = calls_from(&h, from + 1);
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert_eq!(calls[2], calls[1], "the dropped call's chats go first");
    let rechecked: BTreeSet<ChatId> = groups[..5].iter().copied().collect();
    assert_eq!(calls[3], rechecked, "rechecks, once each");
    assert_eq!(h.fake.builds(), 2, "no reconcile of its own");
    // The deferred loads were answered: the rebuild ends them (`Busy`).
    let answered = rebuilt
        .iter()
        .filter(|e| matches!(e, Event::LoadEnded { target: LoadTarget::History { thread, .. }, .. } if *thread == open))
        .count();
    assert_eq!(answered, 2, "{rebuilt:?}");
    assert_spaced(&h);
    h.stop().await;
}

/// Membership churn. A fenced chat whose window is
/// evicted and which leaves the dialogs loses every kind-check record: its
/// deferred load is answered, and its in-flight result (here a changed
/// kind) is ignored, even though the chat came back meanwhile with a newer
/// summary. While a flood wait pauses dispatch, another fenced chat leaves:
/// neither is asked for again (the returning one starts with no state).
#[tokio::test(start_paused = true)]
async fn membership_churn_while_dispatch_is_paused() {
    let (left, gone, still) = (group(2000), group(2001), group(2002));
    let mut script = FakeScript::demo(10, 1);
    for g in [left, gone, still] {
        add_group(&mut script, g, 60);
    }
    let mut h = online(script).await;
    opened(&mut h, whole(left)).await;
    // Past the light refresh's minimum gap, so the refreshes below run at once.
    run_for(Duration::from_secs(16)).await;
    // The check's answer (a forum) lands 25 s later, inside the call deadline.
    h.fake
        .inject(Op::PeerDialogs, Fault::Delay(Duration::from_secs(25)));
    h.fake.set_kind(left, FORUM_KIND).await;
    peer_calls_reached(&mut h, 1).await;
    h.send(Command::LoadOlder(whole(left))).await;
    // The window goes (eight other chats), then the dialog.
    for n in 1..=8 {
        opened(&mut h, whole(super::super::chat(n))).await;
    }
    h.fake.edit_dialogs(|d| d.retain(|s| s.id != left));
    h.send(Command::Window(WindowState::Active)).await;
    h.until(|e| matches!(e, Event::LoadEnded { target: LoadTarget::History { thread, .. }, error: None } if *thread == whole(left)))
        .await;
    // It comes back (a plain group again) before its old response lands.
    h.fake.edit_dialogs(|d| d.push(summary(left, "Group")));
    run_for(Duration::from_secs(16)).await;
    h.send(Command::Window(WindowState::Active)).await;
    h.until(|e| matches!(e, Event::DialogsReset(v) if v.iter().any(|c| c.id == left)))
        .await;
    assert_eq!(peer_calls(&h).len(), 1, "the old response is still out");
    let seen = events_for(&mut h, Duration::from_secs(30)).await;
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, Event::ForumInvalidated(x) if *x == left)),
        "a retired chat's result is ignored: {seen:?}"
    );
    assert!(last_bump(&seen, left).is_none_or(|c| c.kind == GROUP_KIND));
    // Dispatch paused by a flood wait while membership churns.
    h.fake.inject(
        Op::PeerDialogs,
        Fault::Error(BackendError::FloodWait { seconds: 60 }),
    );
    h.fake.push_update(Update::ChatChanged { chat: gone }).await;
    h.fake
        .push_update(Update::ChatChanged { chat: still })
        .await;
    peer_calls_reached(&mut h, 2).await;
    h.fake.edit_dialogs(|d| {
        d.retain(|s| s.id != gone);
    });
    run_for(Duration::from_secs(16)).await;
    h.send(Command::Window(WindowState::Active)).await;
    h.until(|e| matches!(e, Event::DialogsReset(_))).await;
    events_for(&mut h, Duration::from_secs(60)).await;
    // The chat that left is never asked for again, and the one that came
    // back has no check of its own (`still` is settled by the refresh's
    // page, dispatched after its trigger, or by a call of its own).
    let calls = peer_calls(&h);
    for call in calls.iter().skip(2) {
        assert!(
            !call.chats.contains(&gone) && !call.chats.contains(&left),
            "{calls:?}"
        );
    }
    h.stop().await;
}
