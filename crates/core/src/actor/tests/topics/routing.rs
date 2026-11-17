//! Live routing and edits (plan sections 6.3 and 11, "Live routing",
//! "Edits"): every live message routes by `route`, bumps its topic forward
//! only, and is an unread trigger unless its tail is on screen; counts stay
//! the server's until a refetch. Edits touch only the top message's preview.

use std::time::Duration;

use super::*;
use crate::backend::Update;
use crate::backend::fake::text_message;
use crate::rich_text::RichText;

fn bumps(events: &[Event], t: TopicId) -> Vec<Topic> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::TopicBumped { topic, .. } if topic.id == t => Some(topic.clone()),
            _ => None,
        })
        .collect()
}

/// A message into a known topic bumps it (`NewSince`, due) without
/// touching its count; the refetch then sets the server's count.
#[tokio::test(start_paused = true)]
async fn into_a_known_topic() {
    let (script, f) = forum_with([]);
    let mut h = online(script).await;
    open_forum(&mut h, f).await;
    let m = h.fake.incoming(topic_key(f, 2), "new").await.unwrap();
    let seen = events_for(&mut h, Duration::from_secs(5)).await;
    let states = states_of(&seen, tid(2));
    let first = states.first().unwrap();
    assert_eq!(first.last_id, Some(m.id));
    assert_eq!((first.unread, first.freshness), (0, Freshness::NewSince));
    let last = states.last().unwrap();
    assert_eq!((last.unread, last.freshness), (1, Freshness::Current));
    h.stop().await;
}

/// A forum message without a topic header goes to General; a topic's
/// creation message is discovered, fetched by id and appended.
#[tokio::test(start_paused = true)]
async fn into_general_and_into_a_new_topic() {
    let (script, f) = forum_with([]);
    let mut h = online(script).await;
    open_forum(&mut h, f).await;
    let m = h
        .fake
        .incoming(ThreadKey::whole(f), "headerless")
        .await
        .unwrap();
    let seen = events_for(&mut h, Duration::from_secs(2)).await;
    assert!(
        bumps(&seen, TopicId::GENERAL)
            .iter()
            .any(|t| t.last_id == Some(m.id))
    );
    let topic = h.fake.create_topic(f, "Fresh").await.unwrap();
    h.until(|e| matches!(e, Event::TopicsAppended { topics, .. } if topics.iter().any(|t| t.id == topic)))
        .await;
    h.stop().await;
}

/// A message for a channel core knows nothing about (not listed, no view)
/// routes `ChatOnly`: it shows up nowhere.
#[tokio::test(start_paused = true)]
async fn for_a_chat_of_unknown_status() {
    let (script, _) = forum_with([]);
    let mut h = online(script).await;
    events_for(&mut h, Duration::from_secs(1)).await;
    let stranger = forum(4242);
    let mut m = text_message(stranger, 7, "who?");
    m.thread.topic = Some(tid(3));
    h.fake.push_update(Update::NewMessage(m)).await;
    let seen = events_for(&mut h, Duration::from_secs(5)).await;
    assert!(seen.is_empty(), "{seen:?}");
    assert!(peer_calls(&h).is_empty(), "nothing to fence or refresh");
    h.stop().await;
}

/// `TopicsPinned` refetches the shown forum's first page.
#[tokio::test(start_paused = true)]
async fn a_pin_refetches_the_first_page() {
    let (script, f) = forum_with([]);
    let mut h = online(script).await;
    open_forum(&mut h, f).await;
    let pages = page_calls(&h, f);
    h.fake.pin_topic(f, tid(2)).await.unwrap();
    let seen = events_for(&mut h, Duration::from_secs(5)).await;
    assert_eq!(page_calls(&h, f), pages + 1);
    assert!(states_of(&seen, tid(2)).last().is_some_and(|t| t.pinned));
    h.stop().await;
}

/// An edit of an old message changes neither count nor preview nor order;
/// an edit of the top message updates the preview; a stale edit (older
/// version) is ignored.
#[tokio::test(start_paused = true)]
async fn edits_touch_only_the_top_preview() {
    let (script, f) = forum_with([(10, 2), (11, 2)]);
    let mut h = online(script).await;
    open_forum(&mut h, f).await;
    h.drain();
    let edit = |id: i32, text: &str, at: i64| {
        let mut m = message_in(topic_key(f, 2), id, text);
        m.body = crate::model::Body::Text(RichText::plain(text));
        m.edit_date = Some(at);
        Update::Edited(m)
    };
    h.fake.push_update(edit(10, "old one, edited", 100)).await;
    let seen = events_for(&mut h, Duration::from_secs(3)).await;
    assert!(bumps(&seen, tid(2)).is_empty(), "{seen:?}");
    // The server holds the edit too (the refetch it causes agrees).
    let Update::Edited(top) = edit(11, "top, edited", 200) else {
        unreachable!()
    };
    let mut history = h.fake.server_history(f);
    for m in history.iter_mut().filter(|m| m.id == top.id) {
        *m = top.clone();
    }
    h.fake.set_history(f, history);
    h.fake.push_update(Update::Edited(top)).await;
    h.fake.push_update(edit(11, "top, stale", 150)).await;
    let seen = events_for(&mut h, Duration::from_secs(3)).await;
    let bumped = bumps(&seen, tid(2));
    assert!(!bumped.is_empty());
    for topic in &bumped {
        let preview = format!("{:?}", topic.last);
        assert!(preview.contains("top, edited"), "{preview}");
        assert_eq!(topic.unread, 2, "the count is untouched");
    }
    h.stop().await;
}
