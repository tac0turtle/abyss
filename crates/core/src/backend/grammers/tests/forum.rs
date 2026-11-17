//! Forum topics over grammers (Stage 2 plan sections 5 and 11): the pure
//! mappers on hand-built TL, and the requests the call layer builds.

use grammers_client::tl::{self, Deserializable as _, Serializable as _};

use super::calls::{ANN, GROUP, LogBuf, Mock, capture_logs, rpc, setup};
use super::{
    channel, dialog, message, notify_settings, peer_channel, peer_user, reply_header, service,
    user, users,
};
use crate::backend::grammers::map::lists::{self, MessagesParts, RESPONSE_SLACK};
use crate::backend::grammers::map::peers::{Directory, MAX_DIRECTORY};
use crate::backend::grammers::map::topics::{self, TopicsParts};
use crate::backend::{BackendError, HistoryQuery, RejectReason, Update};
use crate::model::{
    ChatId, ChatKind, Freshness, MsgId, PeerKind, Preview, ThreadKey, TopicId, TopicOrder,
    Visibility,
};
use crate::store::{ForumStatus, MAX_PEER_DIALOGS, MAX_TOPICS_BY_ID, PostClosed};

/// The forum used throughout: channel 10, which `setup` knows the hash of.
const FORUM: i64 = 10;

fn cid(id: i64) -> ChatId {
    ChatId {
        kind: PeerKind::Channel,
        id,
    }
}

fn topic_id(id: i32) -> TopicId {
    TopicId::try_from(id).expect("positive")
}

fn thread(topic: Option<i32>) -> ThreadKey {
    ThreadKey {
        chat: GROUP,
        topic: topic.map(topic_id),
    }
}

/// An open, unpinned topic of the forum created at `1_600_000_000 + id`,
/// whose top message is `top`.
fn forum_topic(id: i32, title: &str, top: i32) -> tl::types::ForumTopic {
    tl::types::ForumTopic {
        my: false,
        closed: false,
        pinned: false,
        short: false,
        hidden: false,
        title_missing: false,
        id,
        date: 1_600_000_000 + id,
        peer: peer_channel(FORUM),
        title: title.to_owned(),
        icon_color: 0x6F_B9F0,
        icon_emoji_id: Some(5),
        top_message: top,
        read_inbox_max_id: 0,
        read_outbox_max_id: 0,
        unread_count: 0,
        unread_mentions_count: 0,
        unread_reactions_count: 0,
        unread_poll_votes_count: 0,
        from_id: peer_user(7),
        notify_settings: notify_settings(),
        draft: None,
    }
}

fn listed(t: tl::types::ForumTopic) -> tl::enums::ForumTopic {
    tl::enums::ForumTopic::Topic(t)
}

fn deleted(id: i32) -> tl::enums::ForumTopic {
    tl::enums::ForumTopic::Deleted(tl::types::ForumTopicDeleted { id })
}

/// A post into topic `topic` of the forum (a forum header without a
/// reply), or into General without a header for `None`.
fn post(id: i32, topic: Option<i32>, text: &str) -> tl::types::Message {
    let mut m = message(peer_channel(FORUM), id, text);
    m.reply_to = topic.map(|t| {
        let mut h = reply_header(Some(t), None);
        h.forum_topic = true;
        tl::enums::MessageReplyHeader::Header(h)
    });
    m
}

fn forum_channel(title: &str) -> tl::types::Channel {
    let mut c = channel(FORUM, title);
    c.forum = true;
    c
}

fn forum_topics(
    order: TopicOrder,
    count: i32,
    topics: Vec<tl::enums::ForumTopic>,
    messages: Vec<tl::types::Message>,
) -> tl::enums::messages::ForumTopics {
    tl::enums::messages::ForumTopics::Topics(tl::types::messages::ForumTopics {
        order_by_create_date: order == TopicOrder::Created,
        count,
        topics,
        messages: messages
            .into_iter()
            .map(tl::enums::Message::Message)
            .collect(),
        chats: vec![tl::enums::Chat::Channel(forum_channel("Forum"))],
        users: vec![],
        pts: 1,
    })
}

fn parts(resp: &tl::enums::messages::ForumTopics, limit: usize) -> TopicsParts<'_> {
    topics::topics_parts(resp, limit).expect("within bounds")
}

// --- forumTopic to TopicSnapshot ---

#[test]
fn forum_topics_map_to_sanitized_snapshots_with_their_top_message() {
    let mut planning = forum_topic(5, "\u{202e}Plan\u{2066}ning\n", 50);
    planning.closed = true;
    planning.pinned = true;
    planning.unread_count = 3;
    planning.unread_mentions_count = 1;
    planning.read_inbox_max_id = 40;
    let mut top = post(50, Some(5), "latest\nnews");
    top.edit_date = Some(1_700_000_100);
    let mut general = forum_topic(1, "General", 60);
    general.title_missing = true;
    general.hidden = true;
    let quiet = forum_topic(9, "\u{202e}\u{2066}", 70);
    let resp = forum_topics(
        TopicOrder::LastActivity,
        3,
        vec![listed(planning), listed(general), listed(quiet)],
        vec![top, post(60, None, "g")],
    );
    let parts = parts(&resp, 50);
    let dir = Directory::from_lists(parts.users, parts.chats);
    let mapped = topics::topics(&parts, &dir, cid(FORUM));
    assert_eq!(mapped.skipped, 0);
    // `Debug` of the mapping types shows counts, never titles or text.
    for shown in [format!("{parts:?}"), format!("{mapped:?}")] {
        assert!(
            !shown.contains("Plan") && !shown.contains("latest"),
            "{shown}"
        );
    }
    let [planning, general, quiet] = mapped.found.as_slice() else {
        panic!("three topics: {mapped:?}");
    };

    let t = &planning.topic;
    assert_eq!(t.id, topic_id(5));
    assert_eq!(t.title, "Planning");
    assert!(t.is_valid());
    assert_eq!(t.created, 1_600_000_005);
    assert!(t.closed && t.pinned);
    assert_eq!((t.unread, t.unread_mentions), (3, 1));
    assert_eq!(t.freshness, Freshness::Current);
    assert_eq!(t.read_inbox_max, MsgId(40));
    assert_eq!(t.last, Some(Preview::Text("latest news".to_owned())));
    assert_eq!(t.last_date, 1_700_000_050);
    assert_eq!(t.last_id, Some(MsgId(50)));
    assert_eq!(planning.visibility, Visibility::Shown);
    assert_eq!(planning.top_version, 1_700_000_100);

    // `title_missing`: the app shows the General fallback.
    assert_eq!(general.topic.id, TopicId::GENERAL);
    assert_eq!(general.topic.title, "");
    assert_eq!(general.visibility, Visibility::Hidden);
    assert_eq!(general.top_version, i64::MIN);
    assert_eq!(general.topic.last_id, Some(MsgId(60)));

    // A title that sanitizes to nothing, and a top message the response
    // lacks: no preview, but the top id stands.
    assert_eq!(quiet.topic.title, "");
    assert_eq!(quiet.topic.last, None);
    assert_eq!(quiet.topic.last_date, 0);
    assert_eq!(quiet.topic.last_id, Some(MsgId(70)));
}

#[test]
fn deleted_and_malformed_topics_are_skipped_item_by_item() {
    let resp = forum_topics(
        TopicOrder::LastActivity,
        10,
        vec![
            deleted(4),
            listed(forum_topic(0, "zero", 1)),
            listed(forum_topic(-3, "negative", 2)),
            listed(forum_topic(7, "kept", 3)),
            deleted(0),
        ],
        vec![],
    );
    let parts = parts(&resp, 50);
    let dir = Directory::from_lists(parts.users, parts.chats);

    // A page never removes a topic: deleted entries are skipped.
    let (page, skipped) = topics::page(&parts, &dir, cid(FORUM), None);
    let ids: Vec<TopicId> = page.topics.iter().map(|s| s.topic.id).collect();
    assert_eq!(ids, vec![topic_id(7)]);
    assert_eq!(skipped, 4);

    // By id, a deleted entry is a deletion; a malformed one is skipped.
    let (result, skipped) = topics::by_id(&parts, &dir, cid(FORUM));
    assert_eq!(result.deleted, vec![topic_id(4)]);
    assert_eq!(result.found.len(), 1);
    assert_eq!(skipped, 3);
}

#[test]
fn oversized_topic_responses_are_rejected_before_mapping() {
    let limit = 5;
    let items = limit + RESPONSE_SLACK;
    let n = |k: usize| i32::try_from(k).unwrap();
    let too_many_topics = forum_topics(
        TopicOrder::LastActivity,
        100,
        (1..=n(items) + 1)
            .map(|i| listed(forum_topic(i, "t", i)))
            .collect(),
        vec![],
    );
    assert!(topics::topics_parts(&too_many_topics, limit).is_err());
    let at_bound = forum_topics(
        TopicOrder::LastActivity,
        100,
        (1..=n(items))
            .map(|i| listed(forum_topic(i, "t", i)))
            .collect(),
        (1..=n(2 * items)).map(|i| post(i, Some(1), "m")).collect(),
    );
    assert!(topics::topics_parts(&at_bound, limit).is_ok());
    let too_many_messages = forum_topics(
        TopicOrder::LastActivity,
        100,
        vec![],
        (1..=n(2 * items) + 1)
            .map(|i| post(i, Some(1), "m"))
            .collect(),
    );
    assert!(topics::topics_parts(&too_many_messages, limit).is_err());
    let mut too_many_peers = forum_topics(TopicOrder::LastActivity, 1, vec![], vec![]);
    let tl::enums::messages::ForumTopics::Topics(r) = &mut too_many_peers;
    r.users = users(
        (1..=i64::try_from(MAX_DIRECTORY).unwrap())
            .map(|id| user(id, "U", ""))
            .collect(),
    );
    assert!(topics::topics_parts(&too_many_peers, limit).is_err());
}

// --- order and cursor ---

#[test]
fn order_by_create_date_sets_the_order_and_the_cursor_per_mode() {
    let page_of = |order: TopicOrder| {
        forum_topics(
            order,
            10,
            vec![
                listed(forum_topic(3, "a", 30)),
                listed(forum_topic(2, "b", 20)),
                deleted(9),
            ],
            vec![post(30, Some(3), "x"), post(20, Some(2), "y")],
        )
    };

    let activity = page_of(TopicOrder::LastActivity);
    let parts = parts(&activity, 3);
    assert_eq!(parts.order, TopicOrder::LastActivity);
    let next = topics::next_cursor(&parts, cid(FORUM), None).expect("more");
    // The last raw topic that is not a deletion: its top message's date.
    assert_eq!(next.order(), TopicOrder::LastActivity);
    assert_eq!(
        (
            next.offset_date,
            next.offset_id,
            next.offset_topic,
            next.index
        ),
        (1_700_000_020, 20, 2, 3)
    );

    let created = page_of(TopicOrder::Created);
    let parts_c = topics::topics_parts(&created, 3).unwrap();
    assert_eq!(parts_c.order, TopicOrder::Created);
    let next = topics::next_cursor(&parts_c, cid(FORUM), None).expect("more");
    // Creation order pages by `forumTopic.date`.
    assert_eq!(next.order(), TopicOrder::Created);
    assert_eq!(
        (next.offset_date, next.offset_id, next.offset_topic),
        (1_600_000_002, 20, 2)
    );

    // The index counts the topics listed so far; `count` ends the list.
    let later = topics::next_cursor(&parts, cid(FORUM), Some(&next)).expect("more");
    assert_eq!(later.index, 6);
    let mut near_end = next;
    near_end.index = 7;
    assert_eq!(
        topics::next_cursor(&parts, cid(FORUM), Some(&near_end)),
        None
    );
}

/// `getForumTopics` may return fewer topics than asked for while more
/// remain: only an empty page or reaching `count` ends the list.
#[test]
fn a_short_page_continues_until_count_is_reached() {
    let short = forum_topics(
        TopicOrder::LastActivity,
        80,
        (0..20)
            .map(|k| listed(forum_topic(80 - k, "t", 1000 + 80 - k)))
            .collect(),
        vec![],
    );
    let parts = parts(&short, 50);
    let next = topics::next_cursor(&parts, cid(FORUM), None).expect("60 topics remain");
    assert_eq!(
        (next.offset_topic, next.offset_id, next.index),
        (61, 1061, 20)
    );
    let mut at_end = next.clone();
    at_end.index = 60;
    assert_eq!(topics::next_cursor(&parts, cid(FORUM), Some(&at_end)), None);
    let empty = forum_topics(TopicOrder::LastActivity, 80, vec![], vec![]);
    assert_eq!(
        topics::next_cursor(&self::parts(&empty, 50), cid(FORUM), Some(&next)),
        None
    );
}

#[test]
fn closed_topic_posting_comes_from_the_channel_rights() {
    let rights = |manage_topics: bool| {
        tl::enums::ChatAdminRights::Rights(tl::types::ChatAdminRights {
            change_info: false,
            post_messages: false,
            edit_messages: false,
            delete_messages: true,
            ban_users: true,
            invite_users: false,
            pin_messages: true,
            add_admins: false,
            anonymous: false,
            manage_call: false,
            other: true,
            manage_topics,
            post_stories: false,
            edit_stories: false,
            delete_stories: false,
            manage_direct_messages: false,
            manage_ranks: false,
            manage_linked_peers: false,
            manage_welcome_messages: false,
        })
    };
    let with = |c: tl::types::Channel| {
        let dir = Directory::from_lists(&[], &[tl::enums::Chat::Channel(c)]);
        topics::post_closed(&dir, cid(FORUM))
    };
    let mut creator = forum_channel("f");
    creator.creator = true;
    assert_eq!(with(creator), PostClosed::Allowed);
    let mut moderator = forum_channel("f");
    moderator.admin_rights = Some(rights(true));
    assert_eq!(with(moderator), PostClosed::Allowed);
    let mut admin = forum_channel("f");
    admin.admin_rights = Some(rights(false));
    assert_eq!(with(admin), PostClosed::Refused);
    assert_eq!(with(forum_channel("f")), PostClosed::Refused);
    assert_eq!(
        topics::post_closed(&Directory::default(), cid(FORUM)),
        PostClosed::Refused
    );
}

// --- topic history mapping ---

#[test]
fn topic_history_keeps_only_the_requested_thread() {
    let root = service(
        peer_channel(FORUM),
        5,
        None,
        tl::enums::MessageAction::TopicCreate(tl::types::MessageActionTopicCreate {
            title_missing: false,
            title: "t".into(),
            icon_color: 0,
            icon_emoji_id: None,
        }),
    );
    let mut in_reply = post(8, Some(7), "reply inside 5");
    if let Some(tl::enums::MessageReplyHeader::Header(h)) = &mut in_reply.reply_to {
        h.reply_to_top_id = Some(5);
    }
    let raw: Vec<tl::enums::Message> = vec![
        tl::enums::Message::Service(root),
        tl::enums::Message::Message(post(6, Some(5), "in 5")),
        tl::enums::Message::Message(post(7, Some(6), "in 6")),
        tl::enums::Message::Message(in_reply),
        tl::enums::Message::Message(post(9, None, "general")),
        tl::enums::Message::Message(message(peer_user(7), 10, "other chat")),
    ];
    let parts = MessagesParts {
        messages: &raw,
        users: &[],
        chats: &[],
        complete: false,
    };
    let dir = Directory::default();
    let ids = |t: ThreadKey| {
        lists::history(&parts, &dir, t, HistoryQuery::Tail, 100)
            .messages
            .iter()
            .map(|m| m.id.0)
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(thread(Some(5))), vec![5, 6, 8]);
    assert_eq!(ids(thread(Some(1))), vec![9]);
    assert_eq!(ids(thread(None)), vec![5, 6, 7, 8, 9]);
    // `has_more` follows the raw page, whatever the filter left.
    assert!(lists::history(&parts, &dir, thread(Some(5)), HistoryQuery::Tail, 6).has_more);
}

// --- updates ---

fn edit_topic(topic: i32) -> tl::types::UpdateEditForumTopic {
    tl::types::UpdateEditForumTopic {
        peer: peer_channel(FORUM),
        topic_id: topic,
        title: Some("\u{202e}Re\nnamed".into()),
        icon_emoji_id: Some(9),
        closed: Some(true),
        hidden: None,
    }
}

fn discussion_read(top: i32) -> tl::types::UpdateReadChannelDiscussionInbox {
    tl::types::UpdateReadChannelDiscussionInbox {
        channel_id: FORUM,
        top_msg_id: top,
        read_max_id: 44,
        broadcast_id: None,
        broadcast_post: None,
    }
}

fn history_read(peer: tl::enums::Peer, top: Option<i32>) -> tl::types::UpdateReadHistoryInbox {
    tl::types::UpdateReadHistoryInbox {
        folder_id: None,
        peer,
        top_msg_id: top,
        max_id: 9,
        still_unread_count: 2,
        pts: 1,
        pts_count: 1,
    }
}

#[test]
fn forum_updates_map_to_domain_updates() {
    use tl::enums::Update as U;
    let dir = Directory::default();
    let forum = cid(FORUM);

    let read = U::ReadHistoryInbox(history_read(peer_user(7), Some(3)));
    assert!(matches!(
        lists::update(&read, &dir),
        Some(Update::ReadInbox { chat, topic: Some(t), still_unread: 2, max_id: MsgId(9) })
            if chat == ANN && t == topic_id(3)
    ));
    let chat_read = U::ReadHistoryInbox(history_read(peer_user(7), None));
    assert!(matches!(
        lists::update(&chat_read, &dir),
        Some(Update::ReadInbox { topic: None, .. })
    ));
    let topic_read = U::ReadChannelDiscussionInbox(discussion_read(5));
    assert!(matches!(
        lists::update(&topic_read, &dir),
        Some(Update::ReadTopicMax { chat, topic, max_id: MsgId(44) })
            if chat == forum && topic == topic_id(5)
    ));

    let edit = U::EditForumTopic(edit_topic(5));
    let Some(Update::TopicEdited {
        chat,
        topic,
        title,
        closed,
        visibility,
    }) = lists::update(&edit, &dir)
    else {
        panic!("topic edit not mapped");
    };
    assert_eq!((chat, topic), (forum, topic_id(5)));
    assert_eq!(title.as_deref(), Some("Re named"));
    assert_eq!((closed, visibility), (Some(true), None));
    let mut hide = edit_topic(1);
    hide.title = None;
    hide.closed = None;
    hide.hidden = Some(true);
    assert!(matches!(
        lists::update(&U::EditForumTopic(hide), &dir),
        Some(Update::TopicEdited {
            title: None,
            closed: None,
            visibility: Some(Visibility::Hidden),
            ..
        })
    ));

    let pin = U::PinnedForumTopic(tl::types::UpdatePinnedForumTopic {
        pinned: true,
        peer: peer_channel(FORUM),
        topic_id: 5,
    });
    assert!(
        matches!(lists::update(&pin, &dir), Some(Update::TopicsPinned { chat }) if chat == forum)
    );
    let order = U::PinnedForumTopics(tl::types::UpdatePinnedForumTopics {
        peer: peer_channel(FORUM),
        order: Some(vec![5, 1]),
    });
    assert!(
        matches!(lists::update(&order, &dir), Some(Update::TopicsPinned { chat }) if chat == forum)
    );

    let changed = U::Channel(tl::types::UpdateChannel { channel_id: FORUM });
    assert!(
        matches!(lists::update(&changed, &dir), Some(Update::ChatChanged { chat }) if chat == forum)
    );
}

#[test]
fn malformed_and_broadcast_forum_updates_are_dropped() {
    use tl::enums::Update as U;
    let dir = Directory::default();
    for bad in [0, -5, i32::MIN] {
        let cases = vec![
            U::ReadHistoryInbox(history_read(peer_user(7), Some(bad))),
            U::ReadChannelDiscussionInbox(discussion_read(bad)),
            U::EditForumTopic(edit_topic(bad)),
            U::PinnedForumTopic(tl::types::UpdatePinnedForumTopic {
                pinned: false,
                peer: peer_channel(FORUM),
                topic_id: bad,
            }),
        ];
        for u in &cases {
            assert!(lists::update(u, &dir).is_none(), "topic id {bad}");
        }
    }

    // A discussion read of a channel post (comments are out of v1).
    let mut comments = discussion_read(5);
    comments.broadcast_id = Some(99);
    comments.broadcast_post = Some(3);
    assert!(lists::update(&U::ReadChannelDiscussionInbox(comments), &dir).is_none());
    let mut news = channel(FORUM, "news");
    news.broadcast = true;
    news.megagroup = false;
    let broadcast = Directory::from_lists(&[], &[tl::enums::Chat::Channel(news)]);
    let read = U::ReadChannelDiscussionInbox(discussion_read(5));
    assert!(lists::update(&read, &broadcast).is_none());
    let changed = U::Channel(tl::types::UpdateChannel { channel_id: FORUM });
    assert!(lists::update(&changed, &broadcast).is_none());
    // Out-of-range channel ids.
    let far = U::Channel(tl::types::UpdateChannel { channel_id: -1 });
    assert!(lists::update(&far, &dir).is_none());
}

// --- requests ---

fn topics_reply(order: TopicOrder, count: i32, n: i32, first: i32) -> Vec<u8> {
    forum_topics(
        order,
        count,
        (0..n)
            .map(|k| listed(forum_topic(first - k, "secret title", 1000 + first - k)))
            .collect(),
        (0..n)
            .map(|k| post(1000 + first - k, Some(first - k), "secret text"))
            .collect(),
    )
    .to_bytes()
}

fn messages_reply(ids: &[(i32, Option<i32>)]) -> Vec<u8> {
    tl::enums::messages::Messages::ChannelMessages(tl::types::messages::ChannelMessages {
        inexact: false,
        pts: 1,
        count: 100,
        offset_id_offset: None,
        messages: ids
            .iter()
            .map(|&(id, topic)| tl::enums::Message::Message(post(id, topic, "m")))
            .collect(),
        topics: vec![],
        chats: vec![],
        users: vec![],
    })
    .to_bytes()
}

#[tokio::test]
async fn topics_requests_carry_the_cursor_of_the_previous_page() {
    let mock = Mock::with(vec![
        Ok(topics_reply(TopicOrder::LastActivity, 10, 3, 40)),
        Ok(topics_reply(TopicOrder::LastActivity, 10, 3, 37)),
        Ok(topics_reply(TopicOrder::Created, 10, 3, 9)),
    ]);
    let calls = setup(&mock).await;
    let first = calls.topics(GROUP, None, 3).await.unwrap();
    assert_eq!(first.order, TopicOrder::LastActivity);
    assert_eq!(first.topics.len(), 3);
    assert_eq!(first.post_closed, PostClosed::Refused);
    let second = calls.topics(GROUP, first.next, 3).await.unwrap();
    let created = calls.topics(GROUP, second.next, 3).await.unwrap();
    // The cursor is per page: core sees the order change and restarts.
    assert_eq!(created.order, TopicOrder::Created);

    let requests = mock.requests();
    let get =
        |i: usize| tl::functions::messages::GetForumTopics::from_boxed_bytes(&requests[i]).unwrap();
    let one = get(0);
    assert!(matches!(
        one.peer,
        tl::enums::InputPeer::Channel(c) if c.channel_id == FORUM && c.access_hash == 110
    ));
    assert_eq!(one.q, None);
    assert_eq!(
        (one.offset_date, one.offset_id, one.offset_topic, one.limit),
        (0, 0, 0, 3)
    );
    let two = get(1);
    assert_eq!(
        (two.offset_date, two.offset_id, two.offset_topic),
        (1_700_001_038, 1038, 38)
    );
    let three = get(2);
    assert_eq!(three.offset_topic, 35);
}

#[tokio::test]
async fn topics_by_id_sends_the_ids_and_refuses_too_many() {
    let found = forum_topics(
        TopicOrder::Created,
        2,
        vec![listed(forum_topic(5, "t", 50)), deleted(6)],
        vec![],
    );
    let mock = Mock::with(vec![Ok(found.to_bytes())]);
    let calls = setup(&mock).await;
    let result = calls
        .topics_by_id(GROUP, &[topic_id(5), topic_id(6)])
        .await
        .unwrap();
    assert_eq!(result.found.len(), 1);
    assert_eq!(result.deleted, vec![topic_id(6)]);
    let request =
        tl::functions::messages::GetForumTopicsById::from_boxed_bytes(&mock.requests()[0]).unwrap();
    assert_eq!(request.topics, vec![5, 6]);

    let many: Vec<TopicId> = (1..=i32::try_from(MAX_TOPICS_BY_ID).unwrap() + 1)
        .map(topic_id)
        .collect();
    assert_eq!(
        calls.topics_by_id(GROUP, &many).await.unwrap_err(),
        BackendError::Rejected(RejectReason::Other)
    );
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn topic_calls_for_chats_that_cannot_be_forums_send_nothing() {
    let mock = Mock::with(vec![]);
    let calls = setup(&mock).await;
    let not_forum = BackendError::Rejected(RejectReason::NotForum);
    assert_eq!(calls.topics(ANN, None, 10).await.unwrap_err(), not_forum);
    assert_eq!(
        calls.topics_by_id(ANN, &[topic_id(2)]).await.unwrap_err(),
        not_forum
    );
    let ann_topic = ThreadKey {
        chat: ANN,
        topic: Some(topic_id(2)),
    };
    assert_eq!(
        calls
            .history(ann_topic, HistoryQuery::Tail, 10)
            .await
            .unwrap_err(),
        not_forum
    );
    assert_eq!(
        calls.send_text(ann_topic, "x".into(), 1).await.unwrap_err(),
        not_forum
    );
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn a_chat_that_is_not_a_forum_answers_not_forum() {
    let mock = Mock::with(vec![rpc(400, "CHANNEL_FORUM_MISSING")]);
    let calls = setup(&mock).await;
    assert_eq!(
        calls.topics(GROUP, None, 10).await.unwrap_err(),
        BackendError::Rejected(RejectReason::NotForum)
    );
}

#[tokio::test]
async fn topic_history_uses_get_replies_and_general_follows_u1() {
    let mock = Mock::with(vec![
        Ok(messages_reply(&[(6, Some(5)), (7, Some(6)), (8, None)])),
        Ok(messages_reply(&[(8, None), (6, Some(5))])),
        Ok(messages_reply(&[(6, Some(5)), (8, None)])),
    ]);
    let calls = setup(&mock).await;
    let five = calls
        .history(thread(Some(5)), HistoryQuery::Before(MsgId(100)), 20)
        .await
        .unwrap();
    assert_eq!(
        five.messages.iter().map(|m| m.id.0).collect::<Vec<_>>(),
        vec![6]
    );
    let general = calls
        .history(thread(Some(1)), HistoryQuery::After(MsgId(3)), 20)
        .await
        .unwrap();
    assert_eq!(
        general.messages.iter().map(|m| m.id.0).collect::<Vec<_>>(),
        vec![8]
    );
    let whole = calls
        .history(thread(None), HistoryQuery::Tail, 20)
        .await
        .unwrap();
    assert_eq!(whole.messages.len(), 2);

    let requests = mock.requests();
    let replies = tl::functions::messages::GetReplies::from_boxed_bytes(&requests[0]).unwrap();
    assert_eq!(
        (
            replies.msg_id,
            replies.offset_id,
            replies.add_offset,
            replies.limit,
            replies.min_id
        ),
        (5, 100, 0, 20, 0)
    );
    assert!(matches!(replies.peer, tl::enums::InputPeer::Channel(c) if c.channel_id == FORUM));
    // U1: General is `getReplies(msg_id = 1)` until verified live.
    let general = tl::functions::messages::GetReplies::from_boxed_bytes(&requests[1]).unwrap();
    assert_eq!(
        (
            general.msg_id,
            general.offset_id,
            general.add_offset,
            general.min_id
        ),
        (1, 3, -20, 3)
    );
    assert!(tl::functions::messages::GetHistory::from_boxed_bytes(&requests[2]).is_ok());
}

/// `Around` asks both calls from the anchor's successor, 40 messages back
/// above it (10 at or below), keeps both sides of the anchor, and is
/// refused without a call outside its bounds: no successor, a negative
/// anchor, or a limit outside `AROUND_CONTEXT < limit ≤ 100`.
#[tokio::test]
async fn around_pages_ask_from_the_successor_in_both_calls() {
    let mock = Mock::with(vec![
        Ok(messages_reply(&[(9, Some(5)), (6, Some(5))])),
        Ok(messages_reply(&[(2, None), (1, None)])),
    ]);
    let calls = setup(&mock).await;
    let topic = calls
        .history(thread(Some(5)), HistoryQuery::Around(MsgId(7)), 50)
        .await
        .unwrap();
    assert_eq!(
        topic.messages.iter().map(|m| m.id.0).collect::<Vec<_>>(),
        vec![6, 9],
        "both sides of the anchor, ascending"
    );
    calls
        .history(thread(None), HistoryQuery::Around(MsgId(0)), 50)
        .await
        .unwrap();
    for (anchor, limit) in [(i32::MAX, 50), (-1, 50), (7, 0), (7, 10), (7, 101)] {
        assert_eq!(
            calls
                .history(thread(Some(5)), HistoryQuery::Around(MsgId(anchor)), limit)
                .await
                .unwrap_err(),
            BackendError::Rejected(RejectReason::Other),
            "anchor {anchor}, limit {limit}"
        );
    }

    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "refused pages make no call");
    let replies = tl::functions::messages::GetReplies::from_boxed_bytes(&requests[0]).unwrap();
    assert_eq!(
        (
            replies.msg_id,
            replies.offset_id,
            replies.add_offset,
            replies.limit,
            replies.max_id,
            replies.min_id
        ),
        (5, 8, -40, 50, 0, 0)
    );
    let history = tl::functions::messages::GetHistory::from_boxed_bytes(&requests[1]).unwrap();
    assert_eq!(
        (
            history.offset_id,
            history.add_offset,
            history.limit,
            history.max_id,
            history.min_id
        ),
        (1, -40, 50, 0, 0),
        "an anchor of 0 asks for the oldest messages"
    );
}

#[tokio::test]
async fn sends_into_a_topic_reply_to_its_root_and_general_has_no_reply() {
    let short_sent = |id: i32| {
        Ok(
            tl::enums::Updates::UpdateShortSentMessage(tl::types::UpdateShortSentMessage {
                out: true,
                id,
                pts: 1,
                pts_count: 1,
                date: 0,
                media: None,
                entities: None,
                ttl_period: None,
            })
            .to_bytes(),
        )
    };
    let mock = Mock::with(vec![
        short_sent(1),
        short_sent(2),
        short_sent(3),
        rpc(406, "TOPIC_CLOSED"),
        rpc(406, "TOPIC_DELETED"),
    ]);
    let calls = setup(&mock).await;
    calls
        .send_text(thread(Some(5)), "a".into(), 11)
        .await
        .unwrap();
    calls
        .send_text(thread(Some(1)), "b".into(), 12)
        .await
        .unwrap();
    calls
        .send_text(ThreadKey::whole(ANN), "c".into(), 13)
        .await
        .unwrap();
    assert_eq!(
        calls.send_text(thread(Some(5)), "d".into(), 14).await,
        Err(BackendError::Rejected(RejectReason::TopicClosed))
    );
    assert_eq!(
        calls.send_text(thread(Some(5)), "e".into(), 15).await,
        Err(BackendError::Rejected(RejectReason::TopicDeleted))
    );

    let requests = mock.requests();
    let send =
        |i: usize| tl::functions::messages::SendMessage::from_boxed_bytes(&requests[i]).unwrap();
    let topic = send(0);
    assert_eq!(topic.random_id, 11);
    let Some(tl::enums::InputReplyTo::Message(r)) = topic.reply_to else {
        panic!("a topic post replies to the topic root");
    };
    assert_eq!((r.reply_to_msg_id, r.top_msg_id), (5, None));
    assert!(r.reply_to_peer_id.is_none() && r.quote_text.is_none());
    assert!(send(1).reply_to.is_none(), "General has no reply_to");
    assert!(
        send(2).reply_to.is_none(),
        "a non-forum chat has no reply_to"
    );
}

#[tokio::test]
async fn topic_reads_use_read_discussion_and_general_follows_u2() {
    let mock = Mock::with(vec![
        Ok(true.to_bytes()),
        Ok(true.to_bytes()),
        Ok(true.to_bytes()),
    ]);
    let calls = setup(&mock).await;
    calls
        .read_history(thread(Some(5)), MsgId(41))
        .await
        .unwrap();
    calls
        .read_history(thread(Some(1)), MsgId(42))
        .await
        .unwrap();
    calls.read_history(thread(None), MsgId(43)).await.unwrap();
    let requests = mock.requests();
    let topic = tl::functions::messages::ReadDiscussion::from_boxed_bytes(&requests[0]).unwrap();
    assert_eq!((topic.msg_id, topic.read_max_id), (5, 41));
    assert!(matches!(topic.peer, tl::enums::InputPeer::Channel(c) if c.channel_id == FORUM));
    // U2: General is `readDiscussion(msg_id = 1)`, never `channels.readHistory`.
    let general = tl::functions::messages::ReadDiscussion::from_boxed_bytes(&requests[1]).unwrap();
    assert_eq!((general.msg_id, general.read_max_id), (1, 42));
    let whole = tl::functions::channels::ReadHistory::from_boxed_bytes(&requests[2]).unwrap();
    assert_eq!(whole.max_id, 43);
}

fn peer_dialogs_reply(forum: ForumStatus) -> Vec<u8> {
    let mut group = dialog(peer_channel(FORUM), 30);
    group.unread_count = 4;
    let mut c = channel(FORUM, "secret forum");
    c.forum = forum == ForumStatus::Forum;
    tl::enums::messages::PeerDialogs::Dialogs(tl::types::messages::PeerDialogs {
        dialogs: vec![
            tl::enums::Dialog::Dialog(group),
            tl::enums::Dialog::Dialog(dialog(peer_user(7), 20)),
            // Not asked for: not this call's answer.
            tl::enums::Dialog::Dialog(dialog(peer_user(8), 21)),
        ],
        messages: vec![tl::enums::Message::Message(post(
            30,
            Some(5),
            "secret text",
        ))],
        chats: vec![tl::enums::Chat::Channel(c)],
        users: users(vec![user(7, "Ann", ""), user(8, "Bob", "")]),
        state: tl::enums::updates::State::State(tl::types::updates::State {
            pts: 1,
            qts: 0,
            date: 0,
            seq: 0,
            unread_count: 0,
        }),
    })
    .to_bytes()
}

#[tokio::test]
async fn peer_dialogs_ask_for_known_peers_and_map_the_kind() {
    let stranger = ChatId {
        kind: PeerKind::User,
        id: 99,
    };
    let mock = Mock::with(vec![
        Ok(peer_dialogs_reply(ForumStatus::NotForum)),
        Ok(peer_dialogs_reply(ForumStatus::Forum)),
    ]);
    let calls = setup(&mock).await;
    let before = calls
        .peer_dialogs(&[GROUP, ANN, stranger, GROUP])
        .await
        .unwrap();
    let after = calls.peer_dialogs(&[GROUP]).await.unwrap();

    let ids: Vec<ChatId> = before.iter().map(|c| c.id).collect();
    assert_eq!(ids, vec![GROUP, ANN]);
    let group = &before[0];
    assert_eq!(group.read.unread, 4);
    assert_eq!(group.last_id, Some(MsgId(30)));
    assert_eq!(
        group.kind,
        ChatKind::Supergroup {
            forum: false,
            gigagroup: false
        }
    );
    // The forum flag changed between the calls.
    assert_eq!(
        after[0].kind,
        ChatKind::Supergroup {
            forum: true,
            gigagroup: false
        }
    );

    let request =
        tl::functions::messages::GetPeerDialogs::from_boxed_bytes(&mock.requests()[0]).unwrap();
    let asked: Vec<i64> = request
        .peers
        .iter()
        .map(|p| match p {
            tl::enums::InputDialogPeer::Peer(p) => match &p.peer {
                tl::enums::InputPeer::Channel(c) => c.channel_id,
                tl::enums::InputPeer::User(u) => u.user_id,
                _ => panic!("unexpected peer"),
            },
            _ => panic!("not a peer"),
        })
        .collect();
    // Deduplicated, and the stranger (no access hash) is left out.
    assert_eq!(asked, vec![FORUM, 7]);
}

#[tokio::test]
async fn peer_dialogs_without_known_peers_or_over_the_bound_send_nothing() {
    let mock = Mock::with(vec![]);
    let calls = setup(&mock).await;
    let stranger = ChatId {
        kind: PeerKind::User,
        id: 99,
    };
    assert_eq!(calls.peer_dialogs(&[stranger]).await, Ok(Vec::new()));
    let many = vec![GROUP; MAX_PEER_DIALOGS + 1];
    assert_eq!(
        calls.peer_dialogs(&many).await,
        Err(BackendError::Rejected(RejectReason::Other))
    );
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn oversized_peer_dialogs_are_rejected() {
    let mut reply =
        tl::enums::messages::PeerDialogs::from_bytes(&peer_dialogs_reply(ForumStatus::Forum))
            .unwrap();
    let tl::enums::messages::PeerDialogs::Dialogs(r) = &mut reply;
    r.dialogs = (1..=40)
        .map(|id| tl::enums::Dialog::Dialog(dialog(peer_user(id), 1)))
        .collect();
    let mock = Mock::with(vec![Ok(reply.to_bytes())]);
    let calls = setup(&mock).await;
    assert_eq!(
        calls.peer_dialogs(&[GROUP]).await,
        Err(BackendError::Rejected(RejectReason::Other))
    );
}

// --- logging ---

#[tokio::test(start_paused = true)]
async fn forum_logs_never_carry_titles_or_text() {
    let buf = LogBuf::default();
    let _capture = capture_logs(&buf);

    let page = forum_topics(
        TopicOrder::LastActivity,
        3,
        vec![
            listed(forum_topic(5, "secret title", 50)),
            listed(forum_topic(0, "secret malformed", 51)),
            deleted(6),
        ],
        vec![post(50, Some(5), "secret text")],
    );
    let mock = Mock::with(vec![
        Ok(page.to_bytes()),
        Ok(page.to_bytes()),
        Ok(peer_dialogs_reply(ForumStatus::Forum)),
        rpc(400, "TOPIC_CLOSED"),
    ]);
    let calls = setup(&mock).await;
    calls.topics(GROUP, None, 10).await.unwrap();
    calls.topics_by_id(GROUP, &[topic_id(5)]).await.unwrap();
    calls.peer_dialogs(&[GROUP]).await.unwrap();
    let _ = calls
        .send_text(thread(Some(5)), "secret outgoing".into(), 1)
        .await;
    let edit = tl::enums::Update::EditForumTopic(edit_topic(0));
    assert!(lists::update(&edit, &Directory::default()).is_none());

    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    for event in [
        "topic entries skipped",
        "telegram call failed",
        "TOPIC_CLOSED",
    ] {
        assert!(out.contains(event), "{event:?} is logged: {out}");
    }
    for needle in ["secret", "Re named", "Renamed"] {
        assert!(!out.contains(needle), "{needle:?} leaked into: {out}");
    }
}
