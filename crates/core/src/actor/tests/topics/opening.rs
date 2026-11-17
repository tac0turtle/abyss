//! Opening a forum: paging to the end and to the cap, stale pages, LRU
//! eviction, the instant reset on reopen, both orders, an order switch
//! between pages, and the duplicate-page stop.

use std::time::Duration;

use super::*;
use crate::backend::fake::{Fault, Op};

/// First page, then the rest; reopening shows the retained list at once,
/// before the server answers.
#[tokio::test(start_paused = true)]
async fn opening_a_forum_pages_and_reopens_instantly() {
    let f = forum(100);
    let mut h = online(demo_forums(&[(f, TopicOrder::LastActivity)])).await;
    let first = open_forum(&mut h, f).await;
    assert_eq!(first.len(), 50);
    let pinned = first.iter().take_while(|t| t.pinned).count();
    assert_eq!(pinned, 2, "pinned topics lead");
    h.send(Command::LoadTopics(f)).await;
    let (_, seen) = h
        .until(|e| {
            matches!(
                e,
                Event::TopicsPaging {
                    has_more: false,
                    ..
                }
            )
        })
        .await;
    let added: usize = seen
        .iter()
        .map(|e| match e {
            Event::TopicsAppended { topics, .. } => topics.len(),
            _ => 0,
        })
        .sum();
    assert_eq!(added, 11, "General and 60 topics in all");
    h.send(Command::LoadTopics(f)).await;
    let ended = topics_ended(&mut h, f).await;
    assert!(matches!(ended, Event::LoadEnded { error: None, .. }));
    assert_eq!(page_calls(&h, f), 2, "every page is loaded");

    h.send(Command::CloseForum(f)).await;
    h.fake.inject(Op::Topics, Fault::Hang);
    h.send(Command::OpenForum(f)).await;
    let (_, _, retained) = reset(&mut h, f).await;
    assert_eq!(retained.len(), 61, "the retained list, at once");
    assert!(!paging(&mut h, f).await);
    h.stop().await;
}

/// Paging stops at `MAX_TOPICS_PER_FORUM`.
#[tokio::test(start_paused = true)]
async fn paging_stops_at_the_cap() {
    let f = forum(100);
    let mut script = FakeScript::demo(1, 1);
    add_big_forum(&mut script, f, 520);
    let mut h = online(script).await;
    let mut shown = open_forum(&mut h, f).await.len();
    let mut pages = 1;
    loop {
        h.send(Command::LoadTopics(f)).await;
        let (ev, seen) = h
            .until(|e| matches!(e, Event::TopicsPaging { .. } | Event::LoadEnded { .. }))
            .await;
        for e in &seen {
            match e {
                Event::TopicsAppended { topics, .. } => shown += topics.len(),
                Event::TopicRemoved { .. } => shown -= 1,
                Event::TopicsReset { topics, .. } => shown = topics.len(),
                _ => {}
            }
        }
        pages += 1;
        if matches!(
            ev,
            Event::TopicsPaging {
                has_more: false,
                ..
            }
        ) {
            break;
        }
        assert!(pages < 20, "paging ends");
    }
    assert_eq!(shown, crate::store::MAX_TOPICS_PER_FORUM);
    assert_eq!(page_calls(&h, f), 10, "no page past the cap");
    h.send(Command::LoadTopics(f)).await;
    let ended = topics_ended(&mut h, f).await;
    assert!(matches!(ended, Event::LoadEnded { error: None, .. }));
    assert_eq!(page_calls(&h, f), 10);
    h.stop().await;
}

/// A page whose list was closed (and reopened under a new generation) is
/// dropped on arrival: nothing of it reaches the UI.
#[tokio::test(start_paused = true)]
async fn a_stale_page_after_close_is_dropped() {
    let f = forum(100);
    let mut h = online(demo_forums(&[(f, TopicOrder::LastActivity)])).await;
    h.fake
        .inject(Op::Topics, Fault::Delay(Duration::from_secs(5)));
    h.send(Command::OpenForum(f)).await;
    h.send(Command::CloseForum(f)).await;
    let seen = events_for(&mut h, Duration::from_secs(10)).await;
    assert_eq!(page_calls(&h, f), 1, "the delayed page completed");
    assert!(
        !seen.iter().any(|e| matches!(
            e,
            Event::TopicsReset { .. } | Event::TopicsAppended { .. } | Event::TopicsPaging { .. }
        )),
        "{seen:?}"
    );

    // Reopened while an old page is still in flight: only the new view's
    // page is applied.
    h.fake
        .inject(Op::Topics, Fault::Delay(Duration::from_secs(5)));
    h.send(Command::OpenForum(f)).await;
    run_for(Duration::from_millis(10)).await;
    let (old, _, _) = reset(&mut h, f).await; // the retained (empty) list
    h.send(Command::OpenForum(f)).await;
    let (new, _, retained) = reset(&mut h, f).await;
    assert!(new > old && retained.is_empty());
    let (view, _, topics) = reset(&mut h, f).await;
    assert_eq!(view, new);
    assert_eq!(topics.len(), 50);
    let seen = events_for(&mut h, Duration::from_secs(10)).await;
    assert!(
        !seen.iter().any(|e| matches!(
            e,
            Event::TopicsReset { view, .. }
                | Event::TopicsAppended { view, .. }
                | Event::TopicsPaging { view, .. } if *view != new
        )),
        "{seen:?}"
    );
    assert_eq!(page_calls(&h, f), 3);
    h.stop().await;
}

/// At most `MAX_TOPIC_LISTS` lists are retained; the least recently used
/// goes first.
#[tokio::test(start_paused = true)]
async fn lists_are_evicted_least_recently_used_first() {
    let forums: Vec<ChatId> = (100..107).map(forum).collect();
    let script = demo_forums(
        &forums
            .iter()
            .map(|f| (*f, TopicOrder::LastActivity))
            .collect::<Vec<_>>(),
    );
    let mut h = online(script).await;
    for f in &forums {
        open_forum(&mut h, *f).await;
    }
    let retained_at_once = |events: &[Event], f: ChatId| {
        events
            .iter()
            .any(|e| matches!(e, Event::TopicsReset { chat, .. } if *chat == f))
    };
    // Forum 101 is retained (forum 100 was evicted by the seventh).
    h.fake.inject(Op::Topics, Fault::Hang);
    h.send(Command::OpenForum(forums[1])).await;
    let seen = events_for(&mut h, Duration::from_millis(50)).await;
    assert!(retained_at_once(&seen, forums[1]));
    h.fake.inject(Op::Topics, Fault::Hang);
    h.send(Command::OpenForum(forums[0])).await;
    let seen = events_for(&mut h, Duration::from_millis(50)).await;
    assert!(!retained_at_once(&seen, forums[0]), "{seen:?}");
    h.stop().await;
}

/// Both orders; a forum whose order changes between pages discards the
/// later page and gets its first page again, in the new order.
#[tokio::test(start_paused = true)]
async fn both_orders_and_a_switch_between_pages() {
    let f = forum(100);
    let mut h = online(demo_forums(&[(f, TopicOrder::Created)])).await;
    h.send(Command::OpenForum(f)).await;
    let (_, order, topics) = reset(&mut h, f).await;
    assert_eq!(order, TopicOrder::Created);
    let unpinned: Vec<i64> = topics
        .iter()
        .filter(|t| !t.pinned)
        .map(|t| t.created)
        .collect();
    assert!(
        unpinned.windows(2).all(|w| w[0] >= w[1]),
        "newest created first"
    );
    paging(&mut h, f).await;

    h.fake.set_topic_order(f, TopicOrder::LastActivity);
    h.send(Command::LoadTopics(f)).await;
    let (_, order, topics) = reset(&mut h, f).await;
    assert_eq!(order, TopicOrder::LastActivity, "a fresh first page");
    assert_eq!(topics.len(), 50);
    let firsts = h
        .fake
        .topics_calls()
        .iter()
        .filter(|r| matches!(r.request, TopicsRequest::Page { first: true, .. }))
        .count();
    assert_eq!(firsts, 2, "the discarded page's first page, once");
    h.stop().await;
}

/// A non-empty page that adds no topic ends paging, whatever the server's
/// count says (task 4 review): no redundant requests.
#[tokio::test(start_paused = true)]
async fn a_repeated_page_stops_paging() {
    let f = forum(100);
    let mut h = online(demo_forums(&[(f, TopicOrder::LastActivity)])).await;
    open_forum(&mut h, f).await;
    h.fake.repeat_topic_pages(f);
    h.send(Command::LoadTopics(f)).await;
    assert!(!paging(&mut h, f).await, "duplicates only: paging stops");
    h.send(Command::LoadTopics(f)).await;
    let ended = topics_ended(&mut h, f).await;
    assert!(matches!(ended, Event::LoadEnded { error: None, .. }));
    assert_eq!(page_calls(&h, f), 2, "no request after the repeated page");
    h.stop().await;
}
