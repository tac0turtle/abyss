//! Settings, including the user's hue and tint for night and for day
//! (`docs/brand/brand.md`). The app behind the panel is the preview; a choice
//! applies and saves as it is made.

use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, Hsla, MouseButton, MouseDownEvent, Rgba,
    Stateful, div, prelude::*, px, transparent_black,
};

use super::Root;
use super::view::{button, muted};
use crate::ui::copy;
use crate::ui::theme::{self, Choice, Depth, Hue, Mode, Settings, Theme, Themes, Tint};

const PANEL_WIDTH: f32 = 580.;
const LABEL_WIDTH: f32 = 40.;
const CHIP_HEIGHT: f32 = 26.;
const HUE_CHIP_WIDTH: f32 = 15.;
const TINT_CHIP_WIDTH: f32 = 44.;
/// How much of the app shows through behind the panel.
const VEIL: f32 = 0.6;

/// The panel over the whole window. `showing` is the mode on screen now.
pub(super) fn panel(
    theme: &Theme,
    showing: Mode,
    settings: Settings,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let sections = Mode::ALL.map(|mode| mode_section(mode, showing, settings, theme, cx));
    let card = div()
        .id("settings-panel")
        .w(px(PANEL_WIDTH))
        .max_h_full()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap_4()
        .p_6()
        .rounded(px(theme::RADIUS))
        .border_1()
        .border_color(theme::line(theme.text))
        .bg(theme.surface(Depth::NOW))
        .debug_selector(|| "settings-panel".to_owned())
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(
            div()
                .text_xl()
                .font_weight(FontWeight::SEMIBOLD)
                .child(copy::SETTINGS),
        )
        .child(
            div()
                .font_weight(FontWeight::MEDIUM)
                .child(copy::APPEARANCE),
        )
        .child(muted(copy::APPEARANCE_NOTE).text_sm())
        .children(sections)
        .child(storage_section(theme, cx))
        .child(footer(theme, cx));
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
            cx.listener(|root, _: &MouseDownEvent, _, cx| {
                root.appearance_open = false;
                cx.notify();
            }),
        )
        .child(card)
        .into_any_element()
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Night => copy::NIGHT,
        Mode::Day => copy::DAY,
    }
}

/// One mode's two rows of choices.
fn mode_section(
    mode: Mode,
    showing: Mode,
    settings: Settings,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> Div {
    let chosen = settings.get(mode);
    let ring = |choice: Choice| -> Hsla {
        if choice == chosen {
            theme.text.into()
        } else {
            transparent_black()
        }
    };
    let hues: Vec<_> = (0_u64..)
        .zip(Hue::all())
        .map(|(ix, hue)| {
            let choice = Choice { hue, ..chosen };
            let chip = Chip {
                id: (hue_row(mode), ix),
                width: HUE_CHIP_WIDTH,
                fill: theme::swatch(choice),
                ring: ring(choice),
            };
            chip.render(mode, choice, cx)
        })
        .collect();
    let tints: Vec<_> = (0_u64..)
        .zip(Tint::all())
        .map(|(ix, tint)| {
            let choice = Choice { tint, ..chosen };
            let chip = Chip {
                id: (tint_row(mode), ix),
                width: TINT_CHIP_WIDTH,
                fill: theme::tint_swatch(choice),
                ring: ring(choice),
            };
            chip.render(mode, choice, cx)
        })
        .collect();
    let heading = div()
        .flex()
        .gap_2()
        .items_baseline()
        .child(div().font_weight(FontWeight::MEDIUM).child(mode_name(mode)))
        .children((mode == showing).then(|| muted(copy::IN_USE).text_xs()));
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(heading)
        .child(row(copy::HUE, hues))
        .child(row(copy::TINT, tints))
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

fn row(label: &'static str, chips: Vec<Stateful<Div>>) -> Div {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(muted(label).text_xs().w(px(LABEL_WIDTH)).flex_none())
        .children(chips)
}

/// One choice on offer, shown as the color it gives.
struct Chip {
    id: (&'static str, u64),
    width: f32,
    fill: Rgba,
    /// The text color around the chosen chip, transparent around the rest.
    ring: Hsla,
}

impl Chip {
    /// Clicking sets `mode` to `choice`.
    fn render(self, mode: Mode, choice: Choice, cx: &mut Context<'_, Root>) -> Stateful<Div> {
        div()
            .id(self.id)
            .w(px(self.width))
            .h(px(CHIP_HEIGHT))
            .flex_none()
            .rounded(px(5.))
            .border_2()
            .border_color(self.ring)
            .bg(self.fill)
            .cursor_pointer()
            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                let settings = Themes::settings(cx).with(mode, choice);
                Themes::set(settings, cx);
            }))
    }
}

fn footer(theme: &Theme, cx: &mut Context<'_, Root>) -> Div {
    let reset = div()
        .id("appearance-reset")
        .px_4()
        .py_2()
        .rounded(px(theme::RADIUS))
        .border_1()
        .border_color(theme::line(theme.text))
        .cursor_pointer()
        .child(copy::RESET)
        .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
            Themes::set(Settings::default(), cx);
        }));
    let done = button("appearance-done", copy::DONE, theme).on_click(cx.listener(
        |root, _: &ClickEvent, _, cx| {
            root.toggle_appearance(cx);
        },
    ));
    div()
        .flex()
        .justify_between()
        .items_center()
        .child(reset)
        .child(done)
}

fn storage_section(theme: &Theme, cx: &mut Context<'_, Root>) -> Div {
    let prefs = cx.try_global::<crate::preferences::Preferences>();
    let enabled = prefs.is_some_and(|p| p.use_keychain == Some(true));
    let saving = prefs.is_some_and(|p| p.saving);
    let failed = prefs.is_some_and(|p| p.failed);
    let label = if saving {
        "Saving…"
    } else if enabled {
        "On"
    } else {
        "Off"
    };
    let toggle = button("keychain-storage", label, theme).on_click(
        cx.listener(|_, _: &ClickEvent, _, cx| crate::preferences::Preferences::toggle(cx)),
    );
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
        .child(muted(copy::KEYCHAIN_STORAGE_DETAIL).text_sm())
        .child(muted(copy::KEYCHAIN_MIGRATION_NOTE).text_sm())
        .children(failed.then(|| muted(copy::PREFERENCE_FAILED).text_sm()))
}
