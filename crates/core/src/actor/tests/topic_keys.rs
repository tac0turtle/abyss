//! Stage 2 task 7: messages are normalized by `route` before they reach a
//! window, the cache or an event. Chat 1 is a private chat, so a message
//! the server tagged with a topic is `ForumSuspected` there: it only bumps
//! the chat. Every window of the chat loses cache continuity, because its
//! cache may now miss a message; only the thread's next tail restores it.

use std::time::Duration;

use super::{CacheDir, Harness, cache_at, chat, harness, open_settled};
use crate::api::{Command, Event, HistoryDir};
use crate::backend::Update;
use crate::backend::fake::{FakeScript, Fault, Op, text_message};
use crate::cache::{CacheHandle, CacheKey};
use crate::model::{Message, MsgId, ThreadKey, TopicId};

/// Message `id` of chat 1, tagged with a forum topic it cannot have.
fn tagged(id: i32) -> Message {
    let mut m = text_message(chat(1), id, "tagged");
    m.thread.topic = TopicId::try_from(7).ok();
    m
}

async fn cached(cache: &CacheHandle, id: i32) -> Option<Message> {
    cache.barrier().await.unwrap();
    cache
        .tail(ThreadKey::whole(chat(1)), 200)
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.id == MsgId(id))
}

async fn settled() -> (Harness, CacheHandle, CacheDir) {
    let dir = CacheDir::new();
    let key = CacheKey::generate().unwrap();
    let cache = cache_at(dir.path(), &key);
    let mut h = harness(FakeScript::demo(1, 10), Some(cache.clone())).await;
    open_settled(&mut h, &cache).await;
    // Taken here, so a later wait for `Reconciled` sees a later tail's.
    let whole = ThreadKey::whole(chat(1));
    h.until(|e| matches!(e, Event::Reconciled { thread, .. } if *thread == whole))
        .await;
    (h, cache, dir)
}

/// A live message of chat 1 that reached its window.
async fn delivered(h: &mut Harness, m: Message) -> Vec<Event> {
    let id = m.id;
    h.fake.push_update(Update::NewMessage(m.into())).await;
    let whole = ThreadKey::whole(chat(1));
    let (ev, skipped) = h
        .until(|e| matches!(e, Event::History { thread, dir: HistoryDir::Newer, .. } if *thread == whole))
        .await;
    let Event::History { messages, .. } = &ev else {
        unreachable!()
    };
    assert_eq!(messages.iter().map(|m| m.id).collect::<Vec<_>>(), [id]);
    skipped
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_topic_tagged_message_of_a_non_forum_chat_bypasses_its_window() {
    let (mut h, cache, _dir) = settled().await;
    h.fake
        .push_update(Update::NewMessage(tagged(11).into()))
        .await;
    h.until(|e| matches!(e, Event::ChatBumped(s) if s.last_id == Some(MsgId(11))))
        .await;
    h.fake.push_update(Update::Edited(tagged(10))).await;
    // The next plain message reaches the window: nothing before it did.
    let skipped = delivered(&mut h, text_message(chat(1), 12, "plain")).await;
    assert!(
        !skipped
            .iter()
            .any(|e| matches!(e, Event::History { .. } | Event::Edited(_))),
        "the tagged message and its edit stayed out: {skipped:?}"
    );
    assert_eq!(cached(&cache, 11).await, None, "not cached");
    assert_eq!(
        cached(&cache, 12).await,
        None,
        "the window lost continuity: its cache may miss 11"
    );

    // Only the thread's next tail restores continuity.
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h.until(
        |e| matches!(e, Event::Reconciled { thread, .. } if *thread == ThreadKey::whole(chat(1))),
    )
    .await;
    delivered(&mut h, text_message(chat(1), 13, "after the tail")).await;
    assert!(cached(&cache, 13).await.is_some(), "written through again");
    h.stop().await;
    cache.close().await.unwrap();
}

/// A tail requested before the window lost continuity must not restore it
/// (review of task 7, P1). 1..=10 are cached; a reopened tail is requested
/// (its snapshot ends at 10) and held by the server; tagged 11..=19 then
/// bypass the window. The late tail still answers the view, but neither
/// writes the cache nor restores continuity, so a later headerless 20 is
/// not written beside 10.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tail_requested_before_a_loss_of_continuity_does_not_restore_it() {
    let (mut h, cache, _dir) = settled().await;
    let whole = ThreadKey::whole(chat(1));
    h.fake
        .inject(Op::History, Fault::Delay(Duration::from_millis(500)));
    let calls = h.fake.calls(Op::History);
    h.send(Command::OpenThread(whole)).await;
    while h.fake.calls(Op::History) == calls {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for id in 11..=19 {
        h.fake
            .push_update(Update::NewMessage(tagged(id).into()))
            .await;
    }
    h.until(|e| matches!(e, Event::Reconciled { thread, .. } if *thread == whole))
        .await;
    delivered(&mut h, text_message(chat(1), 20, "after the late tail")).await;
    assert_eq!(cached(&cache, 20).await, None, "continuity stayed lost");
    let rows = cache.tail(whole, 200).await.unwrap();
    assert_eq!(rows.last().map(|m| m.id), Some(MsgId(10)));
    h.stop().await;
    cache.close().await.unwrap();
}
