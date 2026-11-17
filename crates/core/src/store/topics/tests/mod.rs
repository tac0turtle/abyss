//! Tests of the pure topic list (plan section 11, task 2).

mod budget;
mod drain;
mod fixed;
mod merge;
mod order;
mod reducers;
mod review;

use std::collections::BTreeSet;
use std::time::Instant;

use super::entry::TopicEntry;
use super::{
    ByIdSnapshot, FetchSeq, ListChange, MAX_TOPICS_PER_FORUM, PageKind, PageSnapshot, PostClosed,
    TopicList, TopicsGen,
};
use crate::model::{
    Body, ChatId, Freshness, Message, MsgId, PeerKind, Preview, ThreadKey, Topic, TopicGone,
    TopicId, TopicOrder, TopicSnapshot, Visibility,
};
use crate::sanitize::{MAX_NAME_BYTES, is_clean};
use crate::store::Rest;

pub(super) fn tid(n: i32) -> TopicId {
    TopicId::try_from(n).expect("positive")
}

pub(super) fn chat() -> ChatId {
    ChatId {
        kind: PeerKind::Channel,
        id: 77,
    }
}

/// Topic `n`, created at `n`, top message `n * 10`, nothing unread.
pub(super) fn topic(n: i32) -> Topic {
    Topic {
        id: tid(n),
        title: format!("topic {n}"),
        created: i64::from(n),
        closed: false,
        pinned: false,
        unread: 0,
        unread_mentions: 0,
        freshness: Freshness::Current,
        read_inbox_max: MsgId(0),
        last: Some(Preview::Service),
        last_date: i64::from(n),
        last_id: Some(MsgId(n * 10)),
    }
}

pub(super) fn snap(topic: Topic) -> TopicSnapshot {
    TopicSnapshot {
        topic,
        visibility: Visibility::Shown,
        top_version: i64::MIN,
    }
}

pub(super) fn snaps(ns: &[i32]) -> Vec<TopicSnapshot> {
    ns.iter().map(|&n| snap(topic(n))).collect()
}

pub(super) fn message(topic: TopicId, id: i32) -> Message {
    Message {
        id: MsgId(id),
        thread: ThreadKey {
            chat: chat(),
            topic: Some(topic),
        },
        sender: None,
        date: i64::from(id),
        edit_date: None,
        outgoing: false,
        reply_to: None,
        body: Body::Unsupported,
    }
}

pub(super) fn page(order: TopicOrder, topics: Vec<TopicSnapshot>, rest: Rest) -> PageSnapshot {
    PageSnapshot {
        order,
        topics,
        rest,
        post_closed: PostClosed::Refused,
    }
}

pub(super) fn by_id(found: Vec<TopicSnapshot>, deleted: Vec<TopicId>) -> ByIdSnapshot {
    ByIdSnapshot {
        found,
        deleted,
        post_closed: PostClosed::Refused,
    }
}

pub(super) fn list() -> TopicList {
    TopicList::new(TopicsGen(1))
}

/// Dispatches and applies a page of `kind` at once.
pub(super) fn load(
    list: &mut TopicList,
    kind: PageKind,
    p: PageSnapshot,
    now: Instant,
) -> Vec<ListChange> {
    list.request_page(kind);
    let call = list.dispatch_page().expect("a page is dispatched");
    list.apply_page(call.seq, p, now).expect("current")
}

/// A list holding `ns` from one first page in `order`.
pub(super) fn loaded(order: TopicOrder, ns: &[i32], now: Instant) -> TopicList {
    let mut l = list();
    load(
        &mut l,
        PageKind::First,
        page(order, snaps(ns), Rest::More),
        now,
    );
    l
}

/// Dispatches a by-id call and returns its seq and ids.
pub(super) fn dispatch(list: &mut TopicList, now: Instant) -> (FetchSeq, Vec<TopicId>) {
    let call = list
        .dispatch_by_id(now)
        .expect("a by-id call is dispatched");
    (call.seq, call.ids)
}

pub(super) fn ids(list: &TopicList) -> Vec<i32> {
    list.entries.iter().map(|e| e.id().get()).collect()
}

/// The invariants every operation keeps.
pub(super) fn check_invariants(list: &TopicList) -> Result<(), String> {
    let all: Vec<TopicId> = list.entries.iter().map(TopicEntry::id).collect();
    let unique: BTreeSet<TopicId> = all.iter().copied().collect();
    if unique.len() != all.len() {
        return Err(format!("duplicate ids {all:?}"));
    }
    let pinned = list.entries.iter().take_while(|e| e.topic.pinned).count();
    if list.entries.iter().skip(pinned).any(|e| e.topic.pinned) {
        return Err("pinned topics are not a prefix".into());
    }
    if list.len() > MAX_TOPICS_PER_FORUM + 1 {
        return Err(format!("{} topics", list.len()));
    }
    if let Some(t) = list
        .entries
        .iter()
        .find(|e| !is_clean(&e.topic.title, MAX_NAME_BYTES))
    {
        return Err(format!("unclean title in {:?}", t.id()));
    }
    let hidden: BTreeSet<TopicId> = list
        .entries
        .iter()
        .filter(|e| !e.shown())
        .map(TopicEntry::id)
        .collect();
    if list.topics().any(|t| hidden.contains(&t.id)) {
        return Err("a hidden topic is emitted".into());
    }
    if list.due_len() > list.len() {
        return Err("due outgrew the list".into());
    }
    Ok(())
}

/// No change emits a hidden topic.
pub(super) fn check_emitted(list: &TopicList, changes: &[ListChange]) -> Result<(), String> {
    let hidden = |id: TopicId| list.entries.iter().any(|e| e.id() == id && !e.shown());
    for change in changes {
        let topics: Vec<&Topic> = match change {
            ListChange::Reset { topics, .. } | ListChange::Appended(topics) => {
                topics.iter().collect()
            }
            ListChange::Bumped(t) => vec![t],
            ListChange::Removed(..) => Vec::new(),
        };
        if let Some(t) = topics.into_iter().find(|t| hidden(t.id)) {
            return Err(format!("emitted hidden {:?}", t.id));
        }
    }
    Ok(())
}
