//! The archive and folder menu (folders plan, part C) through the real
//! window: each choice sends one command and closes the menu; a new folder
//! takes its title from the field; deleting asks first.

use abyss_core::model::{ChatFolder, FolderRules, PeerFolder};
use gpui::{point, px};

use super::*;

fn chat(id: i64) -> ChatId {
    ChatId {
        kind: PeerKind::User,
        id,
    }
}

fn listed(root: &Entity<Root>, cx: &mut VisualTestContext, harness: &Harness) {
    sign_in(root, cx, harness);
    let script = FakeScript::demo(3, 0);
    harness
        .events
        .unbounded_send(Event::DialogsReset(script.dialogs))
        .unwrap();
    let work = ChatFolder {
        id: 2,
        title: "Work".into(),
        rules: FolderRules::default(),
        pinned: Vec::new(),
        include: vec![chat(1)],
        exclude: Vec::new(),
        shared: false,
    };
    harness
        .events
        .unbounded_send(Event::ChatFolders(vec![work]))
        .unwrap();
    cx.run_until_parked();
}

fn sent_since(harness: &Harness, from: usize) -> Vec<String> {
    harness
        .sent
        .borrow()
        .commands
        .iter()
        .skip(from)
        .map(|c| format!("{c:?}"))
        .collect()
}

fn menu_open(root: &Entity<Root>, cx: &mut VisualTestContext) -> bool {
    read(root, cx, |s| s.menu.is_some())
}

#[gpui::test]
fn archiving_sends_one_command_and_closes_the_menu(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    listed(&root, cx, &harness);
    let from = harness.sent.borrow().commands.len();
    root.update_in(cx, |root, window, cx| {
        root.with_session(
            |s, cx| {
                s.open_chat_menu(chat(1), point(px(100.), px(100.)), window, cx);
                s.set_peer_folder(chat(1), PeerFolder::Archive, window, cx);
            },
            cx,
        );
    });
    cx.run_until_parked();
    assert!(!menu_open(&root, cx));
    let sent = sent_since(&harness, from);
    assert_eq!(sent.len(), 1);
    assert!(sent[0].starts_with("SetPeerFolder") && sent[0].contains("Archive"));
    // Nothing changes before core says so.
    assert_eq!(
        read(&root, cx, |s| s.shell.list.get(chat(1)).map(|c| c.folder)),
        Some(PeerFolder::Main)
    );
}

#[gpui::test]
fn a_new_folder_takes_the_typed_title(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    listed(&root, cx, &harness);
    root.update_in(cx, |root, window, cx| {
        root.with_session(
            |s, cx| {
                s.open_chat_menu(chat(1), point(px(100.), px(100.)), window, cx);
                s.start_naming(window, cx);
            },
            cx,
        );
    });
    cx.run_until_parked();
    let from = harness.sent.borrow().commands.len();
    cx.simulate_input("Family");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let sent = sent_since(&harness, from);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].starts_with("EditChatFolder(Create"));
    assert!(!sent[0].contains("Family"), "titles stay out of logs");
    assert!(!menu_open(&root, cx));
}

#[gpui::test]
fn deleting_a_folder_asks_first(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    listed(&root, cx, &harness);
    let from = harness.sent.borrow().commands.len();
    let delete = |root: &Entity<Root>, cx: &mut VisualTestContext| {
        root.update_in(cx, |root, window, cx| {
            root.with_session(|s, cx| s.delete_folder(window, cx), cx);
        });
        cx.run_until_parked();
    };
    root.update_in(cx, |root, window, cx| {
        root.with_session(
            |s, cx| s.open_folder_menu(2, point(px(50.), px(60.)), window, cx),
            cx,
        );
    });
    delete(&root, cx);
    assert!(
        sent_since(&harness, from).is_empty(),
        "the first click asks"
    );
    assert!(menu_open(&root, cx));
    delete(&root, cx);
    let sent = sent_since(&harness, from);
    assert_eq!(sent, ["EditChatFolder(Delete { folder: 2, .. })"]);
    assert!(!menu_open(&root, cx));
}

/// Work (id 2) is replaced by Family (id 2) on another device between the
/// two clicks: the pending delete is dropped with its menu, and nothing is
/// deleted.
#[gpui::test]
fn a_folder_replaced_between_the_clicks_is_not_deleted(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    listed(&root, cx, &harness);
    let from = harness.sent.borrow().commands.len();
    let delete = |root: &Entity<Root>, cx: &mut VisualTestContext| {
        root.update_in(cx, |root, window, cx| {
            root.with_session(|s, cx| s.delete_folder(window, cx), cx);
        });
        cx.run_until_parked();
    };
    root.update_in(cx, |root, window, cx| {
        root.with_session(
            |s, cx| s.open_folder_menu(2, point(px(50.), px(60.)), window, cx),
            cx,
        );
    });
    delete(&root, cx);
    assert!(menu_open(&root, cx), "the first click asks");
    let family = ChatFolder {
        id: 2,
        title: "Family".into(),
        rules: FolderRules::default(),
        pinned: Vec::new(),
        include: Vec::new(),
        exclude: Vec::new(),
        shared: false,
    };
    harness
        .events
        .unbounded_send(Event::ChatFolders(vec![family]))
        .unwrap();
    cx.run_until_parked();
    assert!(!menu_open(&root, cx), "the pending delete is dropped");
    delete(&root, cx);
    assert!(sent_since(&harness, from).is_empty(), "nothing is deleted");
}

/// Signed in with the folders listed, then "Logging out" and the report
/// that waiting folder changes were cancelled.
fn cancelled_behind_a_logout(root: &Entity<Root>, cx: &mut VisualTestContext, harness: &Harness) {
    listed(root, cx, harness);
    core_says(harness, cx, AuthState::LogoutStarting);
    harness
        .events
        .unbounded_send(Event::Error(UserError::FolderWritesCancelled))
        .unwrap();
    cx.run_until_parked();
    assert!(read(root, cx, |s| s.writes_cancelled));
}

/// The cancellation notice outlives a passing notice.
fn still_told_after_a_while(root: &Entity<Root>, cx: &mut VisualTestContext) {
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(30));
    cx.run_until_parked();
    assert!(
        read(root, cx, |s| s.writes_cancelled),
        "kept while a logout screen shows"
    );
}

/// Told as (or in) a passing notice now.
fn told_in_passing(root: &Entity<Root>, cx: &mut VisualTestContext) {
    assert!(!read(root, cx, |s| s.writes_cancelled));
    let cancelled = crate::ui::copy::user_error(UserError::FolderWritesCancelled);
    let notice = read(root, cx, |s| s.notice.clone()).expect("a notice");
    assert!(notice.ends_with(cancelled.as_ref()), "{notice}");
}

/// "Stay signed in" after a logout that could not start.
#[gpui::test]
fn cancelled_folder_changes_are_told_until_stay_signed_in(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    cancelled_behind_a_logout(&root, cx, &harness);
    core_says(
        &harness,
        cx,
        AuthState::LogoutNotStarted {
            error: abyss_core::api::StorageError::Keychain,
            trigger: abyss_core::api::LogoutTrigger::User,
        },
    );
    still_told_after_a_while(&root, cx);
    core_says(
        &harness,
        cx,
        AuthState::SignedIn {
            display_name: "Demo User".into(),
            new_login: None,
        },
    );
    told_in_passing(&root, cx);
}

/// A logout that finishes: told through "Logging out", then on the login
/// screen.
#[gpui::test]
fn cancelled_folder_changes_are_told_through_a_finished_logout(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    cancelled_behind_a_logout(&root, cx, &harness);
    core_says(&harness, cx, AuthState::LoggingOut);
    still_told_after_a_while(&root, cx);
    core_says(
        &harness,
        cx,
        AuthState::LoggedOut {
            remote: abyss_core::api::RemoteLogout::Revoked,
            local: abyss_core::api::LocalDeletion::Deleted,
        },
    );
    told_in_passing(&root, cx);
}

/// A logout whose local step failed: still told on "Logout incomplete".
#[gpui::test]
fn cancelled_folder_changes_are_told_through_an_incomplete_logout(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    cancelled_behind_a_logout(&root, cx, &harness);
    core_says(&harness, cx, AuthState::LoggingOut);
    core_says(
        &harness,
        cx,
        AuthState::LoggedOut {
            remote: abyss_core::api::RemoteLogout::Revoked,
            local: abyss_core::api::LocalDeletion::Failed(abyss_core::api::StorageError::Keychain),
        },
    );
    still_told_after_a_while(&root, cx);
}

/// Forty folders in a short window: the chat menu fits the window and
/// scrolls, so its last entry ("New folder…") can be reached.
#[gpui::test]
fn a_long_menu_scrolls_to_its_last_entry(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    listed(&root, cx, &harness);
    let folders = (2..42)
        .map(|id| ChatFolder {
            id,
            title: format!("f{id}"),
            rules: FolderRules::default(),
            pinned: Vec::new(),
            include: Vec::new(),
            exclude: Vec::new(),
            shared: false,
        })
        .collect();
    harness
        .events
        .unbounded_send(Event::ChatFolders(folders))
        .unwrap();
    cx.simulate_resize(gpui::size(px(800.), px(300.)));
    cx.run_until_parked();
    root.update_in(cx, |root, window, cx| {
        root.with_session(
            |s, cx| s.open_chat_menu(chat(1), point(px(100.), px(100.)), window, cx),
            cx,
        );
    });
    cx.run_until_parked();
    let below = cx.debug_bounds("menu-new").expect("the entry is laid out");
    assert!(below.bottom() > px(300.), "starts out of view: {below:?}");
    cx.simulate_event(gpui::ScrollEvent {
        position: point(px(150.), px(150.)),
        delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-5000.))),
        ..Default::default()
    });
    cx.run_until_parked();
    let after = cx.debug_bounds("menu-new").expect("the entry is laid out");
    assert!(
        after.bottom() <= px(300.) && after.top() >= px(0.),
        "scrolled into view: {after:?}"
    );
}
