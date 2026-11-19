// Copyright 2022 - 2025 Zed Industries, Inc.
// Copyright 2026 the abyss contributors.
//
// Licensed under the Apache License, Version 2.0 (the "License"); you may not
// use this file except in compliance with the License. You may obtain a copy of
// the License at http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS, WITHOUT
// WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//
// Derived from Zed's `crates/gpui/examples/input.rs` (rev 917559da). Modified:
// the editing state moved into a GPUI-free `EditBuffer` (character filters,
// UTF-16 length caps, line policy); a soft-wrapped multi-line composer mode
// (Enter submits, Shift+Enter inserts a newline, grows to 8 rows then
// scrolls, length counter); Up/Down, submit and cancel actions; key bindings
// scoped to the input's key context; the demo window was removed.

//! Text fields: single-line fields (phone, code, `api_id`) and the message
//! composer. Secrets (the 2FA password, the `api_hash`) use the separate
//! [`super::password::PasswordInput`], which never hands plaintext to the
//! platform.

mod edit;
mod element;
#[cfg(test)]
mod tests;

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Entity, EntityInputHandler, EventEmitter,
    FocusHandle, Focusable, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, Point, ScrollEvent, SharedString, UTF16Selection, Window, actions, div, point,
    prelude::*, px, size,
};

pub(crate) use self::edit::{COMPOSER_MAX_UTF16, Limits};
use self::edit::{EditBuffer, Fit, Lines};
use self::element::{TextElement, TextLayout};

/// Key context shared by [`TextInput`] and the password field.
pub(crate) const KEY_CONTEXT: &str = "TextInput";

/// The composer shows a length counter from this many UTF-16 units on.
const COUNTER_FROM: usize = COMPOSER_MAX_UTF16 - 500;

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        Home,
        End,
        ShowCharacterPalette,
        Paste,
        Cut,
        Copy,
        /// Enter: submit the field.
        Submit,
        /// Shift+Enter: a line break in the composer.
        Newline,
        /// Escape.
        Cancel,
    ]
);

/// Key bindings for text fields, scoped to [`KEY_CONTEXT`].
pub(crate) fn key_bindings() -> Vec<KeyBinding> {
    let cx = Some(KEY_CONTEXT);
    vec![
        KeyBinding::new("backspace", Backspace, cx),
        KeyBinding::new("delete", Delete, cx),
        KeyBinding::new("left", Left, cx),
        KeyBinding::new("right", Right, cx),
        KeyBinding::new("up", Up, cx),
        KeyBinding::new("down", Down, cx),
        KeyBinding::new("shift-left", SelectLeft, cx),
        KeyBinding::new("shift-right", SelectRight, cx),
        KeyBinding::new("shift-up", SelectUp, cx),
        KeyBinding::new("shift-down", SelectDown, cx),
        KeyBinding::new("cmd-a", SelectAll, cx),
        KeyBinding::new("cmd-v", Paste, cx),
        KeyBinding::new("cmd-c", Copy, cx),
        KeyBinding::new("cmd-x", Cut, cx),
        KeyBinding::new("home", Home, cx),
        KeyBinding::new("end", End, cx),
        KeyBinding::new("cmd-left", Home, cx),
        KeyBinding::new("cmd-right", End, cx),
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, cx),
        KeyBinding::new("enter", Submit, cx),
        KeyBinding::new("shift-enter", Newline, cx),
        KeyBinding::new("escape", Cancel, cx),
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextInputEvent {
    /// Enter was pressed. The owner reads the text with [`TextInput::take_text`].
    Submit,
}

pub(crate) struct TextInput {
    focus_handle: FocusHandle,
    buffer: EditBuffer,
    placeholder: SharedString,
    last_layout: Option<TextLayout>,
    last_bounds: Option<Bounds<Pixels>>,
    /// Vertical scroll of the composer, in pixels from the top.
    scroll_top: Pixels,
    /// Set by edits and caret moves: the next paint scrolls the caret into view.
    autoscroll: bool,
    is_selecting: bool,
}

impl EventEmitter<TextInputEvent> for TextInput {}

impl TextInput {
    pub(crate) fn new(
        limits: Limits,
        placeholder: &'static str,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            buffer: EditBuffer::new(limits),
            placeholder: placeholder.into(),
            last_layout: None,
            last_bounds: None,
            scroll_top: px(0.),
            autoscroll: false,
            is_selecting: false,
        }
    }

    pub(crate) fn text(&self) -> &str {
        self.buffer.text()
    }

    /// Changes the field's length bound; text over it is cleared.
    pub(crate) fn set_max_len(&mut self, max: usize, cx: &mut Context<'_, Self>) {
        if self.buffer.limits().max_utf16 != max {
            self.buffer.set_max_utf16(max);
            cx.notify();
        }
    }

    /// Clears the field.
    pub(crate) fn clear(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        drop(self.take_text(window, cx));
    }

    /// The text as drawn.
    pub(super) fn display_text(&self) -> String {
        self.buffer.text().to_owned()
    }

    /// Fills an empty field with `text`, whole. Returns whether it did:
    /// never over typed text, and never clipped.
    pub(crate) fn put_text(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if !self.buffer.text().is_empty() {
            return false;
        }
        if self.buffer.replace(None, text) == Fit::Clipped {
            drop(self.buffer.take());
            return false;
        }
        self.after_edit(Fit::Whole, window, cx);
        true
    }

    /// Takes the text, leaving the field empty.
    pub(crate) fn take_text(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> String {
        let text = self.buffer.take();
        self.after_edit(Fit::Whole, window, cx);
        text
    }

    fn after_edit(&mut self, fit: Fit, window: &mut Window, cx: &mut Context<'_, Self>) {
        if fit == Fit::Clipped {
            window.play_system_bell();
        }
        self.autoscroll = true;
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    fn after_move(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        self.autoscroll = true;
        window.notify_text_selection_changed(&self.focus_handle);
        cx.notify();
    }

    fn move_to(&mut self, offset: usize, window: &Window, cx: &mut Context<'_, Self>) {
        self.buffer.move_to(offset);
        self.after_move(window, cx);
    }

    fn select_to(&mut self, offset: usize, window: &Window, cx: &mut Context<'_, Self>) {
        self.buffer.select_to(offset);
        self.after_move(window, cx);
    }

    fn left(&mut self, _: &Left, window: &mut Window, cx: &mut Context<'_, Self>) {
        let sel = self.buffer.selected();
        let to = if sel.is_empty() {
            self.buffer.previous_boundary(self.buffer.cursor())
        } else {
            sel.start
        };
        self.move_to(to, window, cx);
    }

    fn right(&mut self, _: &Right, window: &mut Window, cx: &mut Context<'_, Self>) {
        let sel = self.buffer.selected();
        let to = if sel.is_empty() {
            self.buffer.next_boundary(sel.end)
        } else {
            sel.end
        };
        self.move_to(to, window, cx);
    }

    fn up(&mut self, _: &Up, window: &mut Window, cx: &mut Context<'_, Self>) {
        let to = self.vertical_target(-1);
        self.move_to(to, window, cx);
    }

    fn down(&mut self, _: &Down, window: &mut Window, cx: &mut Context<'_, Self>) {
        let to = self.vertical_target(1);
        self.move_to(to, window, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, window: &mut Window, cx: &mut Context<'_, Self>) {
        let to = self.buffer.previous_boundary(self.buffer.cursor());
        self.select_to(to, window, cx);
    }

    fn select_right(&mut self, _: &SelectRight, window: &mut Window, cx: &mut Context<'_, Self>) {
        let to = self.buffer.next_boundary(self.buffer.cursor());
        self.select_to(to, window, cx);
    }

    fn select_up(&mut self, _: &SelectUp, window: &mut Window, cx: &mut Context<'_, Self>) {
        let to = self.vertical_target(-1);
        self.select_to(to, window, cx);
    }

    fn select_down(&mut self, _: &SelectDown, window: &mut Window, cx: &mut Context<'_, Self>) {
        let to = self.vertical_target(1);
        self.select_to(to, window, cx);
    }

    fn select_all(&mut self, _: &SelectAll, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.buffer.select_all();
        self.after_move(window, cx);
    }

    fn home(&mut self, _: &Home, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.move_to(0, window, cx);
    }

    fn end(&mut self, _: &End, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.move_to(self.buffer.text().len(), window, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.buffer.selected().is_empty() {
            let cursor = self.buffer.cursor();
            let prev = self.buffer.previous_boundary(cursor);
            if cursor == prev {
                window.play_system_bell();
                return;
            }
            self.buffer.select_to(prev);
        }
        let fit = self.buffer.replace(None, "");
        self.after_edit(fit, window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.buffer.selected().is_empty() {
            let cursor = self.buffer.cursor();
            let next = self.buffer.next_boundary(cursor);
            if cursor == next {
                window.play_system_bell();
                return;
            }
            self.buffer.select_to(next);
        }
        let fit = self.buffer.replace(None, "");
        self.after_edit(fit, window, cx);
    }

    #[expect(clippy::unused_self, reason = "GPUI action listeners receive the view")]
    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<'_, Self>) {
        cx.emit(TextInputEvent::Submit);
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.buffer.limits().lines == Lines::Single {
            cx.propagate();
            return;
        }
        let fit = self.buffer.replace(None, "\n");
        self.after_edit(fit, window, cx);
    }

    #[expect(clippy::unused_self, reason = "GPUI action listeners receive the view")]
    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<'_, Self>) {
        cx.propagate();
    }

    #[expect(clippy::unused_self, reason = "GPUI action listeners receive the view")]
    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<'_, Self>,
    ) {
        window.show_character_palette();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let fit = self.buffer.replace(None, &text);
            self.after_edit(fit, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<'_, Self>) {
        let text = self.buffer.selected_text();
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<'_, Self>) {
        let text = self.buffer.selected_text();
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
            let fit = self.buffer.replace(None, "");
            self.after_edit(fit, window, cx);
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.is_selecting = true;
        window.request_virtual_keyboard();
        let at = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            self.select_to(at, window, cx);
        } else {
            self.move_to(at, window, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<'_, Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.is_selecting {
            let at = self.index_for_mouse_position(event.position);
            self.select_to(at, window, cx);
        }
    }

    fn on_scroll(&mut self, event: &ScrollEvent, window: &mut Window, cx: &mut Context<'_, Self>) {
        let (Some(layout), Some(bounds)) = (&self.last_layout, self.last_bounds) else {
            return;
        };
        let max = (layout.height - bounds.size.height).max(px(0.));
        if max <= px(0.) {
            return;
        }
        let delta = event.delta.pixel_delta(window.line_height());
        self.scroll_top = (self.scroll_top - delta.y).clamp(px(0.), max);
        cx.stop_propagation();
        cx.notify();
    }

    /// The current layout, if it was shaped from the current content.
    fn layout(&self) -> Option<&TextLayout> {
        self.last_layout
            .as_ref()
            .filter(|l| l.revision == self.buffer.revision())
    }

    /// Converts a window point to content space.
    fn to_content(&self, position: Point<Pixels>) -> Option<Point<Pixels>> {
        let bounds = self.last_bounds?;
        Some(point(
            position.x - bounds.left(),
            position.y - bounds.top() + self.scroll_top,
        ))
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.buffer.text().is_empty() {
            return 0;
        }
        match (self.layout(), self.to_content(position)) {
            (Some(layout), Some(p)) => layout.index_for(p),
            _ => self.buffer.cursor(),
        }
    }

    /// The offset one visual row above (`-1`) or below (`1`) the caret.
    fn vertical_target(&self, rows: i8) -> usize {
        let cursor = self.buffer.cursor();
        let Some(layout) = self.layout() else {
            return cursor;
        };
        let Some(caret) = layout.position_for(cursor) else {
            return cursor;
        };
        let lh = layout.line_height();
        let y = caret.y + lh * (f32::from(rows) + 0.5);
        if y < px(0.) {
            0
        } else if y >= layout.height {
            self.buffer.text().len()
        } else {
            layout.index_for(point(caret.x, y))
        }
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Option<String> {
        let (text, actual) = self.buffer.text_for_utf16(&range_utf16);
        actual_range.replace(actual);
        Some(text)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.buffer.range_to_utf16(&self.buffer.selected()),
            reversed: self.buffer.reversed(),
        })
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.buffer.set_selected_utf16(&range_utf16);
        self.after_move(window, cx);
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Option<usize> {
        Some(self.buffer.utf16_len())
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Option<Range<usize>> {
        self.buffer
            .marked()
            .map(|range| self.buffer.range_to_utf16(&range))
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.buffer.unmark();
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let fit = self.buffer.replace(range_utf16.as_ref(), new_text);
        self.after_edit(fit, window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let fit = self.buffer.replace_and_mark(
            range_utf16.as_ref(),
            new_text,
            new_selected_range_utf16.as_ref(),
        );
        self.after_edit(fit, window, cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.layout()?;
        let range = self.buffer.range_from_utf16(&range_utf16);
        let start = layout.position_for(range.start)?;
        let end = layout
            .position_for(range.end)
            .filter(|e| e.y == start.y)
            .map_or(start.x + px(1.), |e| e.x.max(start.x + px(1.)));
        let origin = point(bounds.left(), bounds.top() - self.scroll_top);
        Some(Bounds::new(
            point(origin.x + start.x, origin.y + start.y),
            size(end - start.x, layout.line_height()),
        ))
    }

    fn selection_bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Vec<Bounds<Pixels>> {
        let Some(layout) = self.layout() else {
            return Vec::new();
        };
        let range = self.buffer.range_from_utf16(&range_utf16);
        let origin = point(bounds.left(), bounds.top() - self.scroll_top);
        layout
            .selection_rects(&range, bounds.size.width)
            .into_iter()
            .map(|r| Bounds::new(r.origin + origin, r.size))
            .collect()
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Option<usize> {
        let p = self.to_content(point)?;
        let index = self.layout()?.index_for(p);
        Some(self.buffer.range_to_utf16(&(index..index)).start)
    }
}

impl Render for TextInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let len = self.buffer.utf16_len();
        let counter = (self.buffer.limits().lines == Lines::Multi && len >= COUNTER_FROM)
            .then(|| format!("{len} / {COMPOSER_MAX_UTF16}"));
        div()
            .flex()
            .flex_col()
            .w_full()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::cancel))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll(cx.listener(Self::on_scroll))
            .child(TextElement { input: cx.entity() })
            .children(counter.map(|c| {
                div()
                    .flex()
                    .justify_end()
                    .text_xs()
                    .opacity(crate::ui::theme::QUIET)
                    .child(c)
            }))
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Creates a text field entity.
pub(crate) fn text_input(
    limits: Limits,
    placeholder: &'static str,
    cx: &mut App,
) -> Entity<TextInput> {
    cx.new(|cx| TextInput::new(limits, placeholder, cx))
}
