//! The main window: startup failures, the auth router, the chat shell, and
//! the shutdown protocol (plan sections 1, 4, 5, 12).

mod appearance;
mod auth;
mod bridge;
mod chat_view;
mod chats;
mod folder_menu;
mod nav;
mod notification_events;
mod onboarding;
mod session;
pub(crate) mod shutdown;
mod tabs;
mod topic_view;
mod view;

use abyss_core::StartError;
use abyss_core::api::Event;
use abyss_core::events::NotificationCleanupPolicy;
use gpui::{
    App, AppContext, Bounds, Context, KeyBinding, Menu, MenuItem, Subscription, Task,
    TitlebarOptions, Window, WindowBounds, WindowOptions, actions, point, px, size,
};

pub(crate) use self::bridge::EventSink;
use self::session::{Session, SettingsPanel, Started};
use self::shutdown::{Closing, Next};
use crate::launch::{Launch, LaunchError};
use crate::notifications::{Notifications, SystemEvents};

/// Content inset below the native macOS controls. The transparent title bar
/// uses the root's theme surface, with no separate system-colored strip.
const TITLEBAR_HEIGHT: f32 = 34.;

/// Window coordinates include the integrated title bar; session content
/// starts below it. Fullscreen and non-macOS windows need no inset.
fn titlebar_inset(window: &Window) -> gpui::Pixels {
    px(if cfg!(target_os = "macos") && !window.is_fullscreen() {
        TITLEBAR_HEIGHT
    } else {
        0.
    })
}

actions!(
    abyss,
    [
        FocusNext,
        FocusPrevious,
        OpenContextMenu,
        DismissOverlay,
        MenuNext,
        MenuPrevious,
        /// Our own quit (Cmd+Q), so shutdown can wait for core (plan section 5).
        Quit,
        /// Close the window (Cmd+W), keeping a signed-in session running.
        CloseWindow,
        /// Shows or hides settings (Cmd+comma).
        Appearance,
        /// Back from a forum topic to its topic list, and from a topic list
        /// to no chat (Cmd+[). Not Escape: that cancels IME composition in
        /// the composer.
        ForumBack,
        /// The sidebar's next tab, wrapping (Ctrl+Tab).
        NextTab,
        /// The sidebar's previous tab, wrapping (Ctrl+Shift+Tab).
        PreviousTab,
        /// Turns the sidebar's unread filter on or off (Cmd+Shift+U; plan
        /// section 17.4).
        ToggleUnread,
        /// Selects the sidebar's next chat or topic row (Option+Cmd+Down).
        NextRow,
        /// Selects the sidebar's previous chat or topic row (Option+Cmd+Up).
        PreviousRow,
        /// Shows the pane's forum's topics under its row (Option+Cmd+Right).
        ExpandForum,
        /// Hides them (Option+Cmd+Left).
        CollapseForum,
        /// Escape in the chat shell: focus returns to the sidebar row that
        /// opened the pane (plan section 4.3). A text field in the way lets
        /// Escape through only when no input method is composing.
        ReturnToRow
    ]
);

/// App-level key bindings.
pub(crate) fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("tab", FocusNext, None),
        KeyBinding::new("shift-tab", FocusPrevious, None),
        KeyBinding::new("shift-f10", OpenContextMenu, None),
        KeyBinding::new("escape", DismissOverlay, Some("Overlay")),
        KeyBinding::new("down", MenuNext, Some("MenuChoices")),
        KeyBinding::new("up", MenuPrevious, Some("MenuChoices")),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-,", Appearance, None),
        KeyBinding::new("cmd-[", ForumBack, None),
        // Bound by no text input, and none of them is part of IME
        // composition (plan section 17.2, "Keyboard").
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, None),
        KeyBinding::new("cmd-shift-u", ToggleUnread, None),
        KeyBinding::new("alt-cmd-down", NextRow, None),
        KeyBinding::new("alt-cmd-up", PreviousRow, None),
        KeyBinding::new("alt-cmd-right", ExpandForum, None),
        KeyBinding::new("alt-cmd-left", CollapseForum, None),
        KeyBinding::new("escape", ReturnToRow, Some(chat_view::KEY_CONTEXT)),
    ]
}

/// The app menu, so the menu bar's Quit goes through [`Quit`] too.
pub(crate) fn menus() -> Vec<Menu> {
    vec![Menu::new("abyss").items([
        MenuItem::action(crate::ui::copy::APPEARANCE_MENU, Appearance),
        MenuItem::separator(),
        MenuItem::action("Quit abyss", Quit),
    ])]
}

/// Why abyss can't run; shown as a full-window explanation.
#[derive(Debug)]
pub(crate) enum Failure {
    Launch(LaunchError),
    Start(StartError),
}

enum State {
    Failed(Failure),
    Running(Box<Session>),
}

pub(crate) struct Root {
    state: State,
    /// Whether the appearance panel is showing.
    appearance_open: bool,
    appearance_focus: crate::ui::a11y::OverlayFocus,
    /// The closing overlay and its quit cards, which own focus while they show.
    closing_focus: crate::ui::a11y::OverlayFocus,
    notifications: Option<Notifications>,
    badge_wake: Option<(i64, Task<()>)>,
    _on_app_quit: Subscription,
    /// Redraws when the system switches between light and dark.
    _on_appearance: Subscription,
    _on_focus_lost: Subscription,
}

impl Root {
    pub(crate) fn new(
        launch: Result<Launch, LaunchError>,
        notifications: Option<Notifications>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let cleanup = if notifications.is_some() {
            NotificationCleanupPolicy::Required
        } else {
            NotificationCleanupPolicy::Disabled
        };
        let started = launch.map_err(Failure::Launch).and_then(|l| {
            session::start(l, crate::preferences::Preferences::config(cx), cleanup)
                .map_err(Failure::Start)
        });
        let mut root = Self::with_started(started, window, cx);
        root.notifications = notifications;
        root
    }

    pub(crate) fn with_started(
        started: Result<Started, Failure>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let state = match started {
            Ok(started) => State::Running(Box::new(Session::new(started, window, cx))),
            Err(failure) => {
                tracing::warn!(?failure, "abyss cannot start");
                State::Failed(failure)
            }
        };
        let this = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            this.update(cx, |root, cx| root.request_window_close(window, cx, hide))
                .unwrap_or(true)
        });
        let on_app_quit = cx.on_app_quit(|root: &mut Root, _| {
            quit_signals(&|| match &root.state {
                State::Running(session) => session.cancel_core(),
                State::Failed(_) => None,
            })
        });
        let on_appearance = cx.observe_window_appearance(window, |_, _, cx| cx.notify());
        let on_focus_lost = cx.on_focus_lost(window, |_, window, cx| {
            let Some(target) = window.focus_lost_restore_target(cx) else {
                return;
            };
            let lost = window.focused(cx);
            window.defer(cx, move |window, cx| {
                if window.focused(cx) == lost && target.is_rendered(window) {
                    target.focus(window, cx);
                    window.focus_next(cx);
                    window.dispatch_action(Box::new(crate::ui::a11y::RevealFocus::default()), cx);
                }
            });
        });
        Self {
            state,
            appearance_open: false,
            appearance_focus: crate::ui::a11y::OverlayFocus::new(cx),
            closing_focus: crate::ui::a11y::OverlayFocus::new(cx),
            notifications: None,
            badge_wake: None,
            _on_app_quit: on_app_quit,
            _on_appearance: on_appearance,
            _on_focus_lost: on_focus_lost,
        }
    }

    /// Shows or hides the appearance panel.
    pub(crate) fn toggle_appearance(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.appearance_open {
            self.appearance_focus.close(window, cx);
            self.set_appearance(SettingsPanel::Closed);
            if let State::Running(session) = &mut self.state {
                session.overlay_closed(window, cx);
            }
        } else {
            if let State::Running(session) = &mut self.state {
                if session.confirming_logout() || session.closing != Closing::Open {
                    return;
                }
                session.close_menu(window, cx);
            }
            self.appearance_focus.open(window, cx);
            self.set_appearance(SettingsPanel::Open);
            self.refresh_notification_settings();
        }
        cx.notify();
    }

    /// The one place the panel's flag changes: the session keeps a copy, so
    /// that focus moves it makes on its own respect the panel.
    fn set_appearance(&mut self, panel: SettingsPanel) {
        self.appearance_open = panel == SettingsPanel::Open;
        if let State::Running(session) = &mut self.state {
            session.set_settings(panel);
        }
    }

    fn overlay_focus(&self) -> Option<&crate::ui::a11y::OverlayFocus> {
        // The closing overlay covers everything else, and is the only place
        // Tab can go (N43).
        if matches!(&self.state, State::Running(session) if session.closing != Closing::Open) {
            return Some(&self.closing_focus);
        }
        if self.appearance_open {
            return Some(&self.appearance_focus);
        }
        let State::Running(session) = &self.state else {
            return None;
        };
        if session.confirming_logout() {
            Some(&session.logout_focus)
        } else {
            session.menu.as_ref().map(|_| &session.menu_focus)
        }
    }

    fn step_focus(
        &self,
        direction: crate::ui::a11y::FocusDirection,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(overlay) = self.overlay_focus() {
            overlay.group.step(direction, window, cx);
            return;
        }
        direction.advance(window, cx);
        window.dispatch_action(Box::new(crate::ui::a11y::RevealFocus::default()), cx);
    }

    fn with_chat_shortcut(
        &mut self,
        f: impl FnOnce(&mut Session, &mut Context<'_, Self>),
        cx: &mut Context<'_, Self>,
    ) {
        if self.overlay_focus().is_none()
            && let State::Running(session) = &mut self.state
            && session.closing == Closing::Open
        {
            f(session, cx);
        }
    }

    /// Runs `f` on the session if core is running.
    pub(crate) fn with_session<R>(
        &mut self,
        f: impl FnOnce(&mut Session, &mut Context<'_, Self>) -> R,
        cx: &mut Context<'_, Self>,
    ) -> Option<R> {
        match &mut self.state {
            State::Running(session) => Some(f(session, cx)),
            State::Failed(_) => None,
        }
    }

    fn request_window_close(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
        hide: impl FnOnce(&mut Context<'_, Self>),
    ) -> bool {
        if matches!(&self.state, State::Running(session) if session.can_hide()) {
            hide(cx);
            return false;
        }
        self.request_quit(window, cx)
    }

    /// Returns true if it is safe to quit immediately. Otherwise, the
    /// existing core shutdown protocol owns the eventual quit.
    pub(crate) fn request_quit(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        let State::Running(session) = &mut self.state else {
            return true;
        };
        match session.closing {
            Closing::Open => {}
            Closing::Waiting => return false,
            Closing::Unconfirmed | Closing::LogoutUnfinished => return true,
        }
        // What sits under, or beside, the closing overlay closes first, so
        // focus has nowhere to stay but the overlay (N43).
        session.close_menu(window, cx);
        if session.confirming_logout() {
            session.cancel_logout(window, cx);
        }
        session.begin_shutdown(window, cx);
        if self.appearance_open {
            self.set_appearance(SettingsPanel::Closed);
            self.appearance_focus.close(window, cx);
        }
        self.closing_focus.open(window, cx);
        false
    }

    /// What the UI does once core's ack (or the timer) decided how
    /// shutdown ends: quit, or stay with a card that asks the user.
    pub(crate) fn finish_shutdown(&mut self, next: Next, cx: &mut Context<'_, Self>) {
        let closing = match next {
            Next::Quit => return cx.quit(),
            Next::ShowUnconfirmed => Closing::Unconfirmed,
            Next::ShowLogoutUnfinished => Closing::LogoutUnfinished,
        };
        self.with_session(|session, _| session.closing = closing, cx);
        cx.notify();
    }
}

impl EventSink for Root {
    fn apply(
        &mut self,
        events: Vec<(u64, Event)>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let mut badge_changed = false;
        for (epoch, event) in events {
            badge_changed |= self.apply_notification_event(epoch, event, window, cx);
        }
        if badge_changed {
            self.refresh_dock_badge(cx);
        }
        cx.notify();
    }

    fn core_stopped(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let State::Running(session) = &mut self.state
            && session.closing == Closing::Open
        {
            tracing::error!("core stopped unexpectedly");
            session.close_menu(window, cx);
            if session.confirming_logout() {
                session.cancel_logout(window, cx);
            }
            session.core_stopped(cx);
            session.show_notice(crate::ui::copy::CORE_STOPPED.into(), window, cx);
        }
    }
}

/// Opens the main window and routes the Quit action to its shutdown.
pub(crate) fn open(
    launch: Result<Launch, LaunchError>,
    notifications: Option<(Notifications, SystemEvents)>,
    cx: &mut App,
) -> gpui::Result<()> {
    let bounds = Bounds::centered(None, size(px(960.), px(640.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some("abyss".into()),
            appears_transparent: cfg!(target_os = "macos"),
            traffic_light_position: Some(point(px(12.), px(10.))),
        }),
        ..Default::default()
    };
    crate::ui::display::DisplayPreferences::init(cx);
    crate::ui::theme::Themes::init(cx);
    crate::preferences::Preferences::init(cx);
    crate::chat::sidebar_file::SidebarFile::init(cx);
    let (notifications, receiver) = notifications.unzip();
    let handle = cx.open_window(options, |window, cx| {
        cx.new(|cx| Root::new(launch, notifications, window, cx))
    })?;
    route_actions(handle, cx);
    if let Some(receiver) = receiver {
        notification_events::wire(handle, receiver, cx);
    }
    Ok(())
}

/// Routes the app-level actions (Cmd+comma, Cmd+Q and the menu's Quit, Cmd+W)
/// to the window.
///
/// A global action listener runs inside the update of the window that
/// dispatched the action, and that window is then leased: updating it from
/// the listener fails with "window not found". Quit would then take the
/// "gone" branch and quit at once, skipping the shutdown protocol, and Cmd+W
/// would quit instead of hiding. So each route runs once that dispatch is
/// over.
pub(crate) fn route_actions(handle: gpui::WindowHandle<Root>, cx: &mut App) {
    route::<Appearance>(handle, cx, Gone::Stay, |root, window, cx| {
        root.toggle_appearance(window, cx);
        false
    });
    route::<Quit>(handle, cx, Gone::Quit, Root::request_quit);
    route::<CloseWindow>(handle, cx, Gone::Quit, |root, window, cx| {
        root.request_window_close(window, cx, hide)
    });
}

/// What a route does when its window no longer exists.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gone {
    Quit,
    Stay,
}

/// Runs `run` on the window's root after `A` was dispatched; it returns
/// whether to quit the app now. A window that is gone does `gone`.
fn route<A: gpui::Action>(
    handle: gpui::WindowHandle<Root>,
    cx: &mut App,
    gone: Gone,
    run: fn(&mut Root, &mut Window, &mut Context<'_, Root>) -> bool,
) {
    cx.on_action(move |_: &A, cx: &mut App| {
        cx.defer(move |cx| {
            if handle.update(cx, run).unwrap_or(gone == Gone::Quit) {
                cx.quit();
            }
        });
    });
}

fn hide(cx: &mut Context<'_, Root>) {
    cx.defer(|cx| cx.hide());
}

/// Fallback for Dock reopen events. macOS also unhides the existing window
/// itself when a hidden app is selected.
pub(crate) fn reopen(cx: &mut App) {
    cx.activate(true);
    if let Some(handle) = cx.windows().first().copied() {
        let _ = handle.update(cx, |_, window, _| window.activate_window());
    }
}

/// The body of the quit callback. Every quit passes here: after core's ack,
/// the UI's timeout, and an OS-initiated quit. It only signals, and returns
/// at once (round 30):
/// - [`abyss_core::cache::begin_exit`], an atomic store: no cache opens
///   from now on. The exit hook then drains what is open, or ends the
///   process with `_exit` rather than let SQLCipher tear down under a live
///   connection (plan section 13).
/// - `cancel_core`, a `try_send` of a stop request; core cancels on its
///   own runtime thread. Its ack, when this is the first request, is what
///   the returned future waits for (N48): an OS-initiated quit (Dock
///   Quit, Cmd+Tab Q, logout, shutdown) reaches `applicationWillTerminate:`
///   without passing `Root::request_quit`, so this is its only chance to let
///   core save the session.
/// - In fake runs, a `try_send` that wakes the cleanup worker (after
///   `begin_exit`, so nothing reopens the directory). The returned future
///   waits for it too, after core.
///
/// GPUI bounds the wait: 200 ms for every quit future together, then it
/// goes on and exits, with the cache protected by the exit hook.
///
/// No lock we own, no file system call, no census wait and no logging runs
/// here. What GPUI does around it (dropping windows, flushing effects,
/// waiting up to 200 ms for these futures, `exit`) allocates and takes
/// its own locks, and is not bounded by us.
fn quit_signals(
    cancel_core: &dyn Fn() -> Option<bridge::ShutdownAck>,
) -> impl Future<Output = ()> + use<> {
    abyss_core::cache::begin_exit();
    let ack = cancel_core();
    #[cfg(feature = "fake")]
    let cleanup = crate::storage::request_fake_cleanup();
    #[cfg(not(feature = "fake"))]
    let cleanup = std::future::ready(());
    async move {
        if let Some(ack) = ack {
            let _ = ack.await;
        }
        cleanup.await;
    }
}

#[cfg(test)]
pub(crate) mod tests;
