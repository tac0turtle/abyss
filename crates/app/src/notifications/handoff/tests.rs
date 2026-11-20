use futures::executor::block_on;

use super::*;
use crate::notifications::Payload;

fn payload(chat: i64) -> Payload {
    Payload {
        epoch: 1,
        chat,
        topic: 0,
    }
}

fn clicked(event: &Result<SystemEvent, RecvError>) -> Option<i64> {
    match event {
        Ok(SystemEvent::Click(Some(payload))) => Some(payload.chat),
        _ => None,
    }
}

/// Room for the delivery answers the cap on outstanding requests allows, and
/// the enumeration, the authorization reply and a settings read.
const _: () = assert!(EVENT_CAPACITY >= MAX_SUBMITTED + 3);

/// Review A53: the main thread never waits for a click's room, and a full
/// queue drops the oldest click, never the latest.
#[test]
fn a_full_click_queue_never_blocks_and_keeps_the_latest() {
    let (_, clicks, events) = channels();
    let sent = CLICK_CAPACITY + 3;
    for chat in 1..=sent {
        clicks.push(Some(payload(i64::try_from(chat).unwrap_or_default())));
    }
    let mut seen = Vec::new();
    while let Ok(event) = events.clicks.try_recv() {
        seen.extend(clicked(&Ok(event)));
    }
    let expected: Vec<_> = (sent - CLICK_CAPACITY + 1..=sent)
        .map(|chat| i64::try_from(chat).unwrap_or_default())
        .collect();
    assert_eq!(seen, expected, "bounded, oldest dropped, latest kept");
}

/// Review A53: a burst of delivery answers that fills the event queue cannot
/// drop a click, and the click is served before the backlog.
#[test]
fn a_full_event_queue_cannot_starve_a_click() {
    let (tx, clicks, events) = channels();
    for _ in 0..EVENT_CAPACITY {
        assert!(tx.try_send(SystemEvent::Existing(Vec::new())).is_ok());
    }
    assert!(
        tx.try_send(SystemEvent::AuthorizationFinished).is_err(),
        "the event queue is full"
    );
    clicks.push(Some(payload(7)));
    assert_eq!(clicked(&block_on(events.recv())), Some(7), "served first");
    assert!(matches!(
        block_on(events.recv()),
        Ok(SystemEvent::Existing(_))
    ));
}

#[test]
fn the_app_end_ends_once_the_system_side_is_gone() {
    let (tx, clicks, events) = channels();
    drop((tx, clicks));
    assert!(block_on(events.recv()).is_err());
}
