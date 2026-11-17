//! Bootstrap (plan section 11): topic edits and new topic messages arrive
//! before, during and after the topics snapshot and the open topic's by-id
//! snapshot. Each is stamped when received: one older than a snapshot is in
//! it, one crossing it is kept live and refetched, one after it is live.

use std::time::Duration;

use super::*;
use crate::backend::fake::{Fault, Op};
use crate::store::TopicEdit;

async fn rename(h: &Harness, f: ChatId, t: i32, title: &str) {
    let edit = TopicEdit {
        title: Some(title.to_owned()),
        ..TopicEdit::default()
    };
    h.fake.edit_topic(f, tid(t), edit).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn updates_around_the_bootstrap_topic_snapshots() {
    let (mut h, f) = opened_big(80).await;
    open_topic(&mut h, f, 5).await;
    events_for(&mut h, Duration::from_secs(2)).await;
    let pages = page_calls(&h, f);
    let by_id = by_id_calls(&h).len();

    let delay = Duration::from_secs(3);
    h.fake.inject(Op::Topics, Fault::Delay(delay));
    h.fake.inject(Op::TopicsById, Fault::Delay(delay));
    h.fake.runner_exit();
    // Before the snapshots: in them.
    rename(&h, f, 70, "before").await;
    rename(&h, f, 5, "before").await;
    let mut seen = Vec::new();
    calls_reached(&mut h, pages + 1, is_page).await;
    calls_reached(&mut h, by_id + 1, is_by_id).await;
    // During both (they were taken already): kept live, then refetched.
    rename(&h, f, 70, "during").await;
    rename(&h, f, 5, "during").await;
    let during = h.fake.incoming(topic_key(f, 71), "during").await.unwrap();
    seen.extend(events_for(&mut h, Duration::from_secs(10)).await);
    // After them: live.
    rename(&h, f, 72, "after").await;
    let after = h.fake.incoming(topic_key(f, 73), "after").await.unwrap();
    seen.extend(events_for(&mut h, Duration::from_secs(10)).await);

    let last = |t: i32| states_of(&seen, tid(t)).pop().unwrap();
    assert_eq!(last(70).title, "during");
    assert_eq!(last(5).title, "during");
    assert_eq!(last(71).last_id, Some(during.id));
    assert_eq!(last(72).title, "after");
    assert_eq!(last(73).last_id, Some(after.id));
    for t in [70, 5, 71, 72, 73] {
        assert_eq!(last(t).freshness, Freshness::Current, "topic {t} settled");
    }
    // The crossing snapshots never showed an older title after a newer one.
    for t in [70, 5] {
        let titles: Vec<String> = states_of(&seen, tid(t))
            .into_iter()
            .map(|s| s.title)
            .collect();
        let first_during = titles.iter().position(|s| s == "during");
        assert!(
            first_during.is_some_and(|i| titles[i..].iter().all(|s| s == "during")),
            "topic {t}: {titles:?}"
        );
    }
    h.stop().await;
}
