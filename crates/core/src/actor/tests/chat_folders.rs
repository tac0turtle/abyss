//! Chat folders: loaded after the first dialogs page, refetched on change.

use std::time::Duration;

use super::{Harness, harness};
use crate::api::{AuthState, Command, Event, UserError};
use crate::backend::Update;
use crate::backend::fake::{FakeScript, Fault, Op};
use crate::model::{ChatFolder, FolderEdit, FolderRules, PeerFolder};
use crate::secret::SecretItem;

fn folder(id: i32, title: &str) -> ChatFolder {
    ChatFolder {
        id,
        title: title.into(),
        rules: FolderRules::GROUPS,
        pinned: Vec::new(),
        include: Vec::new(),
        exclude: Vec::new(),
        shared: false,
    }
}

#[tokio::test(start_paused = true)]
async fn folders_follow_the_first_page_and_every_change() {
    let mut script = FakeScript::demo(3, 1);
    script.folders = vec![folder(2, "Work")];
    let mut h = harness(script, None).await;
    let (ev, skipped) = h.until(|e| matches!(e, Event::ChatFolders(_))).await;
    assert!(matches!(ev, Event::ChatFolders(f) if f == vec![folder(2, "Work")]));
    assert!(
        skipped.iter().any(|e| matches!(e, Event::DialogsReset(_))),
        "after the first page"
    );
    h.fake
        .set_folders(vec![folder(2, "Work"), folder(3, "Home")]);
    h.fake.push_update(Update::ChatFoldersChanged).await;
    let (ev, _) = h.until(|e| matches!(e, Event::ChatFolders(_))).await;
    assert!(matches!(ev, Event::ChatFolders(f) if f.len() == 2));
    assert_eq!(h.fake.calls(Op::ChatFolders), 2);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn archiving_applies_once_the_server_accepts() {
    use crate::api::Command;
    use crate::model::PeerFolder;
    let mut h = harness(FakeScript::demo(3, 1), None).await;
    h.online().await;
    h.until(|e| matches!(e, Event::DialogsReset(c) if c.len() == 3))
        .await;
    let chat = super::chat(2);
    h.send(Command::SetPeerFolder {
        chat,
        folder: PeerFolder::Archive,
    })
    .await;
    let (ev, _) = h.until(|e| matches!(e, Event::ChatBumped(_))).await;
    assert!(matches!(ev, Event::ChatBumped(s) if s.id == chat && s.folder == PeerFolder::Archive));
    assert_eq!(h.fake.calls(Op::SetPeerFolder), 1);
    let server = h.fake.dialogs();
    assert!(
        server
            .iter()
            .any(|c| c.id == chat && c.folder == PeerFolder::Archive)
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn folder_edits_refetch_and_bad_titles_never_leave() {
    use crate::api::{Command, UserError};
    use crate::model::FolderEdit;
    let mut script = FakeScript::demo(3, 1);
    script.folders = vec![folder(2, "Work")];
    let mut h = harness(script, None).await;
    h.until(|e| matches!(e, Event::ChatFolders(_))).await;
    let chat = super::chat(1);
    h.send(Command::EditChatFolder(FolderEdit::AddChat {
        folder: 2,
        tag: folder(2, "Work").tag(),
        chat,
    }))
    .await;
    let (ev, _) = h.until(|e| matches!(e, Event::ChatFolders(_))).await;
    assert!(matches!(ev, Event::ChatFolders(f) if f[0].include == vec![chat]));
    h.send(Command::EditChatFolder(FolderEdit::Create {
        title: "   ".into(),
        chat,
    }))
    .await;
    h.until(|e| matches!(e, Event::Error(UserError::Rejected)))
        .await;
    assert_eq!(
        h.fake.calls(Op::EditChatFolder),
        1,
        "the blank title was not sent"
    );
    h.send(Command::EditChatFolder(FolderEdit::Delete {
        folder: 9,
        tag: folder(9, "Gone").tag(),
    }))
    .await;
    h.until(|e| matches!(e, Event::Error(UserError::FolderChanged)))
        .await;
    h.stop().await;
}

// --- folder writes run one at a time (review: P1) ---

/// Online with folder 2 "Work" (no chats) and its first load in.
async fn with_work() -> Harness {
    let mut script = FakeScript::demo(3, 1);
    script.folders = vec![folder(2, "Work")];
    let mut h = harness(script, None).await;
    h.until(|e| matches!(e, Event::ChatFolders(_))).await;
    h
}

fn add(chat: i64) -> Command {
    Command::EditChatFolder(FolderEdit::AddChat {
        folder: 2,
        tag: folder(2, "Work").tag(),
        chat: super::chat(chat),
    })
}

fn create(title: &str) -> Command {
    Command::EditChatFolder(FolderEdit::Create {
        title: title.into(),
        chat: super::chat(1),
    })
}

/// Waits until `n` folder edits have reached the server and every
/// refetch after them is in.
async fn edits_done(h: &mut Harness, n: usize) {
    for _ in 0..2000 {
        if h.fake.calls(Op::EditChatFolder) >= n {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(h.fake.calls(Op::EditChatFolder) >= n, "edit #{n} never ran");
    tokio::time::sleep(Duration::from_secs(60)).await;
}

/// Two adds to one folder: the first is slow (it fetched, then its write
/// lands 2 s later). The second waits for it, so it starts from the first
/// one's result and both chats end up in the folder.
#[tokio::test(start_paused = true)]
async fn two_adds_to_one_folder_both_land() {
    let mut h = with_work().await;
    h.fake
        .inject(Op::EditChatFolder, Fault::Delay(Duration::from_secs(2)));
    h.send(add(1)).await;
    h.send(add(2)).await;
    edits_done(&mut h, 2).await;
    assert_eq!(
        h.fake.folders()[0].include,
        [super::chat(1), super::chat(2)]
    );
    h.stop().await;
}

/// Two creates: the second sees the first one's folder and takes the next
/// free id.
#[tokio::test(start_paused = true)]
async fn two_creates_take_two_ids() {
    let mut h = with_work().await;
    h.fake
        .inject(Op::EditChatFolder, Fault::Delay(Duration::from_secs(2)));
    h.send(create("A")).await;
    h.send(create("B")).await;
    edits_done(&mut h, 2).await;
    let ids: Vec<(i32, String)> = h
        .fake
        .folders()
        .into_iter()
        .map(|f| (f.id, f.title))
        .collect();
    assert_eq!(ids, [(2, "Work".into()), (3, "A".into()), (4, "B".into())]);
    h.stop().await;
}

/// An add, then a delete of the same folder: the delete runs after the
/// add's write, so the folder stays deleted.
#[tokio::test(start_paused = true)]
async fn a_delete_after_an_edit_is_not_undone() {
    let mut h = with_work().await;
    h.fake
        .inject(Op::EditChatFolder, Fault::Delay(Duration::from_secs(2)));
    h.send(add(1)).await;
    h.send(Command::EditChatFolder(FolderEdit::Delete {
        folder: 2,
        tag: folder(2, "Work").tag(),
    }))
    .await;
    edits_done(&mut h, 2).await;
    assert!(h.fake.folders().is_empty());
    h.stop().await;
}

/// An uncertain outcome: the first add lands but its answer never comes.
/// The next write waits for its deadline, then starts from the server's
/// state, so neither is lost.
#[tokio::test(start_paused = true)]
async fn after_an_uncertain_write_the_next_starts_from_the_server() {
    let mut h = with_work().await;
    h.fake.inject(Op::EditChatFolder, Fault::CompleteThenHang);
    h.send(add(1)).await;
    h.send(add(2)).await;
    h.until(|e| matches!(e, Event::Error(UserError::Network)))
        .await;
    edits_done(&mut h, 2).await;
    assert_eq!(
        h.fake.folders()[0].include,
        [super::chat(1), super::chat(2)]
    );
    h.stop().await;
}

/// A write that timed out may still land: in the fake it sits in the
/// transport and lands when its instance's connection closes. The next
/// write does not reach the server while that instance lives (core retires
/// it at once), so here its fetch sees the late write. (On a real server
/// that ordering is best-effort, not guaranteed: folders plan, "Risks".)
#[tokio::test(start_paused = true)]
async fn a_timed_out_write_holds_the_next_until_its_transport_is_retired() {
    let mut h = with_work().await;
    let retired = h.fake.retired();
    h.fake.inject(Op::EditChatFolder, Fault::LandsLate);
    h.send(add(1)).await;
    h.send(add(2)).await;
    h.until(|e| matches!(e, Event::Error(UserError::Network)))
        .await;
    for _ in 0..2000 {
        if h.fake.retired() > retired {
            break;
        }
        assert_eq!(
            h.fake.calls(Op::EditChatFolder),
            1,
            "the next write waits for the old transport"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(h.fake.retired() > retired, "the instance was retired");
    edits_done(&mut h, 2).await;
    assert_eq!(
        h.fake.folders()[0].include,
        [super::chat(1), super::chat(2)],
        "the next write started from the late one's result"
    );
    h.stop().await;
}

/// A transport error may also hide an unknown outcome: it is handled like
/// a timeout (the instance is retired before the next write starts).
#[tokio::test(start_paused = true)]
async fn a_transport_error_holds_the_next_write_like_a_timeout() {
    let mut h = with_work().await;
    let retired = h.fake.retired();
    h.fake.inject(
        Op::EditChatFolder,
        Fault::Error(crate::backend::BackendError::Transient),
    );
    h.send(add(1)).await;
    h.send(add(2)).await;
    h.until(|e| matches!(e, Event::Error(UserError::Network)))
        .await;
    for _ in 0..2000 {
        if h.fake.retired() > retired {
            break;
        }
        assert_eq!(h.fake.calls(Op::EditChatFolder), 1, "held");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(h.fake.retired() > retired, "the instance was retired");
    edits_done(&mut h, 2).await;
    assert_eq!(h.fake.folders()[0].include, [super::chat(2)]);
    h.stop().await;
}

/// The retirement of the instance carrying a write panics, and the write's
/// completion is lost with it: the write counts as uncertain, the slot is
/// freed with the retirement, and the next write runs on the new instance.
#[tokio::test(start_paused = true)]
async fn a_write_lost_with_a_panicked_retirement_frees_the_slot() {
    let mut h = with_work().await;
    h.fake.inject(Op::EditChatFolder, Fault::Hang);
    h.send(add(1)).await;
    for _ in 0..1000 {
        if h.fake.calls(Op::EditChatFolder) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    h.send(add(2)).await;
    h.fake.panic_next_retire();
    h.internal.send(crate::actor::Internal::Wake).await.unwrap();
    edits_done(&mut h, 2).await;
    assert_eq!(h.fake.folders()[0].include, [super::chat(2)]);
    h.stop().await;
}

// --- the logout marker gate (review: P2) ---

/// While "Logging out" shows (the marker write waits), archive moves and
/// folder edits are refused and none reaches the server; one that waited
/// behind a slow write is dropped.
#[tokio::test(start_paused = true)]
async fn folder_writes_wait_for_no_pending_logout() {
    let mut h = with_work().await;
    h.fake
        .inject(Op::EditChatFolder, Fault::Delay(Duration::from_secs(2)));
    h.send(add(1)).await;
    h.send(add(2)).await;
    h.faults.hold_item(SecretItem::LogoutPending);
    h.send(Command::Logout).await;
    h.auth(|s| *s == AuthState::LogoutStarting).await;
    // The waiting add(2) was dropped, and the user is told.
    h.until(|e| matches!(e, Event::Error(UserError::FolderWritesCancelled)))
        .await;
    h.send(Command::SetPeerFolder {
        chat: super::chat(1),
        folder: PeerFolder::Archive,
    })
    .await;
    h.send(add(3)).await;
    h.until(|e| matches!(e, Event::Error(UserError::Busy)))
        .await;
    h.until(|e| matches!(e, Event::Error(UserError::Busy)))
        .await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(h.fake.calls(Op::SetPeerFolder), 0);
    assert_eq!(
        h.fake.calls(Op::EditChatFolder),
        1,
        "only the one in flight"
    );
    assert_eq!(h.fake.folders()[0].include, [super::chat(1)]);
    h.faults.release();
    h.stop().await;
}

/// "Couldn't start logout" (the marker write failed) refuses them too.
#[tokio::test(start_paused = true)]
async fn folder_writes_are_refused_after_a_failed_marker() {
    let mut h = with_work().await;
    h.faults.fail_set_of(SecretItem::LogoutPending);
    h.send(Command::Logout).await;
    h.auth(|s| matches!(s, AuthState::LogoutNotStarted { .. }))
        .await;
    h.send(add(1)).await;
    h.until(|e| matches!(e, Event::Error(UserError::Busy)))
        .await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(h.fake.calls(Op::EditChatFolder), 0);
    h.stop().await;
}

// --- a folders load that panics (review: P6) ---

/// A change arrives while a load runs, so another is wanted; the running
/// one panics. The wanted load still runs and its folders arrive.
#[tokio::test(start_paused = true)]
async fn a_panicking_load_keeps_the_next_one() {
    let mut h = with_work().await;
    // No dialogs page completes from here on, so none asks for the folders.
    for _ in 0..4 {
        h.fake.inject(Op::Dialogs, Fault::Hang);
    }
    h.fake.lead(Op::ChatFolders, Duration::from_secs(1));
    h.fake.inject(Op::ChatFolders, Fault::Panic);
    h.fake.push_update(Update::ChatFoldersChanged).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.fake
        .set_folders(vec![folder(2, "Work"), folder(3, "Home")]);
    h.fake.push_update(Update::ChatFoldersChanged).await;
    let (ev, _) = h
        .until(|e| matches!(e, Event::ChatFolders(f) if f.len() == 2))
        .await;
    assert!(matches!(ev, Event::ChatFolders(_)));
    assert_eq!(h.fake.calls(Op::ChatFolders), 3);
    h.stop().await;
}

// --- a stale edit cannot hit a replacement folder (review: P2) ---

/// Folder 2 is deleted and another folder 2 made on another device before
/// a delete made for the old one runs: it is refused (`FolderChanged`)
/// and the new folder stays.
#[tokio::test(start_paused = true)]
async fn a_delete_for_a_replaced_folder_is_refused() {
    let mut h = with_work().await;
    h.fake.set_folders(vec![folder(2, "Family")]);
    h.send(Command::EditChatFolder(FolderEdit::Delete {
        folder: 2,
        tag: folder(2, "Work").tag(),
    }))
    .await;
    h.until(|e| matches!(e, Event::Error(UserError::FolderChanged)))
        .await;
    assert_eq!(h.fake.folders(), [folder(2, "Family")]);
    // An edit of the folder's chats keeps its tag.
    let mut named = folder(2, "Family");
    named.include = vec![super::chat(3)];
    h.fake.set_folders(vec![named]);
    h.send(Command::EditChatFolder(FolderEdit::AddChat {
        folder: 2,
        tag: folder(2, "Family").tag(),
        chat: super::chat(1),
    }))
    .await;
    edits_done(&mut h, 2).await;
    assert_eq!(
        h.fake.folders()[0].include,
        [super::chat(3), super::chat(1)]
    );
    h.stop().await;
}
