//! Keyboard and `AppKit` targets for the ranges drawn by `InteractiveText`.

use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AccessibleAction, AnyElement, App, AvailableSpace, Bounds, Element, ElementId, GlobalElementId,
    Hitbox, InspectorElementId, InteractiveText, KeyDownEvent, LayoutId, Modifiers, Pixels, Rgba,
    Role, StyledText, TextLayout, Window, point, prelude::*, px, size,
};

use super::{Body, Click};
use crate::ui::a11y;

type Activate = Rc<dyn Fn(&Click, &mut Window, &mut App)>;

struct Target {
    range: Range<usize>,
    label: String,
    click: Click,
}

pub(super) struct AccessibleText {
    text: InteractiveText,
    layout: TextLayout,
    targets: Vec<Target>,
    ink: Rgba,
    activate: Activate,
}

impl AccessibleText {
    pub(super) fn new(
        id: ElementId,
        body: Body,
        ink: Rgba,
        activate: impl Fn(&Click, &mut Window, &mut App) + 'static,
    ) -> Self {
        let mut targets = body.targets();
        targets.sort_by_key(|target| target.range.start);
        let ranges = targets.iter().map(|target| target.range.clone()).collect();
        let clicks: Vec<_> = targets.iter().map(|target| target.click.clone()).collect();
        let activate: Activate = Rc::new(activate);
        let mouse = Rc::clone(&activate);
        let text = StyledText::new(body.text).with_runs(body.runs);
        let layout = text.layout().clone();
        let text = InteractiveText::new(id, text).on_click(ranges, move |index, window, cx| {
            if let Some(click) = clicks.get(index) {
                mouse(click, window, cx);
            }
        });
        Self {
            text,
            layout,
            targets,
            ink,
            activate,
        }
    }

    fn control(&self, target: &Target) -> AnyElement {
        let keyboard = Rc::clone(&self.activate);
        let key_click = target.click.clone();
        let native = Rc::clone(&self.activate);
        let native_click = target.click.clone();
        let kind = if target.click == Click::Reveal {
            "spoiler"
        } else {
            "copy-link"
        };
        a11y::control(
            (kind, target.range.start),
            Role::Button,
            target.label.clone(),
        )
        .size_full()
        .border_2()
        .rounded(px(2.))
        .focus(|style| style.border_color(self.ink))
        .on_key_down(move |event: &KeyDownEvent, window, cx| {
            if !event.is_held
                && event.keystroke.modifiers == Modifiers::none()
                && matches!(event.keystroke.key.as_str(), "enter" | "space")
            {
                keyboard(&key_click, window, cx);
                cx.stop_propagation();
            }
        })
        .on_a11y_action(AccessibleAction::Click, move |_, window, cx| {
            native(&native_click, window, cx);
        })
        .into_any_element()
    }

    fn bounds(&self, text: Bounds<Pixels>, range: &Range<usize>) -> Option<Bounds<Pixels>> {
        let first = self.layout.position_for_index(range.start)?;
        let last = self.layout.position_for_index(range.end)?;
        let (left, right) = if first.y == last.y {
            (first.x.min(last.x), first.x.max(last.x))
        } else {
            (text.left(), text.right())
        };
        let top = first.y.min(last.y);
        let bottom = first.y.max(last.y) + self.layout.line_height();
        Some(Bounds::from_corners(point(left, top), point(right, bottom)).dilate(px(2.)))
    }
}

impl Element for AccessibleText {
    type RequestLayoutState = ();
    type PrepaintState = (Hitbox, Vec<AnyElement>);

    fn id(&self) -> Option<ElementId> {
        self.text.id()
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        self.text.source_location()
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        self.text.request_layout(id, inspector, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let hitbox = self.text.prepaint(id, inspector, bounds, state, window, cx);
        let viewport = window.content_mask().bounds;
        let controls = self
            .targets
            .iter()
            .filter_map(|target| {
                let bounds = self.bounds(bounds, &target.range)?.intersect(&viewport);
                if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
                    return None;
                }
                let mut control = self.control(target);
                control.prepaint_as_root(
                    bounds.origin,
                    size(
                        AvailableSpace::Definite(bounds.size.width),
                        AvailableSpace::Definite(bounds.size.height),
                    ),
                    window,
                    cx,
                );
                Some(control)
            })
            .collect();
        (hitbox, controls)
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut (),
        (hitbox, controls): &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.text
            .paint(id, inspector, bounds, layout, hitbox, window, cx);
        for control in controls {
            control.paint(window, cx);
        }
    }
}

impl Body {
    /// The display spoilers are ordered, with several runs possible under
    /// one uninterrupted cover.
    fn hidden_ranges(&self) -> Vec<Range<usize>> {
        let mut combined: Vec<Range<usize>> = Vec::new();
        for range in &self.spoilers {
            if let Some(last) = combined.last_mut()
                && range.start <= last.end
            {
                last.end = last.end.max(range.end);
            } else {
                combined.push(range.clone());
            }
        }
        combined
    }

    fn targets(&self) -> Vec<Target> {
        let links = self.links.iter().filter_map(|(range, url)| {
            let visible = self.text.get(range.clone())?;
            Some(Target {
                range: range.clone(),
                label: format!("Copy link, {visible}"),
                click: Click::Copy(url.clone()),
            })
        });
        let spoilers = self.hidden_ranges().into_iter().map(|range| Target {
            range,
            label: "Reveal spoiler".to_owned(),
            click: Click::Reveal,
        });
        links.chain(spoilers).collect()
    }

    /// The list item is the reading surface. The underlying `InteractiveText`
    /// is only painted, so its full display buffer stays out of the AX tree.
    pub(crate) fn redacted_text(&self) -> Option<String> {
        if self.spoilers.is_empty() {
            return None;
        }
        let mut text = String::new();
        let mut end = 0;
        for range in self.hidden_ranges() {
            text.push_str(self.text.get(end..range.start).unwrap_or_default());
            text.push_str(abyss_core::rich_text::SPOILER_EXCERPT);
            end = range.end;
        }
        text.push_str(self.text.get(end..).unwrap_or_default());
        Some(text)
    }
}
