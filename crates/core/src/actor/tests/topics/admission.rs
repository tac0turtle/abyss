//! Topics calls under admission pressure and panics (task 6 review): a
//! flood wait is honoured at admission, not only at dispatch, and a panic
//! ends a call with the same bookkeeping as any other outcome.

use std::time::Duration;

use super::*;
use crate::api::UserError;
use crate::backend::BackendError;
use crate::backend::fake::{Fault, Op};
use crate::model::LocalId;
use crate::store::{TOPICS_CALL_INTERVAL, TopicEdit};

/// Waits until `n` admission permits are free.
async fn permits_free(h: &mut Harness, n: usize) {
    for _ in 0..1000 {
        if h.permits.available_permits() == n {
            return;
        }
        events_for(h, Duration::from_millis(10)).await;
    }
    panic!("permits never reached {n}");
}

/// Every permit is taken, so a topics page waits in the queue; the by-id
/// call holding the last permit then returns a 60 s flood wait and frees
/// it. The queued page is not admitted until the wait ends, while other
/// calls still go through.
#[tokio::test(start_paused = true)]
async fn a_flood_wait_holds_a_queued_topics_job_at_admission() {
    let (mut h, f) = opened_big(80).await;
    let plain = ThreadKey::whole(super::super::chat(1));
    for i in 0..15 {
        h.fake
            .inject(Op::Send, Fault::Delay(Duration::from_secs(20)));
        h.send(Command::SendText {
            thread: plain,
            text: format!("send {i}"),
            local_id: LocalId(i),
        })
        .await;
    }
    permits_free(&mut h, 1).await;
    h.fake.lead(Op::TopicsById, Duration::from_secs(5));
    h.fake.inject(
        Op::TopicsById,
        Fault::Error(BackendError::FloodWait { seconds: 60 }),
    );
    let edit = TopicEdit {
        title: Some("renamed".to_owned()),
        ..TopicEdit::default()
    };
    h.fake.edit_topic(f, tid(70), edit).await.unwrap();
    calls_reached(&mut h, 1, is_by_id).await;
    permits_free(&mut h, 0).await;
    h.send(Command::LoadTopics(f)).await; // queued: no permit
    let pages = page_calls(&h, f);
    let until = by_id_calls(&h)[0].1 + Duration::from_secs(65);

    // The flood answer frees a permit at 5 s, the sends theirs at 20 s:
    // the page still waits, and a plain history load passes it.
    events_for(&mut h, Duration::from_secs(25)).await;
    assert_eq!(h.permits.available_permits(), crate::actor::MAX_CONCURRENT);
    h.send(Command::OpenThread(plain)).await;
    h.history(super::super::chat(1), crate::api::HistoryDir::Tail)
        .await;
    let left = until - tokio::time::Instant::now();
    events_for(&mut h, left.saturating_sub(Duration::from_millis(100))).await;
    assert_eq!(page_calls(&h, f), pages, "held until the wait ends");

    events_for(&mut h, Duration::from_secs(1)).await;
    assert_eq!(page_calls(&h, f), pages + 1, "admitted after it");
    let at = h.fake.topics_calls().last().unwrap().at;
    assert!(at >= until);
    h.stop().await;
}

/// Repeated by-id panics while an unlisted open topic is wanted: each panic
/// ends the call like any outcome (the next waits at least the call
/// interval) and the open topic backs off, so panics can't burn calls.
#[tokio::test(start_paused = true)]
async fn repeated_by_id_panics_keep_the_interval() {
    let (mut h, f) = opened_big(80).await;
    for _ in 0..6 {
        h.fake.inject(Op::TopicsById, Fault::Panic);
    }
    h.send(Command::OpenThread(topic_key(f, 5))).await;
    let seen = events_for(&mut h, Duration::from_secs(40)).await;
    let panics = seen
        .iter()
        .filter(|e| matches!(e, Event::Error(UserError::Internal)))
        .count();
    let calls = by_id_calls(&h);
    assert_eq!(panics, calls.len(), "every call so far panicked");
    // 0, 1, 3, 7, 15 and 31 s: the call interval, then the backoff.
    assert_eq!(calls.len(), 6, "{calls:?}");
    for pair in calls.windows(2) {
        assert!(pair[1].1 - pair[0].1 >= TOPICS_CALL_INTERVAL);
    }
    // The panics are over: the next call finds it.
    h.until(|e| matches!(e, Event::TopicsAppended { topics, .. } if topics.iter().any(|t| t.id == tid(5))))
        .await;
    assert_eq!(by_id_calls(&h).len(), 7);
    h.stop().await;
}

/// A page panic ends the UI's load and leaves paging usable.
#[tokio::test(start_paused = true)]
async fn a_page_panic_ends_the_load() {
    let (mut h, f) = opened_big(80).await;
    h.fake.inject(Op::Topics, Fault::Panic);
    h.send(Command::LoadTopics(f)).await;
    let ended = topics_ended(&mut h, f).await;
    assert!(matches!(
        ended,
        Event::LoadEnded {
            error: Some(UserError::Internal),
            ..
        }
    ));
    h.send(Command::LoadTopics(f)).await;
    assert!(!paging(&mut h, f).await, "the retry loads the last page");
    h.stop().await;
}
