//! Topic read marks (plan section 6.8): coalesced per thread, and only a
//! read that landed refetches the topic and refreshes the forum's badge.
//! Reads on another device are triggers too.

use std::time::Duration;

use super::*;
use crate::backend::fake::{Fault, Op};
use crate::backend::{BackendError, Update};

async fn mark_read(h: &Harness, thread: ThreadKey, max_id: i32) {
    h.send(Command::MarkRead {
        thread,
        max_id: MsgId(max_id),
    })
    .await;
}

/// A topic read fails transiently, then times out, then lands only after
/// a long delay: the intent is retried with backoff without a second
/// `MarkRead`. Until it lands nothing is refetched and the badge keeps the
/// server's count; once it lands the topic's pointer rises to it, the topic
/// shows `ReadSince`, is refetched by id, and the badge is refreshed to the
/// count after the read. Remote reads of each kind make their topic due
/// (`Stale`). Marks of two threads in one flush are two reads.
#[tokio::test(start_paused = true)]
async fn a_topic_read_triggers_only_once_it_lands() {
    let f = forum(100);
    let mut script = FakeScript::demo(1, 1);
    add_unread_forum(&mut script, f);
    let mut h = online(script).await;
    open_forum(&mut h, f).await;
    let (peers, by_id) = (peer_calls_with(&h, f).len(), by_id_calls(&h).len());
    h.fake
        .inject(Op::Read, Fault::Error(BackendError::Transient));
    h.fake.inject(Op::Read, Fault::Hang);
    h.fake
        .inject(Op::Read, Fault::Delay(Duration::from_secs(10)));
    for id in [8, 10, 9] {
        mark_read(&h, topic_key(f, 2), id).await;
    }
    // Fails at 0.5 s, times out at about 32 s, lands at about 44 s.
    let early = events_for(&mut h, Duration::from_secs(40)).await;
    assert_eq!(h.fake.calls(Op::Read), 3, "retried with backoff");
    assert_eq!(peer_calls_with(&h, f).len(), peers, "{early:?}");
    assert_eq!(by_id_calls(&h).len(), by_id);
    assert!(states_of(&early, tid(2)).is_empty(), "{early:?}");
    assert!(last_bump(&early, f).is_none(), "{early:?}");

    let late = events_for(&mut h, Duration::from_secs(10)).await;
    assert_eq!(h.fake.read_calls(), vec![(topic_key(f, 2), MsgId(10))]);
    let states = states_of(&late, tid(2));
    assert_eq!(
        states.first().map(|t| (t.freshness, t.read_inbox_max)),
        Some((Freshness::ReadSince, MsgId(10))),
        "the pointer rises with the read"
    );
    assert_eq!(
        states.last().map(|t| (t.unread, t.freshness)),
        Some((0, Freshness::Current)),
        "{states:?}"
    );
    assert!(
        by_id_calls(&h)[by_id..]
            .iter()
            .any(|(ids, _)| ids.contains(&tid(2)))
    );
    assert_eq!(peer_calls_with(&h, f).len(), peers + 1);
    assert_eq!(last_bump(&late, f).map(|c| c.read.unread), Some(0));

    h.fake
        .push_update(Update::ReadInbox {
            chat: f,
            topic: Some(TopicId::GENERAL),
            still_unread: 0,
            max_id: MsgId(0),
        })
        .await;
    h.fake
        .read_remotely(topic_key(f, 2), MsgId(10))
        .await
        .unwrap();
    let remote = events_for(&mut h, Duration::from_secs(5)).await;
    for t in [TopicId::GENERAL, tid(2)] {
        let states = states_of(&remote, t);
        assert!(
            states.iter().any(|s| s.freshness == Freshness::Stale),
            "{t:?}: {states:?}"
        );
    }
    assert!(peer_calls_with(&h, f).len() > peers + 1);

    mark_read(&h, topic_key(f, 1), 1).await;
    mark_read(&h, topic_key(f, 2), 10).await;
    events_for(&mut h, Duration::from_secs(1)).await;
    let reads = h.fake.read_calls().split_off(1);
    assert_eq!(reads.len(), 2, "{reads:?}");
    for read in [(topic_key(f, 1), MsgId(1)), (topic_key(f, 2), MsgId(10))] {
        assert!(reads.contains(&read), "{reads:?}");
    }
    h.stop().await;
}
