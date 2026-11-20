//! A custom text element shaped like gpui's `input.rs` `TextElement`, with the
//! AccessKit hooks the abyss inputs lack: an element id, a role, a value, and
//! synthetic `TextRun` children carrying the text and the selection.

use accesskit::{Node, Role, TextPosition, TextSelection};
use gpui::{
    A11ySubtreeBuilder, App, Bounds, Context, ElementId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Pixels, Render, ShapedLine, SharedString, Style, TextRun, Window, div,
    prelude::*, px, relative,
};

pub(crate) struct SpikeField {
    secret: bool,
    text: SharedString,
    /// Selection as character indices (anchor, focus).
    selection: (usize, usize),
}

impl SpikeField {
    pub(crate) fn new(text: &str, secret: bool, _cx: &mut Context<Self>) -> Self {
        Self {
            secret,
            text: text.to_owned().into(),
            selection: (6, 11),
        }
    }
}

impl Render for SpikeField {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // The wrapper has no role: the element below is what assistive tech sees.
        div().child(FieldElement {
            secret: self.secret,
            text: self.text.clone(),
            selection: self.selection,
        })
    }
}

struct FieldElement {
    secret: bool,
    text: SharedString,
    selection: (usize, usize),
}

impl gpui::Element for FieldElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<ShapedLine>;

    fn id(&self) -> Option<ElementId> {
        Some(
            if self.secret {
                "spike-secret"
            } else {
                "spike-field"
            }
            .into(),
        )
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
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        let style = window.text_style();
        let run = TextRun {
            len: self.text.len(),
            font: style.font(),
            color: style.color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let font_size = style.font_size.to_pixels(window.rem_size());
        Some(
            window
                .text_system()
                .shape_line(self.text.clone(), font_size, &[run], None),
        )
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
        if let Some(line) = prepaint.take() {
            let _ = line.paint(
                bounds.origin,
                window.line_height(),
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            );
        }
        let _ = px(0.);
    }

    fn a11y_role(&self) -> Option<Role> {
        Some(if self.secret {
            Role::PasswordInput
        } else {
            Role::TextInput
        })
    }

    fn write_a11y_info(&self, node: &mut Node) {
        node.set_label(if self.secret {
            "Spike password"
        } else {
            "Custom field"
        });
    }

    fn a11y_synthetic_children(
        &mut self,
        _prepaint: &mut Self::PrepaintState,
        builder: &mut A11ySubtreeBuilder,
    ) {
        let text = self.text.to_string();
        let mut run = Node::new(Role::TextRun);
        run.set_value(text.clone());
        run.set_character_lengths(
            text.chars()
                .map(|c| u8::try_from(c.len_utf8()).unwrap_or(1))
                .collect::<Vec<_>>(),
        );
        // Word starts as character indices: 0 and after each space.
        let mut starts = vec![0u8];
        for (i, c) in text.chars().enumerate() {
            if c == ' ' {
                starts.push(u8::try_from(i + 1).unwrap_or(0));
            }
        }
        run.set_word_starts(starts);
        let run_id = builder.synthetic_node_id(0);
        builder.push_child(run_id, run);
        builder.parent_node().set_text_selection(TextSelection {
            anchor: TextPosition {
                node: run_id,
                character_index: self.selection.0,
            },
            focus: TextPosition {
                node: run_id,
                character_index: self.selection.1,
            },
        });
    }
}
