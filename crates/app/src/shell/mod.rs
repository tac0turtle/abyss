//! The main window: startup failures, the auth router, the chat shell, and
//! the shutdown protocol (plan sections 1, 4, 5, 12).

mod appearance;
mod auth;
mod bridge;
mod chat_view;
mod chats;
mod folder_menu;
mod onboarding;
mod session;
mod shutdown;
mod topic_view;
mod view;

use abyss_core::StartError;
use abyss_core::api::Event;
use gpui::{
    App, AppContext, Bounds, Context, KeyBinding, Menu, MenuItem, Subscription, TitlebarOptions,
    Window, WindowBounds, WindowOptions, actions, px, size,
};

use self::bridge::EventSink;
use self::session::{Session, Started};
use self::shutdown::Closing;
use crate::launch::{Launch, LaunchError};

actions!(
    abyss,
    [
        /// Our own quit (Cmd+Q), so shutdown can wait for core (plan section 5).
        Quit,
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
        /// Turns the sidebar's unread filter on or off (Cmd+Shift+U). Bound
        /// now so the chord is reserved; the filter handles it (plan
        /// section 17.4, task 15).
        ToggleUnread
    ]
);

/// App-level key bindings.
pub(crate) fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-w", Quit, None),
        KeyBinding::new("cmd-,", Appearance, None),
        KeyBinding::new("cmd-[", ForumBack, None),
        // Bound by no text input, and none of them is part of IME
        // composition (plan section 17.2, "Keyboard").
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, None),
        KeyBinding::new("cmd-shift-u", ToggleUnread, None),
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
    _on_app_quit: Subscription,
    /// Redraws when the system switches between light and dark.
    _on_appearance: Subscription,
}

impl Root {
    fn new(
        launch: Result<Launch, LaunchError>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let started = launch.map_err(Failure::Launch).and_then(|l| {
            session::start(l, crate::preferences::Preferences::config(cx)).map_err(Failure::Start)
        });
        Self::with_started(started, window, cx)
    }

    fn with_started(
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
            this.update(cx, |root, cx| root.request_close(window, cx))
                .unwrap_or(true)
        });
        let on_app_quit = cx.on_app_quit(|root: &mut Root, _| {
            quit_signals(&|| {
                if let State::Running(session) = &root.state {
                    session.cancel_core();
                }
            })
        });
        let on_appearance = cx.observe_window_appearance(window, |_, _, cx| cx.notify());
        Self {
            state,
            appearance_open: false,
            _on_app_quit: on_app_quit,
            _on_appearance: on_appearance,
        }
    }

    /// Shows or hides the appearance panel.
    pub(crate) fn toggle_appearance(&mut self, cx: &mut Context<'_, Self>) {
        self.appearance_open = !self.appearance_open;
        cx.notify();
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

    /// Whether the window may close now. While core runs, starts shutdown
    /// instead and answers `false`; the ack handler quits.
    fn request_close(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        let State::Running(session) = &mut self.state else {
            return true;
        };
        match session.closing {
            Closing::Open => {
                session.begin_shutdown(window, cx);
                false
            }
            Closing::Waiting => false,
            Closing::Unconfirmed | Closing::LogoutUnfinished => true,
        }
    }
}

impl EventSink for Root {
    fn apply(&mut self, events: Vec<Event>, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let State::Running(session) = &mut self.state {
            for event in events {
                session.apply_event(event, window, cx);
            }
            cx.notify();
        }
    }

    fn core_stopped(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let State::Running(session) = &mut self.state
            && session.closing == Closing::Open
        {
            tracing::error!("core stopped unexpectedly");
            session.core_stopped(cx);
            session.show_notice(crate::ui::copy::CORE_STOPPED.into(), window, cx);
        }
    }
}

/// Opens the main window and routes the Quit action to its shutdown.
pub(crate) fn open(launch: Result<Launch, LaunchError>, cx: &mut App) -> gpui::Result<()> {
    let bounds = Bounds::centered(None, size(px(960.), px(640.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some("abyss".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    crate::ui::theme::Themes::init(cx);
    crate::preferences::Preferences::init(cx);
    crate::chat::sidebar_file::SidebarFile::init(cx);
    let handle = cx.open_window(options, |window, cx| {
        cx.new(|cx| Root::new(launch, window, cx))
    })?;
    cx.on_action(move |_: &Appearance, cx: &mut App| {
        handle
            .update(cx, |root, _, cx| root.toggle_appearance(cx))
            .ok();
    });
    cx.on_action(move |_: &Quit, cx: &mut App| {
        let quit_now = handle.update(cx, Root::request_close).unwrap_or(true);
        if quit_now {
            cx.quit();
        }
    });
    Ok(())
}

/// The body of the quit callback. Every quit passes here: after core's ack,
/// the UI's timeout, and an OS-initiated quit. It only signals, and returns
/// at once (round 30):
/// - [`abyss_core::cache::begin_exit`], an atomic store: no cache opens
///   from now on. The exit hook then drains what is open, or ends the
///   process with `_exit` rather than let SQLCipher tear down under a live
///   connection (plan section 13).
/// - `cancel_core`, a `try_send` of a stop request; core cancels on its
///   own runtime thread.
/// - In fake runs, a `try_send` that wakes the cleanup worker (after
///   `begin_exit`, so nothing reopens the directory). The returned future
///   waits for it, bounded by GPUI.
///
/// No lock we own, no file system call, no census wait and no logging runs
/// here. What GPUI does around it (dropping windows, flushing effects,
/// waiting up to 200 ms for these futures, `exit`) allocates and takes
/// its own locks, and is not bounded by us.
fn quit_signals(cancel_core: &dyn Fn()) -> impl Future<Output = ()> + use<> {
    abyss_core::cache::begin_exit();
    cancel_core();
    #[cfg(feature = "fake")]
    let cleanup = crate::storage::request_fake_cleanup();
    #[cfg(not(feature = "fake"))]
    let cleanup = std::future::ready(());
    cleanup
}

#[cfg(test)]
mod tests;
