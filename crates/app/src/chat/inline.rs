//! Inline forum topics (plan section 17.3): the forum the pane shows lists
//! some of its topics under its row in the sidebar, read from the topic
//! list the pane already holds, so nothing new is asked of core. Only that
//! forum expands (decision D7); its row's disclosure glyph or
//! Option+Cmd+Right/Left expand and collapse it without changing the pane.

use std::collections::HashSet;

use abyss_core::model::{ChatId, ChatSummary, Topic, TopicId};

use super::list::{Beneath, ChatList, Opened, SidebarRow, Step, topic_unread};
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
/// Under the unread filter (plan section 17.4) the slots go to the unread
/// and kept topics, and More also shows while the forum's badge counts
/// more unread topics than are shown. Before a copy exists, one pending
/// row.
fn rows(
    topics: Option<&TopicList>,
    chats: &ChatList,
    forum: ChatId,
    open: Option<TopicId>,
) -> Vec<SidebarRow> {
    let Some(copy) = topics.filter(|l| l.chat() == forum && l.is_loaded()) else {
        return vec![SidebarRow::TopicsPending { forum }];
    };
    let filtered = chats.view().unread_only;
    // The open topic counts as kept while it is open.
    let candidates: Vec<&Topic> = copy
        .topics()
        .iter()
        .filter(|t| !filtered || topic_unread(t) || chats.topic_kept(t.id) || open == Some(t.id))
        .collect();
    let shown: Vec<&Topic> = candidates
        .iter()
        .enumerate()
        .filter(|&(ix, t)| ix < INLINE_TOPICS || open == Some(t.id))
        .map(|(_, &t)| t)
        .collect();
    let badge = chats.get(forum).map_or(0, ChatSummary::displayed_unread);
    let unread_shown = shown.iter().filter(|t| topic_unread(t)).count();
    let more = candidates.len() > shown.len()
        || copy.has_more()
        || (filtered && usize::try_from(badge).is_ok_and(|b| b > unread_shown));
    let mut rows: Vec<SidebarRow> = shown
        .iter()
        .map(|t| SidebarRow::Topic { forum, topic: t.id })
        .collect();
    if more {
        rows.push(SidebarRow::MoreTopics { forum });
    }
    rows
}

impl ChatShell {
    /// Brings the sidebar's inline rows in line with the pane, its forum's
    /// topic list, the expansion and the unread filter, and tells the chat
    /// list which chat the pane shows (the filter keeps it).
    pub(super) fn sync_inline(&mut self) {
        let opened = self.pane.chat().map(|chat| Opened {
            chat,
            unread_topics: self
                .forum_copy()
                .is_some_and(|copy| copy.chat() == chat && copy.topics().iter().any(topic_unread)),
        });
        self.list.set_opened(opened);
        let beneath = match (self.pane, self.inline) {
            (Pane::Forum { chat, open }, Inline::Expanded) => Some(Beneath {
                forum: chat,
                rows: rows(self.topics.as_ref(), &self.list, chat, open),
            }),
            _ => None,
        };
        self.list.set_beneath(beneath);
    }

    /// The pane's forum's topic copy, once core sent one.
    fn forum_copy(&self) -> Option<&TopicList> {
        match self.pane {
            Pane::Forum { .. } => self.topics.as_ref().filter(|l| l.is_loaded()),
            Pane::Empty | Pane::Thread(_) => None,
        }
    }

    /// Topic events that can drop topics from the copy (a reset, a
    /// removal, an invalidation) drop them from the filter's kept topics
    /// too.
    pub(super) fn retain_kept_topics(&mut self) {
        let held: HashSet<TopicId> = self
            .forum_copy()
            .map(|copy| copy.topics().iter().map(|t| t.id).collect())
            .unwrap_or_default();
        self.list.retain_kept_topics(|t| held.contains(&t));
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
