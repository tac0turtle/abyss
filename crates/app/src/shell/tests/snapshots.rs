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
//!
//! Every run renders the same pixels: the clock and the time zone are pinned
//! ([`format::forced`]) and the theme is kept in memory at its defaults, so
//! two runs on one machine can be compared byte for byte.
//!
//! Every scene is shot in each colourway: abyss under its plain names, the
//! others prefixed with their name (`sky-chat-day.png`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use abyss_core::api::{AuthState, Event, HistoryDir};
use abyss_core::model::{ChatId, ThreadKey};
use abyss_core::store::ViewGen;
use gpui::{
    AnyWindowHandle, AppContext, HeadlessAppContext, Modifiers, MouseMoveEvent, PlatformInput,
    ScrollDelta, ScrollEvent, Window, WindowHandle, point, px, size,
};

use super::{Harness, link};
use crate::chat::list::{Shown, Tab};
use crate::chat::{demo, format};
use crate::shell::Root;
use crate::shell::session::Started;
use crate::ui::theme::colourway::Colourway;
use crate::ui::theme::{self, Mode, Settings, Stores, Themes};

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
/// The pinned clock: 2026-11-16 14:05:00 UTC, a Monday.
const NOW: i64 = 1_794_837_900;

/// One window on the headless platform, driven through the core double.
struct Shot {
    cx: HeadlessAppContext,
    root: WindowHandle<Root>,
    harness: Harness,
    dir: PathBuf,
    /// Put before every file name: empty for abyss.
    prefix: String,
}

impl Shot {
    fn open(dir: &Path, colourway: Colourway) -> Result<Self> {
        let mut cx = HeadlessAppContext::with_platform(
            Arc::new(gpui_apple::AppleTextSystem::new()),
            Arc::new(()),
            gpui_platform::current_headless_renderer,
        );
        // Defaults, kept in memory: never the user's saved choice.
        cx.update(|cx| {
            cx.set_global(Themes::new(
                colourway,
                Settings::default(),
                Stores::default(),
            ));
        });
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
            prefix: match colourway {
                Colourway::Abyss => String::new(),
                other => format!("{}-", other.key()),
            },
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
        let path = self.dir.join(format!("{}{name}", self.prefix));
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
        self.core_says(Event::ChatFolders(script.folders))?;
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
            thread: ThreadKey::whole(SHOWCASE),
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

fn signin(dir: &Path, colourway: Colourway) -> Result<()> {
    let mut shot = Shot::open(dir, colourway)?;
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

fn chat(dir: &Path, colourway: Colourway) -> Result<()> {
    let mut shot = Shot::open(dir, colourway)?;
    shot.sign_in()?;
    shot.open_showcase()?;
    shot.save(Mode::Day, "chat-day.png")?;
    shot.save(Mode::Night, "chat-night.png")?;
    shot.toggle_appearance()?;
    shot.save(Mode::Day, "appearance-day.png")?;
    shot.save(Mode::Night, "appearance-night.png")
}

fn list_deep(dir: &Path, colourway: Colourway) -> Result<()> {
    let mut shot = Shot::open(dir, colourway)?;
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
fn archive(dir: &Path, colourway: Colourway) -> Result<()> {
    let mut shot = Shot::open(dir, colourway)?;
    shot.sign_in()?;
    shot.root.update(&mut shot.cx, |root, window, cx| {
        root.with_session(|s, cx| s.show_chats(Shown::Archive, window, cx), cx);
    })?;
    shot.cx.run_until_parked();
    shot.save(Mode::Day, "archive-day.png")?;
    shot.save(Mode::Night, "archive-night.png")
}

/// A built-in tab selected in the strip (plan section 17.2): the forums.
fn tabs(dir: &Path, colourway: Colourway) -> Result<()> {
    let mut shot = Shot::open(dir, colourway)?;
    shot.sign_in()?;
    shot.root.update(&mut shot.cx, |root, window, cx| {
        root.with_session(
            |s, cx| s.show_chats(Shown::Tab(Tab::Forums), window, cx),
            cx,
        );
    })?;
    shot.cx.run_until_parked();
    shot.save(Mode::Day, "tabs-day.png")?;
    shot.save(Mode::Night, "tabs-night.png")
}

/// The demo forum shown in the pane, its topics under its sidebar row
/// (plan section 17.3), pinned first, with one of them open.
fn inline_topics(dir: &Path, colourway: Colourway) -> Result<()> {
    use abyss_core::model::TopicOrder;
    use abyss_core::store::TopicsGen;

    let forum = ChatId {
        kind: abyss_core::model::PeerKind::Channel,
        id: 2,
    };
    let mut shot = Shot::open(dir, colourway)?;
    shot.sign_in()?;
    shot.root.update(&mut shot.cx, |root, window, cx| {
        root.with_session(|s, cx| s.select_chat(forum, window, cx), cx);
    })?;
    shot.cx.run_until_parked();
    // Core's first page, in its display order: pinned first, then by
    // activity.
    let mut topics: Vec<_> = demo::script(format::now())
        .topics
        .remove(&forum)
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t.visibility == abyss_core::model::Visibility::Shown)
        .map(|t| t.topic)
        .collect();
    topics.sort_by_key(|t| {
        (
            !t.pinned,
            std::cmp::Reverse(crate::chat::topics::activity(t)),
        )
    });
    topics.truncate(50);
    let open = topics.get(2).map(|t| t.id);
    shot.core_says(Event::TopicsReset {
        chat: forum,
        view: TopicsGen(1),
        order: TopicOrder::LastActivity,
        topics,
        can_post_closed: false,
    })?;
    shot.core_says(Event::TopicsPaging {
        chat: forum,
        view: TopicsGen(1),
        has_more: true,
    })?;
    if let Some(topic) = open {
        shot.root.update(&mut shot.cx, |root, window, cx| {
            root.with_session(|s, cx| s.select_topic(topic, window, cx), cx);
        })?;
        shot.cx.run_until_parked();
    }
    shot.save(Mode::Day, "inline-topics-day.png")?;
    shot.save(Mode::Night, "inline-topics-night.png")
}

/// The showcase opened at its first unread message (plan section 17.5):
/// core's page around the read position, the divider at the top of the
/// list. Unverified: written on Linux, where the snapshots don't build.
fn unread_divider(dir: &Path, colourway: Colourway) -> Result<()> {
    let mut shot = Shot::open(dir, colourway)?;
    shot.sign_in()?;
    shot.root.update(&mut shot.cx, |root, window, cx| {
        root.with_session(|s, cx| s.select_chat(SHOWCASE, window, cx), cx);
    })?;
    shot.cx.run_until_parked();
    let mut script = demo::script(format::now());
    let after = script
        .dialogs
        .iter()
        .find(|c| c.id == SHOWCASE)
        .map_or(abyss_core::model::MsgId(0), |c| c.read.read_inbox_max);
    let messages = script.history.remove(&SHOWCASE).unwrap_or_default();
    // Like core's page: 10 messages at or below the read position, 40
    // above it.
    let above = messages.partition_point(|m| m.id <= after);
    let start = above.saturating_sub(10);
    shot.core_says(Event::History {
        thread: ThreadKey::whole(SHOWCASE),
        view: ViewGen(1),
        dir: HistoryDir::Around,
        messages: messages.into_iter().skip(start).take(50).collect(),
        removed: Vec::new(),
        has_more: true,
    })?;
    shot.save(Mode::Day, "unread-divider-day.png")?;
    shot.save(Mode::Night, "unread-divider-night.png")
}

/// A chat folder's tab selected: its pinned chat first, then its own.
fn folder(dir: &Path, colourway: Colourway) -> Result<()> {
    let mut shot = Shot::open(dir, colourway)?;
    shot.sign_in()?;
    shot.root.update(&mut shot.cx, |root, window, cx| {
        root.with_session(
            |s, cx| s.show_chats(Shown::Tab(Tab::Folder(4)), window, cx),
            cx,
        );
    })?;
    shot.cx.run_until_parked();
    shot.save(Mode::Day, "folder-day.png")?;
    // Right-click on the Work folder's pinned forum: in Work only.
    let forum = ChatId {
        kind: abyss_core::model::PeerKind::Channel,
        id: 2,
    };
    shot.root.update(&mut shot.cx, |root, window, cx| {
        root.with_session(
            |s, cx| s.open_chat_menu(forum, point(px(160.), px(115.)), window, cx),
            cx,
        );
    })?;
    shot.cx.run_until_parked();
    shot.save(Mode::Day, "menu-day.png")?;
    shot.save(Mode::Night, "menu-night.png")
}

mod scenes;

#[test]
fn snapshots() -> Result<()> {
    let Some(dir) = std::env::var_os(DIR_VAR).map(PathBuf::from) else {
        println!("{DIR_VAR} is not set: no snapshots written");
        return Ok(());
    };
    std::fs::create_dir_all(&dir)?;
    let _pinned = Pinned::new();
    all(&dir)
}

/// The pinned clock, released on drop: on an error and on a panic alike, so
/// a test run on the same thread afterwards sees the real one.
struct Pinned;

impl Pinned {
    fn new() -> Self {
        format::forced::set(Some((NOW, jiff::tz::TimeZone::UTC)));
        Self
    }
}

impl Drop for Pinned {
    fn drop(&mut self) {
        format::forced::set(None);
        theme::forced::set(None);
    }
}

fn all(dir: &Path) -> Result<()> {
    for colourway in Colourway::ALL {
        signin(dir, colourway)?;
        chat(dir, colourway)?;
        archive(dir, colourway)?;
        tabs(dir, colourway)?;
        inline_topics(dir, colourway)?;
        unread_divider(dir, colourway)?;
        folder(dir, colourway)?;
        scenes::group(dir, colourway)?;
        scenes::hover(dir, colourway)?;
    }
    // Age only shows in abyss: every flat surface is the same.
    list_deep(dir, Colourway::Abyss)
}
