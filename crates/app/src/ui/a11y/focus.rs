//! Focus boundaries for overlays and groups, using GPUI's rendered tab order.

use gpui::{App, Div, FocusHandle, ScrollHandle, Stateful, Window, div, prelude::*};

#[derive(Clone, Copy, Debug)]
pub(crate) enum FocusDirection {
    Next,
    Previous,
}

impl FocusDirection {
    pub(crate) fn advance(self, window: &mut Window, cx: &mut App) {
        match self {
            Self::Next => window.focus_next(cx),
            Self::Previous => window.focus_prev(cx),
        }
    }
}

/// A contiguous group in the tab order. The boundary handles let navigation
/// wrap through the group's controls, even when its contents change.
#[derive(Clone, Debug)]
pub(crate) struct FocusGroup {
    start: FocusHandle,
    end: FocusHandle,
}

impl FocusGroup {
    pub(crate) fn new(cx: &App) -> Self {
        Self {
            start: cx.focus_handle(),
            end: cx.focus_handle(),
        }
    }

    pub(crate) fn frame(&self, element: Stateful<Div>) -> Stateful<Div> {
        element.track_focus(&self.start)
    }

    /// Append this after the group's controls.
    pub(crate) fn end(&self) -> Stateful<Div> {
        div()
            .id("focus-end")
            .absolute()
            .size_0()
            .tab_index(0)
            .tab_stop(false)
            .track_focus(&self.end)
    }

    pub(crate) fn step(&self, direction: FocusDirection, window: &mut Window, cx: &mut App) {
        let edge = match direction {
            FocusDirection::Next => &self.start,
            FocusDirection::Previous => &self.end,
        };
        if self.start.contains_focused(window, cx) {
            direction.advance(window, cx);
        }
        if !self.start.contains_focused(window, cx) || self.start.is_focused(window) {
            edge.focus(window, cx);
            direction.advance(window, cx);
            if !self.start.contains_focused(window, cx) {
                self.start.focus(window, cx);
            }
        }
        window.dispatch_action(Box::new(super::RevealFocus::default()), cx);
    }

    /// The first control is known once the group has been drawn.
    pub(crate) fn enter_after_draw(&self, window: &Window, cx: &mut App) {
        if self.start.is_focused(window) {
            let group = self.clone();
            window.defer(cx, move |window, cx| {
                if group.start.is_focused(window) {
                    group.step(FocusDirection::Next, window, cx);
                }
            });
        }
    }
}

/// An overlay remembers the underlying control that opened it.
#[derive(Debug)]
pub(crate) struct OverlayFocus {
    pub(crate) group: FocusGroup,
    pub(crate) scroll: ScrollHandle,
    previous: Option<FocusHandle>,
}

impl OverlayFocus {
    pub(crate) fn new(cx: &App) -> Self {
        Self {
            group: FocusGroup::new(cx),
            scroll: ScrollHandle::new(),
            previous: None,
        }
    }

    pub(crate) fn open(&mut self, window: &mut Window, cx: &mut App) {
        self.previous = window.focused(cx);
        self.scroll
            .set_offset(gpui::point(gpui::px(0.), gpui::px(0.)));
        self.group.start.focus(window, cx);
    }

    pub(crate) fn close(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(handle) = self.previous.take().filter(|h| h.is_rendered(window)) {
            handle.focus(window, cx);
        } else {
            window.blur(cx);
            window.focus_next(cx);
        }
    }
}
