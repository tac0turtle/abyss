//! The sidebar's tabs (plan section 17.2) through the real window: Ctrl+Tab
//! and Ctrl+Shift+Tab walk the rail, the choice is kept in the sidebar
//! file, a restart restores it once the folders arrive, and logout deletes
//! the file. The unread filter removes chats as soon as they are read.

use abyss_core::model::{ChatFolder, ChatSummary, FolderRules};
use gpui::px;

use super::*;
use crate::chat::list::{Shown, Tab};
use crate::chat::sidebar_file::SidebarFile;

fn folders(harness: &Harness, cx: &mut VisualTestContext) {
    let work = ChatFolder {
        id: 2,
        title: "Work".into(),
        rules: FolderRules::GROUPS,
        pinned: Vec::new(),
        include: Vec::new(),
        exclude: Vec::new(),
        shared: false,
    };
    harness
        .events
        .unbounded_send(Event::ChatFolders(vec![work]))
        .unwrap();
    cx.run_until_parked();
}

fn shown(root: &Entity<Root>, cx: &mut VisualTestContext) -> Shown {
    read(root, cx, |s| s.shell.list.shown())
}

/// Many folders overflow the rail: it scrolls vertically, and selecting a
/// tab off-screen scrolls it into view. The archive keeps the rail, with
/// nothing selected.
#[gpui::test]
fn the_rail_scrolls_and_selection_reveals_hidden_folders(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    sign_in(&root, cx, &harness);
    let folders = (2..80)
        .map(|id| ChatFolder {
            id,
            title: format!("Folder {id}"),
            rules: FolderRules::GROUPS,
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
    cx.run_until_parked();
    assert!(
        read(&root, cx, |s| s.tabs.scroll.max_offset().y) > px(0.),
        "the folders overflow the rail"
    );
    assert_eq!(read(&root, cx, |s| s.tabs.scroll.offset().y), px(0.));

    act(&root, cx, |s, window, cx| {
        s.show_chats(Shown::Tab(Tab::Folder(79)), window, cx);
    });
    assert!(read(&root, cx, |s| s.tabs.scroll.offset().y) < px(0.));
    let selected = cx.debug_bounds("selected-tab").unwrap();
    let viewport = read(&root, cx, |s| s.tabs.scroll.bounds());
    assert!(
        selected.top() >= viewport.top() - px(1.),
        "{selected:?} {viewport:?}"
    );
    assert!(
        selected.bottom() <= viewport.bottom() + px(1.),
        "{selected:?} {viewport:?}"
    );

    act(&root, cx, |s, window, cx| {
        s.show_chats(Shown::Archive, window, cx);
    });
    assert!(cx.debug_bounds("selected-tab").is_none());
    assert!(cx.debug_bounds("unread-toggle").is_some());
}

#[gpui::test]
fn ctrl_tab_walks_the_strip_and_a_restart_restores_the_tab(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sidebar");
    cx.update(|cx| {
        cx.bind_keys(crate::shell::key_bindings());
        cx.set_global(SidebarFile::at(path.clone()));
    });
    let (root, vcx, harness) = open(cx);
    sign_in(&root, vcx, &harness);
    harness
        .events
        .unbounded_send(Event::DialogsReset(FakeScript::demo(3, 0).dialogs))
        .unwrap();
    folders(&harness, vcx);
    assert_eq!(shown(&root, vcx), Shown::Tab(Tab::All));
    for tab in [
        Tab::Private,
        Tab::Groups,
        Tab::Forums,
        Tab::Folder(2),
        Tab::All,
    ] {
        vcx.simulate_keystrokes("ctrl-tab");
        assert_eq!(shown(&root, vcx), Shown::Tab(tab));
    }
    vcx.simulate_keystrokes("ctrl-shift-tab");
    assert_eq!(shown(&root, vcx), Shown::Tab(Tab::Folder(2)), "wraps back");
    act(&root, vcx, |s, window, cx| {
        s.show_chats(Shown::Archive, window, cx);
    });
    vcx.simulate_keystrokes("ctrl-tab");
    assert_eq!(shown(&root, vcx), Shown::Archive, "no tabs in the archive");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "abyss-sidebar 1\ntab=all\nunread=0\n",
        "the archive is never kept: it leads back to All"
    );
    act(&root, vcx, |s, window, cx| {
        s.show_chats(Shown::Tab(Tab::Folder(2)), window, cx);
    });

    // A restart: the folder tab waits for the account's folders.
    let (root, vcx, harness) = open(cx);
    sign_in(&root, vcx, &harness);
    assert_eq!(shown(&root, vcx), Shown::Tab(Tab::All));
    folders(&harness, vcx);
    assert_eq!(shown(&root, vcx), Shown::Tab(Tab::Folder(2)));

    // Logout deletes the file, so the next account starts on All, and
    // Ctrl+Tab on the login screen leaves it deleted.
    core_says(&harness, vcx, AuthState::LoggingOut);
    assert!(!path.exists());
    core_says(&harness, vcx, AuthState::Phone { error: None });
    vcx.simulate_keystrokes("ctrl-tab");
    assert_eq!(shown(&root, vcx), Shown::Tab(Tab::All));
    assert!(!path.exists(), "nothing written while signed out");
}

fn user(id: i64) -> ChatId {
    ChatId {
        kind: PeerKind::User,
        id,
    }
}

/// Chat `id` as core lists it, with `unread` messages.
fn listed(id: i64, unread: u32) -> ChatSummary {
    ChatSummary {
        read: abyss_core::model::ReadState {
            unread,
            ..Default::default()
        },
        ..summary(user(id), &format!("Chat {id}"))
    }
}

fn chat_rows(root: &Entity<Root>, cx: &mut VisualTestContext) -> Vec<i64> {
    read(root, cx, |s| {
        s.shell
            .list
            .rows()
            .iter()
            .filter_map(|r| match r {
                crate::chat::list::SidebarRow::Chat(c) => Some(c.id),
                _ => None,
            })
            .collect()
    })
}

/// Reading a chat removes its row immediately, including from a folder
/// that excludes read chats. The open conversation remains in the pane.
#[gpui::test]
fn reading_through_the_unread_filter_removes_rows_and_keeps_the_pane(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sidebar");
    cx.update(|cx| {
        cx.bind_keys(crate::shell::key_bindings());
        cx.set_global(SidebarFile::at(path.clone()));
    });
    let (root, vcx, harness) = open(cx);
    sign_in(&root, vcx, &harness);
    let says = |vcx: &mut VisualTestContext, event: Event| {
        harness.events.unbounded_send(event).unwrap();
        vcx.run_until_parked();
    };
    let chats = |unread: [u32; 4]| -> Vec<ChatSummary> {
        (1..).zip(unread).map(|(id, n)| listed(id, n)).collect()
    };
    says(vcx, Event::DialogsReset(chats([2, 2, 0, 2])));
    let unread = ChatFolder {
        id: 5,
        title: "Unread".into(),
        rules: FolderRules::NON_CONTACTS.with(FolderRules::EXCLUDE_READ),
        pinned: Vec::new(),
        include: Vec::new(),
        exclude: Vec::new(),
        shared: false,
    };
    says(vcx, Event::ChatFolders(vec![unread]));
    act(&root, vcx, |s, window, cx| {
        s.show_chats(Shown::Tab(Tab::Folder(5)), window, cx);
    });
    vcx.simulate_keystrokes("cmd-shift-u");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "abyss-sidebar 1\ntab=folder:5\nunread=1\n"
    );
    assert_eq!(chat_rows(&root, vcx), [1, 2, 4]);

    // Read rows disappear even while their conversation is open.
    act(&root, vcx, |s, w, cx| s.select_chat(user(1), w, cx));
    says(vcx, Event::ChatBumped(listed(1, 0)));
    assert_eq!(chat_rows(&root, vcx), [2, 4]);
    assert_eq!(
        read(&root, vcx, |s| s.shell.selected()),
        Some(ThreadKey::whole(user(1)))
    );
    act(&root, vcx, |s, w, cx| s.select_chat(user(4), w, cx));
    says(vcx, Event::ChatBumped(listed(4, 0)));
    assert_eq!(chat_rows(&root, vcx), [2]);
    // Read on another device, never opened here: it leaves.
    says(vcx, Event::ChatBumped(listed(2, 0)));
    assert!(chat_rows(&root, vcx).is_empty());

    // A reset and a later read summary cannot bring a read row back.
    says(vcx, Event::DialogsReset(chats([0, 0, 0, 0]).split_off(1)));
    says(vcx, Event::ChatBumped(listed(1, 0)));
    assert!(chat_rows(&root, vcx).is_empty());

    // Switching off and back on never retains read rows.
    vcx.simulate_keystrokes("cmd-shift-u");
    assert!(chat_rows(&root, vcx).is_empty());
    act(&root, vcx, |s, w, cx| {
        s.show_chats(Shown::Tab(Tab::All), w, cx);
    });
    assert_eq!(chat_rows(&root, vcx), [1, 2, 3, 4]);
    act(&root, vcx, |s, w, cx| s.select_chat(user(2), w, cx));
    vcx.simulate_keystrokes("cmd-shift-u");
    assert!(chat_rows(&root, vcx).is_empty());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "abyss-sidebar 1\ntab=all\nunread=1\n"
    );
}
