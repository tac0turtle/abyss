//! Glue between the session and the chat shell (`crate::chat`): routes chat
//! events in, lends the shell an admission function so its UI changes follow
//! only admitted commands, wires window focus and system wake, checks what
//! the message list showed after each painted frame, and wakes the shell
//! when a refused or failed command's backoff ends (`chat::retry`).

use std::time::Duration;

use abyss_core::SendError;
use abyss_core::api::{Command, ConnState, Event, LoadTarget, SendState, UserError, WindowState};
use abyss_core::model::{ChatId, LocalId, ThreadKey};
use gpui::{App, Context, Focusable, Subscription, WeakEntity, Window};

use super::Root;
use super::auth::AuthView;
use super::bridge::SharedCore;
use super::session::Session;
use crate::chat::copy as chat_copy;
use crate::chat::{ChatShell, Refused, format};
use crate::ui::copy;

/// How often a flood-wait countdown redraws.
const COUNTDOWN_TICK: Duration = Duration::from_secs(1);

/// What to do when core refuses a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OnRefusal {
    /// The user asked for it: say so.
    Notice,
    /// Driven by frames or rendering, and asked again on the next one. A
    /// notice would redraw, and the redraw would ask again at frame rate.
    Quiet,
}

/// Queues `cmd` with core without waiting.
pub(super) fn admit(core: &SharedCore, cmd: Command) -> Result<(), Refused> {
    let core = core.try_borrow().map_err(|_| Refused::Busy)?;
    match core.send(cmd) {
        Ok(()) => Ok(()),
        // The rejected command is dropped here (a password in it is
        // zeroized on drop).
        Err(SendError::Full(_)) => Err(Refused::Busy),
        Err(SendError::Closed(_)) => Err(Refused::Closed),
    }
}

/// Subscriptions that feed the chat shell: window focus and system wake.
pub(super) fn wire(window: &mut Window, cx: &mut Context<'_, Root>) -> [Subscription; 2] {
    let activation = cx.observe_window_activation(window, |root, window, cx| {
        root.with_session(|s, cx| s.on_activation(window, cx), cx);
    });
    let this = cx.weak_entity();
    let wake = cx.on_system_wake(move |cx| {
        this.update(cx, |root, cx| {
            root.with_session(|s, _| s.notify_wake(), cx);
        })
        .ok();
    });
    [activation, wake]
}

/// Called from the message pane's paint: once this frame is drawn, check
/// what the message list shows. Runs at the end of the frame's effect
/// cycle, after layout and paint, so it reads the layout on screen.
pub(super) fn after_paint(root: WeakEntity<Root>, window: &mut Window, cx: &mut App) {
    window.defer(cx, move |window, cx| {
        root.update(cx, |root, cx| {
            root.with_session(|s, cx| s.check_frame(window, cx), cx);
        })
        .ok();
    });
}

impl Session {
    /// Runs `f` on the chat shell with an admission function, then reports
    /// a refusal as `on_refusal` says.
    fn drive<R>(
        &mut self,
        on_refusal: OnRefusal,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
        f: impl FnOnce(&mut ChatShell, crate::chat::Admit<'_>) -> Result<R, Refused>,
    ) -> Option<R> {
        let offered = self.shell.retry_offered();
        let core = &self.core;
        // Core stopped (task 6l): nothing is asked of it.
        let stopped = self.shell.is_stopped();
        let mut admit = |cmd: Command| {
            if stopped {
                return Err(Refused::Closed);
            }
            admit(core, cmd)
        };
        let out = match f(&mut self.shell, &mut admit) {
            Ok(r) => Some(r),
            Err(refused) => {
                tracing::debug!(?refused, "core refused a chat command");
                if on_refusal == OnRefusal::Notice {
                    self.show_refused(refused, window, cx);
                }
                None
            }
        };
        // Retries that just gave up (or started over) change the Retry
        // pills. A quiet refusal redraws nothing else, and the last one
        // often comes from the after-paint check, so redraw once here.
        if self.shell.retry_offered() != offered {
            cx.notify();
        }
        self.schedule_retry(window, cx);
        out
    }

    /// Wakes the shell once when the earliest waiting retry is due (a
    /// refused focus change, dialogs page, frame command or a failed load),
    /// unless a wake-up no later is already scheduled. The shell owns the
    /// one timer task: a new wake-up drops (cancels) the one it replaces.
    fn schedule_retry(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        let now = cx.background_executor().now();
        let Some(due) = self.shell.retry_due(now) else {
            return;
        };
        let Some(id) = self.shell.schedule_retry(due) else {
            return;
        };
        let wait = due.saturating_duration_since(now);
        let task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(wait).await;
            this.update_in(cx, |root, window, cx| {
                root.with_session(|s, cx| s.retry_fired(id, window, cx), cx);
            })
            .ok();
        });
        self.shell.arm_retry(id, task);
    }

    /// Wake-up `id` fired. Unless it is stale, re-sends what is due
    /// (focus, dialogs) and redraws, so the frame check retries
    /// frame-driven commands once their backoff allows.
    fn retry_fired(&mut self, id: u64, window: &mut Window, cx: &mut Context<'_, Root>) {
        if !self.shell.retry_fired(id) {
            return;
        }
        let now = cx.background_executor().now();
        self.drive(OnRefusal::Quiet, window, cx, |s, admit| {
            s.retry_wakeup(now, admit)
        });
        cx.notify();
    }

    /// The user pressed Retry after automatic retries gave up (history or
    /// dialogs): everything waiting may ask again now.
    pub(super) fn retry_loads(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        self.shell.retry_loads();
        let now = cx.background_executor().now();
        self.drive(OnRefusal::Notice, window, cx, |s, admit| {
            s.request_dialogs(now, admit)
        });
        // Redraw, so the frame check asks for history.
        cx.notify();
    }

    pub(super) fn show_refused(
        &mut self,
        refused: Refused,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        let text = match refused {
            Refused::Busy => copy::user_error(UserError::Busy),
            Refused::Closed => copy::CORE_GONE.into(),
        };
        self.show_notice(text, window, cx);
    }

    pub(super) fn apply_chat_event(
        &mut self,
        event: Event,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        let flood = matches!(
            event,
            Event::SendState {
                state: SendState::FloodWait { .. },
                ..
            }
        );
        let online = matches!(event, Event::Connection(ConnState::Online));
        let paged = online
            || matches!(
                event,
                Event::DialogsPaging { .. }
                    | Event::LoadEnded {
                        target: LoadTarget::Dialogs,
                        ..
                    }
            );
        let now = cx.background_executor().now();
        if let Event::LoadEnded { target, error } = event {
            self.shell.load_ended(target, error, now);
        } else {
            self.shell.apply(event);
        }
        if flood {
            self.start_countdown(window, cx);
        }
        // The sidebar may want the next page whatever it renders: an empty
        // list renders no row to ask (a first page of broadcast channels
        // only). A refusal retries after a backoff.
        if paged {
            self.drive(OnRefusal::Quiet, window, cx, |s, admit| {
                s.request_dialogs(now, admit)
            });
        }
        if online {
            // Back online (a fresh trigger): redraw, so the frame check asks
            // for what gave up meanwhile.
            cx.notify();
        }
        self.schedule_retry(window, cx);
    }

    /// After a painted frame: load more, mark read, report the live tail.
    pub(super) fn check_frame(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        let seen = self.shell.seen();
        let now = cx.background_executor().now();
        self.drive(OnRefusal::Quiet, window, cx, |s, admit| {
            s.frame_seen(seen, now, admit)
        });
    }

    fn on_activation(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        let state = if window.is_window_active() {
            WindowState::Active
        } else {
            WindowState::Inactive
        };
        let now = cx.background_executor().now();
        self.drive(OnRefusal::Quiet, window, cx, |s, admit| {
            s.set_focus(state, now, admit)
        });
        // Redraw, so the frame check sees the active window.
        cx.notify();
    }

    /// The Mac woke from sleep: core rebuilds rather than trust old sockets.
    fn notify_wake(&self) {
        tracing::info!("system woke");
        if let Ok(core) = self.core.try_borrow() {
            core.notify_wake();
        }
    }

    pub(super) fn select_chat(
        &mut self,
        chat: ChatId,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        // A chat row opens the whole chat; a forum shows a placeholder
        // until its topic list arrives (stage 2, task 10).
        let thread = ThreadKey::whole(chat);
        let from = self.shell.selected();
        if from != Some(thread) {
            let text = self.composer.read(cx).text().to_owned();
            if !self.shell.can_switch(thread, &text) {
                // Never drop a draft (task 6l): the chat and its text stay.
                self.show_notice(chat_copy::TOO_MANY_DRAFTS.into(), window, cx);
                return;
            }
        }
        let opened = self.drive(OnRefusal::Notice, window, cx, |s, admit| {
            s.select(thread, admit)
        });
        if opened.is_some() && from != Some(thread) {
            self.swap_draft(from, thread, window, cx);
        }
        let composer = self
            .shell
            .selected_chat()
            .is_some_and(super::chat_view::shows_composer);
        if opened.is_some() && composer && self.auth == AuthView::SignedIn {
            window.focus(&self.composer.focus_handle(cx), cx);
        }
        cx.notify();
    }

    /// Enter in the composer. The text leaves the composer only once core
    /// admitted the send, together with its echo.
    pub(super) fn submit_message(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if self.shell.is_stopped() {
            self.show_notice(chat_copy::CORE_STOPPED_COPY.into(), window, cx);
            return;
        }
        if self.auth != AuthView::SignedIn {
            return;
        }
        let thread = match self.shell.send_target() {
            Ok(Some(thread)) => thread,
            Ok(None) => return,
            Err(error) => {
                self.show_notice(copy::user_error(error), window, cx);
                return;
            }
        };
        let text = self.composer.read(cx).text().to_owned();
        let sent = self.drive(OnRefusal::Notice, window, cx, |s, admit| {
            s.submit(thread, &text, format::now(), admit)
        });
        if let Some(Some(_)) = sent {
            self.composer
                .update(cx, |c, cx| drop(c.take_text(window, cx)));
        }
        cx.notify();
    }

    pub(super) fn retry_send(
        &mut self,
        local_id: LocalId,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        self.drive(OnRefusal::Notice, window, cx, |s, admit| {
            s.retry(local_id, admit)
        });
        cx.notify();
    }

    pub(super) fn discard_send(
        &mut self,
        local_id: LocalId,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        self.drive(OnRefusal::Notice, window, cx, |s, admit| {
            s.discard(local_id, admit)
        });
        cx.notify();
    }

    /// The open thread changed from `from` to `to` (task 6k): the box's text
    /// stays `from`'s draft, and `to`'s draft comes back. Transactional
    /// (task 6l): `to`'s draft leaves first, freeing its slot, and
    /// `select_chat` checked there is room, so no draft is dropped.
    fn swap_draft(
        &mut self,
        from: Option<ThreadKey>,
        to: ThreadKey,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        let incoming = self.shell.take_draft(to);
        let text = self.composer.update(cx, |c, cx| c.take_text(window, cx));
        let kept = from.map_or(Ok(()), |from| self.shell.stash_draft(from, text));
        debug_assert!(kept.is_ok(), "room was checked before the switch");
        // Checked above; should it fail, the text stays in the box rather
        // than be lost (the incoming draft then stays stashed).
        let restore = match (kept, incoming) {
            (Err(text), incoming) => {
                if let Some(draft) = incoming {
                    drop(self.shell.stash_draft(to, draft));
                }
                text
            }
            (Ok(()), incoming) => incoming.unwrap_or_default(),
        };
        if !restore.is_empty() {
            let placed = self
                .composer
                .update(cx, |c, cx| c.put_text(&restore, window, cx));
            debug_assert!(placed, "the box was emptied above");
        }
    }

    /// Puts a refused send's text back into the message box, which must be
    /// empty (nothing typed there is ever overwritten), and only while the
    /// send's own thread is open: the box belongs to it.
    pub(super) fn put_back(
        &mut self,
        local_id: LocalId,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        let Some((thread, text)) = self
            .shell
            .returned(local_id)
            .map(|(thread, text)| (thread, text.to_owned()))
        else {
            return;
        };
        if self.shell.selected() != Some(thread) {
            self.show_notice(chat_copy::OTHER_CHAT.into(), window, cx);
            return;
        }
        let placed = self
            .composer
            .update(cx, |c, cx| c.put_text(&text, window, cx));
        if placed {
            drop(self.shell.take_returned(local_id));
            self.show_notice(chat_copy::PUT_BACK_DONE.into(), window, cx);
            window.focus(&self.composer.focus_handle(cx), cx);
        } else {
            self.show_notice(chat_copy::BOX_NOT_EMPTY.into(), window, cx);
        }
        cx.notify();
    }

    /// Copies a stranded send's text (core stopped before confirming it).
    pub(super) fn copy_stranded(
        &mut self,
        local_id: LocalId,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        let Some(text) = self.shell.stranded_text(local_id).map(str::to_owned) else {
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.show_notice(chat_copy::TEXT_COPIED.into(), window, cx);
    }

    /// Core's event stream closed while it ran: pending echoes are
    /// stranded with their text (task 6k).
    pub(super) fn core_stopped(&mut self, cx: &mut Context<'_, Root>) {
        self.shell.core_stopped();
        cx.notify();
    }

    /// Drops a refused or stranded send's text (the user chose to).
    pub(super) fn dismiss_returned(&mut self, local_id: LocalId, cx: &mut Context<'_, Root>) {
        drop(self.shell.take_returned(local_id));
        cx.notify();
    }

    pub(super) fn jump_to_latest(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        self.drive(OnRefusal::Notice, window, cx, |s, admit| {
            s.jump_to_latest(admit)
        });
        cx.notify();
    }

    /// Chat list row `last_visible` was rendered: maybe load the next page.
    /// Called during render, so the request goes out after it.
    pub(super) fn dialogs_seen(
        &mut self,
        last_visible: usize,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        let now = cx.background_executor().now();
        if !self.shell.dialogs_seen(last_visible, now) {
            return;
        }
        cx.defer_in(window, move |root, window, cx| {
            root.with_session(
                |s, cx| {
                    s.drive(OnRefusal::Quiet, window, cx, |s, admit| {
                        s.request_dialogs(now, admit)
                    })
                },
                cx,
            );
        });
    }

    /// Redraws flood-wait countdowns every second while one is shown.
    fn start_countdown(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if !self.shell.start_countdown() {
            return; // already ticking
        }
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(COUNTDOWN_TICK).await;
                let more = this.update(cx, |root, cx| {
                    let more = root.with_session(|s, _| s.shell.countdown_tick(), cx);
                    cx.notify();
                    more
                });
                if more.ok().flatten() != Some(true) {
                    return;
                }
            }
        })
        .detach();
    }
}
