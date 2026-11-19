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
// Derived from `TextElement` in Zed's `crates/gpui/examples/input.rs` (rev
// 917559da). Modified: soft-wrapped multi-line layout through
// `TextSystem::shape_text`, a measured height that grows to `MAX_ROWS` rows,
// vertical scrolling that follows the caret, and multi-row selection.

use std::ops::Range;

use gpui::{
    App, AvailableSpace, Bounds, ContentMask, Element, ElementId, ElementInputHandler, Entity,
    GlobalElementId, InspectorElementId, LayoutId, PaintQuad, Pixels, Point, SharedString, Style,
    TextAlign, TextRun, TextStyle, UnderlineStyle, Window, WrappedLine, fill, point, px, relative,
    size,
};

use super::TextInput;
use super::edit::Lines;

/// The composer grows to this many rows, then scrolls.
pub(crate) const MAX_ROWS: usize = 8;

struct LineGeom {
    line: WrappedLine,
    /// Byte offset of the line's first char in the content.
    start: usize,
    /// Top of the line's first row, in content space.
    top: Pixels,
    rows: usize,
}

/// Shaped content: one entry per hard line, each soft-wrapped into rows.
pub(super) struct TextLayout {
    lines: Vec<LineGeom>,
    line_height: Pixels,
    /// The buffer revision this layout was shaped from.
    pub(super) revision: u64,
    pub(super) height: Pixels,
}

impl TextLayout {
    fn shape(
        window: &Window,
        text: SharedString,
        runs: &[TextRun],
        font_size: Pixels,
        wrap: Option<Pixels>,
    ) -> Option<Self> {
        let line_height = window.line_height();
        let shaped = window
            .text_system()
            .shape_text(text, font_size, runs, wrap, None)
            .ok()?;
        let mut lines = Vec::with_capacity(shaped.len());
        let (mut start, mut top) = (0, px(0.));
        for line in shaped {
            let rows = line.wrap_boundaries().len() + 1;
            let len = line.len();
            lines.push(LineGeom {
                line,
                start,
                top,
                rows,
            });
            start += len + '\n'.len_utf8();
            top += line_height * rows;
        }
        Some(Self {
            lines,
            line_height,
            revision: 0,
            height: top,
        })
    }

    pub(super) fn line_height(&self) -> Pixels {
        self.line_height
    }

    fn line_for_offset(&self, offset: usize) -> Option<&LineGeom> {
        self.lines.iter().rev().find(|l| l.start <= offset)
    }

    /// Top-left of the caret slot at `offset`, in content space.
    pub(super) fn position_for(&self, offset: usize) -> Option<Point<Pixels>> {
        let l = self.line_for_offset(offset)?;
        let p = l
            .line
            .position_for_index(offset - l.start, self.line_height)?;
        Some(point(p.x, p.y + l.top))
    }

    /// The closest byte offset to a content-space point.
    pub(super) fn index_for(&self, p: Point<Pixels>) -> usize {
        let Some(first) = self.lines.first() else {
            return 0;
        };
        let l = self
            .lines
            .iter()
            .rev()
            .find(|l| l.top <= p.y)
            .unwrap_or(first);
        // Keep the point inside the line's rows: below them the wrapped
        // layout answers with the line start.
        let last_row_mid = self.line_height * (l.rows - 1) + self.line_height / 2.;
        let local = point(p.x.max(px(0.)), (p.y - l.top).clamp(px(0.), last_row_mid));
        match l.line.closest_index_for_position(local, self.line_height) {
            Ok(i) | Err(i) => l.start + i,
        }
    }

    /// Content-space rectangles covering `range`, one per visual row.
    pub(super) fn selection_rects(
        &self,
        range: &Range<usize>,
        width: Pixels,
    ) -> Vec<Bounds<Pixels>> {
        let mut rects = Vec::new();
        let lh = self.line_height;
        for l in &self.lines {
            let end = l.start + l.line.len();
            if range.end < l.start || range.start > end {
                continue;
            }
            let s = range.start.max(l.start) - l.start;
            let e = range.end.min(end) - l.start;
            let (Some(ps), Some(pe)) = (
                l.line.position_for_index(s, lh),
                l.line.position_for_index(e, lh),
            ) else {
                continue;
            };
            let (mut x0, mut row) = (ps.x, ps.y);
            // Bounded by the line's row count.
            for _ in 0..l.rows {
                if row >= pe.y {
                    break;
                }
                rects.push(row_rect(x0, width, l.top + row, lh));
                row += lh;
                x0 = px(0.);
            }
            // A selection that runs past the line includes its line break.
            let x1 = if range.end > end { width } else { pe.x };
            if x1 > x0 {
                rects.push(row_rect(x0, x1, l.top + row, lh));
            }
        }
        rects
    }
}

fn row_rect(x0: Pixels, x1: Pixels, top: Pixels, lh: Pixels) -> Bounds<Pixels> {
    Bounds::from_corners(point(x0, top), point(x1, top + lh))
}

/// The text to shape (content or placeholder) and its runs, with the marked
/// (composing) range underlined.
fn display_runs(input: &TextInput, style: &TextStyle) -> (SharedString, Vec<TextRun>) {
    let content = input.buffer.text();
    let (text, color) = if content.is_empty() {
        (
            input.placeholder.clone(),
            style.color.opacity(crate::ui::theme::PLACEHOLDER),
        )
    } else {
        (SharedString::from(input.display_text()), style.color)
    };
    let run = TextRun {
        len: text.len(),
        font: style.font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let Some(marked) = input.buffer.marked().filter(|_| !content.is_empty()) else {
        return (text, vec![run]);
    };
    let runs = [
        TextRun {
            len: marked.start,
            ..run.clone()
        },
        TextRun {
            len: marked.len(),
            underline: Some(UnderlineStyle {
                color: Some(color),
                thickness: px(1.),
                wavy: false,
            }),
            ..run.clone()
        },
        TextRun {
            len: text.len().saturating_sub(marked.end),
            ..run
        },
    ]
    .into_iter()
    .filter(|r| r.len > 0)
    .collect();
    (text, runs)
}

pub(super) struct TextElement {
    pub(super) input: Entity<TextInput>,
}

pub(super) struct PrepaintState {
    layout: Option<TextLayout>,
    cursor: Option<PaintQuad>,
    selection: Vec<PaintQuad>,
    origin: Point<Pixels>,
    scroll: Pixels,
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let line_height = window.line_height();
        let input = self.input.read(cx);
        if input.buffer.limits().lines == Lines::Single {
            style.size.height = line_height.into();
            return (window.request_layout(style, [], cx), ());
        }
        let text_style = window.text_style();
        let (text, runs) = display_runs(input, &text_style);
        let font_size = text_style.font_size.to_pixels(window.rem_size());
        let id = window.request_measured_layout(style, move |known, available, window, _| {
            let width = known.width.or(match available.width {
                AvailableSpace::Definite(w) => Some(w),
                AvailableSpace::MinContent | AvailableSpace::MaxContent => None,
            });
            let rows = window
                .text_system()
                .shape_text(text.clone(), font_size, &runs, width, None)
                .map_or(1, |lines| {
                    lines.iter().map(|l| l.wrap_boundaries().len() + 1).sum()
                });
            size(
                width.unwrap_or(px(0.)),
                line_height * rows.clamp(1, MAX_ROWS),
            )
        });
        (id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let style = window.text_style();
        let (text, runs) = display_runs(input, &style);
        let wrap = match input.buffer.limits().lines {
            Lines::Single => None,
            Lines::Multi => Some(bounds.size.width),
        };
        let font_size = style.font_size.to_pixels(window.rem_size());
        let mut layout = TextLayout::shape(window, text, &runs, font_size, wrap);
        if let Some(l) = layout.as_mut() {
            l.revision = input.buffer.revision();
        }
        let selected = input.buffer.selected();
        let caret = layout
            .as_ref()
            .and_then(|l| l.position_for(input.buffer.cursor()));
        let lh = window.line_height();
        let content_height = layout.as_ref().map_or(lh, |l| l.height);
        let mut scroll = input.scroll_top;
        if input.autoscroll
            && let Some(c) = caret
        {
            if c.y < scroll {
                scroll = c.y;
            } else if c.y + lh > scroll + bounds.size.height {
                scroll = c.y + lh - bounds.size.height;
            }
        }
        let max_scroll = (content_height - bounds.size.height).max(px(0.));
        scroll = scroll.clamp(px(0.), max_scroll);
        let origin = point(bounds.left(), bounds.top() - scroll);
        let selection = match (&layout, selected.is_empty()) {
            (Some(l), false) => l
                .selection_rects(&selected, bounds.size.width)
                .into_iter()
                .map(|r| {
                    fill(
                        Bounds::new(r.origin + origin, r.size),
                        style.color.opacity(crate::ui::theme::SELECTION),
                    )
                })
                .collect(),
            _ => Vec::new(),
        };
        let cursor = caret
            .filter(|_| selected.is_empty())
            .map(|c| fill(Bounds::new(c + origin, size(px(1.5), lh)), style.color));
        PrepaintState {
            layout,
            cursor,
            selection,
            origin,
            scroll,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        let lh = window.line_height();
        let origin = prepaint.origin;
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for quad in prepaint.selection.drain(..) {
                window.paint_quad(quad);
            }
            for l in prepaint.layout.iter().flat_map(|l| &l.lines) {
                let at = point(origin.x, origin.y + l.top);
                if l.line
                    .paint(at, lh, TextAlign::Left, None, window, cx)
                    .is_err()
                {
                    tracing::warn!("text input: failed to paint a line");
                }
            }
            if focus_handle.is_focused(window)
                && let Some(cursor) = prepaint.cursor.take()
            {
                window.paint_quad(cursor);
            }
        });
        let layout = prepaint.layout.take();
        let scroll = prepaint.scroll;
        self.input.update(cx, |input, _| {
            input.last_layout = layout;
            input.last_bounds = Some(bounds);
            input.scroll_top = scroll;
            input.autoscroll = false;
        });
    }
}
