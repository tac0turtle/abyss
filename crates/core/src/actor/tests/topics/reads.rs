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

#[tokio::test(start_paused = true)]
async fn reading_general_leaves_unread_sibling_topics_alone() {
    let f = forum(100);
    let mut script = FakeScript::demo(1, 1);
    add_unread_forum(&mut script, f);
    script.history.entry(f).or_default().extend([
        message_in(topic_key(f, 1), 11, "first General post"),
        message_in(topic_key(f, 1), 12, "last General post"),
    ]);
    let mut h = online(script).await;
    open_forum(&mut h, f).await;
    h.send(Command::OpenThread(topic_key(f, 1))).await;
    h.until(|e| matches!(e, Event::Reconciled { thread, .. } if *thread == topic_key(f, 1)))
        .await;

    mark_read(&h, topic_key(f, 1), 12).await;
    let events = events_for(&mut h, Duration::from_secs(5)).await;
    assert_eq!(h.fake.read_calls(), [(topic_key(f, 1), MsgId(12))]);
    let general = states_of(&events, TopicId::GENERAL);
    assert_eq!(
        general.last().map(|t| (t.unread, t.freshness)),
        Some((0, Freshness::Current)),
        "{events:?}"
    );
    assert_eq!(last_bump(&events, f).map(|c| c.read.unread), Some(1));
    h.stop().await;
}

/// The dialog's raw count can disagree with the per-topic counts after
/// reading General. A complete topic refresh must win for display, and a
/// sibling that is still unread must keep the parent badge on.
#[tokio::test(start_paused = true)]
async fn the_parent_badge_uses_confirmed_topics_instead_of_a_stale_dialog_total() {
    for unread_sibling in [false, true] {
        let f = forum(100);
        let mut script = FakeScript::demo(1, 1);
        add_unread_forum(&mut script, f);
        if !unread_sibling {
            script.topics.get_mut(&f).unwrap().iter_mut().for_each(|s| {
                s.topic.read_inbox_max = MsgId(10);
            });
        }
        let mut h = online(script).await;
        open_forum(&mut h, f).await;
        h.fake.inject(Op::PeerDialogs, Fault::Hang);
        h.fake
            .push_update(Update::ReadInbox {
                chat: f,
                topic: None,
                still_unread: 37,
                max_id: MsgId(0),
            })
            .await;
        let (event, _) = h
            .until(|e| matches!(e, Event::ChatBumped(c) if c.id == f && c.read.unread == 37 && c.topic_unread.is_some()))
            .await;
        let Event::ChatBumped(summary) = event else {
            unreachable!();
        };
        assert_eq!(summary.displayed_unread(), u32::from(unread_sibling));
        h.stop().await;
    }
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
