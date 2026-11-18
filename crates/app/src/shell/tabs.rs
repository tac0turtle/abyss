//! Scrollable chat and folder tabs, with explicit overflow navigation.

use gpui::{
    ClickEvent, Context, Div, ElementId, Hsla, MouseButton, MouseDownEvent, ScrollHandle,
    SharedString, Stateful, canvas, div, point, prelude::*, px, transparent_black,
};

use super::{Root, session::Session};
use crate::chat::copy;
use crate::chat::list::{ChatList, Shown, Step, Tab, View};
use crate::ui::theme::{self, Depth, Theme};

const ARROW_WIDTH: f32 = 18.;
const EDGE_EPSILON: f32 = 0.5;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Position {
    #[default]
    Fits,
    Start,
    Middle,
    End,
}

#[derive(Default)]
pub(super) struct Tabs {
    pub(super) scroll: ScrollHandle,
    position: Position,
    selected: Option<(Tab, usize)>,
}

impl Tabs {
    fn controls(&self) -> bool {
        self.position != Position::Fits
    }

    fn can_scroll(&self, step: Step) -> bool {
        matches!(
            (self.position, step),
            (Position::Middle, _) | (Position::Start, Step::Next) | (Position::End, Step::Previous)
        )
    }

    fn page(&self, step: Step) {
        let amount = (self.scroll.bounds().size.width * 0.8).max(px(80.));
        let offset = self.scroll.offset().x;
        let x = match step {
            Step::Previous => (offset + amount).min(px(0.)),
            Step::Next => (offset - amount).max(-self.scroll.max_offset().x),
        };
        self.scroll.set_offset(point(x, px(0.)));
    }

    /// Read geometry after paint, when scrolling and new labels are laid
    /// out. Subtract the controls' width so showing them cannot create its
    /// own permanent overflow when the tabs would otherwise fit.
    fn laid_out(&mut self, selected: Option<(Tab, usize)>, reserved: gpui::Pixels) -> bool {
        let mut changed = false;
        if self.selected != selected {
            self.selected = selected;
            if let Some((_, ix)) = selected {
                self.scroll.scroll_to_item(ix);
                changed = true;
            }
        }
        let max = self.scroll.max_offset().x;
        let offset = -self.scroll.offset().x;
        let position = if selected.is_none() || max <= reserved + px(EDGE_EPSILON) {
            Position::Fits
        } else if offset <= px(EDGE_EPSILON) {
            Position::Start
        } else if offset >= max - px(EDGE_EPSILON) {
            Position::End
        } else {
            Position::Middle
        };
        if self.position != position {
            self.position = position;
            changed = true;
        }
        changed
    }
}

fn after_paint(reserved: gpui::Pixels, cx: &Context<'_, Root>) -> impl IntoElement {
    let root = cx.weak_entity();
    canvas(
        |_, _, _| {},
        move |_, (), window, cx| {
            let root = root.clone();
            window.defer(cx, move |_, cx| {
                root.update(cx, |root, cx| {
                    root.with_session(
                        |s, cx| {
                            let selected = s
                                .shell
                                .list
                                .tabs()
                                .enumerate()
                                .find(|(_, tab)| s.shell.list.shown() == Shown::Tab(*tab))
                                .map(|(ix, tab)| (tab, ix));
                            if s.tabs.laid_out(selected, reserved) {
                                cx.notify();
                            }
                        },
                        cx,
                    );
                })
                .ok();
            });
        },
    )
    .absolute()
    .size_0()
}

fn arrow(step: Step, tabs: &Tabs, theme: &Theme, cx: &mut Context<'_, Root>) -> Stateful<Div> {
    let enabled = tabs.can_scroll(step);
    let (id, label) = match step {
        Step::Previous => ("tabs-previous", "‹"),
        Step::Next => ("tabs-next", "›"),
    };
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .w(px(ARROW_WIDTH))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .text_lg()
        .child(label)
        .when(!enabled, |d| d.opacity(0.25))
        .when(enabled, |d| {
            d.cursor_pointer()
                .hover(|d| d.bg(theme::faded(theme.text, 0.08)))
                .on_click(cx.listener(move |root, _: &ClickEvent, _, cx| {
                    root.with_session(
                        |s, cx| {
                            s.tabs.page(step);
                            cx.notify();
                        },
                        cx,
                    );
                }))
        })
}

/// The strip under the heading: All, Private, Groups and Forums, then each
/// chat folder in server order (plan section 17.2). The tabs scroll
/// sideways; the unread toggle sits at the strip's right end, outside the
/// scroll. The archive has no tabs, only the toggle.
pub(super) fn strip(session: &Session, theme: &Theme, cx: &mut Context<'_, Root>) -> Div {
    let list = &session.shell.list;
    let controls = session.tabs.controls();
    let shown = match list.shown() {
        Shown::Tab(tab) => Some(tab),
        Shown::Archive => None,
    };
    let tabs = list.tabs().filter(|_| shown.is_some()).map(|tab| {
        let selected = Some(tab) == shown;
        let mark: Hsla = if selected {
            theme.text.into()
        } else {
            transparent_black()
        };
        div()
            .id(tab_id(tab))
            .flex_none()
            .px_2()
            .pb_1()
            .border_b_2()
            .border_color(mark)
            .text_sm()
            .cursor_pointer()
            .child(tab_title(tab, list))
            .when(selected, |d| d.debug_selector(|| "selected-tab".to_owned()))
            .when(!selected, |d| d.text_color(theme::quiet(theme.text)))
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
    });
    let scroll = div()
        .id("tabs")
        .flex_1()
        .min_w_0()
        .flex()
        .gap_1()
        .px_1()
        .pt_1()
        .overflow_x_scroll()
        .track_scroll(&session.tabs.scroll)
        .children(tabs);
    div()
        .flex()
        .flex_none()
        .bg(theme.surface(Depth::NOW))
        .text_color(theme.text)
        .children(controls.then(|| arrow(Step::Previous, &session.tabs, theme, cx)))
        .child(scroll)
        .children(controls.then(|| arrow(Step::Next, &session.tabs, theme, cx)))
        .child(unread_toggle(list.view(), theme, cx))
        .child(after_paint(
            px(if controls { ARROW_WIDTH * 2. } else { 0. }),
            cx,
        ))
}

/// The unread filter's toggle (plan section 17.4): on, it carries the ink
/// rule and full ink like a selected tab; off, quiet ink. No hue.
fn unread_toggle(view: View, theme: &Theme, cx: &mut Context<'_, Root>) -> Stateful<Div> {
    let on = view.unread_only;
    let mark: Hsla = if on {
        theme.text.into()
    } else {
        transparent_black()
    };
    div()
        .id("unread-toggle")
        .flex_none()
        .mx_1()
        .px_2()
        .py_1()
        .border_b_2()
        .border_color(mark)
        .text_sm()
        .cursor_pointer()
        .child(copy::UNREAD_FILTER)
        .when(!on, |d| d.text_color(theme::quiet(theme.text)))
        .on_click(cx.listener(|root, _: &ClickEvent, window, cx| {
            root.with_session(|s, cx| s.toggle_unread(window, cx), cx);
        }))
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
        Tab::Folder(id) => Some(id),
        Tab::All | Tab::Private | Tab::Groups | Tab::Forums => None,
    }
}
