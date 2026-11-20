//! macOS accessibility display preferences. The `AppKit` observer coalesces
//! notifications, and GPUI reads the latest settings on its main thread.

use gpui::{App, Global};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DisplayPreferences {
    pub(crate) increase_contrast: bool,
    pub(crate) reduce_motion: bool,
}

impl Global for DisplayPreferences {}

impl DisplayPreferences {
    pub(crate) fn of(cx: &App) -> Self {
        cx.try_global::<Self>().copied().unwrap_or_default()
    }

    pub(crate) fn apply(self, cx: &mut App) {
        cx.set_reduce_motion(self.reduce_motion);
        if Self::of(cx) != self {
            cx.set_global(self);
            cx.refresh_windows();
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn init(cx: &mut App) {
        macos::init(cx);
    }

    #[cfg(not(target_os = "macos"))]
    pub(crate) fn init(cx: &mut App) {
        Self::default().apply(cx);
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ptr::NonNull;

    use block2::RcBlock;
    use gpui::App;
    use objc2::rc::Retained;
    use objc2::runtime::{NSObjectProtocol, ProtocolObject};
    use objc2_app_kit::{NSWorkspace, NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification};
    use objc2_foundation::{NSNotification, NSNotificationCenter};

    use super::DisplayPreferences;

    struct Observer {
        center: Retained<NSNotificationCenter>,
        token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
    }

    impl Observer {
        fn new(workspace: &NSWorkspace, wake: async_channel::Sender<()>) -> Self {
            let center = workspace.notificationCenter();
            let callback = RcBlock::new(move |_: NonNull<NSNotification>| {
                let _ = wake.try_send(());
            });
            // SAFETY: AppKit provides this process-wide notification name.
            let name = unsafe { NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification };
            // SAFETY: The callback only uses a thread-safe sender, and ignores
            // the notification argument. No delivery queue or sender filter is needed.
            let token = unsafe {
                center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &callback)
            };
            Self { center, token }
        }
    }

    impl Drop for Observer {
        fn drop(&mut self) {
            // SAFETY: This center registered the live token in Observer::new.
            unsafe { self.center.removeObserver((*self.token).as_ref()) };
        }
    }

    fn read(workspace: &NSWorkspace) -> DisplayPreferences {
        DisplayPreferences {
            increase_contrast: workspace.accessibilityDisplayShouldIncreaseContrast(),
            reduce_motion: workspace.accessibilityDisplayShouldReduceMotion(),
        }
    }

    pub(super) fn init(cx: &mut App) {
        let workspace = NSWorkspace::sharedWorkspace();
        let (wake, changes) = async_channel::bounded(1);
        let observer = Observer::new(&workspace, wake);
        read(&workspace).apply(cx);
        cx.spawn(async move |cx| {
            let _observer = observer;
            while changes.recv().await.is_ok() {
                let next = read(&workspace);
                cx.update(|cx| next.apply(cx));
            }
        })
        .detach();
    }
}
