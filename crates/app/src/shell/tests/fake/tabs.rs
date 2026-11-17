//! The sidebar's tabs (plan section 17.2) through the real window: Ctrl+Tab
//! and Ctrl+Shift+Tab walk the strip, the choice is kept in the sidebar
//! file, a restart restores it once the folders arrive, and logout deletes
//! the file. The unread filter (section 17.4) keeps the chats read through
//! it.

use abyss_core::model::{ChatFolder, ChatSummary, FolderRules};

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
        unread,
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

/// Plan section 17.4: under the unread filter (Cmd+Shift+U, kept in the
/// sidebar file), chats opened from the view stay when they are read, also
/// in a folder that excludes read chats, until the filter is turned off;
/// a chat read elsewhere that isn't kept leaves at once, and a reload of
/// the list keeps only kept chats it still lists.
#[gpui::test]
fn reading_through_the_unread_filter_keeps_the_list_still(cx: &mut TestAppContext) {
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

    // Chat 1 is opened and read, then chat 4: chat 1 stays, though the
    // folder excludes read chats.
    act(&root, vcx, |s, w, cx| s.select_chat(user(1), w, cx));
    says(vcx, Event::ChatBumped(listed(1, 0)));
    act(&root, vcx, |s, w, cx| s.select_chat(user(4), w, cx));
    says(vcx, Event::ChatBumped(listed(4, 0)));
    assert_eq!(chat_rows(&root, vcx), [1, 2, 4]);
    // Read on another device, never opened here: it leaves.
    says(vcx, Event::ChatBumped(listed(2, 0)));
    assert_eq!(chat_rows(&root, vcx), [1, 4]);

    // A reload that no longer lists chat 1 forgets it: listed again, read,
    // it doesn't come back.
    says(vcx, Event::DialogsReset(chats([0, 0, 0, 0]).split_off(1)));
    says(vcx, Event::ChatBumped(listed(1, 0)));
    assert_eq!(chat_rows(&root, vcx), [4], "only the open chat");

    // Off: the folder's own rule again, nothing waived; and back on,
    // nothing is kept but the open chat.
    vcx.simulate_keystrokes("cmd-shift-u");
    assert!(chat_rows(&root, vcx).is_empty());
    act(&root, vcx, |s, w, cx| {
        s.show_chats(Shown::Tab(Tab::All), w, cx);
    });
    assert_eq!(chat_rows(&root, vcx), [1, 2, 3, 4]);
    act(&root, vcx, |s, w, cx| s.select_chat(user(2), w, cx));
    vcx.simulate_keystrokes("cmd-shift-u");
    assert_eq!(
        chat_rows(&root, vcx),
        [2],
        "the open chat; chat 4 is no longer kept"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "abyss-sidebar 1\ntab=all\nunread=1\n"
    );
}
