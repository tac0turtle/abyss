//! Settings: the colourway and its colours: under abyss the user's hue and
//! tint for night and for day, elsewhere an accent hue (`docs/brand/brand.md`,
//! `docs/plans/colourways.md`).
//! The app behind the panel is the preview; a choice applies and saves as it
//! is made.

pub(super) mod notifications;

use gpui::{
    AnyElement, App, Context, Div, FontWeight, KeyDownEvent, MouseButton, Rgba, Role, SharedString,
    Stateful, Window, div, prelude::*, px, transparent_black,
};

use super::view::{button, muted};
use super::{DismissOverlay, Root};
use crate::ui::theme::{self, Choice, Colourway, Depth, Hue, Mode, Settings, Theme, Themes, Tint};
use crate::ui::{a11y, a11y::FocusDirection, copy};

const PANEL_WIDTH: f32 = 580.;
const LABEL_WIDTH: f32 = 40.;
const CHIP_HEIGHT: f32 = 26.;
const HUE_CHIP_WIDTH: f32 = 15.;
const TINT_CHIP_WIDTH: f32 = 44.;
const COLOURWAY_SWATCH_WIDTH: f32 = 120.;
const COLOURWAY_SWATCH_HEIGHT: f32 = 44.;
/// How much of the app shows through behind the panel.
const VEIL: f32 = 0.6;

/// The panel over the whole window. `showing` is the mode on screen now.
pub(super) fn panel(
    theme: &Theme,
    showing: Mode,
    settings: Settings,
    root: &Root,
    window: &Window,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let focus = &root.appearance_focus;
    let card = settings_card(theme, showing, settings, root, cx);
    focus.group.enter_after_draw(window, cx);
    div()
        .id("settings-backdrop")
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .occlude()
        .bg(theme::faded(theme.surface(Depth::NOW), VEIL))
        .flex()
        .justify_center()
        .items_center()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|root, _, window, cx| root.toggle_appearance(window, cx)),
        )
        .child(card)
        .into_any_element()
}

fn settings_card(
    theme: &Theme,
    showing: Mode,
    settings: Settings,
    root: &Root,
    cx: &mut Context<'_, Root>,
) -> Stateful<Div> {
    let colourway = Themes::colourway(cx);
    let focus = &root.appearance_focus;
    let scroll = focus.scroll.clone();
    let notifications = notifications::Options::of(root);
    let abyss = (colourway == Colourway::Abyss).then(|| {
        let sections = Mode::ALL.map(|mode| mode_section(mode, showing, settings, theme));
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(note("abyss-colour-note", copy::APPEARANCE_NOTE, theme))
            .children(sections)
    });
    let accent = colourway
        .takes_accent()
        .then(|| accent_section(colourway, showing, settings, theme));
    focus
        .group
        .frame(a11y::labelled(
            div(),
            "settings-panel",
            Role::Dialog,
            copy::SETTINGS,
        ))
        .a11y_synthetic_children(|builder| builder.parent_node().set_modal())
        .key_context("Overlay")
        .on_action(
            cx.listener(|root, _: &DismissOverlay, window, cx| root.toggle_appearance(window, cx)),
        )
        .w(px(PANEL_WIDTH))
        .max_h_full()
        .overflow_y_scroll()
        .track_scroll(&scroll)
        .on_action(move |action: &a11y::RevealFocus, window, _| action.scroll_page(&scroll, window))
        .flex()
        .flex_col()
        .gap_4()
        .p_6()
        .rounded(px(theme::RADIUS))
        .border_1()
        .border_color(theme.line(theme.text))
        .bg(theme.surface(Depth::NOW))
        .debug_selector(|| "settings-panel".to_owned())
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(
            a11y::heading("settings-title", copy::SETTINGS, 1)
                .text_xl()
                .font_weight(FontWeight::SEMIBOLD),
        )
        .child(
            a11y::heading("appearance-title", copy::APPEARANCE, 2).font_weight(FontWeight::MEDIUM),
        )
        .child(note("colourway-note", copy::COLOURWAY_NOTE, theme))
        .child(colourway_row(colourway, showing, settings, theme))
        .children(abyss)
        .children(accent)
        .child(notifications::section(notifications, theme, cx))
        .child(storage_section(theme, cx))
        .child(footer(colourway, theme, cx))
        .child(focus.group.end())
}

fn colourway_name(colourway: Colourway) -> &'static str {
    match colourway {
        Colourway::Ultramarine => copy::ULTRAMARINE,
        Colourway::Sky => copy::SKY,
        Colourway::Fern => copy::FERN,
        Colourway::Violet => copy::VIOLET,
        Colourway::Ember => copy::EMBER,
        Colourway::Graphite => copy::GRAPHITE,
        Colourway::Abyss => copy::ABYSS_COLOURWAY,
    }
}

pub(super) fn colourway_id(colourway: Colourway) -> &'static str {
    match colourway {
        Colourway::Ultramarine => "colourway-ultramarine",
        Colourway::Sky => "colourway-sky",
        Colourway::Fern => "colourway-fern",
        Colourway::Violet => "colourway-violet",
        Colourway::Ember => "colourway-ember",
        Colourway::Graphite => "colourway-graphite",
        Colourway::Abyss => "colourway-abyss",
    }
}

/// The colourways on offer, each shown as its own-message fill on its
/// surface in the mode showing. The chosen one is marked by a ring in the
/// text color and a heavier label, never by color alone.
fn colourway_row(chosen: Colourway, showing: Mode, settings: Settings, theme: &Theme) -> Radios {
    let swatches = Colourway::ALL.map(|colourway| {
        let looks = Theme::new(showing, colourway, settings);
        let selected = colourway == chosen;
        let outline = if selected || theme.has_increased_contrast() {
            looks.text
        } else {
            theme.line(theme.text)
        };
        let sample = colourway_sample(&looks, outline);
        let weight = if selected {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        };
        let id = colourway_id(colourway);
        let label = colourway_name(colourway);
        let control = a11y::radio(id, label, a11y::toggled(selected))
            .debug_selector(move || id.to_owned())
            .flex()
            .flex_col()
            .gap_1()
            .child(sample)
            .child(div().text_xs().font_weight(weight).child(label));
        radio_action(control, theme, move |cx| {
            Themes::set_colourway(colourway, cx);
        })
    });
    Radios::new(
        "colourways",
        "Colourway",
        div().flex().flex_wrap().gap_3().children(swatches),
    )
}

fn colourway_sample(looks: &Theme, outline: Rgba) -> Div {
    div()
        .w(px(COLOURWAY_SWATCH_WIDTH))
        .h(px(COLOURWAY_SWATCH_HEIGHT))
        .p_2()
        .flex()
        .justify_end()
        .items_end()
        .rounded(px(theme::RADIUS))
        .border_2()
        .border_color(outline)
        .bg(looks.surface(Depth::NOW))
        .child(div().w(px(48.)).h(px(14.)).rounded_full().bg(looks.own))
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Night => copy::NIGHT,
        Mode::Day => copy::DAY,
    }
}

/// One mode's two rows of choices.
fn mode_section(mode: Mode, showing: Mode, settings: Settings, theme: &Theme) -> Div {
    let name = mode_name(mode);
    let spoken = if mode == showing {
        format!("{name}, {}", copy::IN_USE)
    } else {
        name.to_owned()
    };
    let heading = a11y::heading_for(SharedString::from(format!("{name}-title")), spoken, 3)
        .flex()
        .gap_2()
        .items_baseline()
        .child(div().font_weight(FontWeight::MEDIUM).child(name))
        .children((mode == showing).then(|| muted(copy::IN_USE, theme).text_xs()));
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(heading)
        .child(mode_hues(mode, settings.get(mode), theme))
        .child(mode_tints(mode, settings.get(mode), theme))
}

fn mode_hues(mode: Mode, chosen: Choice, theme: &Theme) -> Radios {
    let name = mode_name(mode);
    let id = hue_row(mode);
    let label = format!("{name} hue");
    let chips = (0_u64..).zip(Hue::all()).map(|(ix, hue)| {
        let choice = Choice { hue, ..chosen };
        let chip = Chip {
            id: (id, ix),
            label: format!("{label}, {}", hue.accessible_name()),
            width: HUE_CHIP_WIDTH,
            fill: theme::swatch(choice),
            selected: choice == chosen,
        };
        chip.render(theme, move |s| s.with(mode, choice))
    });
    Radios::new(id, label.clone(), row(copy::HUE, chips, theme))
}

fn mode_tints(mode: Mode, chosen: Choice, theme: &Theme) -> Radios {
    let name = mode_name(mode);
    let id = tint_row(mode);
    let label = format!("{name} tint");
    let chips = (0_u64..).zip(Tint::all()).map(|(ix, tint)| {
        let choice = Choice { tint, ..chosen };
        let chip = Chip {
            id: (id, ix),
            label: format!("{label}, {:.0} percent", tint.factor() * 100.),
            width: TINT_CHIP_WIDTH,
            fill: theme::tint_swatch(choice),
            selected: choice == chosen,
        };
        chip.render(theme, move |s| s.with(mode, choice))
    });
    Radios::new(id, label.clone(), row(copy::TINT, chips, theme))
}

/// While the colourway's default accent is in use, no chip is selected.
fn accent_section(colourway: Colourway, showing: Mode, settings: Settings, theme: &Theme) -> Div {
    let chips = (0_u64..).zip(Hue::all()).map(|(ix, hue)| {
        let accent = Some(hue);
        let looks = Theme::new(showing, colourway, settings.with_accent(accent));
        let chip = Chip {
            id: ("accent-hue", ix),
            label: format!("Accent hue, {}", hue.accessible_name()),
            width: HUE_CHIP_WIDTH,
            fill: looks.accent,
            selected: settings.accent == accent,
        };
        chip.render(theme, move |s| s.with_accent(accent))
    });
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(note("accent-note", copy::ACCENT_NOTE, theme))
        .child(Radios::new(
            "accent-hues",
            "Accent hue",
            row(copy::HUE, chips, theme),
        ))
}

fn hue_row(mode: Mode) -> &'static str {
    match mode {
        Mode::Night => "night-hue",
        Mode::Day => "day-hue",
    }
}

fn tint_row(mode: Mode) -> &'static str {
    match mode {
        Mode::Night => "night-tint",
        Mode::Day => "day-tint",
    }
}

fn row(label: &'static str, chips: impl IntoIterator<Item = Stateful<Div>>, theme: &Theme) -> Div {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(muted(label, theme).text_xs().w(px(LABEL_WIDTH)).flex_none())
        .children(chips)
}

fn note(id: &'static str, label: &'static str, theme: &Theme) -> Div {
    div()
        .opacity(theme.quiet_opacity())
        .text_sm()
        .child(a11y::text(id, label))
}

#[derive(IntoElement)]
struct Radios {
    id: &'static str,
    label: SharedString,
    content: Div,
}

impl Radios {
    fn new(id: &'static str, label: impl Into<SharedString>, content: Div) -> Self {
        Self {
            id,
            label: label.into(),
            content,
        }
    }
}

impl RenderOnce for Radios {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state_id = ("radios-state", SharedString::from(self.id));
        let state =
            window.use_keyed_state(gpui::ElementId::NamedInteger(state_id.1, 0), cx, |_, cx| {
                a11y::FocusGroup::new(cx)
            });
        let group = state.read(cx);
        let arrows = group.clone();
        let end = group.end();
        group
            .frame(a11y::labelled(
                self.content,
                self.id,
                Role::RadioGroup,
                self.label,
            ))
            .on_key_down(move |event: &KeyDownEvent, window, cx| {
                if event.keystroke.modifiers != gpui::Modifiers::none() {
                    return;
                }
                let direction = match event.keystroke.key.as_str() {
                    "right" | "down" => FocusDirection::Next,
                    "left" | "up" => FocusDirection::Previous,
                    _ => return,
                };
                arrows.step(direction, window, cx);
                window.dispatch_action(Box::new(ChooseRadio), cx);
                cx.stop_propagation();
            })
            .child(end)
    }
}

#[derive(Clone, PartialEq, gpui::Action)]
#[action(namespace = abyss, no_json, no_register)]
struct ChooseRadio;

fn radio_action(
    control: Stateful<Div>,
    theme: &Theme,
    choose: impl Fn(&mut App) + Clone + 'static,
) -> Stateful<Div> {
    let keys = choose.clone();
    let control = a11y::focus_ring(control, theme.text)
        .cursor_pointer()
        .on_action(move |_: &ChooseRadio, _, cx| keys(cx));
    a11y::on_activate(control, move |_, cx| choose(cx))
}

/// One choice on offer, shown as the color it gives.
struct Chip {
    id: (&'static str, u64),
    label: String,
    width: f32,
    fill: Rgba,
    selected: bool,
}

impl Chip {
    fn render(
        self,
        theme: &Theme,
        choose: impl Fn(Settings) -> Settings + Clone + 'static,
    ) -> Stateful<Div> {
        let edge = if self.selected || theme.has_increased_contrast() {
            Theme::sample_outline(self.fill).into()
        } else {
            transparent_black()
        };
        let sample = div()
            .size_full()
            .rounded(px(3.))
            .bg(self.fill)
            .border_1()
            .border_color(edge)
            .when(self.selected, Styled::border_2);
        let control = a11y::radio(self.id, self.label, a11y::toggled(self.selected))
            .w(px(self.width))
            .h(px(CHIP_HEIGHT))
            .flex_none()
            .rounded(px(5.))
            .child(sample);
        radio_action(control, theme, move |cx| {
            Themes::set(choose(Themes::settings(cx)), cx);
        })
    }
}

/// Reset (wherever there are colours to choose: it puts back the showing
/// colourway's and stays in it) on the left, Done on the right; the left
/// slot stays when Reset is hidden.
fn footer(colourway: Colourway, theme: &Theme, cx: &mut Context<'_, Root>) -> Div {
    let choosable = colourway == Colourway::Abyss || colourway.takes_accent();
    let reset = choosable.then(|| reset_button(theme, cx));
    let done = a11y::on_activate_in(
        button("appearance-done", copy::DONE, theme),
        cx,
        Root::toggle_appearance,
    );
    div()
        .flex()
        .justify_between()
        .items_center()
        .child(div().children(reset))
        .child(done)
}

fn reset_button(theme: &Theme, cx: &mut Context<'_, Root>) -> Stateful<Div> {
    let reset = a11y::focus_ring(
        a11y::control("appearance-reset", Role::Button, "Reset colours").child(copy::RESET),
        theme.text,
    )
    .debug_selector(|| "appearance-reset".to_owned())
    .px_4()
    .py_2()
    .rounded(px(theme::RADIUS))
    .border_color(theme.line(theme.text))
    .cursor_pointer();
    a11y::on_activate_in(reset, cx, |_, _, cx| Themes::reset_colours(cx))
}

fn storage_section(theme: &Theme, cx: &App) -> Div {
    let prefs = cx.try_global::<crate::preferences::Preferences>();
    let enabled = prefs.is_some_and(|p| p.use_keychain == Some(true));
    let saving = prefs.is_some_and(|p| p.saving);
    let failed = prefs.is_some_and(|p| p.failed);
    let state = if saving {
        SwitchState::Saving(enabled)
    } else {
        SwitchState::Ready(enabled)
    };
    let toggle = setting_switch("keychain-storage", copy::KEYCHAIN_STORAGE, state, theme);
    let toggle = if saving {
        toggle
    } else {
        a11y::on_activate(toggle, |_, cx| crate::preferences::Preferences::toggle(cx))
    };
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .justify_between()
                .items_center()
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .child(copy::KEYCHAIN_STORAGE),
                )
                .child(toggle),
        )
        .child(note(
            "keychain-detail",
            copy::KEYCHAIN_STORAGE_DETAIL,
            theme,
        ))
        .child(note(
            "keychain-migration",
            copy::KEYCHAIN_MIGRATION_NOTE,
            theme,
        ))
        .children(failed.then(|| a11y::alert("keychain-error", copy::PREFERENCE_FAILED).text_sm()))
}

#[derive(Clone, Copy)]
enum SwitchState {
    Ready(bool),
    Saving(bool),
    Unavailable,
}

fn setting_switch(
    id: &'static str,
    name: &'static str,
    state: SwitchState,
    theme: &Theme,
) -> Stateful<Div> {
    let (visible, element) = match state {
        SwitchState::Ready(on) | SwitchState::Saving(on) => (
            if matches!(state, SwitchState::Saving(_)) {
                "Saving…"
            } else if on {
                "On"
            } else {
                "Off"
            },
            a11y::switch(id, name, a11y::toggled(on)),
        ),
        SwitchState::Unavailable => (
            "Unavailable",
            a11y::labelled(div(), id, Role::Switch, name).aria_value("Unavailable"),
        ),
    };
    super::view::primary_style(a11y::focus_ring(element, theme.on_accent), theme)
        .when(!matches!(state, SwitchState::Ready(_)), |control| {
            control
                .aria_description(visible)
                .a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
        })
        .child(visible)
}
