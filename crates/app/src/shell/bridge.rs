//! Core ↔ UI bridge (plan section 1, "Bridge to GPUI").
//!
//! Core's handle is split: the views send commands through [`SharedCore`]
//! (the command half), and the pump owns the event half. One foreground task
//! awaits the next core event, drains up to [`DRAIN_MAX`] queued events
//! without waiting, and applies the batch in one entity update, so a burst
//! costs one re-render instead of hundreds.

use std::cell::RefCell;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{self, Poll};

use abyss_core::api::{Command, Event};
use abyss_core::{CoreEvents, CoreSender, SendError, ShutdownReport};
use gpui::{AsyncWindowContext, Context, Task, WeakEntity, Window};

/// Events applied per wake.
pub(crate) const DRAIN_MAX: usize = 256;

/// Resolves with core's shutdown report, or `None` if core ended without one.
pub(crate) type ShutdownAck = Pin<Box<dyn Future<Output = Option<ShutdownReport>>>>;

/// The command half of core. [`CoreSender`] in the app; a same-thread double
/// in tests, because GPUI's test scheduler rejects wakeups from core's own
/// threads as non-determinism.
pub(crate) trait CoreLink {
    /// Queues a command without waiting.
    fn send(&self, cmd: Command) -> Result<(), SendError>;
    /// Asks core to stop; `None` if already asked.
    fn begin_shutdown(&mut self) -> Option<ShutdownAck>;
    /// The system woke from sleep; core rebuilds its client. Never blocks.
    fn notify_wake(&self) {}
}

/// The event half of core, owned by the pump.
pub(crate) trait EventSource {
    /// The next event; `Ready(None)` once core has stopped.
    fn poll_recv(&mut self, task: &mut task::Context<'_>) -> Poll<Option<Event>>;
    /// An already queued event, without waiting.
    fn try_recv(&mut self) -> Option<Event>;
    /// The stamp of the event just taken, even when a batch spans a logout.
    fn account_epoch(&self) -> u64 {
        0
    }
}

impl CoreLink for CoreSender {
    fn send(&self, cmd: Command) -> Result<(), SendError> {
        CoreSender::send(self, cmd)
    }

    fn begin_shutdown(&mut self) -> Option<ShutdownAck> {
        let done = CoreSender::begin_shutdown(self)?;
        Some(Box::pin(async move { done.await.ok() }))
    }

    fn notify_wake(&self) {
        CoreSender::notify_wake(self);
    }
}

impl EventSource for CoreEvents {
    fn poll_recv(&mut self, task: &mut task::Context<'_>) -> Poll<Option<Event>> {
        CoreEvents::poll_recv(self, task)
    }

    fn try_recv(&mut self) -> Option<Event> {
        CoreEvents::try_recv(self)
    }

    fn account_epoch(&self) -> u64 {
        CoreEvents::account_epoch(self)
    }
}

/// The UI's command link to core, shared by the views that send commands. It
/// lives on the GPUI main thread only and is never borrowed across an await.
pub(crate) type SharedCore = Rc<RefCell<Box<dyn CoreLink>>>;

/// Receives batches of events on the main thread.
pub(crate) trait EventSink: 'static + Sized {
    fn apply(&mut self, events: Vec<(u64, Event)>, window: &mut Window, cx: &mut Context<'_, Self>);
    /// Core's event stream ended.
    fn core_stopped(&mut self, window: &mut Window, cx: &mut Context<'_, Self>);
}

/// The first event plus whatever else is already queued, up to [`DRAIN_MAX`].
pub(crate) fn drain(events: &mut dyn EventSource, first: Event) -> Vec<(u64, Event)> {
    // Most wakes carry one or two events; the batch grows up to DRAIN_MAX.
    let mut batch = Vec::with_capacity(16);
    batch.push((events.account_epoch(), first));
    while batch.len() < DRAIN_MAX {
        match events.try_recv() {
            Some(event) => batch.push((events.account_epoch(), event)),
            None => break,
        }
    }
    batch
}

/// Starts the pump, which owns the event half. It ends when core's stream
/// ends or the sink is released.
pub(crate) fn spawn_pump<T: EventSink>(
    mut events: Box<dyn EventSource>,
    window: &Window,
    cx: &Context<'_, T>,
) -> Task<()> {
    cx.spawn_in(
        window,
        async move |this: WeakEntity<T>, cx: &mut AsyncWindowContext| {
            loop {
                let Some(first) = poll_fn(|task| events.poll_recv(task)).await else {
                    this.update_in(cx, T::core_stopped).ok();
                    return;
                };
                let batch = drain(events.as_mut(), first);
                if this
                    .update_in(cx, |sink, window, cx| sink.apply(batch, window, cx))
                    .is_err()
                {
                    return;
                }
            }
        },
    )
}
