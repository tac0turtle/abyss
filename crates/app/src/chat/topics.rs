//! The shown forum's topic list as the UI holds it (plan section 8): core's
//! emitted topics in display order, kept with core's own replay rule
//! ([`ListChange::apply`]), so pinned topics lead and live messages move
//! topics exactly as they move in core. Also paging, its bounded retries,
//! and the flood-wait countdown, which keeps the load it interrupted and
//! sends it once when the wait ends.

use std::time::{Duration, Instant};

use abyss_core::api::{Command, Event, UserError};
use abyss_core::model::{ChatId, Freshness, Topic, TopicId, TopicOrder};
use abyss_core::store::{ListChange, TopicsGen};
use gpui::SharedString;

use super::copy;
use super::list::Pages;
use super::retry::{Backoff, earliest};

/// Rows before the end of the list at which the next page is requested.
const LOAD_MARGIN: usize = 10;

/// A topics load the UI asks core for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Intent {
    /// The list from its first page: `OpenForum` (core shows the retained
    /// copy at once, then fetches the first page).
    First,
    /// The next page: `LoadTopics`.
    Next,
}

impl Intent {
    pub(crate) fn command(self, chat: ChatId) -> Command {
        match self {
            Self::First => Command::OpenForum(chat),
            Self::Next => Command::LoadTopics(chat),
        }
    }
}

/// Telegram asked to wait: no topics load before `until`, and then `intent`
/// once. Not a failure: the retry backoff does not count it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Flood {
    until: Instant,
    intent: Intent,
}

/// How a topic row marks unread messages, in the row's ink (plan section 8):
/// the server's count, adjusted for display only by [`Freshness`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnreadMark {
    None,
    Count {
        unread: u32,
        mentions: u32,
    },
    /// New messages since the server's count, which said none.
    Dot,
}

impl UnreadMark {
    pub(crate) fn of(topic: &Topic) -> Self {
        let count = Self::Count {
            unread: topic.unread,
            mentions: topic.unread_mentions,
        };
        match (topic.freshness, topic.unread) {
            // Just read here: the refetch will confirm it.
            (Freshness::ReadSince, _) | (Freshness::Current | Freshness::Stale, 0) => Self::None,
            (Freshness::NewSince, 0) => Self::Dot,
            _ => count,
        }
    }
}

/// A topic's title, or the fixed fallback when it has none.
pub(crate) fn title(topic: &Topic) -> SharedString {
    if topic.title.is_empty() {
        fallback_title(topic.id)
    } else {
        topic.title.clone().into()
    }
}

/// The title of a topic whose own is unknown or empty.
pub(crate) fn fallback_title(id: TopicId) -> SharedString {
    if id == TopicId::GENERAL {
        copy::GENERAL_TOPIC.into()
    } else {
        copy::UNTITLED_TOPIC.into()
    }
}

/// When a topic last changed: its newest message, else its creation.
pub(crate) fn activity(topic: &Topic) -> i64 {
    if topic.last_date > 0 {
        topic.last_date
    } else {
        topic.created
    }
}

#[derive(Debug)]
pub(crate) struct TopicList {
    chat: ChatId,
    /// The generation of the copy, from core's last `TopicsReset`; `None`
    /// while there is no copy (opening, or dropped by `ForumInvalidated`).
    view: Option<TopicsGen>,
    order: TopicOrder,
    /// Shown topics in display order.
    topics: Vec<Topic>,
    /// The user may post in closed topics.
    can_post_closed: bool,
    has_more: bool,
    /// The load core has not answered yet.
    in_flight: Option<Intent>,
    /// The end of the list is on screen: the next page is wanted.
    wanted: bool,
    /// A load that failed, retried after `retry` allows.
    failed: Option<Intent>,
    retry: Backoff,
    flood: Option<Flood>,
}

impl TopicList {
    /// The list of `chat`, whose `OpenForum` core just admitted (`None`:
    /// shown locally, nothing asked of core).
    pub(crate) fn new(chat: ChatId, opening: Option<Intent>) -> Self {
        Self {
            chat,
            view: None,
            order: TopicOrder::LastActivity,
            topics: Vec::new(),
            can_post_closed: false,
            has_more: false,
            in_flight: opening,
            wanted: false,
            failed: None,
            retry: Backoff::default(),
            flood: None,
        }
    }

    pub(crate) fn chat(&self) -> ChatId {
        self.chat
    }

    pub(crate) fn topics(&self) -> &[Topic] {
        &self.topics
    }

    pub(crate) fn get(&self, id: TopicId) -> Option<&Topic> {
        self.topics.iter().find(|t| t.id == id)
    }

    /// Whether `topic` takes no message from the user: it is closed, and
    /// the user may not post in closed topics (core refuses such a send).
    pub(crate) fn blocks(&self, topic: &Topic) -> bool {
        topic.closed && !self.can_post_closed
    }

    /// Whether core sent a copy to show (possibly empty).
    pub(crate) fn is_loaded(&self) -> bool {
        self.view.is_some()
    }

    /// Applies a topic event of this forum; anything else is ignored, and
    /// so are paging events of an older generation.
    pub(crate) fn apply(&mut self, event: Event) {
        let change = match event {
            Event::TopicsReset {
                chat,
                view,
                order,
                topics,
                can_post_closed,
            } if chat == self.chat => {
                self.view = Some(view);
                self.can_post_closed = can_post_closed;
                ListChange::Reset { order, topics }
            }
            Event::TopicsAppended { chat, view, topics }
                if chat == self.chat && self.view == Some(view) =>
            {
                ListChange::Appended(topics)
            }
            Event::TopicBumped { chat, topic } if chat == self.chat && self.is_loaded() => {
                ListChange::Bumped(topic)
            }
            Event::TopicRemoved {
                chat,
                topic,
                reason,
            } if chat == self.chat => ListChange::Removed(topic, reason),
            Event::TopicsPaging {
                chat,
                view,
                has_more,
            } if chat == self.chat && self.view == Some(view) => {
                self.paged(if has_more { Pages::More } else { Pages::Done });
                return;
            }
            Event::ForumInvalidated(chat) if chat == self.chat => {
                // Core refetches the first page and sends a new copy.
                self.view = None;
                self.topics = Vec::new();
                self.has_more = false;
                return;
            }
            _ => return,
        };
        // Replayed exactly: core bounds the list after a whole operation,
        // not after each of its changes (an append may come before the
        // removal that makes room for it).
        change.apply(&mut self.order, &mut self.topics);
    }

    /// Core applied a page (`TopicsPaging`). A list showing no row cannot
    /// ask for more by scrolling, so it wants the next page at once.
    fn paged(&mut self, pages: Pages) {
        self.has_more = pages == Pages::More;
        self.in_flight = None;
        self.failed = None;
        self.wanted |= self.topics.is_empty();
        self.retry.reset();
    }

    /// The load in flight ended without a page (`LoadEnded`). A load core
    /// started itself (a refetch after invalidation) has none in flight: a
    /// first page. A flood wait keeps it for when the wait ends.
    pub(crate) fn load_ended(&mut self, error: Option<UserError>, now: Instant) {
        let intent = self.in_flight.take().unwrap_or(Intent::First);
        match error {
            None => self.has_more = false,
            Some(UserError::FloodWait { seconds }) => {
                let wait = Duration::from_secs(u64::from(seconds));
                self.flood = now.checked_add(wait).map(|until| Flood { until, intent });
            }
            Some(_) => {
                self.failed = Some(intent);
                self.retry.failed(now);
            }
        }
    }

    /// Row `last_visible` is on screen: near the end, the next page is
    /// wanted.
    pub(crate) fn seen(&mut self, last_visible: usize) {
        let len = self.topics.len();
        if len == 0 || last_visible.saturating_add(LOAD_MARGIN) >= len {
            self.wanted = true;
        }
    }

    /// The load to ask core for at `now`, if any: a flood wait's kept load
    /// once it ended, a failed one once its backoff allows, or the next
    /// page the list wants. One at a time.
    pub(crate) fn wants(&self, now: Instant) -> Option<Intent> {
        if self.in_flight.is_some() {
            return None;
        }
        if let Some(flood) = self.flood {
            return (now >= flood.until).then_some(flood.intent);
        }
        if !self.retry.ready(now) {
            return None;
        }
        self.failed
            .or_else(|| self.wants_next().then_some(Intent::Next))
    }

    fn wants_next(&self) -> bool {
        self.wanted && self.has_more && self.is_loaded()
    }

    /// Core admitted `intent`'s command.
    pub(crate) fn requested(&mut self, intent: Intent) {
        self.in_flight = Some(intent);
        self.flood = None;
        self.failed = None;
        if intent == Intent::Next {
            self.wanted = false;
        }
    }

    /// Core did not take `intent`'s command: ask again after a backoff (an
    /// expired flood wait's load included).
    pub(crate) fn refused(&mut self, intent: Intent, now: Instant) {
        self.flood = None;
        self.failed = Some(intent);
        self.retry.failed(now);
    }

    /// Seconds left of a flood wait, rounded up; `Some(0)` once it ended
    /// and its load waits to be sent.
    pub(crate) fn countdown(&self, now: Instant) -> Option<u64> {
        let left = self.flood?.until.saturating_duration_since(now);
        Some(left.as_secs() + u64::from(left.subsec_nanos() > 0))
    }

    /// Whether a flood-wait countdown shows.
    pub(crate) fn flooding(&self) -> bool {
        self.flood.is_some()
    }

    /// When a load waits for time alone: a flood wait's end, or a retry.
    pub(crate) fn retry_due(&self, now: Instant) -> Option<Instant> {
        let flood = self.flood.map(|f| f.until).filter(|until| *until > now);
        let retry = self.retrying().then(|| self.retry.due(now)).flatten();
        earliest(flood, retry)
    }

    /// Whether a load waits on the retry backoff.
    fn retrying(&self) -> bool {
        self.failed.is_some() || self.wants_next()
    }

    /// Whether automatic retries of a wanted load gave up (Retry is offered).
    pub(crate) fn gave_up(&self) -> bool {
        self.retrying() && self.retry.gave_up()
    }

    pub(crate) fn exhausted(&self) -> bool {
        self.retry.gave_up()
    }

    /// A fresh trigger: a waiting or exhausted retry may go at once.
    pub(crate) fn retry_now(&mut self) {
        self.retry.reset();
    }

    /// Automatic recovery (see [`Backoff::restart_if_exhausted`]).
    pub(crate) fn recover(&mut self) {
        self.retry.restart_if_exhausted();
    }
}

#[cfg(test)]
mod tests {
    use abyss_core::model::MsgId;

    use super::*;

    fn topic(id: i32) -> Topic {
        Topic {
            id: TopicId::try_from(id).unwrap(),
            title: String::new(),
            created: 0,
            closed: false,
            pinned: false,
            unread: 0,
            unread_mentions: 0,
            freshness: Freshness::Current,
            read_inbox_max: MsgId(0),
            last: None,
            last_date: 0,
            last_id: None,
        }
    }

    #[test]
    fn rows_mark_unread_by_freshness_and_fall_back_on_titles() {
        let with = |unread, freshness| Topic {
            unread,
            freshness,
            ..topic(2)
        };
        assert_eq!(
            UnreadMark::of(&with(0, Freshness::Current)),
            UnreadMark::None
        );
        assert_eq!(
            UnreadMark::of(&with(0, Freshness::NewSince)),
            UnreadMark::Dot
        );
        assert_eq!(
            UnreadMark::of(&with(3, Freshness::ReadSince)),
            UnreadMark::None
        );
        for f in [Freshness::Current, Freshness::Stale, Freshness::NewSince] {
            assert_eq!(
                UnreadMark::of(&with(3, f)),
                UnreadMark::Count {
                    unread: 3,
                    mentions: 0
                }
            );
        }
        assert_eq!(title(&topic(1)), copy::GENERAL_TOPIC);
        assert_eq!(title(&topic(2)), copy::UNTITLED_TOPIC);
        let named = Topic {
            title: "Bugs".into(),
            ..topic(2)
        };
        assert_eq!(title(&named), "Bugs");
    }
}
