//! AccessKit semantics shared by the shell's controls.

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use gpui::accesskit::{Live, Node, TextPosition, TextSelection, Toggled};
use gpui::{
    AccessibleAction, App, Bounds, Context, Div, ElementId, FocusHandle, Pixels, Rgba, Role,
    ScrollHandle, SharedString, Stateful, Text, Window, canvas, div, prelude::*,
};

mod focus;
pub(crate) use focus::{FocusDirection, FocusGroup, OverlayFocus};

use super::password::bullets;
use super::theme::{self, Theme};

pub(crate) fn heading(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    level: usize,
) -> Stateful<Div> {
    let label = label.into();
    labelled(div(), id, Role::Heading, label.clone())
        .aria_level(level)
        .child(label)
}

/// A heading whose spoken name differs from what it shows, such as a wordmark.
/// The caller adds the visible children.
pub(crate) fn heading_for(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    level: usize,
) -> Stateful<Div> {
    labelled(div(), id, Role::Heading, label).aria_level(level)
}

pub(crate) fn button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    let label = label.into();
    control(id, Role::Button, label.clone()).child(label)
}

/// The caller attaches `on_click`, which GPUI also dispatches for
/// Return, Space and AccessKit Click.
pub(crate) fn control(
    id: impl Into<ElementId>,
    role: Role,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    labelled(div(), id, role, label).focusable().tab_stop(true)
}

/// Share activation between mouse, keyboard and AccessKit. A direct
/// AccessKit callback also works if the element is scrolled out of view.
///
/// Every control gets its activation here, never from a bare `on_click`
/// (`on_click_only_in_on_activate` enforces this). Without a registered
/// `AccessibleAction::Click`, GPUI answers `VoiceOver`'s press by synthesizing
/// a mouse click at the node's centre, hit-tested against hitboxes clipped to
/// the scroll container. A control scrolled out of its viewport then does
/// nothing, or presses whatever is drawn over it.
pub(crate) fn on_activate(
    element: Stateful<Div>,
    callback: impl Fn(&mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let callback = Rc::new(callback);
    let mouse_or_key = Rc::clone(&callback);
    element
        .on_click(move |_, window, cx| mouse_or_key(window, cx))
        .on_a11y_action(AccessibleAction::Click, move |_, window, cx| {
            callback(window, cx);
        })
}

/// [`on_activate`] for a callback on a view `V`, dropped if the view is gone.
pub(crate) fn on_activate_in<V: 'static>(
    element: Stateful<Div>,
    cx: &Context<'_, V>,
    callback: impl Fn(&mut V, &mut Window, &mut Context<'_, V>) + 'static,
) -> Stateful<Div> {
    let view = cx.weak_entity();
    on_activate(element, move |window, cx| {
        view.update(cx, |view, cx| callback(view, window, cx)).ok();
    })
}

/// Compose one name for a row, skipping absent labels such as "muted".
pub(crate) fn compose<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// A row that is only read.
pub(crate) fn list_row<'a>(
    id: impl Into<ElementId>,
    parts: impl IntoIterator<Item = &'a str>,
) -> Stateful<Div> {
    labelled(div(), id, Role::ListItem, compose(parts))
}

/// A named container for rows that scroll: a list of chats, topics or
/// messages. Rows may be virtualized, so only those on screen are in it.
pub(crate) fn list(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    labelled(div(), id, Role::List, label)
}

/// Static text that a screen reader reads as it is, under a stable identity.
pub(crate) fn text(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Text {
    Text::new(id.into(), text.into())
}

/// An element with a focus handle of its own, which the owner keeps so it can
/// return focus here, and so it can draw the [`ring`]. The caller attaches the
/// activation.
pub(crate) fn tracked(
    id: impl Into<ElementId>,
    role: Role,
    label: impl Into<SharedString>,
    focus: &FocusHandle,
) -> Stateful<Div> {
    labelled(div(), id, role, label)
        .track_focus(focus)
        .relative()
}

/// One row of a list ([`tracked`]). A selectable one adds `aria_selected`.
pub(crate) fn row(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    focus: &FocusHandle,
) -> Stateful<Div> {
    tracked(id, Role::ListItem, label, focus)
}

/// A two-point ring over a row's padding box while it has focus. The ring is
/// an overlay, so focus never changes the row's size or its selection rule.
pub(crate) fn ring(focus: &FocusHandle, window: &Window, ink: Rgba) -> Option<Div> {
    focus
        .is_focused(window)
        .then(|| div().absolute().inset_0().border_2().border_color(ink))
}

/// A tab of a tab list, which the caller says is selected and whether it is
/// the strip's one tab stop: only that tab sits in the tab order, so Tab
/// crosses the strip in one step.
pub(crate) fn tab(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    labelled(div(), id, Role::Tab, label).focusable()
}

pub(crate) fn tab_list(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    labelled(div(), id, Role::TabList, label)
        .aria_orientation(gpui::accesskit::Orientation::Vertical)
}

/// A switch in `state`. The caller attaches the activation.
pub(crate) fn radio(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: Toggled,
) -> Stateful<Div> {
    control(id, Role::RadioButton, label)
        .aria_selected(selected == Toggled::True)
        .aria_toggled(selected)
}

/// Convert an application's two-state setting to AccessKit's state.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "one boolean converts to the accessibility state"
)]
pub(crate) fn toggled(on: bool) -> Toggled {
    if on { Toggled::True } else { Toggled::False }
}

pub(crate) fn switch(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    state: Toggled,
) -> Stateful<Div> {
    control(id, Role::Switch, label).aria_toggled(state)
}

/// An action for assistive technology only. It is no tab stop and has no
/// place on screen, for what a sighted keyboard user already does another way
/// (Return in the composer sends).
pub(crate) fn hidden_action(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    callback: impl Fn(&mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    labelled(div(), id, Role::Button, label)
        .absolute()
        .size_0()
        .on_a11y_action(AccessibleAction::Click, move |_, window, cx| {
            callback(window, cx);
        })
}

pub(crate) fn labelled(
    element: Div,
    id: impl Into<ElementId>,
    role: Role,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    element.id(id).role(role).aria_label(label)
}

/// AccessKit's macOS adapter announces a live node's value on insertion
/// and on changes. Keep the ID stable when replacing the error copy.
pub(crate) fn alert(id: impl Into<ElementId>, message: impl Into<SharedString>) -> Stateful<Div> {
    let message = message.into();
    div()
        .id(id)
        .role(Role::Alert)
        .aria_value(message.clone())
        .a11y_synthetic_children(|builder| builder.parent_node().set_live(Live::Assertive))
        .child(message)
}

/// Visible status text whose value is announced on arrival or change.
pub(crate) fn status(id: impl Into<ElementId>, message: impl Into<SharedString>) -> Stateful<Div> {
    let message = message.into();
    div()
        .id(id)
        .role(Role::Status)
        .aria_value(message.clone())
        .a11y_synthetic_children(|builder| builder.parent_node().set_live(Live::Polite))
        .child(message)
}

/// A polite live region: a screen reader reads a new or changed value after what it
/// is saying. A new ID reads again an announcement equal to the last, so the
/// caller makes the ID carry a sequence number.
pub(crate) fn polite(id: impl Into<ElementId>, message: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .role(Role::Status)
        .aria_value(message)
        .absolute()
        .size_0()
        .a11y_synthetic_children(|builder| builder.parent_node().set_live(Live::Polite))
}

/// Reserve two points in the layout so keyboard focus has no effect on size.
/// Use the foreground role that already contrasts with the control's fill.
pub(crate) fn focus_ring(element: Stateful<Div>, ink: Rgba) -> Stateful<Div> {
    let bounds = Rc::new(Cell::new(None));
    let measure = Rc::clone(&bounds);
    element
        .relative()
        .border_2()
        .focus(move |style| style.border_color(ink))
        .child(
            canvas(
                // The absolute child measures inside the two-point border.
                move |rect, _, _| measure.set(Some(rect.dilate(gpui::px(2.)))),
                |_, (), _, _| {},
            )
            .absolute()
            .inset_0(),
        )
        .on_action(move |action: &RevealFocus, _, cx| {
            if let Some(bounds) = bounds.get() {
                let _ = action.0.set(bounds);
            }
            cx.propagate();
        })
}

/// After Tab, the focused control reports its bounds to the scrollable page
/// via the action's bubble path. Each action carries one set of bounds.
#[derive(Clone, Debug, Default, PartialEq, gpui::Action)]
#[action(namespace = abyss_a11y, no_json, no_register)]
pub(crate) struct RevealFocus(Arc<OnceLock<Bounds<Pixels>>>);

impl RevealFocus {
    pub(crate) fn scroll_page(&self, scroll: &ScrollHandle, window: &mut Window) {
        let Some(bounds) = self.0.get() else {
            return;
        };
        let viewport = scroll.bounds();
        let delta = if bounds.top() < viewport.top() {
            viewport.top() - bounds.top()
        } else if bounds.bottom() > viewport.bottom() {
            viewport.bottom() - bounds.bottom()
        } else {
            return;
        };
        let mut offset = scroll.offset();
        offset.y = (offset.y + delta).clamp(-scroll.max_offset().y, gpui::px(0.));
        scroll.set_offset(offset);
        window.refresh();
    }
}

pub(crate) fn field_frame(element: Stateful<Div>, theme: &Theme) -> Stateful<Div> {
    focus_ring(element, theme.on_incoming)
        .w_full()
        .px_3()
        .py_2()
        .border_color(theme.line(theme.on_incoming))
        .rounded(gpui::px(theme::RADIUS))
        .bg(theme.incoming)
        .text_color(theme.on_incoming)
}

pub(crate) fn text_field(
    element: Div,
    label: impl Into<SharedString>,
    placeholder: SharedString,
    text: Option<String>,
    selection: Selection,
) -> Stateful<Div> {
    let field = labelled(element, "input", Role::TextInput, label);
    field_text(field, placeholder, text, selection)
}

/// The composer: the same field, with line breaks.
pub(crate) fn text_area(
    element: Div,
    label: impl Into<SharedString>,
    placeholder: SharedString,
    text: Option<String>,
    selection: Selection,
) -> Stateful<Div> {
    let field = labelled(element, "input", Role::MultilineTextInput, label);
    field_text(field, placeholder, text, selection)
}

/// The API accepts a length and character positions. Plaintext and the
/// revealed rendering have no path to this node or its synthetic text run.
pub(crate) fn secure_field(
    element: Div,
    label: impl Into<SharedString>,
    placeholder: SharedString,
    len: Option<usize>,
    selection: Selection,
) -> Stateful<Div> {
    let field = labelled(element, "secure-input", Role::PasswordInput, label);
    field_text(field, placeholder, len.map(bullets), selection)
}

fn field_text(
    field: Stateful<Div>,
    placeholder: SharedString,
    text: Option<String>,
    selection: Selection,
) -> Stateful<Div> {
    field
        .aria_placeholder(placeholder)
        .when_some(text, |field, text| {
            field.a11y_synthetic_children(move |builder| push_runs(builder, &text, selection))
        })
}

/// More lines than the composer's length bound allows are never offered.
const MAX_RUNS: usize = 4096;

/// One text run per line, each with its line break: AccessKit's runs are
/// lines, and the selection's positions are character offsets in a run. These
/// editors count Unicode scalars, and a run's character lengths are UTF-8 bytes.
fn push_runs(builder: &mut gpui::A11ySubtreeBuilder<'_>, text: &str, selection: Selection) {
    let mut runs = Vec::new();
    for (ix, line) in text.split_inclusive('\n').take(MAX_RUNS).enumerate() {
        runs.push(push_run(builder, ix, line));
    }
    if runs.is_empty() {
        runs.push(push_run(builder, 0, ""));
    }
    let Some(&(last, last_count)) = runs.last() else {
        return;
    };
    let position = |index: usize| {
        let mut start = 0;
        for &(node, count) in &runs {
            if index < start + count {
                return TextPosition {
                    node,
                    character_index: index.saturating_sub(start),
                };
            }
            start += count;
        }
        TextPosition {
            node: last,
            character_index: last_count,
        }
    };
    let selection = TextSelection {
        anchor: position(selection.anchor),
        focus: position(selection.focus),
    };
    builder.parent_node().set_text_selection(selection);
}

fn push_run(
    builder: &mut gpui::A11ySubtreeBuilder<'_>,
    ix: usize,
    line: &str,
) -> (gpui::accesskit::NodeId, usize) {
    let id = builder.synthetic_node_id(("text", ix));
    let lengths: Vec<_> = line.chars().map(utf8_len).collect();
    let count = lengths.len();
    let mut run = Node::new(Role::TextRun);
    run.set_character_lengths(lengths);
    run.set_text_direction(gpui::accesskit::TextDirection::LeftToRight);
    run.set_value(line);
    builder.push_child(id, run);
    (id, count)
}

fn utf8_len(ch: char) -> u8 {
    u8::try_from(ch.len_utf8()).unwrap_or_default()
}

/// AccessKit character positions for a selection or a collapsed caret.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Selection {
    anchor: usize,
    focus: usize,
}

impl Selection {
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "both edit buffers store a direction bit"
    )]
    pub(crate) fn new(range: Range<usize>, reversed: bool) -> Self {
        let (anchor, focus) = if reversed {
            (range.end, range.start)
        } else {
            (range.start, range.end)
        };
        Self { anchor, focus }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    /// The only file that may call `.on_click(` for itself, and only inside the
    /// bodies of `on_activate` and `on_activate_in`: they attach it together
    /// with the AccessKit Click listener (review A77). Its test module is
    /// excluded separately, as for every file.
    const ACTIVATE_FILE: &str = "ui/a11y.rs";

    /// Where else a bare `.on_click(` may stay, by file and the text on the
    /// line. The rest of the crate attaches activation with
    /// [`super::on_activate`] or [`super::on_activate_in`], so `VoiceOver`'s
    /// press reaches the control where it is scrolled out of view (review N58).
    const BARE_CLICK: &[(&str, &str, &str)] = &[
        (
            "chat/rich/accessible.rs",
            "InteractiveText::new",
            "link ranges in text; the control beside each registers Click",
        ),
        (
            "shell/view.rs",
            "cx.quit()",
            "Quit in the closing card, over an unscrolled overlay",
        ),
    ];

    fn sources(dir: &Path, depth: usize, found: &mut Vec<std::path::PathBuf>) {
        assert!(depth < 8, "source tree is deeper than expected");
        for entry in fs::read_dir(dir).expect("read the source directory") {
            let path = entry.expect("read an entry").path();
            if path.is_dir() {
                sources(&path, depth + 1, found);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
    }

    /// Whether `line` opens the body that may hold `.on_click(` in
    /// [`ACTIVATE_FILE`].
    fn opens_activate(line: &str) -> bool {
        line.starts_with("pub(crate) fn on_activate(")
            || line.starts_with("pub(crate) fn on_activate_in<")
    }

    /// The 1-based lines of `text` (the source of `name`, relative to `src`)
    /// that call `.on_click(` where that is not allowed: anywhere but the
    /// bodies of `on_activate` and `on_activate_in` in [`ACTIVATE_FILE`] and
    /// the places [`BARE_CLICK`] lists. A `mod tests {` after `#[cfg(test)]`
    /// is not checked (the test code names the literal itself).
    fn bare_clicks(name: &str, text: &str) -> Vec<usize> {
        let lines: Vec<&str> = text.lines().collect();
        let tests_at = lines
            .windows(2)
            .position(|w| {
                matches!(w, [attr, module]
                    if attr.trim() == "#[cfg(test)]" && module.starts_with("mod tests {"))
            })
            .unwrap_or(lines.len());
        let mut in_activate = false;
        let mut bare = Vec::new();
        for (n, line) in lines.iter().enumerate().take(tests_at) {
            if name == ACTIVATE_FILE && opens_activate(line) {
                in_activate = true;
            }
            let next = lines.get(n + 1).copied().unwrap_or_default();
            let allowed = (name == ACTIVATE_FILE && in_activate)
                || BARE_CLICK.iter().any(|(file, needle, _)| {
                    *file == name && (line.contains(needle) || next.contains(needle))
                });
            if line.contains(".on_click(") && !line.trim_start().starts_with("//") && !allowed {
                bare.push(n + 1);
            }
            // The end of a top-level function.
            if *line == "}" {
                in_activate = false;
            }
        }
        bare
    }

    #[test]
    fn on_click_only_in_on_activate() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        sources(&root, 0, &mut files);
        assert!(files.len() > 20, "found the source files");
        let mut bare = Vec::new();
        for path in files {
            let name = path
                .strip_prefix(&root)
                .expect("under src")
                .to_string_lossy()
                .replace('\\', "/");
            if name.contains("tests") || name.ends_with("tests.rs") {
                continue;
            }
            let text = fs::read_to_string(&path).expect("read a source file");
            bare.extend(
                bare_clicks(&name, &text)
                    .into_iter()
                    .map(|n| format!("{name}:{n}")),
            );
        }
        assert!(
            bare.is_empty(),
            "a bare on_click is not activated by VoiceOver where the control is \
             scrolled out of view; use a11y::on_activate or on_activate_in: {bare:?}"
        );
    }

    /// The exemption in `ui/a11y.rs` is the two implementations' bodies and
    /// nothing else (review A77): a bare call in any other function, after
    /// them, or in a function of the same name elsewhere is found, and the
    /// test module is not.
    #[test]
    fn the_checker_exempts_only_the_activation_bodies() {
        let fixture = "\
pub(crate) fn on_activate(
    element: Stateful<Div>,
) -> Stateful<Div> {
    element
        .on_click(move |_, window, cx| mouse_or_key(window, cx))
}

pub(crate) fn on_activate_in<V: 'static>(
    element: Stateful<Div>,
) -> Stateful<Div> {
    element.on_click(|_, _, _| {})
}

pub(crate) fn button(id: u32) -> Stateful<Div> {
    div().on_click(|_, _, _| {})
}

// a comment mentioning .on_click( is fine
#[cfg(test)]
mod tests {
    const NEEDLE: &str = \".on_click(\";
}
";
        assert_eq!(bare_clicks(ACTIVATE_FILE, fixture), [15]);
        assert_eq!(
            bare_clicks("shell/session.rs", fixture),
            [5, 11, 15],
            "no file but a11y.rs has the exemption"
        );
    }

    /// The real file is checked as above, and its one `.on_click(` is inside
    /// `on_activate`: if that moved, the exemption would be vacuous.
    #[test]
    fn a11y_rs_keeps_its_one_on_click_in_on_activate() {
        let text = include_str!("a11y.rs");
        assert!(bare_clicks(ACTIVATE_FILE, text).is_empty());
        assert_eq!(
            bare_clicks("elsewhere.rs", text).len(),
            1,
            "exactly one call outside the test module, in on_activate"
        );
    }
}
