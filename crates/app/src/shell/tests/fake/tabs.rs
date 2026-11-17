//! The sidebar's tabs (plan section 17.2) through the real window: Ctrl+Tab
//! and Ctrl+Shift+Tab walk the strip, the choice is kept in the sidebar
//! file, a restart restores it once the folders arrive, and logout deletes
//! the file.

use abyss_core::model::{ChatFolder, FolderRules};

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
