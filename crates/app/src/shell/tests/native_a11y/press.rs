//! `AXPress` (`VoiceOver`'s `VO+Space`) on a control scrolled out of its viewport
//! (review N58). A control with only `on_click` gets a synthetic mouse click at
//! its centre, hit-tested through the scroll container's clip: out of view it
//! does nothing, or presses what is drawn there. Every control therefore
//! registers its activation through `a11y::on_activate`.
//!
//! GPUI queues the request on the foreground executor, so these scenes are
//! async: they press, yield, then read the tree.

use std::time::Duration;

use abyss_core::api::Event;
use abyss_core::model::{ChatFolder, FolderRules};
use gpui::{App, AppContext, AsyncApp, WindowHandle, px, size};
use objc2_foundation::NSRect;

use super::{
    AnyWindowHandle, NSAccessibility, Root, appkit_window, descendant, find_node, labels_of,
    nodes_of, open_sized, send, shell, signed_in, tree,
};
use crate::ui::theme::{Choice, Colourway, Hue, Settings, Themes};

/// Run the scenes and end the process: `0` if they pass.
pub(super) fn run(cx: &mut App) {
    cx.spawn(async move |cx| {
        settings_controls(cx).await;
        folder_tabs(cx).await;
        std::process::exit(0);
    })
    .detach();
}

/// A failed assertion ends the process nonzero; unwinding through the
/// executor would not.
fn guard<R>(scene: impl FnOnce() -> R) -> R {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(scene))
        .unwrap_or_else(|_| std::process::exit(1))
}

/// `AXPress` on `label`, then time for the foreground executor to run it.
async fn press(handle: WindowHandle<Root>, cx: &AsyncApp, label: &str) {
    guard(|| {
        cx.update(|cx| {
            AnyWindowHandle::from(handle)
                .update(cx, |_, window, _| {
                    let element = descendant(appkit_window(window).accessibilityChildren(), label)
                        .unwrap_or_else(|| panic!("{label} is not in AppKit's tree"));
                    assert!(
                        element.accessibilityPerformPress(),
                        "{label} is not pressable"
                    );
                })
                .unwrap();
        });
    });
    cx.background_executor()
        .timer(Duration::from_millis(100))
        .await;
}

fn frame(handle: WindowHandle<Root>, cx: &mut App, label: &str) -> NSRect {
    AnyWindowHandle::from(handle)
        .update(cx, |_, window, _| {
            descendant(appkit_window(window).accessibilityChildren(), label)
                .unwrap_or_else(|| panic!("{label} is not in AppKit's tree"))
                .accessibilityFrame()
        })
        .unwrap()
}

fn middle(rect: NSRect) -> f64 {
    rect.origin.y + rect.size.height / 2.
}

/// Whether the centre of `inner` is within `outer`'s height.
fn within(outer: NSRect, inner: NSRect) -> bool {
    (outer.origin.y..=outer.origin.y + outer.size.height).contains(&middle(inner))
}

fn window_frame(handle: WindowHandle<Root>, cx: &mut App) -> NSRect {
    AnyWindowHandle::from(handle)
        .update(cx, |_, window, _| appkit_window(window).frame())
        .unwrap()
}

/// A short signed-in window: the Settings card and the folder strip overflow.
fn short_window(cx: &mut App) -> (WindowHandle<Root>, crate::shell::tests::Harness) {
    let (started, harness) = shell::tests::started_fixture();
    let handle = open_sized(cx, size(px(700.), px(365.)), |window, cx| {
        cx.new(|cx| Root::with_started(Ok(started), window, cx))
    });
    signed_in(handle, cx);
    (handle, harness)
}

fn dialog_open(handle: WindowHandle<Root>, cx: &mut App) -> bool {
    !nodes_of(&tree(handle, cx), "Dialog").is_empty()
}

/// Reset colours and Done sit at the card's foot, below the window on the
/// short one. Pressing them must still reset and close, not click the void.
async fn settings_controls(cx: &AsyncApp) {
    let (handle, _harness) = guard(|| {
        cx.update(|cx| {
            let (handle, harness) = short_window(cx);
            Themes::set_colourway(Colourway::Abyss, cx);
            let defaults = Settings::default();
            let moved = Choice {
                hue: Hue::all().next().unwrap(),
                ..defaults.night
            };
            Themes::set(
                Settings {
                    night: moved,
                    ..defaults
                },
                cx,
            );
            assert_ne!(Themes::settings(cx), defaults);
            handle.update(cx, Root::toggle_appearance).unwrap();
            find_node(&tree(handle, cx), "Dialog", "Settings");
            let window = window_frame(handle, cx);
            for label in ["Reset colours", "Done"] {
                assert!(
                    !within(window, frame(handle, cx, label)),
                    "{label} is in view: the scene needs a shorter window"
                );
            }
            (handle, harness)
        })
    });
    press(handle, cx, "Reset colours").await;
    guard(|| {
        cx.update(|cx| {
            assert_eq!(
                Themes::settings(cx),
                Settings::default(),
                "pressing Reset colours out of view did not reset"
            );
            assert!(dialog_open(handle, cx), "the press closed Settings");
        });
    });
    press(handle, cx, "Done").await;
    guard(|| {
        cx.update(|cx| {
            assert!(
                !dialog_open(handle, cx),
                "pressing Done out of view did not close Settings"
            );
        });
    });
}

/// With more folders than the strip shows, the tab nearest the unread switch
/// is out of the strip. Pressing it switches to it and leaves the switch.
async fn folder_tabs(cx: &AsyncApp) {
    let (handle, _harness, target) = guard(|| {
        cx.update(|cx| {
            let (handle, harness) = short_window(cx);
            let folders = (1..=12)
                .map(|id| ChatFolder {
                    id,
                    title: format!("Folder {id}"),
                    rules: FolderRules::GROUPS,
                    pinned: Vec::new(),
                    include: Vec::new(),
                    exclude: Vec::new(),
                    shared: false,
                })
                .collect();
            send(handle, cx, vec![Event::ChatFolders(folders)]);
            let now = tree(handle, cx);
            let switch = labels_of(&now, "Switch")
                .into_iter()
                .find(|label| label.starts_with("Unread"))
                .unwrap();
            assert_eq!(
                find_node(&now, "Switch", &switch)["aria"]["toggled"],
                "False"
            );
            let (strip, toggle) = (frame(handle, cx, "Chat tabs"), frame(handle, cx, &switch));
            let tabs: Vec<_> = labels_of(&now, "Tab")
                .into_iter()
                .map(|label| {
                    let at = frame(handle, cx, &label);
                    (label, at)
                })
                .filter(|(_, at)| !within(strip, *at))
                .collect();
            let (target, _) = tabs
                .into_iter()
                .min_by(|(_, a), (_, b)| {
                    let gap = |at: &NSRect| (middle(*at) - middle(toggle)).abs();
                    gap(a).total_cmp(&gap(b))
                })
                .expect("a tab scrolled out of the strip");
            (handle, harness, target)
        })
    });
    press(handle, cx, &target).await;
    guard(|| {
        cx.update(|cx| {
            let after = tree(handle, cx);
            assert_eq!(
                find_node(&after, "Tab", &target)["aria"]["selected"],
                true,
                "pressing a tab out of view did not select it"
            );
            let switch = labels_of(&after, "Switch")
                .into_iter()
                .find(|label| label.starts_with("Unread"))
                .unwrap();
            assert_eq!(
                find_node(&after, "Switch", &switch)["aria"]["toggled"],
                "False",
                "pressing a tab out of view toggled Unread"
            );
        });
    });
}
