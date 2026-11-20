//! The flat colourways, every one but abyss (colourways plan, section 1):
//! one surface at every depth, a colored fill for the user's own messages
//! and an accent. Each color is fixed here, except that the user may move
//! the own fill and the accent to another hue: they are then redrawn at the
//! same WCAG luminance, so every contrast pair stays what the tests in
//! `theme/tests.rs` measured for the colourway's own hue.

use gpui::Rgba;

use super::{Hue, MARK, MARK_STEPS, Mode, STEPS, Theme, oklch};

/// Halvings of the lightness range when matching a luminance: far below
/// what an 8-bit channel can show.
const MATCH_STEPS: u32 = 24;

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
const ULTRAMARINE_BLUE: Ink = ink(0.51, 0.20, 265.);

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
    // A step lighter than the fill, so the rail still marks a hovered tab.
    accent: ink(0.53, 0.20, 265.),
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

/// WhatsApp-like in structure: a warm pane, soft green messages, a deep
/// green accent; dark teal by night.
const FERN_DAY: Spec = Spec {
    surface: ink(0.955, 0.012, 95.),
    text: ink(0.22, 0.02, 150.),
    incoming: WHITE,
    own: ink(0.91, 0.07, 145.),
    on_own: ink(0.22, 0.02, 150.),
    accent: ink(0.50, 0.12, 155.),
    on_accent: WHITE,
    spoilers: [ink(0.62, 0.02, 100.), ink(0.57, 0.03, 150.)],
    quiet: [super::QUIET, super::QUIET],
};

const FERN_NIGHT: Spec = Spec {
    surface: ink(0.21, 0.02, 200.),
    text: ink(0.94, 0.01, 160.),
    incoming: ink(0.28, 0.025, 200.),
    own: ink(0.40, 0.07, 165.),
    on_own: ink(0.95, 0.01, 160.),
    accent: ink(0.72, 0.14, 155.),
    on_accent: ink(0.18, 0.03, 160.),
    spoilers: [ink(0.62, 0.03, 200.), ink(0.70, 0.03, 165.)],
    quiet: [super::QUIET, super::QUIET],
};

/// Viber-like: flat neutrals and a saturated violet.
const VIOLET_INK: Ink = ink(0.50, 0.19, 300.);

const VIOLET_DAY: Spec = Spec {
    surface: ink(0.995, 0., 0.),
    text: ink(0.20, 0., 0.),
    incoming: ink(0.94, 0., 0.),
    own: VIOLET_INK,
    on_own: WHITE,
    accent: VIOLET_INK,
    on_accent: WHITE,
    spoilers: [ink(0.55, 0., 0.), ink(0.86, 0., 0.)],
    quiet: [super::QUIET, 0.86],
};

const VIOLET_NIGHT: Spec = Spec {
    surface: ink(0.18, 0., 0.),
    text: ink(0.93, 0., 0.),
    incoming: ink(0.33, 0., 0.),
    own: VIOLET_INK,
    on_own: WHITE,
    // Saturated violet is too dark to mark the night surface: a light one,
    // inked dark.
    accent: ink(0.80, 0.10, 300.),
    on_accent: ink(0.18, 0.02, 300.),
    spoilers: [ink(0.70, 0., 0.), ink(0.86, 0., 0.)],
    quiet: [super::QUIET, 0.86],
};

/// Warm paper and a terracotta accent; brown-black by night.
const EMBER_DAY: Spec = Spec {
    surface: ink(0.965, 0.012, 75.),
    text: ink(0.24, 0.02, 50.),
    incoming: ink(0.995, 0.005, 75.),
    own: ink(0.90, 0.045, 50.),
    on_own: ink(0.24, 0.02, 50.),
    accent: ink(0.55, 0.14, 40.),
    on_accent: WHITE,
    spoilers: [ink(0.62, 0.03, 60.), ink(0.57, 0.03, 50.)],
    quiet: [super::QUIET, super::QUIET],
};

const EMBER_NIGHT: Spec = Spec {
    surface: ink(0.22, 0.015, 50.),
    text: ink(0.93, 0.015, 70.),
    incoming: ink(0.29, 0.02, 50.),
    own: ink(0.40, 0.06, 40.),
    on_own: ink(0.94, 0.015, 70.),
    accent: ink(0.72, 0.12, 45.),
    on_accent: ink(0.18, 0.02, 50.),
    spoilers: [ink(0.62, 0.03, 50.), ink(0.70, 0.03, 45.)],
    quiet: [super::QUIET, super::QUIET],
};

/// No hue: the user's own messages in the strongest grey on screen.
const GRAPHITE_DAY: Spec = Spec {
    surface: ink(0.99, 0., 0.),
    text: ink(0.18, 0., 0.),
    incoming: ink(0.93, 0., 0.),
    own: ink(0.25, 0., 0.),
    on_own: ink(0.98, 0., 0.),
    accent: ink(0.25, 0., 0.),
    on_accent: ink(0.98, 0., 0.),
    spoilers: [ink(0.55, 0., 0.), ink(0.60, 0., 0.)],
    quiet: [super::QUIET, super::QUIET],
};

const GRAPHITE_NIGHT: Spec = Spec {
    surface: ink(0.16, 0., 0.),
    text: ink(0.94, 0., 0.),
    incoming: ink(0.27, 0., 0.),
    own: ink(0.90, 0., 0.),
    on_own: ink(0.18, 0., 0.),
    accent: ink(0.90, 0., 0.),
    on_accent: ink(0.18, 0., 0.),
    spoilers: [ink(0.62, 0., 0.), ink(0.55, 0., 0.)],
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
    Fern,
    Violet,
    Ember,
    Graphite,
}

/// `ink` moved to `hue`, at the lightness that keeps its WCAG luminance.
fn rehued(ink: Ink, hue: f32) -> Rgba {
    let target = super::luminance(color(ink));
    let at = |lightness| {
        color(Ink {
            lightness,
            hue,
            ..ink
        })
    };
    let (mut low, mut high) = (0_f32, 1_f32);
    for _ in 0..MATCH_STEPS {
        let mid = f32::midpoint(low, high);
        if super::luminance(at(mid)) < target {
            low = mid;
        } else {
            high = mid;
        }
    }
    at(high)
}

/// The theme for `flat` in `mode`, with its own fill and accent in
/// `accent` if the user chose one.
pub(super) fn theme(flat: Flat, mode: Mode, accent: Option<Hue>) -> Theme {
    let spec = match (flat, mode) {
        (Flat::Ultramarine, Mode::Day) => &ULTRAMARINE_DAY,
        (Flat::Ultramarine, Mode::Night) => &ULTRAMARINE_NIGHT,
        (Flat::Sky, Mode::Day) => &SKY_DAY,
        (Flat::Sky, Mode::Night) => &SKY_NIGHT,
        (Flat::Fern, Mode::Day) => &FERN_DAY,
        (Flat::Fern, Mode::Night) => &FERN_NIGHT,
        (Flat::Violet, Mode::Day) => &VIOLET_DAY,
        (Flat::Violet, Mode::Night) => &VIOLET_NIGHT,
        (Flat::Ember, Mode::Day) => &EMBER_DAY,
        (Flat::Ember, Mode::Night) => &EMBER_NIGHT,
        (Flat::Graphite, Mode::Day) => &GRAPHITE_DAY,
        (Flat::Graphite, Mode::Night) => &GRAPHITE_NIGHT,
    };
    let text = color(spec.text);
    let hued = |ink: Ink| accent.map_or_else(|| color(ink), |hue| rehued(ink, hue.degrees()));
    Theme {
        surfaces: [color(spec.surface); STEPS],
        // No step flips: one surface takes one text color.
        flip: STEPS,
        flat: true,
        increased_contrast: false,
        text,
        // Nothing sits on a `text` fill in a flat colourway; the surface is
        // the natural opposite.
        on_text: color(spec.surface),
        incoming: color(spec.incoming),
        on_incoming: text,
        own: hued(spec.own),
        on_own: color(spec.on_own),
        accent: hued(spec.accent),
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
