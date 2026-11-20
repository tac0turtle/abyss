//! Confirmed topic counts survive process restarts, with version checks.

use super::super::{CacheDir, cache_at};
use super::*;
use crate::backend::Update;
use crate::backend::fake::{Fault, Op};
use crate::cache::{CacheCmd, CacheHandle, CacheKey};

async fn confirmed_badge(cache: &CacheHandle) -> ChatSummary {
    let f = forum(100);
    let mut script = FakeScript::demo(1, 1);
    add_big_forum(&mut script, f, 1);
    let mut h = harness(script, Some(cache.clone())).await;
    h.online().await;
    open_forum(&mut h, f).await;
    h.fake.inject(Op::PeerDialogs, Fault::Hang);
    h.fake
        .push_update(Update::ReadInbox {
            chat: f,
            topic: None,
            still_unread: 2,
            max_id: MsgId(0),
        })
        .await;
    h.until(|e| matches!(e, Event::ChatBumped(c) if c.id == f && c.read.unread == 2 && c.topic_unread == Some(0))).await;
    h.stop().await;
    let saved = cache
        .chats()
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.id == f)
        .unwrap();
    assert_eq!(saved.topic_unread, Some(0));
    saved
}

#[tokio::test]
async fn a_confirmed_topic_badge_survives_shutdown_and_restart() {
    let dir = CacheDir::new();
    let key = CacheKey::generate().unwrap();
    let cache = cache_at(dir.path(), &key);
    let f = forum(100);
    let saved = confirmed_badge(&cache).await;

    // This server keeps reporting its stale parent total. No topic list is
    // opened in the second process: the correction must come from disk.
    let mut script = FakeScript::demo(1, 1);
    script.dialogs.push(ChatSummary {
        topic_unread: None,
        ..saved
    });
    let mut h = harness(script, Some(cache.clone())).await;
    let events = h.online().await;
    let parent = events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::DialogsReset(chats) => chats.iter().find(|c| c.id == f),
            _ => None,
        })
        .unwrap();
    assert_eq!(parent.read.unread, 2);
    if parent.displayed_unread() != 0 {
        // A local cache read can finish after the server page. Its versioned
        // evidence must still correct that row, without opening the forum.
        h.until(|e| matches!(e, Event::ChatBumped(c) if c.id == f && c.displayed_unread() == 0))
            .await;
    }
    assert_eq!(
        cache
            .chats()
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.id == f)
            .unwrap()
            .displayed_unread(),
        0
    );
    assert!(h.fake.topics_calls().is_empty(), "no forum open needed");
    h.fake
        .push_update(Update::ReadInbox {
            chat: f,
            topic: Some(TopicId::GENERAL),
            still_unread: 0,
            max_id: MsgId(1),
        })
        .await;
    h.until(|e| matches!(e, Event::ChatBumped(c) if c.id == f && c.topic_unread.is_none()))
        .await;
    cache.barrier().await.unwrap();
    assert_eq!(
        cache
            .chats()
            .await
            .unwrap()
            .iter()
            .find(|c| c.id == f)
            .unwrap()
            .topic_unread,
        None,
        "a live topic event invalidates the cached evidence too"
    );
    h.stop().await;
    cache.close().await.unwrap();
}

#[tokio::test]
async fn a_new_dialog_version_rejects_the_cached_topic_count() {
    for changed in ["message", "read"] {
        let dir = CacheDir::new();
        let key = CacheKey::generate().unwrap();
        let cache = cache_at(dir.path(), &key);
        let f = forum(100);
        let mut saved = forum_summary(f);
        saved.read.unread = 2;
        saved.last_id = Some(MsgId(10));
        saved.topic_unread = Some(0);
        cache.write(CacheCmd::PutChats(vec![saved.clone()])).await;
        cache.barrier().await.unwrap();
        let mut fresh = ChatSummary {
            topic_unread: None,
            ..saved
        };
        match changed {
            "message" => fresh.last_id = Some(MsgId(11)),
            _ => fresh.read.unread = 3,
        }
        let mut script = FakeScript::demo(1, 1);
        script.dialogs.push(fresh);
        let mut h = harness(script, Some(cache.clone())).await;
        let events = h.online().await;
        let parent = events
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::DialogsReset(chats) => chats.iter().find(|c| c.id == f),
                _ => None,
            })
            .unwrap();
        assert_eq!(parent.topic_unread, None, "{changed}");
        assert!(parent.displayed_unread() > 0);
        h.stop().await;
        cache.close().await.unwrap();
    }
}

#[tokio::test]
async fn a_new_message_discards_a_cached_count_without_a_retained_topic_list() {
    let dir = CacheDir::new();
    let key = CacheKey::generate().unwrap();
    let cache = cache_at(dir.path(), &key);
    let f = forum(100);
    let mut saved = forum_summary(f);
    saved.read.unread = 2;
    saved.last_id = Some(MsgId(10));
    saved.topic_unread = Some(0);
    cache.write(CacheCmd::PutChats(vec![saved.clone()])).await;
    cache.barrier().await.unwrap();
    let mut script = FakeScript::demo(1, 1);
    script.dialogs.push(ChatSummary {
        topic_unread: None,
        ..saved
    });
    let mut h = harness(script, Some(cache.clone())).await;
    h.online().await;
    h.fake.inject(Op::PeerDialogs, Fault::Hang);
    h.fake
        .push_update(Update::NewMessage(
            message_in(topic_key(f, 2), 11, "new").into(),
        ))
        .await;
    let (event, _) = h
        .until(|e| matches!(e, Event::ChatBumped(c) if c.id == f && c.last_id == Some(MsgId(11))))
        .await;
    let Event::ChatBumped(parent) = event else {
        unreachable!()
    };
    assert_eq!(parent.topic_unread, None);
    assert!(parent.has_unread());
    cache.barrier().await.unwrap();
    let saved = cache
        .chats()
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.id == f)
        .unwrap();
    assert_eq!(saved.topic_unread, None);
    h.stop().await;
    cache.close().await.unwrap();
}
