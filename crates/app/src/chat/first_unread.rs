//! First-unread positioning (plan section 17.5): the read position captured
//! when a thread opens, the read gate that holds read marks until the
//! thread's view is positioned and painted, and the divider that marks the
//! read position (decision D10). The chat shell owns one [`OpenRead`] per
//! open thread and drops it with the pane.

use abyss_core::api::Command;
use abyss_core::model::{ChatSummary, MsgId, ThreadKey, Topic};
use abyss_core::store::ViewGen;

use super::list::topic_unread;

/// Whether read marks may go for the open thread's current view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ReadGate {
    /// No history of this view yet: the thread just opened, or core
    /// invalidated its window.
    #[default]
    AwaitingHistory,
    /// The view's first history arrived and was positioned; its first
    /// paint is still to come.
    AwaitingPaint { view: ViewGen },
    /// Read marks may go (Stage 1's rule).
    Ready,
}

impl ReadGate {
    /// The after-paint check of a frame that painted the rows of the
    /// window's `shown` view. Returns whether read marks may go: only once
    /// the view the gate waits for is the one painted.
    pub(crate) fn painted(&mut self, shown: Option<ViewGen>) -> bool {
        if let Self::AwaitingPaint { view } = *self
            && shown == Some(view)
        {
            *self = Self::Ready;
        }
        *self == Self::Ready
    }
}

/// The read position the divider marks, fixed while the pane shows the
/// thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Divider {
    /// Nothing was captured at open: a late divider may still appear,
    /// once.
    #[default]
    Unset,
    /// Right after `after`, before the first message above it.
    At(MsgId),
    /// Removed by a send: none for the rest of this open.
    Gone,
}

/// How the open thread's first view comes, and whether it is to be
/// positioned at the divider.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Opening {
    /// `OpenThreadAt` was admitted and core has not answered: a
    /// `LoadEnded` of its load falls back to `OpenThread`.
    Anchored,
    /// The anchored open failed: `OpenThread` is due (the frame commands
    /// send it).
    FallbackDue,
    /// The first view is still to come; it is positioned at the divider
    /// when it holds it.
    #[default]
    Unpositioned,
    /// The first view arrived; later views keep the reading position
    /// (task 10e).
    Positioned,
}

/// The first-unread state of the open thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OpenRead {
    pub(crate) gate: ReadGate,
    pub(crate) divider: Divider,
    pub(crate) opening: Opening,
    /// The first message above the read position, as an `Around` page
    /// proved it by the server's split around the anchor, even with nothing
    /// at or below the anchor loaded.
    pub(crate) first_new: Option<MsgId>,
}

impl OpenRead {
    /// A thread opened with the read position `after`, if it has unread
    /// messages.
    pub(crate) fn opened(after: Option<MsgId>) -> Self {
        match after {
            Some(after) => Self {
                divider: Divider::At(after),
                opening: Opening::Anchored,
                ..Self::default()
            },
            None => Self::default(),
        }
    }

    /// The read position the divider marks, while it shows.
    pub(crate) fn after(self) -> Option<MsgId> {
        match self.divider {
            Divider::At(after) => Some(after),
            Divider::Unset | Divider::Gone => None,
        }
    }
}

/// How `thread` opens from read position `after` (none: nothing unread).
pub(crate) fn open_command(thread: ThreadKey, after: Option<MsgId>) -> Command {
    match after {
        Some(after) => Command::OpenThreadAt { thread, after },
        None => Command::OpenThread(thread),
    }
}

/// The read position to open a chat that isn't a forum at: its confirmed
/// pointer, when the displayed count is above 0.
pub(crate) fn of_chat(chat: &ChatSummary) -> Option<MsgId> {
    (chat.displayed_unread() > 0).then_some(chat.read.read_inbox_max)
}

/// The read position to open a topic at: its pointer, when its row shows
/// an unread mark.
pub(crate) fn of_topic(topic: &Topic) -> Option<MsgId> {
    topic_unread(topic).then_some(topic.read_inbox_max)
}
