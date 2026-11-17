//! The fake's forum behaviour beyond the contract suite: both orders, hidden
//! General, the server controls and the updates they emit (or lose), the
//! thread-aware sends and reads, and faults on the forum calls.

use std::sync::Arc;
use std::time::Duration;

use super::super::{
    FakeBackend, FakeClient, FakeHandle, FakeScript, FakeUpdates, Fault, Op, summary,
};
use crate::backend::{
    Backend, BackendError, Client, HistoryQuery, Instance, RejectReason, Seed, SessionSeed,
    StreamItem, Update, UpdateStream,
};
use crate::model::{
    Body, ChatId, ChatKind, MsgId, PeerKind, ThreadKey, TopicId, TopicOrder, TopicSnapshot,
    Visibility,
};
use crate::store::{MAX_PEER_DIALOGS, MAX_TOPICS_BY_ID, PostClosed, TOPICS_PAGE};

const NOW: i64 = 1_794_837_900;
const FORUM: ChatId = ChatId {
    kind: PeerKind::Channel,
    id: 500,
};
const PLAIN: ChatId = ChatId {
    kind: PeerKind::User,
    id: 1,
};

fn topic(n: i32) -> TopicId {
    TopicId::try_from(n).unwrap()
}

fn thread(n: i32) -> ThreadKey {
    ThreadKey {
        chat: FORUM,
        topic: Some(topic(n)),
    }
}

fn script(order: TopicOrder) -> FakeScript {
    let mut script = FakeScript::demo(1, 5);
    let mut row = summary(FORUM, "Forum");
    row.kind = ChatKind::Supergroup {
        forum: true,
        gigagroup: false,
    };
    script.dialogs.push(row);
    script.add_demo_forum(FORUM, NOW, order);
    script
}

struct Fixture {
    client: Arc<FakeClient>,
    updates: FakeUpdates,
    fake: FakeHandle,
}

async fn start(script: FakeScript) -> Fixture {
    let (backend, fake) = FakeBackend::new(script);
    let Instance {
        client,
        mut updates,
        ..
    } = backend
        .build(Seed {
            session: SessionSeed::Stored,
        })
        .await
        .unwrap();
    assert!(matches!(updates.next().await, Ok(StreamItem::Baseline)));
    Fixture {
        client: Arc::new(client),
        updates,
        fake,
    }
}

impl Fixture {
    async fn update(&mut self) -> Update {
        match tokio::time::timeout(Duration::from_secs(1), self.updates.next()).await {
            Ok(Ok(StreamItem::Update(u))) => u,
            other => panic!("expected an update, got {other:?}"),
        }
    }

    async fn all_topics(&self) -> (Vec<TopicOrder>, Vec<TopicSnapshot>) {
        let mut orders = Vec::new();
        let mut all = Vec::new();
        let mut cursor = None;
        loop {
            let page = self
                .client
                .topics(FORUM, cursor, TOPICS_PAGE)
                .await
                .unwrap();
            orders.push(page.order);
            all.extend(page.topics);
            match page.next {
                Some(next) => cursor = Some(next),
                None => return (orders, all),
            }
        }
    }

    async fn by_id(&self, n: i32) -> Option<TopicSnapshot> {
        let got = self.client.topics_by_id(FORUM, &[topic(n)]).await.unwrap();
        got.found.into_iter().next()
    }
}

/// Pinned topics first in stored order, then `key` descending.
fn assert_ordered(topics: &[TopicSnapshot], key: impl Fn(&TopicSnapshot) -> i64) {
    let pinned: Vec<i32> = topics
        .iter()
        .take_while(|t| t.topic.pinned)
        .map(|t| t.topic.id.get())
        .collect();
    assert_eq!(pinned, vec![8, 24], "topics 7 and 23 are pinned");
    let rest = &topics[2..];
    assert!(rest.iter().all(|t| !t.topic.pinned));
    assert!(rest.windows(2).all(|w| key(&w[0]) >= key(&w[1])));
}

#[tokio::test(start_paused = true)]
async fn pages_list_every_topic_once_in_both_orders() {
    for order in [TopicOrder::LastActivity, TopicOrder::Created] {
        let f = start(script(order)).await;
        let (orders, all) = f.all_topics().await;
        assert_eq!(orders, vec![order, order], "61 topics are two pages");
        assert_eq!(all.len(), 61);
        let mut ids: Vec<i32> = all.iter().map(|t| t.topic.id.get()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids, (1..=61).collect::<Vec<_>>());
        match order {
            TopicOrder::LastActivity => {
                assert_ordered(&all, |t| i64::from(t.topic.last_id.unwrap().0));
            }
            TopicOrder::Created => assert_ordered(&all, |t| t.topic.created),
        }
        let closed: Vec<i32> = all
            .iter()
            .filter(|t| t.topic.closed)
            .map(|t| t.topic.id.get())
            .collect();
        assert_eq!(closed, vec![14]);
        assert!(all.iter().any(|t| t.topic.unread > 0));
        assert!(all.iter().all(|t| t.topic.is_valid()));
    }
}

#[tokio::test(start_paused = true)]
async fn activity_order_differs_from_creation_order() {
    let f = start(script(TopicOrder::LastActivity)).await;
    let (_, by_activity) = f.all_topics().await;
    f.fake.set_topic_order(FORUM, TopicOrder::Created);
    let (_, by_creation) = f.all_topics().await;
    let ids = |v: &[TopicSnapshot]| v.iter().map(|t| t.topic.id).collect::<Vec<_>>();
    assert_ne!(ids(&by_activity), ids(&by_creation));
}

/// A later page reports the order the server uses now, whatever the cursor
/// was built for (core then discards it, plan section 6.1).
#[tokio::test(start_paused = true)]
async fn an_order_change_between_pages_shows_in_the_next_page() {
    let f = start(script(TopicOrder::LastActivity)).await;
    let first = f.client.topics(FORUM, None, 20).await.unwrap();
    let next = first.next.unwrap();
    assert_eq!(next.order(), TopicOrder::LastActivity);
    f.fake.set_topic_order(FORUM, TopicOrder::Created);
    let second = f.client.topics(FORUM, Some(next), 20).await.unwrap();
    assert_eq!(second.order, TopicOrder::Created);
    assert_eq!(second.next.unwrap().order(), TopicOrder::Created);
}

#[tokio::test(start_paused = true)]
async fn hidden_general_is_listed_with_its_visibility() {
    let mut s = script(TopicOrder::LastActivity);
    for t in s.topics.get_mut(&FORUM).unwrap() {
        if t.topic.id == TopicId::GENERAL {
            t.visibility = Visibility::Hidden;
        }
    }
    let mut f = start(s).await;
    let general = f.by_id(1).await.unwrap();
    assert_eq!(general.visibility, Visibility::Hidden);
    let (_, all) = f.all_topics().await;
    assert!(
        all.iter()
            .any(|t| t.topic.id == TopicId::GENERAL && t.visibility == Visibility::Hidden)
    );
    f.fake
        .set_general_visibility(FORUM, Visibility::Shown)
        .await
        .unwrap();
    assert!(matches!(
        f.update().await,
        Update::TopicEdited { topic: t, visibility: Some(Visibility::Shown), title: None, closed: None, .. }
            if t == TopicId::GENERAL
    ));
    assert_eq!(f.by_id(1).await.unwrap().visibility, Visibility::Shown);
}

/// A dropped update still changes the server: only the next snapshot shows it.
#[tokio::test(start_paused = true)]
async fn a_dropped_update_still_changes_the_server() {
    let mut f = start(script(TopicOrder::LastActivity)).await;
    f.fake.drop_next_update();
    f.fake
        .set_general_visibility(FORUM, Visibility::Hidden)
        .await
        .unwrap();
    f.fake.close_topic(FORUM, topic(2)).await.unwrap();
    assert!(
        matches!(f.update().await, Update::TopicEdited { topic: t, closed: Some(true), .. } if t == topic(2)),
        "only the hide was lost"
    );
    assert_eq!(f.by_id(1).await.unwrap().visibility, Visibility::Hidden);
    assert!(f.by_id(2).await.unwrap().topic.closed);
}

#[tokio::test(start_paused = true)]
async fn history_is_filtered_by_normalized_thread() {
    let f = start(script(TopicOrder::LastActivity)).await;
    let page = |key| f.client.history(key, HistoryQuery::Tail, 500);
    let general = page(thread(1)).await.unwrap().messages;
    assert!(!general.is_empty());
    assert!(
        general.iter().all(|m| m.thread.topic.is_none()),
        "General's messages carry no topic"
    );
    let five = page(thread(5)).await.unwrap().messages;
    assert!(five.iter().all(|m| m.thread.topic == Some(topic(5))));
    assert_eq!(
        five.first().map(|m| m.id),
        Some(MsgId(5)),
        "starts with its creation message"
    );
    assert!(matches!(five[0].body, Body::Service(_)));
    let whole = page(ThreadKey::whole(FORUM)).await.unwrap().messages;
    assert_eq!(whole.len(), 61 + 240, "a whole-chat key is the whole forum");
    let not_forum = ThreadKey {
        chat: PLAIN,
        topic: Some(topic(5)),
    };
    assert!(page(not_forum).await.unwrap().messages.is_empty());
    let older = f
        .client
        .history(thread(5), HistoryQuery::Before(five[1].id), 10)
        .await
        .unwrap();
    assert_eq!(older.messages.len(), 1);
    assert!(!older.has_more);
}

#[tokio::test(start_paused = true)]
async fn sends_follow_the_topic_and_its_closed_flag() {
    let f = start(script(TopicOrder::LastActivity)).await;
    let id = f.client.send_text(thread(5), "hi".into(), 1).await.unwrap();
    let general = f.client.send_text(thread(1), "hi".into(), 2).await.unwrap();
    let history = f.fake.server_history(FORUM);
    let stated = |id: MsgId| history.iter().find(|m| m.id == id).unwrap().thread.topic;
    assert_eq!(stated(id), Some(topic(5)));
    assert_eq!(stated(general), None, "General's posts carry no topic");
    assert_eq!(f.by_id(5).await.unwrap().topic.last_id, Some(id));

    // Topic 13 (id 14) is closed.
    assert_eq!(
        f.client.send_text(thread(14), "x".into(), 3).await,
        Err(BackendError::Rejected(RejectReason::TopicClosed))
    );
    f.fake.set_post_closed(FORUM, PostClosed::Allowed);
    assert!(f.client.send_text(thread(14), "x".into(), 4).await.is_ok());
    assert_eq!(
        f.client.send_text(thread(999), "x".into(), 5).await,
        Err(BackendError::Rejected(RejectReason::TopicDeleted))
    );
    let sent: Vec<ThreadKey> = f.fake.sent().iter().map(|(t, _)| *t).collect();
    assert_eq!(sent, vec![thread(5), thread(1), thread(14)]);
    assert_eq!(f.fake.send_attempts().len(), 5);
}

#[tokio::test(start_paused = true)]
async fn a_topic_read_lowers_its_count_and_the_forum_badge() {
    let f = start(script(TopicOrder::LastActivity)).await;
    // Every third topic has unread posts: General, 3, 6, ... (ids 1, 4, 7, ...).
    let before = f.by_id(4).await.unwrap().topic;
    assert!(before.unread > 0);
    let badge = |peers: Vec<crate::model::ChatSummary>| peers[0].unread;
    let unread_topics = badge(f.client.peer_dialogs(&[FORUM]).await.unwrap());
    assert_eq!(unread_topics, 21);
    f.client
        .read_history(thread(4), before.last_id.unwrap())
        .await
        .unwrap();
    assert_eq!(f.by_id(4).await.unwrap().topic.unread, 0);
    assert_eq!(badge(f.client.peer_dialogs(&[FORUM]).await.unwrap()), 20);
    assert_eq!(
        f.fake.read_calls(),
        vec![(thread(4), before.last_id.unwrap())]
    );
    let page = f.client.dialogs(None, 10).await.unwrap();
    let row = page.chats.iter().find(|c| c.id == FORUM).unwrap();
    assert_eq!(row.unread, 20, "dialogs pages agree");
}

#[tokio::test(start_paused = true)]
async fn deleting_a_topic_deletes_its_messages() {
    let mut f = start(script(TopicOrder::LastActivity)).await;
    let ids: Vec<MsgId> = f
        .fake
        .server_history(FORUM)
        .iter()
        .filter(|m| m.thread.topic == Some(topic(5)))
        .map(|m| m.id)
        .collect();
    f.fake.delete_topic(FORUM, topic(5)).await.unwrap();
    assert!(matches!(
        f.update().await,
        Update::Deleted { chat: Some(FORUM), ids: got } if got == ids
    ));
    let got = f
        .client
        .topics_by_id(FORUM, &[topic(5), topic(6), topic(5)])
        .await
        .unwrap();
    assert_eq!(got.deleted, vec![topic(5)]);
    assert_eq!(got.found.len(), 1, "duplicates are answered once");
    assert_eq!(
        f.fake.delete_topic(FORUM, TopicId::GENERAL).await,
        Err(BackendError::Rejected(RejectReason::Other))
    );
    let (_, all) = f.all_topics().await;
    assert_eq!(all.len(), 60);
}

#[tokio::test(start_paused = true)]
async fn created_and_pinned_topics_lead_and_emit_updates() {
    let mut f = start(script(TopicOrder::Created)).await;
    let id = f.fake.create_topic(FORUM, "New topic").await.unwrap();
    let Update::NewMessage(root) = f.update().await else {
        panic!("a creation message");
    };
    assert_eq!(
        root.thread.topic,
        Some(id),
        "a creation message names its own topic"
    );
    assert_eq!(root.id.0, id.get());
    let (_, all) = f.all_topics().await;
    assert_eq!(
        all[2].topic.id, id,
        "newest created, after the pinned topics"
    );
    f.fake.pin_topic(FORUM, topic(30)).await.unwrap();
    assert!(matches!(
        f.update().await,
        Update::TopicsPinned { chat: FORUM }
    ));
    let (_, all) = f.all_topics().await;
    let pinned: Vec<i32> = all
        .iter()
        .take_while(|t| t.topic.pinned)
        .map(|t| t.topic.id.get())
        .collect();
    assert_eq!(pinned, vec![30, 8, 24]);
    f.fake.unpin_topic(FORUM, topic(8)).await.unwrap();
    assert!(matches!(
        f.update().await,
        Update::TopicsPinned { chat: FORUM }
    ));
    let message = f.fake.incoming(thread(9), "hello").await.unwrap();
    assert!(matches!(f.update().await, Update::NewMessage(m) if m == message));
    assert_eq!(f.by_id(9).await.unwrap().topic.last_id, Some(message.id));
}

#[tokio::test(start_paused = true)]
async fn kind_changes_emit_chat_changed() {
    let mut f = start(script(TopicOrder::LastActivity)).await;
    let group = ChatKind::Supergroup {
        forum: false,
        gigagroup: false,
    };
    f.fake.set_kind(FORUM, group).await;
    assert!(matches!(
        f.update().await,
        Update::ChatChanged { chat: FORUM }
    ));
    let peers = f.client.peer_dialogs(&[FORUM, PLAIN, FORUM]).await.unwrap();
    assert_eq!(
        peers.iter().map(|c| c.id).collect::<Vec<_>>(),
        vec![FORUM, PLAIN]
    );
    assert_eq!(peers[0].kind, group);
    assert_eq!(
        f.client.topics(FORUM, None, 10).await.err(),
        Some(BackendError::Rejected(RejectReason::NotForum))
    );
}

#[tokio::test(start_paused = true)]
async fn oversized_requests_are_rejected() {
    let f = start(script(TopicOrder::LastActivity)).await;
    let ids: Vec<TopicId> = (1..=i32::try_from(MAX_TOPICS_BY_ID).unwrap() + 1)
        .map(topic)
        .collect();
    assert!(f.client.topics_by_id(FORUM, &ids).await.is_err());
    let chats = vec![FORUM; MAX_PEER_DIALOGS + 1];
    assert!(f.client.peer_dialogs(&chats).await.is_err());
}

/// A delayed answer is the server state when the call was made: a change
/// during the delay crosses it as a live update (plan section 11).
#[tokio::test(start_paused = true)]
async fn a_delayed_answer_is_the_state_at_call_time() {
    let mut f = start(script(TopicOrder::LastActivity)).await;
    for op in [Op::Topics, Op::TopicsById, Op::PeerDialogs] {
        f.fake.inject(op, Fault::Delay(Duration::from_secs(5)));
    }
    let c = Arc::clone(&f.client);
    let by_id = tokio::spawn(async move { c.topics_by_id(FORUM, &[topic(2)]).await });
    let c = Arc::clone(&f.client);
    let page = tokio::spawn(async move { c.topics(FORUM, None, 61).await });
    let c = Arc::clone(&f.client);
    let peers = tokio::spawn(async move { c.peer_dialogs(&[FORUM]).await });
    tokio::time::sleep(Duration::from_secs(1)).await;
    f.fake.close_topic(FORUM, topic(2)).await.unwrap();
    // Topic 1 (id 2) starts out read: this post makes it the 22nd unread topic.
    assert_eq!(f.by_id(2).await.unwrap().topic.unread, 0);
    f.fake.incoming(thread(2), "new").await.unwrap();
    assert!(matches!(f.update().await, Update::TopicEdited { .. }));
    let found = by_id.await.unwrap().unwrap().found;
    assert!(!found[0].topic.closed, "taken before the close");
    let listed = page.await.unwrap().unwrap().topics;
    assert!(
        !listed
            .iter()
            .find(|t| t.topic.id == topic(2))
            .unwrap()
            .topic
            .closed
    );
    assert_eq!(
        peers.await.unwrap().unwrap()[0].unread,
        21,
        "taken before the post"
    );
    assert_eq!(f.client.peer_dialogs(&[FORUM]).await.unwrap()[0].unread, 22);
    assert!(
        f.by_id(2).await.unwrap().topic.closed,
        "a later call sees it"
    );
}

#[tokio::test(start_paused = true)]
async fn faults_apply_to_the_forum_calls() {
    let f = start(script(TopicOrder::LastActivity)).await;
    let flood = BackendError::FloodWait { seconds: 60 };
    for op in [Op::Topics, Op::TopicsById, Op::PeerDialogs] {
        f.fake.inject(op, Fault::Error(flood));
    }
    assert_eq!(f.client.topics(FORUM, None, 10).await.err(), Some(flood));
    assert_eq!(
        f.client.topics_by_id(FORUM, &[topic(1)]).await.err(),
        Some(flood)
    );
    assert_eq!(f.client.peer_dialogs(&[FORUM]).await.err(), Some(flood));

    for fault in [Fault::Hang, Fault::CompleteThenHang] {
        f.fake.inject(Op::Topics, fault);
        let call = tokio::time::timeout(Duration::from_secs(60), f.client.topics(FORUM, None, 10));
        assert!(call.await.is_err(), "{fault:?} never answers");
    }

    for op in [Op::Topics, Op::TopicsById, Op::PeerDialogs] {
        f.fake.inject(op, Fault::Panic);
        let c = Arc::clone(&f.client);
        let call = tokio::spawn(async move {
            match op {
                Op::Topics => c.topics(FORUM, None, 10).await.map(drop),
                Op::TopicsById => c.topics_by_id(FORUM, &[topic(1)]).await.map(drop),
                _ => c.peer_dialogs(&[FORUM]).await.map(drop),
            }
        });
        assert!(call.await.unwrap_err().is_panic());
    }
    assert_eq!(f.fake.calls(Op::Topics), 4);
    assert_eq!(f.fake.calls(Op::TopicsById), 2);
    assert_eq!(f.fake.calls(Op::PeerDialogs), 2);
}

/// Message ids, and so topic ids, are never reused, even after the newest
/// messages were deleted with their topic.
#[tokio::test(start_paused = true)]
async fn a_deleted_topic_id_is_never_reused() {
    let mut f = start(script(TopicOrder::LastActivity)).await;
    let first = f.fake.create_topic(FORUM, "First").await.unwrap();
    assert!(matches!(f.update().await, Update::NewMessage(_)));
    f.fake.delete_topic(FORUM, first).await.unwrap();
    assert!(matches!(f.update().await, Update::Deleted { .. }));
    let second = f.fake.create_topic(FORUM, "Second").await.unwrap();
    assert!(second > first, "a fresh id: {second:?} after {first:?}");
    let got = f
        .client
        .topics_by_id(FORUM, &[first, second])
        .await
        .unwrap();
    assert_eq!(got.deleted, vec![first], "the old id stays deleted");
    assert_eq!(got.found.len(), 1);
    assert_eq!(got.found[0].topic.id, second);

    // Replacing history never lowers the mark either.
    f.fake.set_history(FORUM, Vec::new());
    let sent = f.client.send_text(thread(1), "x".into(), 9).await.unwrap();
    assert!(sent.0 > second.get());
}
