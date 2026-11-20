//! Core → UI event channel, bounded both by count and by retained bytes.
//!
//! The sender reserves byte permits before queueing an event and the permit
//! travels with it; the receiver drops the permit as soon as it takes the event.
//! A slow UI therefore makes core wait instead of letting queued payloads grow
//! without limit.
//!
//! Each event also carries the account epoch it was sent in (review round 15,
//! C). The receiver records the epoch of the last event it handed out, and
//! the command half stamps every command with it: core rejects an
//! account-scoped command stamped with another epoch, so a command the UI
//! made for one account never reaches the next.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use crate::api::{Event, NotificationCleanup};

/// Maximum queued events.
pub const EVENT_CAPACITY: usize = 1024;
/// Maximum retained bytes across queued events.
pub const EVENT_BYTE_BUDGET: usize = 16 * 1024 * 1024;

/// Permits are counted in KiB so a single acquire stays within `u32`.
const PERMIT_UNIT: usize = 1024;
const TOTAL_PERMITS: usize = EVENT_BYTE_BUDGET / PERMIT_UNIT;

fn permits_for(event: &Event) -> u32 {
    let units = event
        .retained_bytes()
        .div_ceil(PERMIT_UNIT)
        .clamp(1, TOTAL_PERMITS);
    u32::try_from(units).unwrap_or(u32::MAX)
}

/// An event, the account epoch it was sent in, and the byte budget it holds
/// until the receiver takes it.
type Queued = (Event, u64, OwnedSemaphorePermit);

#[derive(Clone, Debug)]
pub struct EventSender {
    tx: mpsc::Sender<Queued>,
    budget: Arc<Semaphore>,
    /// The epoch stamped on events sent from now on.
    epoch: Arc<AtomicU64>,
    notification_cleanup: NotificationCleanupPolicy,
}

#[derive(Debug)]
pub struct EventReceiver {
    rx: mpsc::Receiver<Queued>,
    /// The epoch of the last event handed out.
    observed: Arc<AtomicU64>,
}

/// The account epoch the UI has seen: shared by the event receiver (which
/// advances it) and the command sender (which stamps commands with it).
#[derive(Clone, Debug, Default)]
pub struct EpochObserver(Arc<AtomicU64>);

impl EpochObserver {
    /// The epoch of the last event the UI received.
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

/// The receiver is gone; core is shutting down or the UI dropped its handle.
#[derive(Debug, PartialEq, Eq)]
pub struct Closed;

/// Whether the installation needs notification cleanup and can perform it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NotificationCleanupPolicy {
    #[default]
    Disabled,
    /// This installation may have old notifications, but cannot reach the center.
    Unavailable,
    /// The event consumer can confirm removal from the center.
    Required,
}

pub fn channel() -> (EventSender, EventReceiver) {
    channel_with_notifications(NotificationCleanupPolicy::Disabled)
}

pub(crate) fn channel_with_notifications(
    notification_cleanup: NotificationCleanupPolicy,
) -> (EventSender, EventReceiver) {
    let (tx, rx) = mpsc::channel(EVENT_CAPACITY);
    let budget = Arc::new(Semaphore::new(TOTAL_PERMITS));
    let sender = EventSender {
        tx,
        budget,
        epoch: Arc::new(AtomicU64::new(0)),
        notification_cleanup,
    };
    let receiver = EventReceiver {
        rx,
        observed: Arc::new(AtomicU64::new(0)),
    };
    (sender, receiver)
}

impl EventSender {
    pub(crate) fn has_notifications(&self) -> bool {
        self.notification_cleanup == NotificationCleanupPolicy::Required
    }

    pub(crate) fn requires_notification_cleanup(&self) -> bool {
        self.notification_cleanup != NotificationCleanupPolicy::Disabled
    }

    /// Returns true only when the consumer confirms the center is empty.
    pub(crate) async fn clear_notifications(&self) -> bool {
        if !self.has_notifications() {
            return false;
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let clearance = async {
            self.send(Event::ClearNotifications(NotificationCleanup(tx)))
                .await
                .ok()?;
            rx.await.ok()
        };
        matches!(
            tokio::time::timeout(std::time::Duration::from_secs(2), clearance).await,
            Ok(Some(()))
        )
    }

    /// Queues `event`, waiting for both a slot and byte budget. If the receiver
    /// is gone, the permit is dropped with the rejected event, returning its budget.
    pub async fn send(&self, event: Event) -> Result<(), Closed> {
        let permit = Arc::clone(&self.budget)
            .acquire_many_owned(permits_for(&event))
            .await
            .map_err(|_| Closed)?;
        let epoch = self.epoch.load(Ordering::Acquire);
        self.tx
            .send((event, epoch, permit))
            .await
            .map_err(|_| Closed)
    }

    /// The current account epoch.
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Starts a new account epoch: events sent from now on carry it.
    pub fn advance_epoch(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::AcqRel).wrapping_add(1)
    }
}

impl EventReceiver {
    /// Waits for the next event; `None` once core has stopped.
    pub async fn recv(&mut self) -> Option<Event> {
        let item = self.rx.recv().await;
        self.take(item)
    }

    /// Polls for the next event; `Ready(None)` once core has stopped.
    pub fn poll_recv(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Event>> {
        self.rx.poll_recv(cx).map(|item| self.take(item))
    }

    /// Takes an event if one is queued, without waiting.
    pub fn try_recv(&mut self) -> Option<Event> {
        let item = self.rx.try_recv().ok();
        self.take(item)
    }

    /// The epoch the UI has seen, for stamping its commands.
    pub fn observer(&self) -> EpochObserver {
        EpochObserver(Arc::clone(&self.observed))
    }

    /// Records the epoch of a received event and drops its byte permit.
    fn take(&self, item: Option<Queued>) -> Option<Event> {
        let (event, epoch, _permit) = item?;
        self.observed.store(epoch, Ordering::Release);
        Some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Body, ChatId, Message, MsgId, PeerKind, ThreadKey};
    use crate::rich_text::RichText;

    fn big_message() -> Event {
        let chat = ChatId {
            kind: PeerKind::User,
            id: 1,
        };
        Event::Edited(Message {
            id: MsgId(1),
            thread: ThreadKey { chat, topic: None },
            sender: None,
            date: 0,
            edit_date: None,
            outgoing: false,
            reply_to: None,
            body: Body::Text(RichText::plain(
                &"x".repeat(crate::rich_text::MAX_TEXT_BYTES),
            )),
        })
    }

    #[tokio::test(start_paused = true)]
    async fn byte_budget_blocks_sender_until_consumed() {
        let (tx, mut rx) = channel();
        let per = usize::try_from(permits_for(&big_message())).unwrap();
        let fits = TOTAL_PERMITS / per;
        for _ in 0..fits {
            tx.send(big_message()).await.unwrap();
        }
        let blocked =
            tokio::time::timeout(std::time::Duration::from_secs(1), tx.send(big_message())).await;
        assert!(
            blocked.is_err(),
            "sender must wait when the byte budget is spent"
        );
        assert!(rx.try_recv().is_some());
        tx.send(big_message()).await.unwrap();
    }

    #[tokio::test]
    async fn closed_receiver_returns_budget() {
        let (tx, rx) = channel();
        drop(rx);
        assert_eq!(tx.send(big_message()).await, Err(Closed));
        assert_eq!(tx.budget.available_permits(), TOTAL_PERMITS);
    }
}
