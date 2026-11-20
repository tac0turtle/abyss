//! The chat and folder tabs, as a vertical rail beside the chat list.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use gpui::accesskit::Toggled;
use gpui::{
    App, Context, Div, ElementId, FocusHandle, Hsla, ScrollHandle, SharedString, Stateful, Window,
    div, prelude::*, px, transparent_black,
};

use super::{Root, session::Session};
use crate::chat::copy;
use crate::chat::list::{ChatList, Shown, Tab};
use crate::ui::a11y;
use crate::ui::theme::{self, Depth, Theme};

/// Narrow: wide enough for the built-in names; longer folder titles end
/// in an ellipsis.
const RAIL_WIDTH: f32 = 76.;

#[derive(Default)]
pub(super) struct Tabs {
    pub(super) scroll: ScrollHandle,
    /// The tab last scrolled into view. A selection reveals itself once, so
    /// scrolling the rail by hand isn't undone on the next frame.
    revealed: Cell<Option<Tab>>,
    handles: RefCell<HashMap<Tab, FocusHandle>>,
}

impl Tabs {
    fn handle(&self, tab: Tab, cx: &App) -> FocusHandle {
        self.handles
            .borrow_mut()
            .entry(tab)
            .or_insert_with(|| cx.focus_handle())
            .clone()
    }

    pub(super) fn move_focus(&self, from: Tab, to: Tab, window: &mut Window, cx: &mut App) {
        if self.handle(from, cx).is_focused(window) {
            self.handle(to, cx).focus(window, cx);
        }
    }

    /// Scrolls a newly shown tab into view: chosen by click, by keyboard, or
    /// restored once the account's folders arrive.
    fn reveal(&self, shown: Option<Tab>, list: &ChatList) {
        if self.revealed.get() == shown {
            return;
        }
        self.revealed.set(shown);
        if let Some(ix) = shown.and_then(|tab| list.tabs().position(|t| t == tab)) {
            self.scroll.scroll_to_item(ix);
        }
    }
}

/// The rail left of the chat list: All, Private, Groups and Forums, then
/// each chat folder in server order (plan section 17.2). The tabs scroll
/// vertically; the unread toggle sits at the rail's foot, outside the
/// scroll. In the archive the rail stays, with no tab selected, so the
/// list doesn't shift sideways; choosing a tab leaves the archive.
pub(super) fn rail(session: &Session, theme: &Theme, cx: &mut Context<'_, Root>) -> Div {
    let list = &session.shell.list;
    let shown = match list.shown() {
        Shown::Tab(tab) => Some(tab),
        Shown::Archive => None,
    };
    session.tabs.reveal(shown, list);
    let tabs: Vec<_> = list
        .tabs()
        .map(|tab| tab_row(tab, shown, session, theme, cx))
        .collect();
    session.tabs.handles.borrow_mut().retain(
        |tab, _| !matches!(tab, Tab::Folder(id) if !list.folders().iter().any(|f| f.id == *id)),
    );
    let scroll_handle = session.tabs.scroll.clone();
    let scroll = a11y::tab_list("tabs", copy::CHAT_TABS)
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .py_2()
        .overflow_y_scroll()
        .track_scroll(&scroll_handle)
        .on_action(move |action: &a11y::RevealFocus, window, _| {
            action.scroll_page(&scroll_handle, window);
        })
        .children(tabs);
    div()
        .w(px(RAIL_WIDTH))
        .flex_none()
        .h_full()
        .flex()
        .flex_col()
        .bg(theme.surface(Depth::NOW))
        .text_color(theme.text)
        .border_r_1()
        .border_color(theme.rule(theme.text, 0.08))
        .child(scroll)
        .child(unread_toggle(list, theme, cx))
}

/// One tab: full ink and a rule on its leading edge when selected, quiet
/// ink otherwise. No hue. Only the selected tab is in the tab order (All in
/// the archive, where none is): Tab crosses the strip in one step, and
/// Ctrl+Tab moves the selection.
fn tab_row(
    tab: Tab,
    shown: Option<Tab>,
    session: &Session,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> Stateful<Div> {
    let list = &session.shell.list;
    let selected = shown == Some(tab);
    let stop = selected || (shown.is_none() && tab == Tab::All);
    let title = tab_title(tab, list);
    let unread = list.unread_chats(Shown::Tab(tab));
    let label = copy::tab_label(&title, unread);
    let focus = session.tabs.handle(tab, cx).tab_stop(stop);
    let row = rail_item(
        a11y::tab(tab_id(tab), label)
            .track_focus(&focus)
            .aria_selected(selected),
        if selected { Mark::On } else { Mark::Off },
        title.clone(),
        theme,
    )
    .when(selected, |d| d.debug_selector(|| "selected-tab".to_owned()));
    let row = a11y::on_activate_in(row, cx, move |root, window, cx| {
        root.with_session(|s, cx| s.show_chats(Shown::Tab(tab), window, cx), cx);
    });
    let Some(folder) = folder_id(tab) else {
        return row;
    };
    let root = cx.weak_entity();
    super::folder_menu::trigger(row, &title, &focus, move |at, window, cx| {
        root.update(cx, |root, cx| {
            root.with_session(|s, cx| s.open_folder_menu(folder, at, window, cx), cx);
        })
        .ok();
    })
}

/// The unread filter's toggle (plan section 17.4): on, it carries the ink
/// rule and full ink like a selected tab; off, quiet ink. No hue. It says
/// how many chats of the shown view are unread.
fn unread_toggle(list: &ChatList, theme: &Theme, cx: &mut Context<'_, Root>) -> Div {
    let view = list.view();
    let label = copy::unread_filter_label(list.unread_chats(view.shown));
    let toggle = rail_item(
        a11y::switch(
            "unread-toggle",
            label,
            if view.unread_only {
                Toggled::True
            } else {
                Toggled::False
            },
        ),
        if view.unread_only {
            Mark::On
        } else {
            Mark::Off
        },
        copy::UNREAD_FILTER.into(),
        theme,
    )
    .debug_selector(|| "unread-toggle".to_owned());
    let toggle = a11y::on_activate_in(toggle, cx, |root, window, cx| {
        root.with_session(|s, cx| s.toggle_unread(window, cx), cx);
    });
    div()
        .flex_none()
        .py_2()
        .border_t_1()
        .border_color(theme.rule(theme.text, 0.08))
        .child(toggle)
}

/// Whether a rail item is selected (a tab) or on (the toggle).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    On,
    Off,
}

/// A rail item around `control`: its two-point focus ring, with the rule and
/// the title inside. The ring and the rule are apart, so selecting never
/// moves the ring.
fn rail_item(
    control: Stateful<Div>,
    mark: Mark,
    title: SharedString,
    theme: &Theme,
) -> Stateful<Div> {
    let rule: Hsla = match mark {
        Mark::On => theme.accent.into(),
        Mark::Off => transparent_black(),
    };
    let label = div()
        .pl_1()
        // Balances the 2px leading rule so the label sits on the centre line.
        .pr(px(6.))
        .py_1()
        .border_l_2()
        .border_color(rule)
        .text_sm()
        .truncate()
        .text_center()
        .when(mark == Mark::Off, |d| d.text_color(theme.quiet(theme.text)))
        .child(title);
    a11y::focus_ring(control, theme.text)
        .flex_none()
        .min_w_0()
        .cursor_pointer()
        .hover(|d| d.bg(theme::faded(theme.text, theme::TAB_HOVER)))
        .child(label)
}

/// A tab's stable element id: built-in tabs by name, folders by id.
fn tab_id(tab: Tab) -> ElementId {
    match tab {
        Tab::All => "tab-all".into(),
        Tab::Private => "tab-private".into(),
        Tab::Groups => "tab-groups".into(),
        Tab::Forums => "tab-forums".into(),
        Tab::Folder(id) => ("tab-folder", u64::from(id.cast_unsigned())).into(),
    }
}

/// What the shown view is called: its tab's name, or the archive's.
pub(super) fn view_title(shown: Shown, list: &ChatList) -> SharedString {
    match shown {
        Shown::Tab(tab) => tab_title(tab, list),
        Shown::Archive => copy::ARCHIVE.into(),
    }
}

fn tab_title(tab: Tab, list: &ChatList) -> SharedString {
    match tab {
        Tab::All => copy::ALL_CHATS.into(),
        Tab::Private => copy::PRIVATE_TAB.into(),
        Tab::Groups => copy::GROUPS_TAB.into(),
        Tab::Forums => copy::FORUMS_TAB.into(),
        Tab::Folder(id) => list
            .folders()
            .iter()
            .find(|f| f.id == id)
            .map(|f| SharedString::from(f.title.clone()))
            .unwrap_or_default(),
    }
}

/// The chat folder a tab shows, if it is one.
fn folder_id(tab: Tab) -> Option<i32> {
    match tab {
        Tab::All | Tab::Private | Tab::Groups | Tab::Forums => None,
        Tab::Folder(id) => Some(id),
    }
}
