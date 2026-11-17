//! Stage 2 task 1: keys are whole chats (`topic: None`) while message
//! payloads keep the raw topic the server stated (`apply::shown_in`). A
//! topic-tagged message of an open chat goes to that chat's window, its
//! events and its contiguous cache unchanged; one arriving while the
//! window is closed (retained, but no longer contiguous) is not written
//! through.

use super::{CacheDir, Harness, cache_at, chat, harness, open_settled};
use crate::api::{Command, Event, HistoryDir};
use crate::backend::Update;
use crate::backend::fake::{FakeScript, text_message};
use crate::cache::{CacheHandle, CacheKey};
use crate::model::{Message, MsgId, ThreadKey, TopicId};
use crate::rich_text::RichText;

fn topic() -> TopicId {
    TopicId::try_from(7).unwrap()
}

/// Message `id` of chat 1 as a forum server sends it: tagged with topic 7.
fn tagged(id: i32, text: &str) -> Message {
    let mut m = text_message(chat(1), id, text);
    m.thread.topic = Some(topic());
    m
}

fn edited(m: &Message, text: &str) -> Message {
    Message {
        edit_date: Some(m.date + 1),
        body: crate::model::Body::Text(RichText::plain(text)),
        ..m.clone()
    }
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
    // Two chats: the second is the closed-window test's command barrier.
    let mut h = harness(FakeScript::demo(2, 10), Some(cache.clone())).await;
    open_settled(&mut h, &cache).await;
    (h, cache, dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_topic_tagged_message_goes_to_its_whole_chat() {
    let (mut h, cache, _dir) = settled().await;
    let new = tagged(11, "in topic 7");
    h.fake.push_update(Update::NewMessage(new.clone())).await;
    let ev = h.history(chat(1), HistoryDir::Newer).await;
    let Event::History {
        thread, messages, ..
    } = &ev
    else {
        unreachable!()
    };
    assert_eq!(*thread, ThreadKey::whole(chat(1)), "the key has no topic");
    assert_eq!(messages, &vec![new.clone()], "the payload keeps its topic");

    let edit = edited(&new, "edited in topic 7");
    h.fake.push_update(Update::Edited(edit.clone())).await;
    let (ev, _) = h.until(|e| matches!(e, Event::Edited(_))).await;
    assert!(matches!(&ev, Event::Edited(m) if *m == edit));

    assert_eq!(
        cached(&cache, 11).await,
        Some(edit.clone()),
        "written through"
    );

    // Reopening shows the retained window first: it holds the edit.
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    let ev = h.history(chat(1), HistoryDir::Tail).await;
    let Event::History { messages, .. } = &ev else {
        unreachable!()
    };
    assert_eq!(messages.last(), Some(&edit));
    assert_eq!(messages.len(), 11);
    h.stop().await;
    cache.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_window_writes_no_topic_tagged_message_through() {
    let (mut h, cache, _dir) = settled().await;
    let before = cached(&cache, 10).await.expect("the tail was cached");
    h.send(Command::CloseThread(ThreadKey::whole(chat(1))))
        .await;
    // Commands and updates reach the actor on separate channels. Opening
    // chat 2 and waiting for its history proves the close was handled
    // before the update below is pushed; otherwise the message can land in
    // the still-open, contiguous window and be written through.
    h.send(Command::OpenThread(ThreadKey::whole(chat(2)))).await;
    h.history(chat(2), HistoryDir::Tail).await;

    h.fake
        .push_update(Update::NewMessage(tagged(11, "while closed")))
        .await;
    h.until(|e| matches!(e, Event::ChatBumped(s) if s.last_id == Some(MsgId(11))))
        .await;
    let edit = edited(&tagged(10, "x"), "edited while closed");
    h.fake.push_update(Update::Edited(edit.clone())).await;
    // The closed window is retained and still takes both (Stage 1).
    let (ev, _) = h.until(|e| matches!(e, Event::Edited(_))).await;
    assert!(matches!(&ev, Event::Edited(m) if *m == edit));

    assert_eq!(cached(&cache, 11).await, None, "not contiguous: no write");
    assert_eq!(
        cached(&cache, 10).await,
        Some(before),
        "the edit is not cached"
    );
    h.stop().await;
    cache.close().await.unwrap();
}
