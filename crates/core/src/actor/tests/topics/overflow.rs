//! Repeated overflow (plan section 11): discovery overflows in the shown
//! forum and in two retained lists, again and again across completed calls.
//! At most one automatic first page per cooldown plus one trailing, only
//! for the shown forum; a flood wait extends the cooldown.

use std::time::Duration;

use super::*;
use crate::backend::fake::{Fault, Op};
use crate::backend::{BackendError, Update};
use crate::store::{AUTO_REFRESH_COOLDOWN, MAX_DISCOVERY, MAX_TOPICS_BY_ID};

/// Messages into more topics `chat`'s list doesn't know than discovery
/// holds, even with a by-id call taking some of them.
async fn overflow(h: &Harness, chat: ChatId, round: i32) {
    let n = i32::try_from(MAX_DISCOVERY + MAX_TOPICS_BY_ID).unwrap() + 1;
    for i in 0..n {
        let topic = 1000 * (round + 1) + i;
        let mut m = message_in(topic_key(chat, topic), 100_000 * (round + 1) + i, "x");
        m.date = NOW;
        h.fake.push_update(Update::NewMessage(m)).await;
    }
}

/// First-page calls of `chat` since call record `from`, with their times.
fn first_pages(h: &Harness, chat: ChatId, from: usize) -> Vec<tokio::time::Instant> {
    h.fake
        .topics_calls()
        .iter()
        .skip(from)
        .filter(|r| matches!(r.request, TopicsRequest::Page { chat: c, first: true } if c == chat))
        .map(|r| r.at)
        .collect()
}

#[tokio::test(start_paused = true)]
async fn repeated_overflow_is_rate_limited() {
    let (a, b, c) = (forum(100), forum(101), forum(102));
    let script = demo_forums(&[
        (a, TopicOrder::LastActivity),
        (b, TopicOrder::LastActivity),
        (c, TopicOrder::Created),
    ]);
    let mut h = online(script).await;
    for f in [b, c, a] {
        open_forum(&mut h, f).await;
    }
    let from = h.fake.topics_calls().len();
    let t0 = tokio::time::Instant::now();
    // Overflow everywhere at 0, 5, 10, 40 and 45 s.
    let mut at = Duration::ZERO;
    for (round, when) in [0u64, 5, 10, 40, 45].into_iter().enumerate() {
        let when = Duration::from_secs(when);
        events_for(&mut h, when.saturating_sub(at)).await;
        at = when;
        let round = i32::try_from(round).unwrap();
        for f in [a, b, c] {
            overflow(&h, f, round).await;
        }
    }
    events_for(&mut h, Duration::from_secs(60)).await;
    let pages = first_pages(&h, a, from);
    for pair in pages.windows(2) {
        assert!(pair[1] - pair[0] >= AUTO_REFRESH_COOLDOWN, "{pages:?}");
    }
    // 0 s (at once), 30 s (the trailing one for 5 and 10 s), 60 s (the
    // trailing one for 40 and 45 s).
    let offsets: Vec<u64> = pages.iter().map(|t| (*t - t0).as_secs()).collect();
    assert_eq!(offsets, [0, 30, 60]);
    assert!(first_pages(&h, b, from).is_empty(), "not shown: no fetch");
    assert!(first_pages(&h, c, from).is_empty(), "not shown: no fetch");

    // A flood wait on the automatic page extends the cooldown (and stops
    // every automatic topics call) to its end.
    h.fake.inject(
        Op::Topics,
        Fault::Error(BackendError::FloodWait { seconds: 90 }),
    );
    overflow(&h, a, 10).await;
    events_for(&mut h, Duration::from_secs(1)).await;
    let flooded = h.fake.topics_calls().len();
    let flood_at = h.fake.topics_calls().last().unwrap().at;
    overflow(&h, a, 11).await;
    events_for(&mut h, Duration::from_secs(85)).await;
    assert_eq!(
        h.fake.topics_calls().len(),
        flooded,
        "nothing during the wait"
    );
    events_for(&mut h, Duration::from_secs(10)).await;
    let after = first_pages(&h, a, flooded);
    assert_eq!(after.len(), 1, "one trailing page after the wait");
    assert!(after[0] - flood_at >= Duration::from_secs(90));
    h.stop().await;
}
