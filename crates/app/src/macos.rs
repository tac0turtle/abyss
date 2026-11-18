//! Native app identity for direct launches (`cargo run`, `cargo dev`).
//! Finder gets its icon from the app bundle; the Dock also needs an icon
//! when the executable runs without a bundle.

use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSImage};
use objc2_foundation::NSData;

/// Use the committed icon without depending on the working directory or
/// a runtime asset path. Called from GPUI's main-thread launch callback.
pub(crate) fn set_app_icon() {
    let Some(main_thread) = MainThreadMarker::new() else {
        tracing::warn!("could not set the app icon off the main thread");
        return;
    };
    let data = NSData::with_bytes(include_bytes!("../../../assets/icon/abyss.icns"));
    let Some(icon) = NSImage::initWithData(NSImage::alloc(), &data) else {
        tracing::warn!("could not decode the app icon");
        return;
    };
    let app = NSApplication::sharedApplication(main_thread);
    // SAFETY: GPUI's launch callback is on the main thread (checked above),
    // and Some supplies a live, non-nil image, retained by NSApplication.
    unsafe { app.setApplicationIconImage(Some(&icon)) };
}
