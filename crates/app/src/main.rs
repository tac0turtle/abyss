//! abyss: a minimal-attack-surface Telegram client for macOS.

mod chat;
mod launch;
mod logging;
#[cfg(target_os = "macos")]
mod macos;
mod notifications;
mod preferences;
mod shell;
mod storage;
mod ui;

use gpui::App;
use gpui_platform::application;

fn main() {
    logging::init();
    let launch = launch::resolve(std::env::args_os(), |name| std::env::var_os(name));
    let application = application();
    // Register the notification delegate before AppKit finishes launching.
    #[cfg(target_os = "macos")]
    let notifications = notifications::macos::open();
    #[cfg(not(target_os = "macos"))]
    let notifications = None;
    application.on_reopen(shell::reopen);
    application.run(move |cx: &mut App| {
        #[cfg(target_os = "macos")]
        macos::set_app_icon();
        cx.bind_keys(ui::text_input::key_bindings());
        cx.bind_keys(shell::key_bindings());
        cx.set_menus(shell::menus());
        // Single-window app: once the window is gone (after shutdown, or on
        // the error screen), quit.
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        match shell::open(launch, notifications, cx) {
            Ok(()) => cx.activate(true),
            Err(error) => {
                tracing::error!(%error, "failed to open the main window");
                cx.quit();
            }
        }
    });
}
