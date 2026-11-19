//! Composite scenes (colourways plan, task 2): one frame each that puts
//! many theme roles side by side, so a colourway is judged on a busy screen
//! rather than a quiet one.

use std::path::Path;

use abyss_core::api::{Command, Event, HistoryDir, SendState};
use abyss_core::model::{ChatId, PeerKind, ThreadKey};
use abyss_core::store::ViewGen;
use gpui::{Focusable, Modifiers, MouseMoveEvent, PlatformInput, point, px};

use super::{Result, Shot};
use crate::chat::list::{Shown, Tab};
use crate::chat::{demo, format};
use crate::ui::text_input::{COMPOSER_MAX_UTF16, SelectAll};
use crate::ui::theme::Mode;
use crate::ui::theme::colourway::Colourway;

/// The demo's small group: senders on every incoming message.
const GROUP: ChatId = ChatId {
    kind: PeerKind::Chat,
    id: 4,
};
/// Messages loaded into the group.
const LOADED: usize = 24;

/// A group chat opened away from its tail: sender headers, reply quotes on
/// both sides, a failed send with its actions, the jump button over the
/// user's own bubble, and a composer long enough to count, all selected.
pub(super) fn group(dir: &Path, colourway: Colourway) -> Result<()> {
    let mut shot = Shot::open(dir, colourway)?;
    shot.sign_in()?;
    shot.root.update(&mut shot.cx, |root, window, cx| {
        root.with_session(|s, cx| s.select_chat(GROUP, window, cx), cx);
    })?;
    shot.cx.run_until_parked();
    load_group(&mut shot)?;
    failed_send(&mut shot)?;
    long_draft(&mut shot)?;
    shot.save(Mode::Day, "group-day.png")?;
    shot.save(Mode::Night, "group-night.png")
}

/// The group's newest messages, as a page around a read position with more
/// after it, so the list is detached and shows the jump button. The last
/// message is the user's own, and two messages quote others.
fn load_group(shot: &mut Shot) -> Result<()> {
    let mut messages = demo::script(format::now())
        .history
        .remove(&GROUP)
        .unwrap_or_default();
    let start = messages.len().saturating_sub(LOADED);
    let mut messages: Vec<_> = messages.drain(start..).collect();
    let own_quoted = messages.iter().find(|m| m.outgoing).map(|m| m.id);
    let incoming_quoted = messages.iter().find(|m| !m.outgoing).map(|m| m.id);
    // The newest incoming message before the last quotes the user.
    if let Some(m) = messages.iter_mut().rev().skip(1).find(|m| !m.outgoing) {
        m.reply_to = own_quoted;
    }
    if let Some(m) = messages.last_mut() {
        m.outgoing = true;
        m.sender = None;
        m.reply_to = incoming_quoted;
    }
    shot.core_says(Event::History {
        thread: ThreadKey::whole(GROUP),
        view: ViewGen(1),
        dir: HistoryDir::Around,
        messages,
        removed: Vec::new(),
        has_more: true,
    })
}

/// Sends a message that core admits and then fails.
fn failed_send(shot: &mut Shot) -> Result<()> {
    shot.root.update(&mut shot.cx, |root, window, cx| {
        root.with_session(
            |s, cx| {
                s.composer.update(cx, |c, cx| {
                    c.put_text("This one didn't go through", window, cx);
                });
                s.submit_message(window, cx);
            },
            cx,
        );
    })?;
    shot.cx.run_until_parked();
    let local_id = match shot.harness.sent.borrow().commands.last() {
        Some(Command::SendText { local_id, .. }) => *local_id,
        _ => return Err("the send was not dispatched".into()),
    };
    shot.core_says(Event::SendAdmitted { local_id })?;
    shot.core_says(Event::SendState {
        local_id,
        state: SendState::Failed,
    })
}

/// A draft past the counter's threshold, every character selected.
fn long_draft(shot: &mut Shot) -> Result<()> {
    let line = "A long draft that keeps going, line after line. ";
    let repeats = (COMPOSER_MAX_UTF16 - 300) / line.len();
    let draft = line.repeat(repeats);
    shot.root.update(&mut shot.cx, |root, window, cx| {
        root.with_session(
            |s, cx| {
                let handle = s.composer.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
                s.composer.update(cx, |c, cx| {
                    c.put_text(&draft, window, cx);
                });
            },
            cx,
        );
        window.dispatch_action(Box::new(SelectAll), cx);
    })?;
    shot.cx.run_until_parked();
    Ok(())
}

/// Where the pointer rests for the hover scene: on an unselected tab in the
/// rail, then on the menu's first entry.
const UNSELECTED_TAB: (f32, f32) = (37., 118.);
const MENU_ENTRY: (f32, f32) = (200., 140.);

/// Hover states: a selected tab beside a hovered unselected one, then a
/// hovered entry in a chat's menu.
pub(super) fn hover(dir: &Path, colourway: Colourway) -> Result<()> {
    let mut shot = Shot::open(dir, colourway)?;
    shot.sign_in()?;
    shot.root.update(&mut shot.cx, |root, window, cx| {
        root.with_session(
            |s, cx| s.show_chats(Shown::Tab(Tab::Forums), window, cx),
            cx,
        );
    })?;
    shot.cx.run_until_parked();
    // A frame first, so the hitboxes exist for the pointer to land on.
    shot.save(Mode::Day, "hover-tab-day.png")?;
    shot.input(pointer_at(UNSELECTED_TAB))?;
    shot.save(Mode::Day, "hover-tab-day.png")?;
    shot.save(Mode::Night, "hover-tab-night.png")?;
    let forum = ChatId {
        kind: PeerKind::Channel,
        id: 2,
    };
    shot.root.update(&mut shot.cx, |root, window, cx| {
        root.with_session(
            |s, cx| s.open_chat_menu(forum, point(px(160.), px(115.)), window, cx),
            cx,
        );
    })?;
    shot.cx.run_until_parked();
    shot.save(Mode::Day, "hover-menu-day.png")?;
    shot.input(pointer_at(MENU_ENTRY))?;
    shot.save(Mode::Day, "hover-menu-day.png")?;
    shot.save(Mode::Night, "hover-menu-night.png")
}

fn pointer_at((x, y): (f32, f32)) -> PlatformInput {
    PlatformInput::MouseMove(MouseMoveEvent {
        position: point(px(x), px(y)),
        pressed_button: None,
        modifiers: Modifiers::default(),
    })
}
