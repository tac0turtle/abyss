//! The chat and folder tabs, as a vertical rail beside the chat list.

use std::cell::Cell;

use gpui::{
    ClickEvent, Context, Div, ElementId, Hsla, MouseButton, MouseDownEvent, ScrollHandle,
    SharedString, Stateful, div, prelude::*, px, transparent_black,
};

use super::{Root, session::Session};
use crate::chat::copy;
use crate::chat::list::{ChatList, Shown, Tab, View};
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
}

impl Tabs {
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
        .map(|tab| tab_row(tab, shown, list, theme, cx))
        .collect();
    let scroll = div()
        .id("tabs")
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .py_2()
        .overflow_y_scroll()
        .track_scroll(&session.tabs.scroll)
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
        .border_color(theme::faded(theme.text, 0.08))
        .child(scroll)
        .child(unread_toggle(list.view(), theme, cx))
}

/// One tab: full ink and a rule on its leading edge when selected, quiet
/// ink otherwise. No hue.
fn tab_row(
    tab: Tab,
    shown: Option<Tab>,
    list: &ChatList,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> Stateful<Div> {
    let selected = shown == Some(tab);
    rail_item(
        tab_id(tab),
        if selected { Mark::On } else { Mark::Off },
        theme,
    )
    .child(tab_title(tab, list))
    .when(selected, |d| d.debug_selector(|| "selected-tab".to_owned()))
    .on_click(cx.listener(move |root, _: &ClickEvent, window, cx| {
        root.with_session(|s, cx| s.show_chats(Shown::Tab(tab), window, cx), cx);
    }))
    // Folder tabs keep their menu; built-in tabs have none.
    .when_some(folder_id(tab), |d, folder| {
        d.on_mouse_down(
            MouseButton::Right,
            cx.listener(move |root, ev: &MouseDownEvent, window, cx| {
                root.with_session(
                    |s, cx| s.open_folder_menu(folder, ev.position, window, cx),
                    cx,
                );
            }),
        )
    })
}

/// The unread filter's toggle (plan section 17.4): on, it carries the ink
/// rule and full ink like a selected tab; off, quiet ink. No hue.
fn unread_toggle(view: View, theme: &Theme, cx: &mut Context<'_, Root>) -> Div {
    let toggle = rail_item(
        "unread-toggle".into(),
        if view.unread_only {
            Mark::On
        } else {
            Mark::Off
        },
        theme,
    )
    .debug_selector(|| "unread-toggle".to_owned())
    .child(copy::UNREAD_FILTER)
    .on_click(cx.listener(|root, _: &ClickEvent, window, cx| {
        root.with_session(|s, cx| s.toggle_unread(window, cx), cx);
    }));
    div()
        .flex_none()
        .py_2()
        .border_t_1()
        .border_color(theme::faded(theme.text, 0.08))
        .child(toggle)
}

/// Whether a rail item is selected (a tab) or on (the toggle).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    On,
    Off,
}

fn rail_item(id: ElementId, mark: Mark, theme: &Theme) -> Stateful<Div> {
    let rule: Hsla = match mark {
        Mark::On => theme.accent.into(),
        Mark::Off => transparent_black(),
    };
    div()
        .id(id)
        .flex_none()
        .min_w_0()
        .pl_1()
        // Balances the 2px leading rule so the label sits on the centre line.
        .pr(px(6.))
        .py_1()
        .border_l_2()
        .border_color(rule)
        .text_sm()
        .truncate()
        .text_center()
        .cursor_pointer()
        .hover(|d| d.bg(theme::faded(theme.text, 0.06)))
        .when(mark == Mark::Off, |d| {
            d.text_color(theme::quiet(theme.text))
        })
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
