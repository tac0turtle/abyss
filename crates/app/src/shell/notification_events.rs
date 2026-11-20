//! Wire notification policy to the core event pump and the single window.

use std::time::Duration;

use abyss_core::api::{AuthState, Event};
use abyss_core::model::{ChatSummary, MsgId, ThreadKey, Topic};
use gpui::{App, Context, Window, WindowHandle};

use super::auth::AuthView;
use super::{Root, State};
use crate::chat::ChatShell;
use crate::notifications::{Notifications, Payload, SystemEvent, SystemEvents};

pub(super) fn wire(handle: WindowHandle<Root>, receiver: SystemEvents, cx: &App) {
    cx.spawn(async move |cx| {
        while let Ok(event) = receiver.recv().await {
            if handle
                .update(cx, |root, window, cx| {
                    root.on_system_event(event, window, cx);
                })
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
}

impl Root {
    pub(super) fn apply_notification_event(
        &mut self,
        epoch: u64,
        event: Event,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let State::Running(session) = &mut self.state else {
            return false;
        };
        match event {
            Event::Notify(notice) => {
                if let Some(center) = &mut self.notifications
                    && session.can_hide()
                {
                    // Covered by any modal overlay (Settings, the logout
                    // confirmation, the closing overlay), the open chat is
                    // not on screen: the frame observer's own notion
                    // (`Session::pane_covered`, review A64, N57).
                    let visible = (!session.pane_covered()
                        && window_is_key(window)
                        && session.auth == AuthView::SignedIn)
                        .then(|| session.shell.selected())
                        .flatten();
                    center.deliver(epoch, notice, visible, |thread, msg| {
                        has_read(&session.shell, thread, msg)
                    });
                }
                return false;
            }
            Event::ClearNotifications(completion) => {
                if let Some(center) = &mut self.notifications {
                    center.clear(Box::new(move || completion.complete()));
                }
                return true;
            }
            _ => {}
        }
        if let Some(notifications) = &mut self.notifications {
            observe(notifications, epoch, &event);
        }
        let rows_or_auth = matches!(
            event,
            Event::ChatBumped(_)
                | Event::DialogsReset(_)
                | Event::DialogsAppended(_)
                | Event::Auth(_)
        );
        session.apply_event(event, window, cx);
        rows_or_auth
    }

    pub(super) fn on_system_event(
        &mut self,
        event: SystemEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match event {
            SystemEvent::Click(payload) => self.notification_clicked(payload, window, cx),
            SystemEvent::AuthorizationFinished => {
                if let Some(center) = &mut self.notifications {
                    center.authorization_finished();
                }
            }
            SystemEvent::Settings(settings) => {
                if let Some(center) = &mut self.notifications {
                    center.apply_settings(settings);
                    self.refresh_dock_badge(cx);
                    cx.notify();
                }
            }
            SystemEvent::Delivered { receipt, outcome } => {
                if let Some(center) = &mut self.notifications {
                    center.delivered(receipt, outcome);
                }
            }
            SystemEvent::Existing(receipts) => {
                if let Some(center) = &mut self.notifications {
                    center.existing(receipts);
                    if let State::Running(session) = &self.state {
                        center.reconcile(|thread, msg| has_read(&session.shell, thread, msg));
                    }
                }
            }
        }
    }

    pub(super) fn refresh_notification_settings(&self) {
        if let Some(center) = &self.notifications {
            center.center.settings();
        }
    }

    pub(super) fn refresh_dock_badge(&mut self, cx: &mut Context<'_, Self>) {
        let expiry = match (&mut self.notifications, &self.state) {
            (Some(center), State::Running(session)) => {
                center.refresh_badge(session.shell.list.as_slice(), crate::chat::format::now())
            }
            _ => None,
        };
        if expiry == self.badge_wake.as_ref().map(|(at, _)| *at) {
            return;
        }
        self.badge_wake = expiry.map(|at| {
            // Recheck after wall-clock adjustments without scheduling a task
            // for every mute, or a very distant timer for indefinite mutes.
            let delay = at
                .saturating_sub(crate::chat::format::now())
                .clamp(1, 60)
                .unsigned_abs();
            let wake = cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_secs(delay))
                    .await;
                this.update(cx, |root, cx| {
                    root.badge_wake = None;
                    root.refresh_dock_badge(cx);
                })
                .ok();
            });
            (at, wake)
        });
    }

    fn notification_clicked(
        &mut self,
        payload: Option<Payload>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.activate_notification(payload, window, cx, |window, cx| {
            // GPUI's on_reopen is not called for every native unhide.
            cx.defer(|cx| cx.activate(true));
            window.activate_window();
        });
    }

    pub(super) fn activate_notification(
        &mut self,
        payload: Option<Payload>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
        activate: impl FnOnce(&mut Window, &mut Context<'_, Self>),
    ) {
        activate(window, cx);
        // Only a banner this epoch delivered opens anything, listed chat or
        // not (reviews A5, A28). Its evidence is bounded: for one that was
        // forgotten the click only activates the window.
        let Some((target, standin)) = self.notifications.as_ref().and_then(|n| {
            let target = n.click_target(payload)?;
            Some((target, n.provisional(target, crate::chat::format::now())?))
        }) else {
            return;
        };
        let State::Running(session) = &mut self.state else {
            return;
        };
        // The logout confirmation is modal: it keeps the window, its focus
        // and the open chat, so the click only activates (review A48).
        // Checked before `openable`, which takes the provisional slot.
        if !session.can_hide() || session.confirming_logout() {
            return;
        }
        let previous = session.shell.list.stand_in();
        if !openable(&mut session.shell, target, standin) {
            return;
        }
        // Dismissible overlays close first, so focus moves only once nothing
        // else owns it.
        session.close_menu(window, cx);
        if self.appearance_open {
            self.toggle_appearance(window, cx);
        }
        let State::Running(session) = &mut self.state else {
            return;
        };
        session.open_chats(window, cx);
        match target.topic {
            Some(topic) => session.select_forum_topic(target.chat, topic, window, cx),
            None => session.select_chat(target.chat, window, cx),
        }
        // `openable` took the slot before core admitted the switch. Refused
        // (a full queue, too many drafts), the pane still shows the chat it
        // had, which may be the stand-in just replaced (review A39). A topic
        // click is one operation, so a refusal never leaves the pane on the
        // forum's list (review A52).
        if session.shell.pane().chat() != Some(target.chat) {
            session.shell.list.restore_stand_in(previous);
        }
        cx.notify();
    }

    /// Settings' "Enable notifications": asks the system while it has not
    /// been asked.
    pub(super) fn enable_notifications(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(center) = &mut self.notifications {
            center.request_authorization();
            cx.notify();
        }
    }

    pub(super) fn toggle_dock_badge(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(center) = &mut self.notifications {
            center.toggle_badge();
            self.refresh_dock_badge(cx);
            cx.notify();
        }
    }
}

fn window_is_key(window: &Window) -> bool {
    if !window.is_window_active() {
        return false;
    }
    #[cfg(target_os = "macos")]
    return !crate::notifications::macos::is_hidden();
    #[cfg(not(target_os = "macos"))]
    true
}

fn observe(notifications: &mut Notifications, epoch: u64, event: &Event) {
    match event {
        Event::Auth(AuthState::SignedIn { .. } | AuthState::SignedInUnsaved { .. }) => {
            notifications.signed_in(epoch);
        }
        Event::Auth(AuthState::LoggingOut | AuthState::LoggedOut { .. }) => {
            notifications.end_account();
        }
        Event::InboxRead { thread, max_id } => notifications.read(*thread, *max_id),
        Event::NotifyDeleted { chat, ids } => notifications.deleted(*chat, ids),
        Event::ChatBumped(chat) => {
            notifications.read(ThreadKey::whole(chat.id), chat.read.read_inbox_max);
        }
        Event::DialogsReset(chats) | Event::DialogsAppended(chats) => notifications.read_snapshot(
            chats
                .iter()
                .map(|chat: &ChatSummary| (ThreadKey::whole(chat.id), chat.read.read_inbox_max)),
        ),
        Event::TopicBumped { chat, topic, .. } => notifications.read(
            ThreadKey {
                chat: *chat,
                topic: Some(topic.id),
            },
            topic.read_inbox_max,
        ),
        Event::TopicsReset { chat, topics, .. } | Event::TopicsAppended { chat, topics, .. } => {
            notifications.read_snapshot(topics.iter().map(|topic: &Topic| {
                (
                    ThreadKey {
                        chat: *chat,
                        topic: Some(topic.id),
                    },
                    topic.read_inbox_max,
                )
            }));
        }
        _ => {}
    }
}

fn has_read(shell: &ChatShell, thread: ThreadKey, msg: MsgId) -> bool {
    shell
        .list
        .get(thread.chat)
        .is_some_and(|chat| chat.read.read_inbox_max >= msg)
        || thread.topic.is_some_and(|topic| {
            shell
                .topics()
                .filter(|topics| topics.chat() == thread.chat)
                .and_then(|topics| topics.get(topic))
                .is_some_and(|row| row.read_inbox_max >= msg)
        })
}

/// Whether the shell can show a delivered banner's `target`. A chat the
/// dialogs list holds is authoritative: it is never replaced by the banner's
/// stand-in (a stale topic banner for a chat that is no longer a forum opens
/// nothing), and a topic needs it to be a forum. Core notifies about chats the
/// list does not hold yet (reviews A5, A25), and the stand-in then opens it
/// read-only in the shell's one provisional slot, whatever the sidebar's
/// capacity (review A27).
fn openable(shell: &mut ChatShell, target: ThreadKey, standin: ChatSummary) -> bool {
    match shell.list.listed(target.chat) {
        Some(chat) => target.topic.is_none() || crate::chat::is_forum(chat),
        None => standin.id == target.chat && shell.list.provide(standin),
    }
}
