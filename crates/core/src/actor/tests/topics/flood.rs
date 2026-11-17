//! By-id flood wait (plan section 11, round 5): a 60 s `FLOOD_WAIT` on a
//! by-id call while topics are due, then a forum switch and back, a light
//! refresh and a rebuild. Nothing automatic is dispatched before the
//! deadline, user opens are answered `FloodWait` at once, and the drain
//! resumes after it.

use std::time::Duration;

use super::*;
use crate::api::{UserError, WindowState};
use crate::backend::BackendError;
use crate::backend::fake::{Fault, Op};
use crate::store::TopicEdit;

/// The `FloodWait` seconds a `LoadEnded` of `chat`'s topics carries.
async fn flood_answer(h: &mut Harness, chat: ChatId) -> u32 {
    match topics_ended(h, chat).await {
        Event::LoadEnded {
            error: Some(UserError::FloodWait { seconds }),
            ..
        } => seconds,
        other => panic!("not a flood wait: {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn a_by_id_flood_wait_holds_every_automatic_call() {
    let (a, b) = (forum(100), forum(101));
    let mut script = demo_forums(&[(b, TopicOrder::LastActivity)]);
    add_big_forum(&mut script, a, 80);
    let mut h = online(script).await;
    open_forum(&mut h, b).await;
    open_forum(&mut h, a).await;
    open_topic(&mut h, a, 5).await;
    events_for(&mut h, Duration::from_secs(2)).await;

    h.fake.inject(
        Op::TopicsById,
        Fault::Error(BackendError::FloodWait { seconds: 60 }),
    );
    let edit = TopicEdit {
        title: Some("renamed".to_owned()),
        ..TopicEdit::default()
    };
    h.fake.edit_topic(a, tid(5), edit).await.unwrap();
    calls_reached(&mut h, 2, is_by_id).await;
    // The fake answers the wait at once: the deadline counts from the call.
    let flood_at = by_id_calls(&h).last().unwrap().1;
    let calls = h.fake.topics_calls().len();

    // A switch to another forum and back: the retained lists show, and each
    // open is answered at once.
    h.send(Command::OpenForum(b)).await;
    reset(&mut h, b).await;
    assert!((55..=60).contains(&flood_answer(&mut h, b).await));
    h.send(Command::OpenForum(a)).await;
    reset(&mut h, a).await;
    assert!((55..=60).contains(&flood_answer(&mut h, a).await));
    h.send(Command::LoadTopics(a)).await;
    flood_answer(&mut h, a).await;

    // A light refresh, then a rebuild.
    events_for(&mut h, Duration::from_secs(16)).await;
    h.send(Command::Window(WindowState::Active)).await;
    events_for(&mut h, Duration::from_secs(2)).await;
    h.fake.runner_exit();
    back_online(&mut h).await;
    let until = flood_at + Duration::from_secs(60);
    let left = until - tokio::time::Instant::now();
    events_for(&mut h, left.saturating_sub(Duration::from_millis(100))).await;
    assert_eq!(
        h.fake.topics_calls().len(),
        calls,
        "no topics call before the deadline"
    );

    // After it: the first page (the rebuild dropped the list) and the open
    // topic by id.
    events_for(&mut h, Duration::from_secs(5)).await;
    let after = &h.fake.topics_calls()[calls..];
    assert!(after.iter().all(|r| r.at >= until));
    assert!(
        after
            .iter()
            .any(|r| matches!(r.request, TopicsRequest::Page { chat, first: true } if chat == a))
    );
    assert!(
        after.iter().any(
            |r| matches!(&r.request, TopicsRequest::ById { ids, .. } if ids.contains(&tid(5)))
        )
    );
    h.stop().await;
}
