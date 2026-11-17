//! Draining (plan section 11): 80 due topics beyond page one of the shown
//! forum, while live messages keep reordering the list and the open topic
//! keeps becoming due, with a discovery overflow in the middle.

use std::time::Duration;

use super::*;
use crate::backend::Update;
use crate::store::{TOPICS_CALL_INTERVAL, TopicEdit};

/// `ceil(500 / 19)`: the drain's bound for any due topic (plan section 6.4).
const MAX_CALLS: usize = 27;

/// A forum of 200 topics, every page loaded.
async fn fully_loaded() -> (Harness, ChatId) {
    let (mut h, f) = opened_big(200).await;
    loop {
        h.send(Command::LoadTopics(f)).await;
        if !paging(&mut h, f).await {
            return (h, f);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn draining_due_topics_beyond_page_one() {
    let (mut h, f) = fully_loaded().await;
    let open = tid(100);
    h.send(Command::OpenThread(topic_key(f, 100))).await;
    events_for(&mut h, Duration::from_secs(2)).await;
    let start = by_id_calls(&h).len();

    // 80 topics beyond page one become due (a live rename each).
    let due: Vec<TopicId> = (2..=81).map(tid).collect();
    for t in &due {
        let edit = TopicEdit {
            title: Some(format!("renamed {}", t.get())),
            ..TopicEdit::default()
        };
        h.fake.edit_topic(f, *t, edit).await.unwrap();
    }
    // Live messages keep reordering the list and the open topic keeps
    // becoming due; halfway, 70 unknown topics overflow discovery.
    for round in 0..60 {
        h.fake.incoming(topic_key(f, 100), "open").await.unwrap();
        let other = 150 + (round * 7) % 50;
        h.fake.incoming(topic_key(f, other), "bump").await.unwrap();
        if round == 10 {
            for i in 0..70 {
                let mut m = message_in(topic_key(f, 5000 + i), 20_000 + i, "unknown");
                m.date = NOW;
                h.fake.push_update(Update::NewMessage(m)).await;
            }
        }
        events_for(&mut h, Duration::from_millis(500)).await;
    }

    let firsts = h
        .fake
        .topics_calls()
        .iter()
        .filter(|r| matches!(r.request, TopicsRequest::Page { first: true, .. }))
        .count();
    assert_eq!(firsts, 2, "the overflow refetched the first page once");
    let calls = &by_id_calls(&h)[start..];
    for t in &due {
        let first = calls.iter().position(|(ids, _)| ids.contains(t));
        assert!(
            first.is_some_and(|i| i < MAX_CALLS),
            "topic {} served at call {first:?}",
            t.get()
        );
    }
    for pair in calls.windows(2) {
        assert!(
            pair[1].1 - pair[0].1 >= TOPICS_CALL_INTERVAL,
            "calls too close"
        );
    }
    let with_open: Vec<_> = calls
        .iter()
        .filter(|(ids, _)| ids.contains(&open))
        .collect();
    assert!(with_open.len() > 3, "the open topic keeps being refetched");
    assert!(
        with_open.iter().all(|(ids, _)| ids[0] == open),
        "in its reserved first slot"
    );
    assert!(
        calls
            .iter()
            .all(|(ids, _)| ids.len() <= crate::store::MAX_TOPICS_BY_ID)
    );
    h.stop().await;
}
