//! Peer folders: chats moving into and out of the archive live.

use std::time::Duration;

use super::{CacheDir, Harness, cache_at, chat, harness, harness_with};
use crate::api::{Command, Event, WindowState};
use crate::backend::fake::{FakeBackend, FakeScript, Fault, Op};
use crate::backend::{BackendError, Update};
use crate::cache::{CacheCmd, CacheHandle, CacheKey};
use crate::model::{ChatId, PeerFolder};

/// A live `updateFolderPeers` changes listed chats in place, once, and
/// ignores chats the list does not hold.
#[tokio::test(start_paused = true)]
async fn archive_moves_update_listed_chats_in_place() {
    let mut h = harness(FakeScript::demo(3, 1), None).await;
    h.online().await;
    h.until(|e| matches!(e, Event::DialogsReset(c) if c.len() == 3))
        .await;
    let moves = vec![
        (chat(2), PeerFolder::Archive),
        (chat(99), PeerFolder::Archive),
    ];
    h.fake.push_update(Update::Folders(moves.clone())).await;
    let (ev, _) = h.until(|e| matches!(e, Event::ChatBumped(_))).await;
    let Event::ChatBumped(summary) = ev else {
        unreachable!()
    };
    assert_eq!((summary.id, summary.folder), (chat(2), PeerFolder::Archive));
    // The same move again changes nothing; a read marks the end.
    h.fake.push_update(Update::Folders(moves)).await;
    h.fake
        .push_update(Update::ReadInbox {
            chat: chat(1),
            topic: None,
            still_unread: 0,
        })
        .await;
    let (ev, skipped) = h.until(|e| matches!(e, Event::ChatBumped(_))).await;
    assert!(matches!(ev, Event::ChatBumped(s) if s.id == chat(1)));
    assert!(!skipped.iter().any(|e| matches!(e, Event::ChatBumped(_))));
    h.fake
        .push_update(Update::Folders(vec![(chat(2), PeerFolder::Main)]))
        .await;
    let (ev, _) = h.until(|e| matches!(e, Event::ChatBumped(_))).await;
    assert!(matches!(ev, Event::ChatBumped(s) if s.id == chat(2) && s.folder == PeerFolder::Main));
    h.stop().await;
}

/// Waits until the server has seen `n` dialogs calls.
async fn dialogs_calls(h: &Harness, n: usize) {
    for _ in 0..1000 {
        if h.fake.calls(Op::Dialogs) >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the server never saw dialogs call #{n}");
}

/// The server archives `target` and tells the client while a dialogs
/// response taken before the move (it answers at call time, then waits 2 s)
/// is in flight. The live move is applied at once.
async fn archive_across_a_response(h: &mut Harness, target: ChatId) {
    h.fake.edit_dialogs(|d| {
        for c in d.iter_mut().filter(|c| c.id == target) {
            c.folder = PeerFolder::Archive;
        }
    });
    h.fake
        .push_update(Update::Folders(vec![(target, PeerFolder::Archive)]))
        .await;
    h.until(
        |e| matches!(e, Event::ChatBumped(s) if s.id == target && s.folder == PeerFolder::Archive),
    )
    .await;
}

fn folder_in(ev: &Event, target: ChatId) -> Option<PeerFolder> {
    match ev {
        Event::DialogsReset(v) | Event::DialogsAppended(v) => {
            v.iter().find(|c| c.id == target).map(|c| c.folder)
        }
        _ => None,
    }
}

/// Waits for the next dialogs page applied, and the folder it shows for
/// `target` (`None` when the page leaves the row unchanged).
async fn next_page(h: &mut Harness, target: ChatId) -> Option<PeerFolder> {
    let (ev, _) = h
        .until(|e| matches!(e, Event::DialogsReset(_) | Event::DialogsAppended(_)))
        .await;
    folder_in(&ev, target)
}

/// Chats 1 to 3 listed from the cache, with no server page applied: the
/// bootstrap's dialogs call failed, so the next first page is the UI's
/// (bootstrap snapshots buffer live updates, so they never cross a move).
/// The directory comes first, so it is dropped last.
async fn cached_list() -> (CacheDir, Harness, CacheHandle) {
    let dir = CacheDir::new();
    let key = CacheKey::generate().unwrap();
    let cache = cache_at(dir.path(), &key);
    let script = FakeScript::demo(3, 1);
    cache
        .write(CacheCmd::PutChats(script.dialogs.clone()))
        .await;
    cache.barrier().await.unwrap();
    let (backend, fake) = FakeBackend::new(script);
    fake.inject(Op::Dialogs, Fault::Error(BackendError::Transient));
    let mut h = harness_with(backend, fake, Some(cache.clone())).await;
    // The bootstrap's dialogs fail; the cached list arrives after.
    h.until(|e| matches!(e, Event::LoadEnded { error: Some(_), .. }))
        .await;
    h.until(|e| matches!(e, Event::DialogsReset(c) if c.len() == 3))
        .await;
    (dir, h, cache)
}

async fn cached_folder(cache: &CacheHandle, target: ChatId) -> Option<PeerFolder> {
    cache.barrier().await.unwrap();
    let cached = cache.chats().await.unwrap();
    cached.iter().find(|c| c.id == target).map(|c| c.folder)
}

/// A first page dispatched before a live move, applied after it, does not
/// move the chat back; the move is kept (and cached) and a first page is
/// fetched again, which agrees with it.
#[tokio::test(start_paused = true)]
async fn a_first_page_crossing_a_move_keeps_the_move() {
    let (_dir, mut h, cache) = cached_list().await;
    let target = chat(2);
    h.fake
        .inject(Op::Dialogs, Fault::Delay(Duration::from_secs(2)));
    h.send(Command::LoadDialogs).await;
    dialogs_calls(&h, 2).await;
    archive_across_a_response(&mut h, target).await;
    let crossing = next_page(&mut h, target).await;
    assert_eq!(crossing, Some(PeerFolder::Archive), "the move is kept");
    assert_eq!(
        next_page(&mut h, target).await,
        Some(PeerFolder::Archive),
        "the refetched first page agrees"
    );
    assert_eq!(h.fake.calls(Op::Dialogs), 3, "one refetch");
    assert_eq!(
        cached_folder(&cache, target).await,
        Some(PeerFolder::Archive)
    );
    h.stop().await;
}

/// A later page dispatched before a live move, applied after it, does not
/// move the chat back either. The chat is a provisional row (kept from
/// page two across a refresh) that the page re-confirms.
#[tokio::test(start_paused = true)]
async fn a_later_page_crossing_a_move_keeps_the_move() {
    let mut h = harness(FakeScript::demo(150, 0), None).await;
    h.online().await;
    h.send(Command::LoadDialogs).await;
    h.until(|e| matches!(e, Event::DialogsPaging { has_more: false }))
        .await;
    tokio::time::sleep(Duration::from_secs(16)).await;
    h.send(Command::Window(WindowState::Active)).await;
    h.until(|e| matches!(e, Event::DialogsPaging { has_more: true }))
        .await;
    let mut target = chat(0);
    h.fake.edit_dialogs(|d| target = d[120].id);
    let calls = h.fake.calls(Op::Dialogs);
    h.fake
        .inject(Op::Dialogs, Fault::Delay(Duration::from_secs(2)));
    h.send(Command::LoadDialogs).await;
    dialogs_calls(&h, calls + 1).await;
    archive_across_a_response(&mut h, target).await;
    let crossing = next_page(&mut h, target).await;
    assert_ne!(crossing, Some(PeerFolder::Main), "the move is kept");
    assert_eq!(
        next_page(&mut h, target).await,
        Some(PeerFolder::Archive),
        "the refetched first page keeps it"
    );
    assert_eq!(h.fake.calls(Op::Dialogs), calls + 2, "one refetch");
    h.stop().await;
}

/// More moves than core remembers cross a first page: the listed chat's
/// move (the oldest) is evicted, so the page could undo it unguarded. The
/// page is dropped instead and a first page fetched again; the chat never
/// shows its stale folder, and the cache keeps the move.
#[tokio::test(start_paused = true)]
async fn a_page_older_than_an_evicted_move_is_dropped() {
    let (_dir, mut h, cache) = cached_list().await;
    let target = chat(2);
    h.fake
        .inject(Op::Dialogs, Fault::Delay(Duration::from_secs(2)));
    h.send(Command::LoadDialogs).await;
    dialogs_calls(&h, 2).await;
    archive_across_a_response(&mut h, target).await;
    // A thousand unlisted chats archived elsewhere, in one update.
    let others = (1000..2000)
        .map(|n| (chat(n), PeerFolder::Archive))
        .collect();
    h.fake.push_update(Update::Folders(others)).await;
    // The crossing page is dropped: the next list is the refetch's.
    let (ev, skipped) = h
        .until(|e| matches!(e, Event::DialogsReset(_) | Event::DialogsAppended(_)))
        .await;
    assert!(
        skipped
            .iter()
            .any(|e| matches!(e, Event::DialogsPaging { .. })),
        "the dropped page still answers the load: {skipped:?}"
    );
    assert_eq!(folder_in(&ev, target), Some(PeerFolder::Archive));
    assert_eq!(h.fake.calls(Op::Dialogs), 3, "one refetch");
    assert_eq!(
        cached_folder(&cache, target).await,
        Some(PeerFolder::Archive)
    );
    h.stop().await;
}

/// Folders plan part C: a local archive goes through the same path as
/// `updateFolderPeers`, so a page dispatched before it cannot undo it.
#[tokio::test(start_paused = true)]
async fn a_local_archive_is_guarded_against_an_older_page() {
    let (_dir, mut h, cache) = cached_list().await;
    let target = chat(2);
    h.fake
        .inject(Op::Dialogs, Fault::Delay(Duration::from_secs(2)));
    h.send(Command::LoadDialogs).await;
    dialogs_calls(&h, 2).await;
    h.send(Command::SetPeerFolder {
        chat: target,
        folder: PeerFolder::Archive,
    })
    .await;
    h.until(
        |e| matches!(e, Event::ChatBumped(s) if s.id == target && s.folder == PeerFolder::Archive),
    )
    .await;
    assert_eq!(
        next_page(&mut h, target).await,
        Some(PeerFolder::Archive),
        "the older page keeps the move"
    );
    assert_eq!(
        next_page(&mut h, target).await,
        Some(PeerFolder::Archive),
        "the refetched first page agrees"
    );
    assert_eq!(
        cached_folder(&cache, target).await,
        Some(PeerFolder::Archive)
    );
    h.stop().await;
}
