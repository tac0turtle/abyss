//! The flat colourways, Ultramarine and Sky (colourways plan, section 1):
//! one surface at every depth, a colored fill for the user's own messages
//! and an accent. Each color is fixed here; the user tunes nothing. The
//! contrast tests in `theme.rs` hold every pair they paint.

use gpui::Rgba;

use super::{MARK, MARK_STEPS, Mode, STEPS, Theme, oklch};

/// An OKLCH color: lightness, chroma, hue in degrees.
#[derive(Clone, Copy, Debug)]
struct Ink {
    lightness: f32,
    chroma: f32,
    hue: f32,
}

const fn ink(lightness: f32, chroma: f32, hue: f32) -> Ink {
    Ink {
        lightness,
        chroma,
        hue,
    }
}

const WHITE: Ink = ink(1., 0., 0.);

/// Everything a flat colourway fixes for one mode.
#[derive(Debug)]
struct Spec {
    surface: Ink,
    text: Ink,
    incoming: Ink,
    own: Ink,
    on_own: Ink,
    accent: Ink,
    on_accent: Ink,
    /// Hidden spoilers on an incoming message and on the user's own.
    spoilers: [Ink; 2],
    /// Quiet text's opacity inside an incoming message and the user's own.
    quiet: [f32; 2],
}

/// Signal-like: flat neutrals and a saturated blue.
const ULTRAMARINE_BLUE: Ink = ink(0.53, 0.20, 265.);

const ULTRAMARINE_DAY: Spec = Spec {
    surface: ink(0.995, 0., 0.),
    text: ink(0.20, 0., 0.),
    incoming: ink(0.94, 0., 0.),
    own: ULTRAMARINE_BLUE,
    on_own: WHITE,
    accent: ULTRAMARINE_BLUE,
    on_accent: WHITE,
    spoilers: [ink(0.55, 0., 0.), ink(0.86, 0., 0.)],
    quiet: [super::QUIET, 0.86],
};

const ULTRAMARINE_NIGHT: Spec = Spec {
    surface: ink(0.18, 0., 0.),
    text: ink(0.93, 0., 0.),
    incoming: ink(0.33, 0., 0.),
    own: ULTRAMARINE_BLUE,
    on_own: WHITE,
    accent: ULTRAMARINE_BLUE,
    on_accent: WHITE,
    spoilers: [ink(0.70, 0., 0.), ink(0.86, 0., 0.)],
    quiet: [super::QUIET, 0.86],
};

/// Telegram-like in structure, with its own hues: white bubbles on a
/// grey-blue pane by day, slate by night.
const SKY_DAY: Spec = Spec {
    surface: ink(0.955, 0.010, 230.),
    text: ink(0.22, 0.02, 240.),
    incoming: WHITE,
    own: ink(0.91, 0.055, 230.),
    on_own: ink(0.22, 0.02, 240.),
    accent: ink(0.52, 0.14, 230.),
    on_accent: WHITE,
    spoilers: [ink(0.62, 0.03, 240.), ink(0.57, 0.03, 240.)],
    quiet: [super::QUIET, super::QUIET],
};

const SKY_NIGHT: Spec = Spec {
    surface: ink(0.23, 0.03, 245.),
    text: ink(0.94, 0.01, 245.),
    incoming: ink(0.29, 0.035, 245.),
    own: ink(0.40, 0.07, 245.),
    on_own: ink(0.94, 0.01, 245.),
    accent: ink(0.70, 0.12, 230.),
    on_accent: ink(0.18, 0.03, 245.),
    spoilers: [ink(0.62, 0.03, 245.), ink(0.70, 0.03, 245.)],
    quiet: [super::QUIET, super::QUIET],
};

fn color(ink: Ink) -> Rgba {
    oklch(
        super::Tone {
            lightness: ink.lightness,
            chroma: ink.chroma,
        },
        ink.hue,
        1.,
    )
}

/// A flat colourway.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Flat {
    Ultramarine,
    Sky,
}

/// The theme for `flat` in `mode`.
pub(super) fn theme(flat: Flat, mode: Mode) -> Theme {
    let spec = match (flat, mode) {
        (Flat::Ultramarine, Mode::Day) => &ULTRAMARINE_DAY,
        (Flat::Ultramarine, Mode::Night) => &ULTRAMARINE_NIGHT,
        (Flat::Sky, Mode::Day) => &SKY_DAY,
        (Flat::Sky, Mode::Night) => &SKY_NIGHT,
    };
    let text = color(spec.text);
    Theme {
        surfaces: [color(spec.surface); STEPS],
        // No step flips: one surface takes one text color.
        flip: STEPS,
        flat: true,
        text,
        // Nothing sits on a `text` fill in a flat colourway; the surface is
        // the natural opposite.
        on_text: color(spec.surface),
        incoming: color(spec.incoming),
        on_incoming: text,
        own: color(spec.own),
        on_own: color(spec.on_own),
        accent: color(spec.accent),
        on_accent: color(spec.on_accent),
        // The mark keeps the public ramp: hue 215 at the default tint.
        mark: public_mark(),
        spoilers: spec.spoilers.map(color),
        quiet: spec.quiet,
    }
}

/// The public ramp the mark is drawn in outside abyss.
fn public_mark() -> [Rgba; MARK_STEPS] {
    MARK.map(|tone| oklch(tone, 215., 1.))
}
