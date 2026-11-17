//! Deletions as count invalidations (plan sections 6.3 and 11, "Deletion
//! counts"): deleting an unread message changes the server's counts and no
//! read update may follow, so a deletion in a forum makes the affected
//! topics due (every listed one when no window held the id) and refreshes
//! the forum's badge. Both settle to the server's values.

use std::time::Duration;

use super::*;
use crate::backend::Update;

/// Deletes `ids` on the server and delivers the deletion.
async fn delete(h: &Harness, f: ChatId, ids: &[i32]) {
    let history = h
        .fake
        .server_history(f)
        .into_iter()
        .filter(|m| !ids.contains(&m.id.0))
        .collect();
    h.fake.set_history(f, history);
    h.fake
        .push_update(Update::Deleted {
            chat: Some(f),
            ids: ids.iter().map(|&id| MsgId(id)).collect(),
        })
        .await;
}

/// The unread counts the events report for topic `t`, in order.
fn counts(events: &[Event], t: TopicId) -> Vec<u32> {
    states_of(events, t).iter().map(|s| s.unread).collect()
}

/// The forum's badge as the events report it, last first.
fn badge(events: &[Event], f: ChatId) -> Option<u32> {
    last_bump(events, f).map(|s| s.unread)
}

/// Forum 100: topics A (2) and B (3), with unread posts 10 and 11 in A
/// (11 its top message) and the open thread A; its list is shown.
async fn with_unread() -> (Harness, ChatId) {
    let (script, f) = forum_with([(10, 2), (11, 2), (12, 3)]);
    let mut h = online(script).await;
    open_forum(&mut h, f).await;
    h.send(Command::OpenThread(topic_key(f, 2))).await;
    reconciled(&mut h, topic_key(f, 2)).await;
    (h, f)
}

/// Deleting a non-top unread message that a window holds: its topic
/// becomes due and the badge refreshes; both settle to the server's values.
#[tokio::test(start_paused = true)]
async fn deleting_an_unread_message_a_window_holds() {
    let (mut h, f) = with_unread().await;
    let by_id = by_id_calls(&h).len();
    delete(&h, f, &[10]).await;
    let seen = events_for(&mut h, Duration::from_secs(10)).await;
    assert!(
        by_id_calls(&h)[by_id..]
            .iter()
            .any(|(ids, _)| ids.contains(&tid(2)))
    );
    assert_eq!(counts(&seen, tid(2)).last(), Some(&1), "{seen:?}");
    assert_eq!(peer_calls_with(&h, f).len(), 1, "the badge is refreshed");
    // Still two unread topics: the refresh changes nothing shown.
    assert!(badge(&seen, f).is_none_or(|n| n == 2), "{seen:?}");
    h.stop().await;
}

/// Deleting ids no window holds: every listed topic becomes due, zero
/// counts included, and the badge refreshes.
#[tokio::test(start_paused = true)]
async fn deleting_ids_no_window_holds() {
    let (mut h, f) = with_unread().await;
    let by_id = by_id_calls(&h).len();
    delete(&h, f, &[12, 9999]).await;
    let seen = events_for(&mut h, Duration::from_secs(10)).await;
    let asked: Vec<TopicId> = by_id_calls(&h)[by_id..]
        .iter()
        .flat_map(|(ids, _)| ids.clone())
        .collect();
    for t in [TopicId::GENERAL, tid(2), tid(3)] {
        assert!(asked.contains(&t), "{t:?} not refetched: {asked:?}");
    }
    assert_eq!(counts(&seen, tid(3)).last(), Some(&0));
    assert_eq!(badge(&seen, f), Some(1));
    h.stop().await;
}
