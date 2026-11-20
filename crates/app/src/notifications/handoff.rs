//! How the system's callbacks reach the app. Two queues, so that neither can
//! starve the other (review A53):
//!
//! - **Events.** Completions that run on the system's own queues (the
//!   enumeration, delivery answers, settings, the authorization reply). They
//!   wait for room, and the queue is sized so that, with the cap on
//!   outstanding delivery requests, they rarely have to.
//! - **Clicks.** The delegate's response handler runs on the main thread,
//!   which must never block. A click has its own small queue that drops its
//!   oldest click to keep the latest, so a burst of delivery answers cannot
//!   drop a click.

use std::pin::pin;

use async_channel::{Receiver, RecvError, Sender, TrySendError};
use futures::future::{Either, select};

#[cfg(any(target_os = "macos", test))]
use super::Payload;
use super::{MAX_SUBMITTED, SystemEvent};

/// Room for every outstanding delivery answer, plus the enumeration, the
/// authorization reply and a few settings reads.
pub(crate) const EVENT_CAPACITY: usize = MAX_SUBMITTED + 16;
/// Clicks waiting for the app. The person clicks one banner at a time.
pub(crate) const CLICK_CAPACITY: usize = 8;

pub(crate) type SystemSender = Sender<SystemEvent>;

/// The app's end of both queues. Clicks are served first.
pub(crate) struct SystemEvents {
    events: Receiver<SystemEvent>,
    clicks: Receiver<SystemEvent>,
}

impl SystemEvents {
    /// The next event, a click before anything else. Errs once the system
    /// side is gone.
    pub(crate) async fn recv(&self) -> Result<SystemEvent, RecvError> {
        if let Ok(click) = self.clicks.try_recv() {
            return Ok(click);
        }
        let clicks = pin!(self.clicks.recv());
        let events = pin!(self.events.recv());
        match select(clicks, events).await {
            Either::Left((next, _)) | Either::Right((next, _)) => next,
        }
    }
}

/// The delegate's end of the click queue.
#[cfg(any(target_os = "macos", test))]
#[derive(Clone)]
pub(crate) struct ClickQueue {
    tx: Sender<SystemEvent>,
    /// Only to drop the oldest click when the queue is full.
    rx: Receiver<SystemEvent>,
}

#[cfg(any(target_os = "macos", test))]
impl ClickQueue {
    /// Queues a click without ever blocking. When the queue is full the
    /// oldest click goes: the latest is the one the person is waiting for.
    pub(crate) fn push(&self, payload: Option<Payload>) {
        let mut event = SystemEvent::Click(payload);
        // One drop frees one slot, and only this thread produces: two tries.
        for _ in 0..2 {
            match self.tx.try_send(event) {
                Ok(()) | Err(TrySendError::Closed(_)) => return,
                Err(TrySendError::Full(back)) => {
                    event = back;
                    drop(self.rx.try_recv());
                }
            }
        }
    }
}

/// The system side's senders and the app's receiver.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn channels() -> (SystemSender, ClickQueue, SystemEvents) {
    let (events_tx, events) = async_channel::bounded(EVENT_CAPACITY);
    let (clicks_tx, clicks) = async_channel::bounded(CLICK_CAPACITY);
    let queue = ClickQueue {
        tx: clicks_tx,
        rx: clicks.clone(),
    };
    (events_tx, queue, SystemEvents { events, clicks })
}

#[cfg(test)]
mod tests;
