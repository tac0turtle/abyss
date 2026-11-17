//! Inline forum topics (plan section 17.3): the forum the pane shows lists
//! some of its topics under its row in the sidebar, read from the topic
//! list the pane already holds, so nothing new is asked of core. Only that
//! forum expands (decision D7); its row's disclosure glyph or
//! Option+Cmd+Right/Left expand and collapse it without changing the pane.

use abyss_core::model::{ChatId, TopicId};

use super::list::{Beneath, SidebarRow, Step};
use super::topics::TopicList;
use super::{ChatShell, Pane};

/// Topics shown under the forum's row, in the list's order; the open topic
/// shows too, wherever it stands.
pub(crate) const INLINE_TOPICS: usize = 8;

/// Whether the pane's forum shows its topics under its row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Inline {
    #[default]
    Expanded,
    Collapsed,
}

impl Inline {
    /// What the disclosure glyph turns it into.
    pub(crate) fn toggled(self) -> Self {
        match self {
            Self::Expanded => Self::Collapsed,
            Self::Collapsed => Self::Expanded,
        }
    }
}

/// The rows under `forum`'s row while it is expanded with `open` the open
/// topic: its first [`INLINE_TOPICS`] topics in display order, the open one
/// wherever it stands, then More when the copy or the server holds more.
/// Before a copy exists, one pending row.
fn rows(list: Option<&TopicList>, forum: ChatId, open: Option<TopicId>) -> Vec<SidebarRow> {
    let Some(list) = list.filter(|l| l.chat() == forum && l.is_loaded()) else {
        return vec![SidebarRow::TopicsPending { forum }];
    };
    let topics = list.topics();
    let mut rows: Vec<SidebarRow> = topics
        .iter()
        .enumerate()
        .filter(|&(ix, t)| ix < INLINE_TOPICS || open == Some(t.id))
        .map(|(_, t)| SidebarRow::Topic { forum, topic: t.id })
        .collect();
    if topics.len() > rows.len() || list.has_more() {
        rows.push(SidebarRow::MoreTopics { forum });
    }
    rows
}

impl ChatShell {
    /// Brings the sidebar's inline rows in line with the pane, its forum's
    /// topic list and the expansion.
    pub(super) fn sync_inline(&mut self) {
        let beneath = match (self.pane, self.inline) {
            (Pane::Forum { chat, open }, Inline::Expanded) => Some(Beneath {
                forum: chat,
                rows: rows(self.topics.as_ref(), chat, open),
            }),
            _ => None,
        };
        self.list.set_beneath(beneath);
    }

    /// The expansion of `chat`'s row: only the pane's forum has one.
    pub(crate) fn inline_of(&self, chat: ChatId) -> Option<Inline> {
        match self.pane {
            Pane::Forum { chat: forum, .. } if forum == chat => Some(self.inline),
            _ => None,
        }
    }

    /// Expands or collapses the pane's forum's row; the pane stays as it
    /// is. Returns whether anything changed.
    pub(crate) fn set_inline(&mut self, inline: Inline) -> bool {
        if !matches!(self.pane, Pane::Forum { .. }) || self.inline == inline {
            return false;
        }
        self.inline = inline;
        self.sync_inline();
        true
    }

    /// The sidebar row carrying the selection mark: the open topic's while
    /// it has one, else the pane's chat's.
    pub(crate) fn marked(&self) -> Option<SidebarRow> {
        match self.pane {
            Pane::Empty => None,
            Pane::Thread(thread) => Some(SidebarRow::Chat(thread.chat)),
            Pane::Forum { chat, open } => {
                let topic = open
                    .map(|topic| SidebarRow::Topic { forum: chat, topic })
                    .filter(|row| self.list.rows().contains(row));
                Some(topic.unwrap_or(SidebarRow::Chat(chat)))
            }
        }
    }

    /// The chat or topic row `step` away from the marked one
    /// (Option+Cmd+Down/Up), skipping the archive's entry, More and
    /// placeholders; with no marked row shown, the first or the last.
    pub(crate) fn row_after(&self, step: Step) -> Option<SidebarRow> {
        let rows = self.list.rows();
        let at = self
            .marked()
            .and_then(|marked| rows.iter().position(|r| *r == marked));
        let found = match (step, at) {
            (Step::Next, Some(at)) => rows.get(at + 1..)?.iter().find(|r| r.selectable()),
            (Step::Next, None) => rows.iter().find(|r| r.selectable()),
            (Step::Previous, Some(at)) => rows.get(..at)?.iter().rev().find(|r| r.selectable()),
            (Step::Previous, None) => rows.iter().rev().find(|r| r.selectable()),
        };
        found.copied()
    }
}
