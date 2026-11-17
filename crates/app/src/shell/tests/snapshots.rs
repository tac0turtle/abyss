//! Renders the main window's states to PNG files, so the look can be checked
//! without a person watching a window. Regenerate with:
//!
//! ```sh
//! ABYSS_SNAPSHOT_DIR=/path/to/shots \
//!     cargo test -p abyss --features snapshots shell::tests::snapshots -- --nocapture
//! ```
//!
//! Without `ABYSS_SNAPSHOT_DIR` the test prints a note and does nothing.
//!
//! No window is shown: GPUI's `HeadlessAppContext` drives the views on its
//! test platform with the real macOS text system (`CoreText`) and an
//! offscreen Metal renderer, and reads the frame back from a texture. The
//! test platform's windows are always light, so night is forced through
//! [`theme::forced`], which only exists in this feature's test builds.
//! Shots are 1280x800 points at the test window's 2x scale (2560x1600 px).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use abyss_core::api::{AuthState, Event, HistoryDir};
use abyss_core::model::ChatId;
use abyss_core::store::ViewGen;
use gpui::{
    AnyWindowHandle, AppContext, HeadlessAppContext, Modifiers, MouseMoveEvent, PlatformInput,
    ScrollDelta, ScrollEvent, Window, WindowHandle, point, px, size,
};

use super::{Harness, link};
use crate::chat::list::Shown;
use crate::chat::{demo, format};
use crate::shell::Root;
use crate::shell::session::Started;
use crate::ui::theme::{self, Mode, Settings, Themes};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const DIR_VAR: &str = "ABYSS_SNAPSHOT_DIR";
/// The formatting showcase: every entity, media kind and message shape.
const SHOWCASE: ChatId = ChatId {
    kind: abyss_core::model::PeerKind::User,
    id: 1,
};
/// Messages loaded into the open chat: more than one screen.
const TAIL: usize = 80;
/// How far the list scrolls for the deep shot: about 155 rows of 56 points,
/// so the rows straddle the six-to-seven-days-old boundary.
const DEEP_SCROLL: f32 = 8_700.;
/// How much further for the flip shot: to about row 715 (30 days old).
const FLIP_SCROLL: f32 = 31_400.;

/// One window on the headless platform, driven through the core double.
struct Shot {
    cx: HeadlessAppContext,
    root: WindowHandle<Root>,
    harness: Harness,
    dir: PathBuf,
}

impl Shot {
    fn open(dir: &Path) -> Result<Self> {
        let mut cx = HeadlessAppContext::with_platform(
            Arc::new(gpui_apple::AppleTextSystem::new()),
            Arc::new(()),
            gpui_platform::current_headless_renderer,
        );
        // Defaults, kept in memory: never the user's saved choice.
        cx.update(|cx| cx.set_global(Themes::new(Settings::default(), None)));
        let (commands, events, harness) = link();
        let started = Started {
            commands,
            events,
            fake: None,
        };
        let root = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
            cx.new(|cx| Root::with_started(Ok(started), window, cx))
        })?;
        cx.run_until_parked();
        Ok(Self {
            cx,
            root,
            harness,
            dir: dir.to_path_buf(),
        })
    }

    fn any(&self) -> AnyWindowHandle {
        self.root.into()
    }

    /// Core sends `event`; the UI takes it in.
    fn core_says(&mut self, event: Event) -> Result<()> {
        self.harness
            .events
            .unbounded_send(event)
            .map_err(|_| "the event channel closed")?;
        self.cx.run_until_parked();
        Ok(())
    }

    fn input(&mut self, event: PlatformInput) -> Result<()> {
        self.cx.update_window(self.any(), |_, window, cx| {
            window.dispatch_event(event, cx);
        })?;
        self.cx.run_until_parked();
        Ok(())
    }

    /// Draws a fresh frame in `mode` (twice, so lists measured on the first
    /// frame lay out on the second) and writes it to `name`.
    fn save(&mut self, mode: Mode, name: &str) -> Result<()> {
        theme::forced::set(Some(mode));
        for _ in 0..2 {
            self.cx.update_window(self.any(), |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            })?;
            self.cx.run_until_parked();
        }
        let image = self.cx.capture_screenshot(self.any())?;
        theme::forced::set(None);
        let path = self.dir.join(name);
        image.save(&path)?;
        println!(
            "wrote {} ({}x{})",
            path.display(),
            image.width(),
            image.height()
        );
        Ok(())
    }

    fn sign_in(&mut self) -> Result<()> {
        self.core_says(Event::Auth(AuthState::SignedIn {
            display_name: "Demo User".into(),
            new_login: None,
        }))?;
        let script = demo::script(format::now());
        self.core_says(Event::DialogsReset(script.dialogs))?;
        Ok(())
    }

    /// Opens the showcase chat and loads its newest messages.
    fn open_showcase(&mut self) -> Result<()> {
        self.root.update(&mut self.cx, |root, window, cx| {
            root.with_session(|s, cx| s.select_chat(SHOWCASE, window, cx), cx);
        })?;
        self.cx.run_until_parked();
        let mut history = demo::script(format::now()).history;
        let messages = history.remove(&SHOWCASE).unwrap_or_default();
        let start = messages.len().saturating_sub(TAIL);
        self.core_says(Event::History {
            chat: SHOWCASE,
            view: ViewGen(1),
            dir: HistoryDir::Tail,
            messages: messages.into_iter().skip(start).collect(),
            removed: Vec::new(),
            has_more: true,
        })
    }

    fn toggle_appearance(&mut self) -> Result<()> {
        self.root.update(&mut self.cx, |root, _: &mut Window, cx| {
            root.toggle_appearance(cx);
        })?;
        self.cx.run_until_parked();
        Ok(())
    }

    /// Scrolls the chat list with the pointer over it, then moves the
    /// pointer to the chat pane so no row shows a hover.
    fn scroll_list(&mut self, by: f32) -> Result<()> {
        let over_list = point(px(150.), px(400.));
        let away = point(px(900.), px(400.));
        let moved = |position| {
            PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button: None,
                modifiers: Modifiers::default(),
            })
        };
        self.input(moved(over_list))?;
        self.input(PlatformInput::Scroll(ScrollEvent {
            position: over_list,
            delta: ScrollDelta::Pixels(point(px(0.), px(-by))),
            ..ScrollEvent::default()
        }))?;
        self.input(moved(away))
    }
}

fn signin(dir: &Path) -> Result<()> {
    let mut shot = Shot::open(dir)?;
    // No credentials on first launch: onboarding starts at Welcome.
    shot.core_says(Event::Auth(AuthState::NeedCredentials {
        error: None,
        source: None,
    }))?;
    shot.save(Mode::Day, "welcome-day.png")?;
    shot.save(Mode::Night, "welcome-night.png")?;
    shot.core_says(Event::Auth(AuthState::Phone { error: None }))?;
    shot.save(Mode::Day, "signin-day.png")?;
    shot.save(Mode::Night, "signin-night.png")
}

fn chat(dir: &Path) -> Result<()> {
    let mut shot = Shot::open(dir)?;
    shot.sign_in()?;
    shot.open_showcase()?;
    shot.save(Mode::Day, "chat-day.png")?;
    shot.save(Mode::Night, "chat-night.png")?;
    shot.toggle_appearance()?;
    shot.save(Mode::Night, "appearance-night.png")
}

fn list_deep(dir: &Path) -> Result<()> {
    let mut shot = Shot::open(dir)?;
    shot.sign_in()?;
    // A frame first, so the list's hitbox exists for the scroll to land on.
    shot.save(Mode::Day, "list-top-day.png")?;
    shot.scroll_list(DEEP_SCROLL)?;
    shot.save(Mode::Day, "list-deep-day.png")?;
    // On to rows a month old, where day mode flips to light text on dark.
    shot.scroll_list(FLIP_SCROLL)?;
    shot.save(Mode::Day, "list-flip-day.png")?;
    shot.save(Mode::Night, "list-flip-night.png")
}

/// The archive opened from the main list's entry.
fn archive(dir: &Path) -> Result<()> {
    let mut shot = Shot::open(dir)?;
    shot.sign_in()?;
    shot.root.update(&mut shot.cx, |root, window, cx| {
        root.with_session(|s, cx| s.show_chats(Shown::Archive, window, cx), cx);
    })?;
    shot.cx.run_until_parked();
    shot.save(Mode::Day, "archive-day.png")?;
    shot.save(Mode::Night, "archive-night.png")
}

#[test]
fn snapshots() -> Result<()> {
    let Some(dir) = std::env::var_os(DIR_VAR).map(PathBuf::from) else {
        println!("{DIR_VAR} is not set: no snapshots written");
        return Ok(());
    };
    std::fs::create_dir_all(&dir)?;
    signin(&dir)?;
    chat(&dir)?;
    archive(&dir)?;
    list_deep(&dir)
}
