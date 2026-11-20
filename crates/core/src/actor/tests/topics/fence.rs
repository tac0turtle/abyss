//! Forum flag changes (plan sections 6.6 and 11, "Forum flag"): a trigger
//! fences a known chat until a `peer_dialogs` check settles its kind. The
//! fence holds the chat's history work; lifting it reloads the open thread
//! (unchanged kind, abandonment) or invalidates the chat's views (changed
//! kind). The check budgets themselves are unit-tested in `store::fence`.

use std::time::Duration;

use super::*;
use crate::api::{UserError, WindowState};
use crate::backend::fake::{Fault, Op, text_message};
use crate::backend::{BackendError, Update};
use crate::store::ForumStatus;

fn whole(chat: ChatId) -> ThreadKey {
    ThreadKey::whole(chat)
}

fn has<P: Fn(&Event) -> bool>(events: &[Event], pred: P) -> bool {
    events.iter().any(pred)
}

fn invalidated(events: &[Event], thread: ThreadKey) -> bool {
    has(
        events,
        |e| matches!(e, Event::ThreadInvalidated(t) if *t == thread),
    )
}

fn reconciled_in(events: &[Event], thread: ThreadKey) -> bool {
    has(
        events,
        |e| matches!(e, Event::Reconciled { thread: t, .. } if *t == thread),
    )
}

fn forum_invalidated(events: &[Event], chat: ChatId) -> bool {
    has(
        events,
        |e| matches!(e, Event::ForumInvalidated(c) if *c == chat),
    )
}

/// Whether core announced `chat` as a forum (`KindChanged`).
fn announced(events: &[Event], chat: ChatId) -> bool {
    has(
        events,
        |e| matches!(e, Event::KindChanged { chat: c, forum: ForumStatus::Forum } if *c == chat),
    )
}

/// Opens `thread` and waits until it is reconciled with the server tail.
async fn opened(h: &mut Harness, thread: ThreadKey) {
    h.send(Command::OpenThread(thread)).await;
    reconciled(h, thread).await;
}

/// A plain supergroup `s` beyond the first dialogs page, loaded and open.
async fn group_beyond_page_one() -> (Harness, ChatId) {
    let s = group(500);
    let mut script = FakeScript::demo(100, 1);
    add_group(&mut script, s, 60);
    let mut h = online(script).await;
    load_all_dialogs(&mut h).await;
    opened(&mut h, whole(s)).await;
    (h, s)
}

/// A chat beyond the first dialogs page becomes a forum. The `peer_dialogs`
/// check reaches it there; the fence holds its history (a `LoadOlder`
/// waits, a live message reaches no window), then the change invalidates
/// its views and answers the waiting load. Nothing reloads the old key, and
/// a dialogs page taken before the check, landing after it, can't revert
/// the committed kind, also of a chat it is the first to list (one the
/// client only had a window of).
#[tokio::test(start_paused = true)]
async fn a_chat_beyond_page_one_becomes_a_forum() {
    let (mut h, s) = group_beyond_page_one().await;
    // Another group the client only has a window of: the server lists it
    // on page two, which the client hasn't fetched since.
    let w = group(600);
    h.fake
        .edit_dialogs(|d| d.push(crate::backend::fake::summary(w, "Window only")));
    let posts = (1..=5).map(|id| text_message(w, id, "post")).collect();
    h.fake.set_history(w, posts);
    opened(&mut h, whole(w)).await;
    opened(&mut h, whole(s)).await;
    // Page two, taken while the chat is still a group, lands only after
    // the check (sent later) has settled it as a forum.
    run_for(Duration::from_secs(16)).await;
    h.send(Command::Window(WindowState::Active)).await;
    h.until(|e| matches!(e, Event::DialogsPaging { has_more: true }))
        .await;
    h.fake
        .inject(Op::Dialogs, Fault::Delay(Duration::from_secs(15)));
    let pages = h.fake.calls(Op::Dialogs);
    h.send(Command::LoadDialogs).await;
    while h.fake.calls(Op::Dialogs) == pages {
        events_for(&mut h, Duration::from_millis(10)).await;
    }
    h.fake
        .inject(Op::PeerDialogs, Fault::Delay(Duration::from_secs(10)));
    let history = h.fake.calls(Op::History);
    h.fake.set_kind(s, FORUM_KIND).await;
    h.fake.set_kind(w, FORUM_KIND).await;
    h.until(|e| matches!(e, Event::ThreadInvalidated(t) if *t == whole(s)))
        .await;
    h.send(Command::LoadOlder(whole(s))).await;
    let message = text_message(s, 61, "while fenced").into();
    h.fake.push_update(Update::NewMessage(message)).await;
    let fenced = events_for(&mut h, Duration::from_secs(5)).await;
    assert!(
        !has(&fenced, |e| matches!(e, Event::History { .. })),
        "{fenced:?}"
    );
    assert_eq!(h.fake.calls(Op::History), history, "no load while fenced");
    let seen = events_for(&mut h, Duration::from_secs(15)).await;
    assert!(forum_invalidated(&seen, s), "{seen:?}");
    assert!(last_bump(&seen, s).is_some_and(|c| c.kind == FORUM_KIND));
    let changes = seen
        .iter()
        .filter(|e| matches!(e, Event::ForumInvalidated(c) if *c == s))
        .count();
    assert_eq!(changes, 1, "the older page doesn't revert it: {seen:?}");
    // The window-only chat too: its committed kind is remembered until the
    // older page lists it, which then shows it as a forum.
    let changes = seen
        .iter()
        .filter(|e| matches!(e, Event::ForumInvalidated(c) if *c == w))
        .count();
    assert_eq!(changes, 1, "{seen:?}");
    assert!(announced(&seen, w), "unlisted, yet announced: {seen:?}");
    let listed = seen.iter().rev().find_map(|e| match e {
        Event::DialogsReset(v) | Event::DialogsAppended(v) => v.iter().find(|c| c.id == w),
        _ => None,
    });
    assert_eq!(listed.map(|c| c.kind), Some(FORUM_KIND), "{seen:?}");
    assert!(has(&seen, |e| matches!(
        e,
        Event::LoadEnded { target: LoadTarget::History { thread, .. }, error: None } if *thread == whole(s)
    )));
    assert_eq!(peer_calls_with(&h, s).len(), 1);
    assert_eq!(
        h.fake.calls(Op::History),
        history,
        "the old key isn't reloaded"
    );
    // The whole-chat key of a forum is refused now.
    h.send(Command::OpenThread(whole(s))).await;
    h.until(|e| matches!(e, Event::Error(UserError::Rejected)))
        .await;
    h.stop().await;
}

/// The forum flag goes off while a topic is open, then on while the
/// whole-chat thread is open: each change drops the topic list and the
/// open views (`ForumInvalidated`), and the keys follow the new kind. The
/// second time the check's snapshot predates the change, and a newer
/// dialogs page crossing it states the forum: the newer statement wins.
#[tokio::test(start_paused = true)]
async fn the_flag_off_and_on_while_a_thread_is_open() {
    let (script, f) = forum_with([(4, 2), (5, 2)]);
    let mut h = online(script).await;
    open_forum(&mut h, f).await;
    opened(&mut h, topic_key(f, 2)).await;
    h.fake.set_kind(f, GROUP_KIND).await;
    let seen = events_for(&mut h, Duration::from_secs(5)).await;
    assert!(forum_invalidated(&seen, f) && invalidated(&seen, topic_key(f, 2)));
    opened(&mut h, whole(f)).await;
    run_for(Duration::from_secs(16)).await;
    h.fake
        .inject(Op::PeerDialogs, Fault::Delay(Duration::from_secs(5)));
    h.fake.push_update(Update::ChatChanged { chat: f }).await;
    peer_calls_reached(&mut h, 2).await; // its snapshot: still a group
    h.fake.edit_dialogs(|d| {
        for c in d.iter_mut().filter(|c| c.id == f) {
            c.kind = FORUM_KIND;
        }
    });
    h.send(Command::Window(WindowState::Active)).await; // a newer first page
    let seen = events_for(&mut h, Duration::from_secs(10)).await;
    assert!(forum_invalidated(&seen, f) && invalidated(&seen, whole(f)));
    h.send(Command::OpenThread(whole(f))).await;
    h.until(|e| matches!(e, Event::Error(UserError::Rejected)))
        .await;
    opened(&mut h, topic_key(f, 2)).await;
    h.stop().await;
}

/// The chat becomes a forum, but its check fails twice, succeeds while a
/// trigger crosses it (so the forum it states is provisional, and it is
/// checked again), then fails a third time: abandoned. The pre-fence kind
/// is kept — no `ForumInvalidated`, the whole-chat key still opens —
/// continuity stays revoked (the windows are invalidated and the open tail
/// reloads), and the chat is suspect: its topic-headed messages don't fence
/// it again until the next light refresh checks it once more, which then
/// commits the change.
#[tokio::test(start_paused = true)]
async fn a_failing_check_is_abandoned_then_rechecked_on_refresh() {
    let s = group(500);
    let mut script = FakeScript::demo(1, 1);
    add_group(&mut script, s, 60);
    let mut h = online(script).await;
    opened(&mut h, whole(s)).await;
    let fail = Fault::Error(BackendError::Transient);
    for fault in [fail, fail, Fault::Delay(Duration::from_secs(5)), fail] {
        h.fake.inject(Op::PeerDialogs, fault);
    }
    h.fake.set_kind(s, FORUM_KIND).await;
    let mut seen = peer_calls_reached(&mut h, 3).await;
    h.fake.push_update(Update::ChatChanged { chat: s }).await;
    seen.extend(peer_calls_reached(&mut h, 4).await);
    seen.extend(events_for(&mut h, Duration::from_secs(2)).await);
    assert!(invalidated(&seen, whole(s)), "fence entry");
    assert!(
        reconciled_in(&seen, whole(s)),
        "abandoned: the tail reloads"
    );
    assert!(
        !forum_invalidated(&seen, s),
        "the provisional kind isn't kept"
    );
    assert!(last_bump(&seen, s).is_none_or(|c| c.kind == GROUP_KIND));
    assert_eq!(peer_calls_with(&h, s).len(), 4);
    opened(&mut h, whole(s)).await;
    // Suspect: a topic-headed message doesn't start another fence.
    let mut m = text_message(s, 61, "topic-headed");
    m.thread.topic = Some(tid(7));
    h.fake.push_update(Update::NewMessage(m.into())).await;
    events_for(&mut h, Duration::from_secs(10)).await;
    assert_eq!(peer_calls_with(&h, s).len(), 4, "not fenced again");
    // The light refresh checks it again, and the change goes through.
    h.send(Command::Window(WindowState::Active)).await;
    let seen = events_for(&mut h, Duration::from_secs(10)).await;
    assert_eq!(peer_calls_with(&h, s).len(), 5);
    assert!(forum_invalidated(&seen, s), "{seen:?}");
    assert_spaced(&h);
    h.stop().await;
}

/// A rebuild with the open chat fenced beyond page one. Bootstrap completes
/// without its tail; the kind check runs right after the baseline; when it
/// completes (or, below, is abandoned) the tail reloads and the load the
/// user asked for meanwhile is answered. Bootstrap never waits on the fence.
async fn rebuild_with_the_open_chat_fenced(after_rebuild: &[Fault]) {
    let (mut h, s) = group_beyond_page_one().await;
    h.fake.inject(Op::PeerDialogs, Fault::Hang);
    for fault in after_rebuild {
        h.fake.inject(Op::PeerDialogs, *fault);
    }
    h.fake.push_update(Update::ChatChanged { chat: s }).await;
    peer_calls_reached(&mut h, 1).await;
    let history = h.fake.calls(Op::History);
    h.internal.send(crate::actor::Internal::Wake).await.unwrap();
    let (_, before) = h
        .until(|e| matches!(e, Event::Connection(ConnState::Online)))
        .await;
    assert!(
        !has(
            &before,
            |e| matches!(e, Event::Reconciled { thread, .. } if *thread == whole(s))
        ),
        "{before:?}"
    );
    assert_eq!(
        h.fake.calls(Op::History),
        history,
        "no fenced tail in the boot"
    );
    h.send(Command::LoadOlder(whole(s))).await;
    let skipped = reconciled(&mut h, whole(s)).await;
    let checks = peer_calls_with(&h, s).len();
    assert_eq!(checks, 1 + after_rebuild.len().max(1));
    assert!(invalidated(&skipped, whole(s)), "the lift invalidates");
    assert!(
        has(&skipped, |e| matches!(
            e,
            Event::LoadEnded { target: LoadTarget::History { thread, .. }, error: None } if *thread == whole(s)
        )),
        "the waiting load is answered: {skipped:?}"
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_rebuild_with_the_open_chat_fenced() {
    rebuild_with_the_open_chat_fenced(&[Fault::Delay(Duration::from_secs(5))]).await;
}

#[tokio::test(start_paused = true)]
async fn a_rebuild_with_the_open_chat_fenced_and_abandoned() {
    let fail = Fault::Error(BackendError::Transient);
    rebuild_with_the_open_chat_fenced(&[fail, fail, fail]).await;
}
