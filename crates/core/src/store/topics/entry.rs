//! One retained topic with its field-group stamps (plan section 6.4).

use super::due::Due;
use super::stamp::{FetchSeq, Stamp, Verdict};
use crate::model::{Freshness, MediaKind, Preview, Topic, TopicSnapshot, Visibility};
use crate::sanitize::{MAX_NAME_BYTES, clean, is_clean};

/// A local suspicion that a topic was deleted. Removal is not a state: only a
/// fresh by-id `deleted` removes a topic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Deletion {
    None,
    Suspected,
}

/// Which kind of fetch a snapshot came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Source {
    Page,
    /// A by-id result: finding the topic also settles a deletion suspicion.
    ById,
}

/// What one snapshot did to each group of an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Merged {
    pub(super) meta: Verdict,
    pub(super) unread: Verdict,
    pub(super) activity: Verdict,
}

impl Merged {
    pub(super) fn conflict(self) -> bool {
        [self.meta, self.unread, self.activity].contains(&Verdict::Conflict)
    }
}

#[derive(Clone, Debug)]
pub(super) struct TopicEntry {
    pub(super) topic: Topic,
    pub(super) visibility: Visibility,
    pub(super) deletion: Deletion,
    /// [`crate::model::Message::version`] of the top message.
    pub(super) last_version: i64,
    /// `title`, `closed`, `pinned`, `created`, `visibility`, `deletion`.
    pub(super) meta: Stamp,
    /// `unread`, `unread_mentions`, `read_inbox_max`, `freshness`.
    pub(super) unread: Stamp,
    /// `last`, `last_date`, `last_id`, `last_version`.
    pub(super) activity: Stamp,
    /// The required refetch, if one is owed. It lives on the entry, so due
    /// state costs nothing beyond the list's entries (plan section 6.4).
    pub(super) due: Option<Due>,
}

impl TopicEntry {
    /// A topic new to the list, applied as is.
    pub(super) fn new(snap: TopicSnapshot, seq: FetchSeq) -> Self {
        let TopicSnapshot {
            mut topic,
            visibility,
            top_version,
        } = snap;
        topic.freshness = Freshness::Current;
        bound(&mut topic);
        Self {
            topic,
            visibility,
            deletion: Deletion::None,
            last_version: top_version,
            meta: Stamp::applied(seq),
            unread: Stamp::applied(seq),
            activity: Stamp::applied(seq),
            due: None,
        }
    }

    pub(super) fn id(&self) -> crate::model::TopicId {
        self.topic.id
    }

    pub(super) fn shown(&self) -> bool {
        self.visibility == Visibility::Shown
    }

    /// Applies each group of `snap` (dispatched at `seq`) the merge rule allows.
    pub(super) fn merge(&mut self, snap: TopicSnapshot, seq: FetchSeq, source: Source) -> Merged {
        let merged = Merged {
            meta: self.meta.judge(seq),
            unread: self.unread.judge(seq),
            activity: self.activity.judge(seq),
        };
        let TopicSnapshot {
            topic: new,
            visibility,
            top_version,
        } = snap;
        let topic = &mut self.topic;
        if merged.meta == Verdict::Apply {
            topic.title = new.title;
            topic.closed = new.closed;
            topic.pinned = new.pinned;
            topic.created = new.created;
            self.visibility = visibility;
            if source == Source::ById {
                self.deletion = Deletion::None;
            }
            self.meta.set_seen(seq);
        }
        if merged.unread == Verdict::Apply {
            topic.unread = new.unread;
            topic.unread_mentions = new.unread_mentions;
            topic.read_inbox_max = new.read_inbox_max;
            topic.freshness = Freshness::Current;
            self.unread.set_seen(seq);
        }
        if merged.activity == Verdict::Apply {
            topic.last = new.last;
            topic.last_date = new.last_date;
            topic.last_id = new.last_id;
            self.last_version = top_version;
            self.activity.set_seen(seq);
        }
        bound(&mut self.topic);
        merged
    }

    /// No group waits for a refetch, and no deletion is suspected.
    pub(super) fn settled(&self) -> bool {
        self.deletion == Deletion::None
            && [self.meta, self.unread, self.activity]
                .iter()
                .all(|s| s.settled())
    }

    /// An unread trigger: the count is left alone, only the hint changes.
    pub(super) fn trigger_unread(&mut self, freshness: Freshness, now: FetchSeq) {
        self.topic.freshness = freshness;
        self.unread.set_changed(now);
    }
}

/// Keeps a retained topic's heap bounded: a title or preview the backend
/// should have cleaned is cleaned here, and capacities are trimmed to the
/// length, so a topic retains at most `MAX_NAME_BYTES + MAX_PREVIEW_BYTES`.
pub(super) fn bound(topic: &mut Topic) {
    if !is_clean(&topic.title, MAX_NAME_BYTES) {
        topic.title = clean(&topic.title, MAX_NAME_BYTES);
    }
    topic.title.shrink_to_fit();
    if !topic.last.as_ref().is_none_or(Preview::is_valid) {
        topic.last = Some(Preview::Service);
    }
    if let Some(Preview::Text(s) | Preview::Media(MediaKind::Document { name: s, .. })) =
        &mut topic.last
    {
        s.shrink_to_fit();
    }
}
