//! Keyboard navigation of the sidebar's rows: a focus handle for each row on
//! screen, so Escape can return to the row that was opened, and a scroll
//! handle for each view, so a selection moved by keyboard is brought into
//! view (plan section 4.3).

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;

use abyss_core::model::{ChatId, TopicId};
use gpui::{App, FocusHandle, ScrollStrategy, UniformListScrollHandle};

use crate::chat::list::{SidebarRow, View};

/// Views a session can show: the built-in tabs, a folder's tab each, and the
/// archive, with the unread filter on or off. Never reached at the folder
/// bound Telegram allows; past it the handles are simply made again.
const MAX_SCROLLS: usize = 128;

/// Which row a focus handle belongs to. A topic is in two lists at once, the
/// sidebar's inline rows and the forum pane's, and one handle can track only
/// one element.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum RowKey {
    Chat(ChatId),
    Inline(TopicId),
    Pane(TopicId),
    Archive,
    More,
}

impl RowKey {
    /// The key of a sidebar row; a placeholder has none.
    pub(super) fn of(row: SidebarRow) -> Option<Self> {
        match row {
            SidebarRow::Chat(id) => Some(Self::Chat(id)),
            SidebarRow::Topic { topic, .. } => Some(Self::Inline(topic)),
            SidebarRow::Archive(_) => Some(Self::Archive),
            SidebarRow::MoreTopics { .. } => Some(Self::More),
            SidebarRow::TopicsPending { .. } => None,
        }
    }
}

/// Rows whose focus handles a list keeps before it lets go of those it no
/// longer shows.
const MAX_HANDLES: usize = 256;

/// Focus handles of the rows a list renders. A handle lives as long as its
/// row keeps being asked for it: a list may render the same row in more than
/// one pass (a uniform list measures its first row apart), and a row that got
/// a new handle each time would lose its focus.
#[derive(Default)]
pub(super) struct Handles(RefCell<HashMap<RowKey, FocusHandle>>);

impl Handles {
    /// The row's handle, a tab stop of its own.
    pub(super) fn get(&self, key: RowKey, cx: &App) -> FocusHandle {
        self.0
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| cx.focus_handle().tab_stop(true))
            .clone()
    }

    /// A list rendered `keys`: once more handles are held than a list shows
    /// rows, those of other rows go.
    pub(super) fn keep(&self, keys: &[RowKey]) {
        let mut handles = self.0.borrow_mut();
        if handles.len() > MAX_HANDLES {
            handles.retain(|key, _| keys.contains(key));
        }
    }
}

#[derive(Default)]
pub(super) struct Nav {
    pub(super) sidebar: Handles,
    pub(super) forum: Handles,
    /// The row Escape asked focus for, until it renders.
    pending: Cell<Option<RowKey>>,
    /// The message list's focus: one tab stop that scrolls by key, and where
    /// focus goes when the button that had it (Jump to latest) goes away.
    history: OnceCell<FocusHandle>,
    pub(super) sidebar_list: OnceCell<FocusHandle>,
    pub(super) forum_list: OnceCell<FocusHandle>,
    scrolls: RefCell<HashMap<View, UniformListScrollHandle>>,
}

impl Nav {
    /// The scroll handle of `view`'s list. Each view keeps its own position.
    pub(super) fn scroll(&self, view: View) -> UniformListScrollHandle {
        let mut scrolls = self.scrolls.borrow_mut();
        if scrolls.len() >= MAX_SCROLLS && !scrolls.contains_key(&view) {
            scrolls.clear();
        }
        scrolls.entry(view).or_default().clone()
    }

    /// Scrolls row `ix` of `view`'s list into view, by the shortest way.
    pub(super) fn reveal(&self, view: View, ix: usize) {
        self.scroll(view)
            .scroll_to_item(ix, ScrollStrategy::Nearest);
    }

    pub(super) fn history(&self, cx: &App) -> FocusHandle {
        self.history
            .get_or_init(|| cx.focus_handle().tab_stop(true))
            .clone()
    }

    /// Asks for focus on `key`'s row the next time it renders.
    pub(super) fn focus_when_rendered(&self, key: RowKey) {
        self.pending.set(Some(key));
    }

    /// Whether focus was asked for `key`'s row, which now renders. Asked once.
    pub(super) fn take_pending(&self, key: RowKey) -> bool {
        let wanted = self.pending.get() == Some(key);
        if wanted {
            self.pending.set(None);
        }
        wanted
    }
}
