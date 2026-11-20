//! Drive the real shell through GPUI's native macOS adapter. GPUI's test and
//! screenshot windows do not implement `PlatformWindow::a11y_init`.

#![cfg_attr(
    target_os = "macos",
    expect(
        dead_code,
        unused_imports,
        reason = "the native harness builds the app and its fixtures without the libtest entry point"
    )
)]

#[cfg(target_os = "macos")]
mod chat;
#[cfg(target_os = "macos")]
mod launch;
#[cfg(target_os = "macos")]
mod notifications;
#[cfg(target_os = "macos")]
mod preferences;
#[cfg(target_os = "macos")]
mod shell;
#[cfg(target_os = "macos")]
mod storage;
#[cfg(target_os = "macos")]
mod ui;

#[cfg(target_os = "macos")]
#[path = "shell/tests/native_a11y.rs"]
mod scenes;

fn main() {
    #[cfg(target_os = "macos")]
    scenes::run();
}
