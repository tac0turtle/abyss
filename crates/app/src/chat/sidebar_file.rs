//! The sidebar's view, kept across restarts (plan section 17.2,
//! "Persistence") in a small file next to the theme's (`ui::small_file`).
//!
//! It holds the selected tab and whether the unread filter is on: no title
//! and nothing secret, and never the archive ([`ChatList::kept_view`]). It
//! is written when the user changes the view, never during render, and
//! deleted at logout so the next account starts on All. Anything
//! unexpected in it means the defaults: All, with the filter off.
//!
//! [`ChatList::kept_view`]: super::list::ChatList::kept_view

use std::io;
use std::path::{Path, PathBuf};

use gpui::{App, Global, Task};

use super::list::{Shown, Tab, View};
use crate::ui::small_file;

const HEADER: &str = "abyss-sidebar 1";
const FILE: &str = "sidebar";

/// The file and its writes. Installed by [`SidebarFile::init`] in the real
/// app only, so tests touch no file unless they install one.
#[derive(Default)]
pub(crate) struct SidebarFile {
    path: Option<PathBuf>,
    /// The last write or removal, which the next one waits for, so the
    /// last change made is the one on disk.
    pending: Option<Task<()>>,
}

impl Global for SidebarFile {}

impl SidebarFile {
    /// The file at `path`.
    pub(crate) fn at(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            pending: None,
        }
    }

    /// Installs the file in Application Support. Call once at startup.
    pub(crate) fn init(cx: &mut App) {
        cx.set_global(small_file::path(FILE).map_or_else(Self::default, Self::at));
    }

    /// The kept view, or the defaults.
    pub(crate) fn load(cx: &App) -> View {
        cx.try_global::<Self>()
            .and_then(|file| file.path.as_deref())
            .map_or_else(View::default, load)
    }

    /// Writes `view` in the background.
    pub(crate) fn save(view: View, cx: &mut App) {
        let text = format(view);
        Self::then(cx, move |path| small_file::write(path, &text), "save");
    }

    /// Deletes the file in the background, best effort.
    pub(crate) fn remove(cx: &mut App) {
        Self::then(cx, small_file::remove, "delete");
    }

    /// Runs `op` on the file after the write or removal before it.
    fn then(
        cx: &mut App,
        op: impl FnOnce(&Path) -> io::Result<()> + Send + 'static,
        what: &'static str,
    ) {
        if !cx.has_global::<Self>() {
            return;
        }
        let executor = cx.background_executor().clone();
        let file = cx.global_mut::<Self>();
        let Some(path) = file.path.clone() else {
            return;
        };
        let before = file.pending.take();
        file.pending = Some(executor.spawn(async move {
            if let Some(before) = before {
                before.await;
            }
            if let Err(error) = op(&path) {
                // The path holds the user's name; the kind is enough.
                tracing::warn!(kind = ?error.kind(), what, "couldn't change the sidebar file");
            }
        }));
    }
}

fn load(path: &Path) -> View {
    match small_file::read(path) {
        Ok(text) => parse(&text).unwrap_or_else(|| {
            tracing::warn!("the sidebar file is not valid; using the defaults");
            View::default()
        }),
        Err(error) => {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!(kind = ?error.kind(), "couldn't read the sidebar file");
            }
            View::default()
        }
    }
}

fn format(view: View) -> String {
    let tab = match view.shown {
        Shown::Tab(Tab::Folder(id)) => format!("folder:{id}"),
        Shown::Tab(Tab::Private) => "private".to_owned(),
        Shown::Tab(Tab::Groups) => "groups".to_owned(),
        Shown::Tab(Tab::Forums) => "forums".to_owned(),
        // Never stored ([`ChatList::kept_view`]); All if it ever were.
        Shown::Tab(Tab::All) | Shown::Archive => "all".to_owned(),
    };
    let unread = u8::from(view.unread_only);
    format!("{HEADER}\ntab={tab}\nunread={unread}\n")
}

/// Exactly the three lines [`format`] writes, or nothing.
fn parse(text: &str) -> Option<View> {
    let mut lines = text.lines();
    if lines.next()? != HEADER {
        return None;
    }
    let tab = parse_tab(lines.next()?.strip_prefix("tab=")?)?;
    let unread_only = match lines.next()?.strip_prefix("unread=")? {
        "0" => false,
        "1" => true,
        _ => return None,
    };
    lines.next().is_none().then_some(View {
        shown: Shown::Tab(tab),
        unread_only,
    })
}

fn parse_tab(word: &str) -> Option<Tab> {
    Some(match word {
        "all" => Tab::All,
        "private" => Tab::Private,
        "groups" => Tab::Groups,
        "forums" => Tab::Forums,
        other => {
            let digits = other.strip_prefix("folder:")?;
            let id: i32 = digits.parse().ok()?;
            // Only the spelling `format` writes: no sign, no leading zeros.
            if id.to_string() != digits {
                return None;
            }
            Tab::Folder(id)
        }
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn view(tab: Tab) -> View {
        View {
            shown: Shown::Tab(tab),
            unread_only: false,
        }
    }

    fn unread(tab: Tab) -> View {
        View {
            unread_only: true,
            ..view(tab)
        }
    }

    #[test]
    fn every_view_survives_the_file_format() {
        let folders = [Tab::Folder(2), Tab::Folder(255), Tab::Folder(-7)];
        for tab in Tab::BUILT_IN.into_iter().chain(folders) {
            for kept in [view(tab), unread(tab)] {
                assert_eq!(parse(&format(kept)), Some(kept));
            }
        }
        assert_eq!(
            format(unread(Tab::Folder(4))),
            "abyss-sidebar 1\ntab=folder:4\nunread=1\n"
        );
    }

    #[test]
    fn anything_unexpected_is_rejected() {
        for bad in [
            "",
            "abyss-sidebar 2\ntab=all\nunread=0\n",
            "abyss-sidebar 1\ntab=archive\nunread=0\n",
            "abyss-sidebar 1\ntab=All\nunread=0\n",
            "abyss-sidebar 1\ntab=folder:\nunread=0\n",
            "abyss-sidebar 1\ntab=folder:+4\nunread=0\n",
            "abyss-sidebar 1\ntab=folder:04\nunread=0\n",
            "abyss-sidebar 1\ntab=folder:99999999999\nunread=0\n",
            "abyss-sidebar 1\ntab=all\nunread=2\n",
            "abyss-sidebar 1\nunread=0\ntab=all\n",
            "abyss-sidebar 1\ntab=all\n",
            "abyss-sidebar 1\ntab=all\nunread=0\nextra\n",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    #[gpui::test]
    fn saves_privately_loads_back_and_logout_deletes(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(FILE);
        cx.update(|cx| {
            cx.set_global(SidebarFile::at(path.clone()));
            SidebarFile::save(view(Tab::Groups), cx);
            SidebarFile::save(unread(Tab::Folder(3)), cx);
        });
        cx.run_until_parked();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(
            cx.update(|cx| SidebarFile::load(cx)),
            unread(Tab::Folder(3)),
            "the last change made is the one kept"
        );
        // A damaged or oversized file is only the defaults.
        fs::write(&path, "x".repeat(4096)).unwrap();
        assert_eq!(cx.update(|cx| SidebarFile::load(cx)), View::default());
        cx.update(SidebarFile::remove);
        cx.run_until_parked();
        assert!(!path.exists());
        assert_eq!(cx.update(|cx| SidebarFile::load(cx)), View::default());
    }
}
