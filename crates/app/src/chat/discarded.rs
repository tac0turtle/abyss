//! Drafts and pending sends discarded because their thread can no longer be
//! sent to (a removed topic, a chat whose forum flag changed), kept for one
//! notice that can copy them all. Bounded: at most [`MAX_DISCARDED`] texts, the oldest
//! dropped first; that and Dismiss are the only ways one is lost.

use std::collections::VecDeque;

use abyss_core::api::Event;
use abyss_core::model::{ChatId, ThreadKey, TopicGone};
use abyss_core::store::ForumStatus;
use gpui::SharedString;

use super::{Pane, Stale};

/// Discarded texts the notice keeps (the draft bound).
pub(crate) const MAX_DISCARDED: usize = super::drafts::MAX_DRAFTS;

/// Threads that can no longer be sent to, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Obsolete {
    /// A topic core deleted (`TopicRemoved` with `Deleted`).
    Topic(ThreadKey),
    /// The open topic was hidden (it can still be posted to), and no
    /// draft slot was free for the box's text.
    HiddenNoRoom(ThreadKey),
    /// Every topic of a chat that stopped being a forum.
    Topics(ChatId),
    /// The whole-chat thread of a chat that became a forum.
    Whole(ChatId),
}

impl Obsolete {
    /// What a core event makes unsendable, whatever the pane shows.
    pub(crate) fn of(event: &Event) -> Option<Self> {
        match *event {
            // Hidden (General) can still be posted to, and an evicted
            // topic still exists: their drafts stay.
            Event::TopicRemoved {
                chat,
                topic,
                reason: TopicGone::Deleted,
            } => Some(Self::Topic(ThreadKey {
                chat,
                topic: Some(topic),
            })),
            Event::KindChanged {
                chat,
                forum: ForumStatus::NotForum,
            } => Some(Self::Topics(chat)),
            Event::KindChanged {
                chat,
                forum: ForumStatus::Forum,
            } => Some(Self::Whole(chat)),
            _ => None,
        }
    }

    /// What the change that made `pane` stale makes unsendable (a hidden
    /// topic: nothing).
    pub(crate) fn of_stale(stale: Stale, pane: Pane) -> Option<Self> {
        let chat = pane.chat()?;
        match stale {
            Stale::TopicRemoved(TopicGone::Deleted) => Some(Self::Topic(pane.thread()?)),
            Stale::TopicRemoved(TopicGone::Hidden | TopicGone::Evicted) => None,
            Stale::Kind { to: Some(_) } => Some(Self::Topics(chat)),
            Stale::Kind { to: None } => Some(Self::Whole(chat)),
        }
    }

    pub(crate) fn covers(self, key: ThreadKey) -> bool {
        match self {
            Self::Topic(thread) | Self::HiddenNoRoom(thread) => key == thread,
            Self::Topics(chat) => key.chat == chat && key.topic.is_some(),
            Self::Whole(chat) => key == ThreadKey::whole(chat),
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Self::Topic(_) => "the topic was deleted",
            Self::HiddenNoRoom(_) => "the topic was hidden and every draft slot is taken",
            Self::Topics(_) => "the chat is no longer a forum",
            Self::Whole(_) => "the chat became a forum",
        }
    }
}

/// What a batch discarded, for the wording of its line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lost {
    /// Composer text: stashed drafts, or the box's.
    Drafts,
    /// Sends core no longer holds (refused, failed and then discarded,
    /// or left when core stopped), whose echoes nobody can see any more.
    Sends,
    /// Such sends whose outcome was unknown: they may have been delivered.
    UnconfirmedSends,
}

impl Lost {
    fn one(self) -> &'static str {
        match self {
            Self::Drafts => "draft",
            Self::Sends | Self::UnconfirmedSends => "pending message",
        }
    }

    fn many(self) -> &'static str {
        match self {
            Self::Drafts => "drafts",
            Self::Sends | Self::UnconfirmedSends => "pending messages",
        }
    }

    /// What the user should know before sending such a text again.
    fn caveat(self, count: usize) -> &'static str {
        match (self, count) {
            (Self::Drafts | Self::Sends, _) => "",
            (Self::UnconfirmedSends, 1) => " It may have been sent: check before sending it again.",
            (Self::UnconfirmedSends, _) => {
                " They may have been sent: check before sending them again."
            }
        }
    }
}

/// The pending notice: discarded texts, each under the label of where it
/// was written, and one line per batch.
#[derive(Default)]
pub(crate) struct Discarded {
    /// (label, text), oldest first.
    texts: VecDeque<(SharedString, String)>,
    /// One per batch, oldest first.
    lines: VecDeque<SharedString>,
}

/// No draft text reaches `Debug`.
impl std::fmt::Debug for Discarded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Discarded")
            .field("texts", &self.texts.len())
            .finish_non_exhaustive()
    }
}

impl Discarded {
    /// Adds a batch of `lost` texts (label, text), all made unsendable by
    /// `why`, to the notice; past [`MAX_DISCARDED`] the oldest go.
    pub(crate) fn add(&mut self, why: Obsolete, what: Lost, drafts: Vec<(SharedString, String)>) {
        let line = match drafts.as_slice() {
            [] => return,
            [(label, _)] => format!(
                "Your {} in {label} was discarded because {}.{}",
                what.one(),
                why.reason(),
                what.caveat(1)
            ),
            many => format!(
                "{} {} were discarded because {}.{}",
                many.len(),
                what.many(),
                why.reason(),
                what.caveat(many.len())
            ),
        };
        self.lines.push_back(line.into());
        self.texts.extend(drafts);
        while self.texts.len() > MAX_DISCARDED {
            self.texts.pop_front();
        }
        while self.lines.len() > MAX_DISCARDED {
            self.lines.pop_front();
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.texts.is_empty()
    }

    /// The batches' lines, newest first.
    pub(crate) fn lines(&self) -> impl Iterator<Item = &SharedString> {
        self.lines.iter().rev()
    }

    /// Every text under its label, newest first, a blank line between.
    pub(crate) fn copy_text(&self) -> String {
        let entries: Vec<String> = self
            .texts
            .iter()
            .rev()
            .map(|(label, text)| format!("{label}\n{text}"))
            .collect();
        entries.join("\n\n")
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }
}
