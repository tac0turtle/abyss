//! Archiving and chat folder editing (folders plan, part C): a small menu
//! opened by right-clicking a chat row or a folder tab. Every choice is one
//! core command; the list changes only when core reports the change
//! (`ChatBumped`, `ChatFolders`), never ahead of it.

use std::cell::Cell;
use std::rc::Rc;

use abyss_core::api::{Command, UserError};
use abyss_core::model::{
    ChatFolder, ChatId, ChatSummary, FolderEdit, FolderTag, PeerFolder, ReadRule,
};
use gpui::{
    AnyElement, App, Context, Div, Entity, FocusHandle, Focusable, MouseButton, MouseDownEvent,
    Pixels, Point, Role, SharedString, Size, Stateful, Subscription, Window, canvas, div, point,
    prelude::*, px,
};

use super::session::Session;
use super::view::field;
use super::{DismissOverlay, MenuNext, MenuPrevious, OpenContextMenu, Root};
use crate::chat::copy;
use crate::ui::a11y::{self, FocusDirection};
use crate::ui::text_input::{Limits, TextInput, TextInputEvent, text_input};
use crate::ui::theme::{self, Depth, Theme};

/// Width of the menu.
const MENU_WIDTH: f32 = 240.;
/// Height of one menu entry, for keeping the menu inside the window.
const ENTRY_HEIGHT: f32 = 30.;

/// What the open menu is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Menu {
    /// A chat row's menu. `naming`: the "New folder" entry became a title
    /// field.
    Chat {
        chat: ChatId,
        at: Frame,
        naming: Naming,
    },
    /// A folder tab's menu. `tag`: the folder's visible tag captured when
    /// the menu opened; a delete carries that tag. `confirm`: Delete was
    /// clicked once.
    Folder {
        id: i32,
        tag: FolderTag,
        at: Frame,
        confirm: Confirm,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Naming {
    No,
    Yes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Confirm {
    No,
    Pending,
}

/// The folders a chat menu can change: regular ones (shared folders are
/// read-only here).
fn editable(folders: &[ChatFolder]) -> impl Iterator<Item = &ChatFolder> {
    folders.iter().filter(|f| !f.shared)
}

/// Where an open menu sits and how tall it may grow: it scrolls past
/// `max_h`, so every entry stays reachable in a short window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Frame {
    pub(crate) origin: Point<Pixels>,
    pub(crate) max_h: Pixels,
}

impl Eq for Frame {}

/// Convert the pointer's window coordinates to the session's content
/// coordinates, then fit the menu below the integrated title bar.
fn placed(at: Point<Pixels>, entries: usize, window: &Window) -> Frame {
    let inset = super::titlebar_inset(window);
    let mut viewport = window.viewport_size();
    viewport.height = (viewport.height - inset).max(px(0.));
    frame(at - point(px(0.), inset), entries, viewport)
}

/// [`placed`] for a viewport of `size`: moved up and left to fit, and no
/// taller than the viewport below its top.
fn frame(at: Point<Pixels>, entries: usize, size: Size<Pixels>) -> Frame {
    let rows = f32::from(u16::try_from(entries).unwrap_or(u16::MAX));
    let height = px(rows * ENTRY_HEIGHT + 16.).min(size.height);
    let x = at.x.min(size.width - px(MENU_WIDTH)).max(px(0.));
    let y = at.y.min(size.height - height).max(px(0.));
    Frame {
        origin: point(x, y),
        max_h: (size.height - y).max(px(ENTRY_HEIGHT)),
    }
}

/// The new-folder title field and its Enter subscription.
pub(super) fn title_field(
    window: &mut Window,
    cx: &mut Context<'_, Root>,
) -> (Entity<TextInput>, Subscription) {
    let field = text_input(Limits::FOLDER_TITLE, copy::FOLDER_NAME_PLACEHOLDER, cx);
    field.update(cx, |input, _| input.set_label("Folder name"));
    let sub = cx.subscribe_in(&field, window, |root, _, _: &TextInputEvent, window, cx| {
        root.with_session(|s, cx| s.submit_folder_name(window, cx), cx);
    });
    (field, sub)
}

impl Session {
    /// Right-click on a chat row.
    pub(super) fn open_chat_menu(
        &mut self,
        chat: ChatId,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        if self.shell.list.get(chat).is_none() {
            return;
        }
        let entries = 2 + editable(self.shell.list.folders()).count();
        self.menu = Some(Menu::Chat {
            chat,
            at: placed(at, entries, window),
            naming: Naming::No,
        });
        self.menu_focus.open(window, cx);
        cx.notify();
    }

    /// Right-click on a folder tab.
    pub(super) fn open_folder_menu(
        &mut self,
        id: i32,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        let Some(folder) = self.shell.list.folders().iter().find(|f| f.id == id) else {
            return;
        };
        if folder.shared {
            self.show_notice(copy::SHARED_FOLDER.into(), window, cx);
            return;
        }
        self.menu = Some(Menu::Folder {
            id,
            tag: folder.tag(),
            at: placed(at, 1, window),
            confirm: Confirm::No,
        });
        self.menu_focus.open(window, cx);
        cx.notify();
    }

    pub(super) fn close_menu(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if self.menu.take().is_some() {
            self.menu_focus.close(window, cx);
            self.overlay_closed(window, cx);
            self.folder_name
                .update(cx, |input, cx| input.clear(window, cx));
            cx.notify();
        }
    }

    /// Sends a folder command and closes the menu. A refusal (core busy or
    /// gone) is shown; the outcome arrives as an event.
    fn menu_command(&mut self, cmd: Command, window: &mut Window, cx: &mut Context<'_, Root>) {
        self.close_menu(window, cx);
        self.send(cmd, window, cx);
    }

    pub(super) fn set_peer_folder(
        &mut self,
        chat: ChatId,
        folder: PeerFolder,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        self.menu_command(Command::SetPeerFolder { chat, folder }, window, cx);
    }

    pub(super) fn edit_folder(
        &mut self,
        edit: FolderEdit,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        self.menu_command(Command::EditChatFolder(edit), window, cx);
    }

    /// "New folder with this chat…": the entry turns into a title field.
    pub(super) fn start_naming(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if let Some(Menu::Chat { naming, .. }) = &mut self.menu {
            *naming = Naming::Yes;
            window.focus(&self.folder_name.focus_handle(cx), cx);
            self.menu_focus.scroll.scroll_to_bottom();
            cx.notify();
        }
    }

    /// Enter in the title field: creates the folder with the menu's chat.
    pub(super) fn submit_folder_name(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        let Some(Menu::Chat {
            chat,
            naming: Naming::Yes,
            ..
        }) = self.menu
        else {
            return;
        };
        let title = self.folder_name.read(cx).text().to_owned();
        if abyss_core::model::folder_title(&title).is_none() {
            self.show_notice(copy::FOLDER_TITLE_INVALID.into(), window, cx);
            return;
        }
        self.edit_folder(FolderEdit::Create { title, chat }, window, cx);
    }

    /// Delete on a folder tab: the first click asks, the second deletes the
    /// folder the menu opened for. If the folder visibly changed since (its
    /// tag differs: renamed, rules changed, or replaced by a different-looking
    /// folder with the same id), nothing is deleted. Best-effort: an
    /// identical replacement is not told apart.
    pub(super) fn delete_folder(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        let Some(Menu::Folder {
            id, tag, confirm, ..
        }) = self.menu.clone()
        else {
            return;
        };
        if !self.folder_is(id, &tag) {
            self.close_menu(window, cx);
            self.show_notice(
                crate::ui::copy::user_error(UserError::FolderChanged),
                window,
                cx,
            );
            return;
        }
        match confirm {
            Confirm::Pending => {
                self.edit_folder(FolderEdit::Delete { folder: id, tag }, window, cx);
            }
            Confirm::No => {
                if let Some(Menu::Folder { confirm, .. }) = &mut self.menu {
                    *confirm = Confirm::Pending;
                }
                cx.notify();
            }
        }
    }

    /// Whether folder `id` is still the folder `tag` names.
    fn folder_is(&self, id: i32, tag: &FolderTag) -> bool {
        self.shell
            .list
            .folders()
            .iter()
            .any(|f| f.id == id && f.tag() == *tag)
    }

    /// Close a menu whose chat or folder disappeared or whose folder tag changed.
    pub(super) fn recheck_menu(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        let stale = match &self.menu {
            Some(Menu::Chat { chat, .. }) => self.shell.list.get(*chat).is_none(),
            Some(Menu::Folder { id, tag, .. }) => !self.folder_is(*id, tag),
            None => false,
        };
        if stale {
            self.close_menu(window, cx);
        }
    }
}

/// A context menu has a keyboard shortcut on its row and an `AXPress`
/// target for `VoiceOver`. The pinned adapter has no `AXShowMenu` action.
pub(super) fn trigger(
    element: Stateful<Div>,
    title: &str,
    focus: &FocusHandle,
    open: impl Fn(Point<Pixels>, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let anchor = Rc::new(Cell::new(Point::default()));
    let measure = Rc::clone(&anchor);
    let trigger = focus.clone();
    let open = Rc::new(move |at, window: &mut Window, cx: &mut App| {
        trigger.focus(window, cx);
        open(at, window, cx);
    });
    let mouse = Rc::clone(&open);
    let key = Rc::clone(&open);
    let key_anchor = Rc::clone(&anchor);
    element
        .relative()
        .aria_keyshortcuts("Shift+F10")
        .on_action(move |_: &OpenContextMenu, window, cx| key(key_anchor.get(), window, cx))
        .on_mouse_down(MouseButton::Right, move |ev, window, cx| {
            mouse(ev.position, window, cx);
        })
        .child(a11y::hidden_action(
            "context-menu-trigger",
            format!("Open menu for {title}"),
            move |window, cx| open(anchor.get(), window, cx),
        ))
        .child(
            canvas(
                move |bounds, _, _| measure.set(bounds.bottom_left()),
                |_, (), _, _| {},
            )
            .absolute()
            .inset_0(),
        )
}

/// One clickable menu entry. The caller supplies the role and name.
fn entry(
    element: Stateful<Div>,
    label: impl IntoElement,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
    activate: impl Fn(&mut Session, &mut Window, &mut Context<'_, Root>) + 'static,
) -> Stateful<Div> {
    let root = cx.weak_entity();
    let control = a11y::focus_ring(element, theme.text)
        .key_context("MenuChoices")
        .h(px(ENTRY_HEIGHT))
        .flex_none()
        .px_3()
        .flex()
        .items_center()
        .rounded(px(theme::RADIUS))
        .cursor_pointer()
        .hover(|d| d.bg(theme::faded(theme.text, theme::MENU_HOVER)))
        .child(label);
    a11y::on_activate(control, move |window, cx| {
        root.update(cx, |root, cx| {
            root.with_session(|s, cx| activate(s, window, cx), cx)
        })
        .ok();
    })
}

/// The open menu over everything, with a backdrop that closes it.
pub(super) fn menu_overlay(
    session: &Session,
    theme: &Theme,
    window: &Window,
    cx: &mut Context<'_, Root>,
) -> Option<AnyElement> {
    let (at, title, entries) = match session.menu.as_ref()? {
        Menu::Chat { chat, at, naming } => (
            *at,
            session.shell.list.get(*chat)?.title.as_str(),
            chat_entries(session, *chat, *naming, theme, window, cx)?,
        ),
        Menu::Folder {
            id,
            tag,
            at,
            confirm,
        } => {
            let folder = session
                .shell
                .list
                .folders()
                .iter()
                .find(|f| f.id == *id && f.tag() == *tag)?;
            (
                *at,
                folder.title.as_str(),
                folder_entries(folder, *confirm, theme, cx),
            )
        }
    };
    let close =
        |root: &mut Root, _: &MouseDownEvent, window: &mut Window, cx: &mut Context<'_, Root>| {
            root.with_session(|s, cx| s.close_menu(window, cx), cx);
        };
    let backdrop = div()
        .id("menu-backdrop")
        .absolute()
        .inset_0()
        .occlude()
        .on_mouse_down(MouseButton::Left, cx.listener(close))
        .on_mouse_down(MouseButton::Right, cx.listener(close));
    let panel = menu_panel(session, at, title, theme, cx)
        .children(entries)
        .child(session.menu_focus.group.end());
    session.menu_focus.group.enter_after_draw(window, cx);
    Some(
        div()
            .absolute()
            .inset_0()
            .child(backdrop)
            .child(panel)
            .into_any_element(),
    )
}

fn menu_panel(
    session: &Session,
    at: Frame,
    title: &str,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> Stateful<Div> {
    let group = &session.menu_focus.group;
    let scroll = session.menu_focus.scroll.clone();
    let next = group.clone();
    let previous = group.clone();
    group
        .frame(a11y::labelled(
            div(),
            "menu",
            Role::Menu,
            format!("Actions for {title}"),
        ))
        .key_context("Overlay")
        .absolute()
        .left(at.origin.x)
        .top(at.origin.y)
        .w(px(MENU_WIDTH))
        .max_h(at.max_h)
        .overflow_y_scroll()
        .track_scroll(&scroll)
        .p_1()
        .flex()
        .flex_col()
        .text_sm()
        .bg(theme.surface(Depth::NOW))
        .text_color(theme.text)
        .border_1()
        .border_color(theme.line(theme.text))
        .rounded(px(theme::RADIUS))
        .shadow_md()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_action(move |action: &a11y::RevealFocus, window, _| action.scroll_page(&scroll, window))
        .on_action(move |_: &MenuNext, window, cx| next.step(FocusDirection::Next, window, cx))
        .on_action(move |_: &MenuPrevious, window, cx| {
            previous.step(FocusDirection::Previous, window, cx);
        })
        .on_action(cx.listener(|root, _: &DismissOverlay, window, cx| {
            root.with_session(|s, cx| s.close_menu(window, cx), cx);
        }))
}

/// Archive or Unarchive, a toggle per editable folder, and New folder.
fn chat_entries(
    session: &Session,
    chat: ChatId,
    naming: Naming,
    theme: &Theme,
    window: &Window,
    cx: &mut Context<'_, Root>,
) -> Option<Vec<AnyElement>> {
    let summary = session.shell.list.get(chat)?;
    let (name, label, target) = match summary.folder {
        PeerFolder::Main => ("Archive chat", copy::ARCHIVE_CHAT, PeerFolder::Archive),
        PeerFolder::Archive => ("Unarchive chat", copy::UNARCHIVE_CHAT, PeerFolder::Main),
    };
    let mut out = vec![
        entry(
            a11y::control("menu-archive", Role::MenuItem, name),
            label,
            theme,
            cx,
            move |s, w, cx| {
                s.set_peer_folder(chat, target, w, cx);
            },
        )
        .into_any_element(),
    ];
    out.extend(
        editable(session.shell.list.folders())
            .map(|folder| membership_entry(folder, summary, theme, cx).into_any_element()),
    );
    let last = match naming {
        Naming::No => entry(
            a11y::control("menu-new", Role::MenuItem, copy::NEW_FOLDER),
            copy::NEW_FOLDER,
            theme,
            cx,
            |s, w, cx| {
                s.start_naming(w, cx);
            },
        )
        .debug_selector(|| "menu-new".to_owned())
        .into_any_element(),
        Naming::Yes => {
            let ink = theme.on_incoming;
            let edge = if session.folder_name.focus_handle(cx).is_focused(window) {
                ink
            } else {
                theme.line(ink)
            };
            div()
                .p_1()
                .child(field(
                    session.folder_name.clone().into_any_element(),
                    edge,
                    theme,
                ))
                .into_any_element()
        }
    };
    out.push(last);
    Some(out)
}

fn membership_entry(
    folder: &ChatFolder,
    summary: &ChatSummary,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> Stateful<Div> {
    let chat = summary.id;
    let (id, tag) = (folder.id, folder.tag());
    // Folder editing sees Telegram's rule, nothing waived.
    let inside = folder.contains(summary, crate::chat::format::now(), ReadRule::Applies);
    let label = div()
        .flex()
        .child(div().w(px(18.)).flex_none().children(inside.then_some("✓")))
        .child(folder.title.clone());
    let toggle = a11y::switch(
        ("menu-folder", u64::from(id.cast_unsigned())),
        format!("Include in {}", folder.title),
        a11y::toggled(inside),
    );
    entry(toggle, label, theme, cx, move |s, w, cx| {
        let tag = tag.clone();
        let edit = if inside {
            FolderEdit::RemoveChat {
                folder: id,
                tag,
                chat,
            }
        } else {
            FolderEdit::AddChat {
                folder: id,
                tag,
                chat,
            }
        };
        s.edit_folder(edit, w, cx);
    })
}

/// Delete, confirmed by a second click.
fn folder_entries(
    folder: &ChatFolder,
    confirm: Confirm,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> Vec<AnyElement> {
    let label: SharedString = match confirm {
        Confirm::No => copy::delete_folder(&folder.title),
        Confirm::Pending => copy::confirm_delete_folder(&folder.title),
    };
    let control = a11y::control("menu-delete", Role::MenuItem, label.clone());
    vec![entry(control, label, theme, cx, Session::delete_folder).into_any_element()]
}
