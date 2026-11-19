//! Stage 3 task 0 spike, GPUI part (docs/plans/stage-3-spikes.md, question 3).
//!
//!   abyss-gpui-spike --cmd-dir <dir>
//!
//! Opens one window with labelled elements and a custom text field. It polls
//! `<dir>/cmd` for one command per file and appends results to `<dir>/log.txt`:
//!
//!   state     log the window and app state (bounds, visibility, counters)
//!   hide      `App::hide`
//!   activate  `App::activate(true)`
//!   ax        wake the adapter, as an AX client's first call does
//!   axdump    walk the window's NSAccessibility tree in-process and log it, then
//!             log `Window::debug_a11y_tree_json`
//!   quit      `App::quit`
//!
//! The a11y tree is only built once an assistive client asks for it, so `ax`
//! plays that client: it calls the same NSAccessibility methods VoiceOver does.
//! Throwaway code, so output and indexing lints are off.
#![allow(clippy::print_stdout, reason = "a spike reports on stdout")]

mod ax;
mod field;

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use gpui::{
    AccessibleAction, App, AppContext as _, Bounds, Context, FocusHandle, KeyBinding, Role,
    SharedString, Toggled, Window, WindowBounds, WindowHandle, WindowOptions, actions, canvas, div,
    prelude::*, px, rgb, size,
};
use gpui_platform::application;
use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;

use field::SpikeField;

actions!(spike, [Tab, TabPrev]);

/// Frames actually painted, counted from a canvas element's paint callback.
pub(crate) static PAINTS: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn log(line: &str) {
    println!("{line}");
    if let Ok(dir) = std::env::var("SPIKE_CMD_DIR") {
        let path = PathBuf::from(dir).join("log.txt");
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

struct Demo {
    focus: FocusHandle,
    field: gpui::Entity<SpikeField>,
    secret: gpui::Entity<SpikeField>,
    ticks: u64,
    renders: u64,
    sent: u64,
    switch_on: bool,
}

impl Demo {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let field = cx.new(|cx| SpikeField::new("héllo wörld", false, cx));
        let secret = cx.new(|cx| SpikeField::new("••••••••••", true, cx));
        // One tick every 500 ms: state the window must still hold after a hide and reopen.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        this.ticks += 1;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Self {
            focus,
            field,
            secret,
            ticks: 0,
            renders: 0,
            sent: 0,
            switch_on: false,
        }
    }
}

impl Render for Demo {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.renders += 1;
        div()
            .id("root")
            .role(Role::Group)
            .aria_label("Spike root")
            .track_focus(&self.focus)
            .on_action(cx.listener(|_, _: &Tab, window, cx| window.focus_next(cx)))
            .on_action(cx.listener(|_, _: &TabPrev, window, cx| window.focus_prev(cx)))
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .bg(rgb(0x1e1e2e))
            .text_color(rgb(0xcdd6f4))
            .child(
                div()
                    .id("heading")
                    .role(Role::Heading)
                    .aria_level(1)
                    .aria_label("Spike heading")
                    .text_xl()
                    .child("abyss spike"),
            )
            .child(
                div()
                    .id("state")
                    .role(Role::Label)
                    .aria_label(SharedString::from(format!(
                        "ticks {} renders {} sent {}",
                        self.ticks, self.renders, self.sent
                    )))
                    .child(format!(
                        "ticks {} renders {} sent {}",
                        self.ticks, self.renders, self.sent
                    )),
            )
            .child(
                div()
                    .id("send")
                    .focusable()
                    .tab_stop(true)
                    .role(Role::Button)
                    .aria_label("Send message")
                    .px_3()
                    .py_1()
                    .bg(rgb(0x585b70))
                    .on_a11y_action(AccessibleAction::Click, {
                        let this = cx.entity().downgrade();
                        move |_, _, cx| {
                            this.update(cx, |this, cx| {
                                this.sent += 1;
                                cx.notify();
                            })
                            .ok();
                        }
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sent += 1;
                        cx.notify();
                    }))
                    .child("Send"),
            )
            .child(
                div()
                    .id("switch")
                    .focusable()
                    .tab_stop(true)
                    .role(Role::Switch)
                    .aria_label("Show message text in banners")
                    .aria_toggled(if self.switch_on {
                        Toggled::True
                    } else {
                        Toggled::False
                    })
                    .w(px(44.))
                    .h(px(24.))
                    .bg(rgb(0x89b4fa))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.switch_on = !this.switch_on;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("chats")
                    .role(Role::List)
                    .aria_label("Chats")
                    .children(["Ada, 2 unread", "Grace, muted"].iter().enumerate().map(
                        |(i, label)| {
                            div()
                                .id(("chat", i))
                                .role(Role::ListItem)
                                .aria_label(SharedString::from(*label))
                                .aria_position_in_set(i + 1)
                                .aria_size_of_set(2)
                                .child(*label)
                        },
                    )),
            )
            // A plain div carrying a text-input role and a value: the cheap route.
            .child(
                div()
                    .id("plain-input")
                    .focusable()
                    .tab_stop(true)
                    .role(Role::TextInput)
                    .aria_label("Plain div input")
                    .aria_value("plain value")
                    .child("plain value"),
            )
            // The custom element route (the shape of gpui's input.rs).
            .child(self.field.clone())
            .child(self.secret.clone())
            .child(
                canvas(
                    |_, _, _| (),
                    |_, (), _, _| {
                        PAINTS.fetch_add(1, Ordering::Relaxed);
                    },
                )
                .size(px(1.)),
            )
    }
}

fn state_line(tag: &str, demo: &Demo, window: &Window) -> String {
    let bounds = window.bounds();
    let mtm = MainThreadMarker::new();
    let (hidden, windows_visible) = mtm.map_or((None, None), |mtm| {
        let app = NSApplication::sharedApplication(mtm);
        let visible = app.windows().iter().filter(|w| w.isVisible()).count();
        let occluded_visible = app
            .windows()
            .iter()
            .filter(|w| {
                w.occlusionState()
                    .contains(objc2_app_kit::NSWindowOcclusionState::Visible)
            })
            .count();
        (Some(app.isHidden()), Some((visible, occluded_visible)))
    });
    format!(
        "{tag}: ticks={} renders={} sent={} paints={} bounds=({:.0},{:.0} {:.0}x{:.0}) active={} NSApp.isHidden={hidden:?} visibleNSWindows,occlusionVisible={windows_visible:?}",
        demo.ticks,
        demo.renders,
        demo.sent,
        PAINTS.load(Ordering::Relaxed),
        bounds.origin.x.as_f32(),
        bounds.origin.y.as_f32(),
        bounds.size.width.as_f32(),
        bounds.size.height.as_f32(),
        window.is_window_active(),
    )
}

fn run_command(command: &str, cx: &mut App, handle: WindowHandle<Demo>) {
    // `frame` and `axdump` draw the window, which reads `Demo`, so they must not
    // hold the root view: the untyped handle updates the window alone.
    if matches!(command, "frame" | "axdump") {
        let any: gpui::AnyWindowHandle = handle.into();
        let result = any.update(cx, |_, window, cx| {
            if command == "frame" {
                window.draw(cx).clear(cx);
                log("frame: Window::draw forced");
            } else {
                ax::dump(window, cx);
            }
        });
        if result.is_err() {
            log(&format!("command {command:?}: window update failed"));
        }
        return;
    }
    let result = handle.update(cx, |demo, window, cx| match command {
        "state" => log(&state_line("state", demo, window)),
        "hide" => {
            log(&state_line("before hide", demo, window));
            cx.hide();
        }
        "activate" => {
            log(&state_line("before activate", demo, window));
            cx.activate(true);
        }
        "ax" => ax::poke(),
        "press" => ax::press_button(),
        "quit" => {
            log("cmd quit");
            cx.quit();
        }
        other => log(&format!("unknown command {other:?}")),
    });
    if result.is_err() {
        log(&format!("command {command:?}: window update failed"));
    }
}

fn main() {
    let cmd_dir = std::env::args()
        .skip_while(|a| a != "--cmd-dir")
        .nth(1)
        .map(PathBuf::from);
    if let Some(dir) = &cmd_dir {
        // SAFETY: still single-threaded here, before the application starts.
        unsafe { std::env::set_var("SPIKE_CMD_DIR", dir) };
    }
    let app = application();
    app.on_reopen(|cx| {
        log(&format!(
            "on_reopen callback fired (windows={})",
            cx.windows().len()
        ));
    });
    app.run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("tab", Tab, None),
            KeyBinding::new("shift-tab", TabPrev, None),
        ]);
        let bounds = Bounds::centered(None, size(px(520.), px(460.)), cx);
        let Ok(handle) = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("abyss spike".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Demo::new(window, cx)),
        ) else {
            log("open_window failed");
            cx.quit();
            return;
        };
        cx.activate(true);
        log("window opened");
        let Some(dir) = cmd_dir.clone() else { return };
        cx.spawn(async move |cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let path = dir.join("cmd");
                let Ok(command) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let _ = std::fs::remove_file(&path);
                let command = command.trim().to_owned();
                cx.update(|cx| run_command(&command, cx, handle));
            }
        })
        .detach();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    /// Does the headless test platform ever build an accessibility tree?
    #[gpui::test]
    fn test_platform_a11y_tree(cx: &mut TestAppContext) {
        let (_demo, cx) = cx.add_window_view(Demo::new);
        cx.run_until_parked();
        cx.draw(
            gpui::point(px(0.), px(0.)),
            size(px(520.), px(460.)),
            |_, _| div(),
        );
        let (enabled, active, json) = cx.update(|window, _| {
            (
                window.is_a11y_enabled(),
                window.is_a11y_active(),
                window.debug_a11y_tree_json(),
            )
        });
        println!(
            "test platform: is_a11y_enabled={enabled} is_a11y_active={active} debug_a11y_tree_json={:?}",
            json.as_ref().map(String::len)
        );
        assert!(enabled, "accessibility must not be force-disabled in tests");
        // The point of the spike: record what the test platform gives, whichever way it goes.
        println!("RESULT tree_available_in_tests={}", json.is_some());
    }
}
