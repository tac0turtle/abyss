//! The tonal theme (`docs/brand/brand.md`): one hue per mode, no accent
//! color, and surfaces that darken with age.
//!
//! The user chooses a hue and a tint amount for night and for day
//! ([`Settings`]). Lightness is fixed here and is never theirs to change:
//! that is what keeps every text pair readable whatever they pick, and the
//! tests check each pair for every choice on offer.

pub(crate) mod settings;

use std::path::PathBuf;

use gpui::{App, AppContext, BorrowAppContext, Global, Rgba, Task, Window, WindowAppearance};

pub(crate) use self::settings::{Choice, Hue, Settings, Tint};

/// Messages, times and states.
pub(crate) const BODY_FONT: &str = "Avenir Next";
/// People: chat titles, sender names, the wordmark.
pub(crate) const NAME_FONT: &str = "Charter";
/// Code and preformatted text.
pub(crate) const MONO_FONT: &str = "Menlo";

/// The one corner radius, for messages, fields and buttons.
pub(crate) const RADIUS: f32 = 12.;
/// Opacity of quiet text (times, previews, hints) over its surface.
pub(crate) const QUIET: f32 = 0.74;
/// Opacity of a hairline drawn in the text color.
const LINE: f32 = 0.24;

const STEPS: usize = 7;
const DEEPEST: u8 = 6;
/// Tones of the mark, outermost first.
pub(crate) const MARK_STEPS: usize = 6;

/// How far a surface has sunk: 0 is today, 6 is a year or more ago.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Depth(u8);

impl Depth {
    pub(crate) const NOW: Self = Self(0);

    /// The depth of something last touched `days` local calendar days ago.
    pub(crate) fn from_days(days: i64) -> Self {
        Self(match days {
            ..=0 => 0,
            1 => 1,
            2..=6 => 2,
            7..=29 => 3,
            30..=89 => 4,
            90..=364 => 5,
            _ => DEEPEST,
        })
    }

    fn step(self) -> usize {
        usize::from(self.0)
    }
}

/// Night or day: which of the user's two choices is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Night,
    Day,
}

impl Mode {
    pub(crate) const ALL: [Self; 2] = [Self::Night, Self::Day];

    /// The mode the system appearance asks for.
    pub(crate) fn of(window: &Window) -> Self {
        #[cfg(all(test, feature = "snapshots"))]
        if let Some(mode) = forced::get() {
            return mode;
        }
        match window.appearance() {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::Night,
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::Day,
        }
    }

    fn spec(self) -> &'static Spec {
        match self {
            Self::Night => &NIGHT,
            Self::Day => &DAY,
        }
    }
}

/// The snapshot tests' override of [`Mode::of`]: GPUI's headless windows are
/// always light, so the tests pin the mode for their thread. Compiled only
/// into `--features snapshots` test builds.
#[cfg(all(test, feature = "snapshots"))]
pub(crate) mod forced {
    use std::cell::Cell;

    use super::Mode;

    thread_local! {
        static MODE: Cell<Option<Mode>> = const { Cell::new(None) };
    }

    /// Pins the mode on this thread; `None` follows the window again.
    pub(crate) fn set(mode: Option<Mode>) {
        MODE.set(mode);
    }

    pub(super) fn get() -> Option<Mode> {
        MODE.get()
    }
}

/// An OKLCH lightness and chroma; the hue comes from the user's choice.
#[derive(Clone, Copy, Debug)]
struct Tone {
    lightness: f32,
    chroma: f32,
}

const fn tone(lightness: f32, chroma: f32) -> Tone {
    Tone { lightness, chroma }
}

/// The fixed part of a mode: everything except hue and tint.
#[derive(Debug)]
struct Spec {
    surfaces: [Tone; STEPS],
    text: Tone,
    on_text: Tone,
    /// The first step whose surface takes `on_text` instead of `text`.
    flip: usize,
    /// Opacity of the white wash behind an incoming message.
    incoming: f32,
}

const NIGHT: Spec = Spec {
    surfaces: [
        tone(0.300, 0.045),
        tone(0.275, 0.044),
        tone(0.250, 0.042),
        tone(0.225, 0.040),
        tone(0.200, 0.037),
        tone(0.175, 0.034),
        tone(0.150, 0.030),
    ],
    text: tone(0.93, 0.015),
    on_text: tone(0.18, 0.03),
    flip: STEPS,
    incoming: 0.08,
};

const DAY: Spec = Spec {
    surfaces: [
        tone(0.970, 0.006),
        tone(0.925, 0.012),
        tone(0.860, 0.020),
        tone(0.790, 0.028),
        tone(0.430, 0.045),
        tone(0.355, 0.045),
        tone(0.280, 0.040),
    ],
    text: tone(0.24, 0.03),
    on_text: tone(0.97, 0.006),
    flip: 4,
    incoming: 0.80,
};

/// The mark and everything public use these six tones, surface to floor.
const MARK: [Tone; MARK_STEPS] = [
    tone(0.93, 0.015),
    tone(0.74, 0.035),
    tone(0.56, 0.050),
    tone(0.40, 0.050),
    tone(0.27, 0.045),
    tone(0.17, 0.032),
];

/// The tone a choice is previewed in: mid water.
const SWATCH: Tone = tone(0.56, 0.050);

/// A hidden spoiler: an opaque block midway between the two text colors, so
/// it shows on an incoming message and on the user's own alike.
const HIDDEN: Tone = tone(0.60, 0.030);

/// The colors of one mode, for one choice of hue and tint.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Theme {
    surfaces: [Rgba; STEPS],
    flip: usize,
    /// Text, and the fill of the user's own messages and of buttons: the
    /// strongest contrast on screen.
    pub(crate) text: Rgba,
    /// Text on a `text` fill.
    pub(crate) on_text: Rgba,
    /// The fill of an incoming message and of input fields; `on_incoming`
    /// reads on it over every surface.
    pub(crate) incoming: Rgba,
    /// Text on an `incoming` fill.
    pub(crate) on_incoming: Rgba,
    /// The fill of the user's own messages.
    pub(crate) own: Rgba,
    /// Text on an `own` fill.
    pub(crate) on_own: Rgba,
    /// The fill of buttons, the floating controls and the tab rail's rule.
    pub(crate) accent: Rgba,
    /// Text on an `accent` fill.
    pub(crate) on_accent: Rgba,
    /// The mark's tones, outermost first.
    pub(crate) mark: [Rgba; MARK_STEPS],
    /// Text and background of a hidden spoiler (the same, opaque color), on
    /// an incoming message and on the user's own.
    spoilers: [Rgba; 2],
    /// The opacity of quiet text inside an incoming message and inside the
    /// user's own.
    quiet: [f32; 2],
}

/// Which side of the conversation a message bubble is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    Incoming,
    Outgoing,
}

impl Side {
    fn index(self) -> usize {
        match self {
            Self::Incoming => 0,
            Self::Outgoing => 1,
        }
    }
}

impl Theme {
    pub(crate) fn new(mode: Mode, choice: Choice) -> Self {
        let spec = mode.spec();
        let color = |t: Tone| oklch(t, choice.hue.degrees(), choice.tint.factor());
        let (text, on_text, hidden) = (color(spec.text), color(spec.on_text), color(HIDDEN));
        // abyss has no accent: the user's own words, buttons and marks all
        // take the text color, and everything is told apart by contrast.
        Self {
            surfaces: spec.surfaces.map(color),
            flip: spec.flip,
            text,
            on_text,
            incoming: Rgba {
                r: 1.,
                g: 1.,
                b: 1.,
                a: spec.incoming,
            },
            on_incoming: text,
            own: text,
            on_own: on_text,
            accent: text,
            on_accent: on_text,
            mark: MARK.map(color),
            spoilers: [hidden; 2],
            quiet: [QUIET; 2],
        }
    }

    /// The fill of a message bubble on `side`.
    pub(crate) fn bubble(&self, side: Side) -> Rgba {
        match side {
            Side::Incoming => self.incoming,
            Side::Outgoing => self.own,
        }
    }

    /// The text color inside a message bubble on `side`.
    pub(crate) fn bubble_ink(&self, side: Side) -> Rgba {
        match side {
            Side::Incoming => self.on_incoming,
            Side::Outgoing => self.on_own,
        }
    }

    /// The opacity of quiet text (times, states, quotes) inside a message
    /// bubble on `side`.
    pub(crate) fn quiet_in(&self, side: Side) -> f32 {
        self.quiet.get(side.index()).copied().unwrap_or(QUIET)
    }

    /// A hidden spoiler's color inside a message bubble on `side`.
    pub(crate) fn spoiler(&self, side: Side) -> Rgba {
        self.spoilers.get(side.index()).copied().unwrap_or_default()
    }

    /// The rule that marks the selected row on the surface at `depth`.
    pub(crate) fn row_mark(&self, depth: Depth) -> Rgba {
        self.text_on(depth)
    }

    /// An unread badge's fill and text on the surface at `depth`. The unread
    /// dot is the fill alone.
    pub(crate) fn badge(&self, depth: Depth) -> (Rgba, Rgba) {
        (self.text_on(depth), self.surface(depth))
    }

    /// The surface at `depth`.
    pub(crate) fn surface(&self, depth: Depth) -> Rgba {
        self.surfaces.get(depth.step()).copied().unwrap_or_default()
    }

    /// The text color that reads on the surface at `depth`.
    pub(crate) fn text_on(&self, depth: Depth) -> Rgba {
        if depth.step() >= self.flip {
            self.on_text
        } else {
            self.text
        }
    }
}

/// How a choice looks, for the controls that offer it. Never below the
/// default tint, so hues stay tellable apart when the user has chosen none.
pub(crate) fn swatch(choice: Choice) -> Rgba {
    let tint = choice.tint.factor().max(Tint::DEFAULT.factor());
    oklch(SWATCH, choice.hue.degrees(), tint)
}

/// How a tint amount looks at the chosen hue, for the controls that offer it.
pub(crate) fn tint_swatch(choice: Choice) -> Rgba {
    oklch(SWATCH, choice.hue.degrees(), choice.tint.factor())
}

/// The user's choices and the two themes built from them; an app global.
pub(crate) struct Themes {
    settings: Settings,
    night: Theme,
    day: Theme,
    /// Where the choice is kept; `None` keeps it for this run only.
    store: Option<PathBuf>,
    /// The save in flight. Each save waits for the one before, so the last
    /// choice made is the last one written.
    saving: Option<Task<()>>,
}

impl Global for Themes {}

impl Themes {
    /// Themes for `settings`, saved to `store` when they change.
    pub(crate) fn new(settings: Settings, store: Option<PathBuf>) -> Self {
        Self {
            settings,
            night: Theme::new(Mode::Night, settings.night),
            day: Theme::new(Mode::Day, settings.day),
            store,
            saving: None,
        }
    }

    /// Installs the saved choice, or the defaults. Call once at startup.
    pub(crate) fn init(cx: &mut App) {
        let store = settings::default_path();
        let settings = store
            .as_deref()
            .map_or_else(Settings::default, settings::load);
        cx.set_global(Self::new(settings, store));
    }

    /// The theme for the window's current system appearance. Views that can
    /// render before [`Themes::init`] (tests) get the defaults.
    pub(crate) fn of(window: &Window, cx: &App) -> Theme {
        let mode = Mode::of(window);
        cx.try_global::<Self>().map_or_else(
            || Theme::new(mode, Settings::default().get(mode)),
            |t| t.theme(mode),
        )
    }

    /// The user's choices, or the defaults before [`Themes::init`].
    pub(crate) fn settings(cx: &App) -> Settings {
        cx.try_global::<Self>()
            .map_or_else(Settings::default, |t| t.settings)
    }

    fn theme(&self, mode: Mode) -> Theme {
        match mode {
            Mode::Night => self.night,
            Mode::Day => self.day,
        }
    }

    /// Replaces the choices, repaints every window and saves.
    pub(crate) fn set(settings: Settings, cx: &mut App) {
        if !cx.has_global::<Self>() {
            cx.set_global(Self::new(Settings::default(), None));
        }
        cx.update_global::<Self, _>(|themes, cx| {
            if themes.settings == settings {
                return;
            }
            let saving = themes.saving.take();
            *themes = Self::new(settings, themes.store.take());
            themes.saving = saving;
            themes.save(cx);
        });
        cx.refresh_windows();
    }

    fn save(&mut self, cx: &App) {
        let Some(path) = self.store.clone() else {
            return;
        };
        let (settings, before) = (self.settings, self.saving.take());
        self.saving = Some(cx.background_spawn(async move {
            if let Some(before) = before {
                before.await;
            }
            if let Err(error) = settings::save(&path, settings) {
                // The path holds the user's name; the kind is enough.
                tracing::warn!(kind = ?error.kind(), "couldn't save the theme choice");
            }
        }));
    }
}

/// `color` at quiet-text strength.
pub(crate) fn quiet(color: Rgba) -> Rgba {
    faded(color, QUIET)
}

/// A hairline in `color`.
pub(crate) fn line(color: Rgba) -> Rgba {
    faded(color, LINE)
}

/// `color` with its opacity scaled by `factor`.
pub(crate) fn faded(color: Rgba, factor: f32) -> Rgba {
    Rgba {
        a: color.a * factor,
        ..color
    }
}

/// OKLCH to sRGB (Björn Ottosson's Oklab matrices), clamped to the gamut.
fn oklch(tone: Tone, hue_degrees: f32, tint: f32) -> Rgba {
    let (sin, cos) = hue_degrees.to_radians().sin_cos();
    let chroma = tone.chroma * tint;
    let (green_red, blue_yellow) = (chroma * cos, chroma * sin);
    let light = tone.lightness;
    let long = (light + 0.396_337_78 * green_red + 0.215_803_76 * blue_yellow).powi(3);
    let medium = (light - 0.105_561_346 * green_red - 0.063_854_17 * blue_yellow).powi(3);
    let short = (light - 0.089_484_18 * green_red - 1.291_485_5 * blue_yellow).powi(3);
    Rgba {
        r: encode(4.076_741_7 * long - 3.307_711_6 * medium + 0.230_969_94 * short),
        g: encode(-1.268_438 * long + 2.609_757_4 * medium - 0.341_319_38 * short),
        b: encode(-0.004_196_086_4 * long - 0.703_418_6 * medium + 1.707_614_7 * short),
        a: 1.,
    }
}

/// Linear light to an sRGB component.
fn encode(linear: f32) -> f32 {
    let linear = linear.clamp(0., 1.);
    if linear <= 0.003_130_8 {
        12.92 * linear
    } else {
        1.055 * linear.powf(1. / 2.4) - 0.055
    }
}

#[cfg(test)]
mod tests {
    use gpui::rgb;

    use super::*;

    /// WCAG AA for body text.
    const AA: f32 = 4.5;

    fn default_theme(mode: Mode) -> Theme {
        Theme::new(mode, Settings::default().get(mode))
    }

    fn luminance(color: Rgba) -> f32 {
        let linear = |c: f32| {
            if c <= 0.040_45 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
    }

    fn contrast(a: Rgba, b: Rgba) -> f32 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// `top` painted over an opaque `under`.
    fn over(top: Rgba, under: Rgba) -> Rgba {
        let mix = |t: f32, u: f32| t * top.a + u * (1. - top.a);
        Rgba {
            r: mix(top.r, under.r),
            g: mix(top.g, under.g),
            b: mix(top.b, under.b),
            a: 1.,
        }
    }

    fn close(a: Rgba, b: Rgba) -> bool {
        let near = |x: f32, y: f32| (x - y).abs() <= 1.5 / 255.;
        near(a.r, b.r) && near(a.g, b.g) && near(a.b, b.b)
    }

    fn depths() -> impl Iterator<Item = Depth> {
        (0..=DEEPEST).map(Depth)
    }

    fn sides() -> [Side; 2] {
        [Side::Incoming, Side::Outgoing]
    }

    /// Every way text can sit on this theme: on a surface, in an incoming
    /// message over that surface, and in the user's own message; each at full
    /// and at its quiet strength.
    fn weakest_pair(theme: &Theme) -> f32 {
        let pair = |ink: Rgba, ground: Rgba, quiet: f32| {
            contrast(ink, ground).min(contrast(over(faded(ink, quiet), ground), ground))
        };
        let (incoming, own) = (Side::Incoming, Side::Outgoing);
        depths()
            .flat_map(|d| {
                let surface = theme.surface(d);
                let wash = over(theme.bubble(incoming), surface);
                [
                    pair(theme.text_on(d), surface, QUIET),
                    pair(theme.bubble_ink(incoming), wash, theme.quiet_in(incoming)),
                ]
            })
            .chain([pair(
                theme.bubble_ink(own),
                theme.bubble(own),
                theme.quiet_in(own),
            )])
            .fold(f32::INFINITY, f32::min)
    }

    /// How far a message bubble on `side` stands out from the surface at
    /// `depth` beneath it, as a contrast ratio.
    fn separation(theme: &Theme, side: Side, depth: Depth) -> f32 {
        let surface = theme.surface(depth);
        contrast(over(theme.bubble(side), surface), surface)
    }

    fn every_abyss_theme() -> impl Iterator<Item = (Mode, Choice, Theme)> {
        Mode::ALL.into_iter().flat_map(|mode| {
            Hue::all().flat_map(move |hue| {
                Tint::all().map(move |tint| {
                    let choice = Choice { hue, tint };
                    (mode, choice, Theme::new(mode, choice))
                })
            })
        })
    }

    /// The roles are named for what they paint, but under abyss each one is
    /// exactly the color that call site painted before roles existed, so
    /// abyss renders unchanged (colourways plan, task 1).
    #[test]
    fn abyss_roles_are_the_colors_they_replace() {
        for (mode, choice, theme) in every_abyss_theme() {
            let hidden = oklch(HIDDEN, choice.hue.degrees(), choice.tint.factor());
            let at = format!("{mode:?} {choice:?}");
            assert_eq!(theme.accent, theme.text, "{at}");
            assert_eq!(theme.on_accent, theme.on_text, "{at}");
            assert_eq!(theme.bubble(Side::Outgoing), theme.text, "{at}");
            assert_eq!(theme.bubble_ink(Side::Outgoing), theme.on_text, "{at}");
            assert_eq!(theme.bubble(Side::Incoming), theme.incoming, "{at}");
            assert_eq!(theme.bubble_ink(Side::Incoming), theme.text, "{at}");
            for side in sides() {
                assert_eq!(theme.quiet_in(side).to_bits(), QUIET.to_bits(), "{at}");
                assert_eq!(theme.spoiler(side), hidden, "{at}");
            }
            for d in depths() {
                assert_eq!(theme.row_mark(d), theme.text_on(d), "{at} {d:?}");
                assert_eq!(
                    theme.badge(d),
                    (theme.text_on(d), theme.surface(d)),
                    "{at} {d:?}"
                );
            }
        }
    }

    /// abyss's weakest bubble against the surface beneath it, per mode: the
    /// floor every colourway's bubbles must clear (colourways plan, section
    /// 6). Measured here so a change to abyss that lowers it is caught.
    #[test]
    fn abyss_bubbles_stand_out_from_their_surfaces() {
        let floor = |mode: Mode| match mode {
            Mode::Day => 1.068,
            Mode::Night => 1.175,
        };
        for (mode, choice, theme) in every_abyss_theme() {
            for side in sides() {
                for d in depths() {
                    let apart = separation(&theme, side, d);
                    assert!(
                        apart >= floor(mode),
                        "{mode:?} {choice:?} {side:?} {d:?}: {apart}"
                    );
                }
            }
        }
    }

    #[test]
    fn defaults_match_the_brand_sheet() {
        let night = default_theme(Mode::Night);
        assert!(close(night.surface(Depth::NOW), rgb(0x0c_333c)));
        assert!(close(night.surface(Depth(DEEPEST)), rgb(0x00_0e13)));
        assert!(close(night.text, rgb(0xdd_ebee)));
        let day = default_theme(Mode::Day);
        assert!(close(day.surface(Depth::NOW), rgb(0xf1_f6f7)));
        assert!(close(day.surface(Depth(DEEPEST)), rgb(0x0b_2f31)));
        assert!(close(day.text, rgb(0x0b_2425)));
        // The mark at the night default is the public ramp, surface to floor.
        assert!(close(night.mark[0], rgb(0xdd_ebee)));
        assert!(close(night.mark[5], rgb(0x00_1318)));
    }

    /// The property the picker relies on: lightness alone decides contrast,
    /// so nothing on offer can make text unreadable.
    #[test]
    fn every_text_pair_reads_for_every_choice_on_offer() {
        for mode in Mode::ALL {
            for hue in Hue::all() {
                for tint in Tint::all() {
                    let weakest = weakest_pair(&Theme::new(mode, Choice { hue, tint }));
                    assert!(weakest >= AA, "{mode:?} {hue:?} {tint:?}: {weakest}");
                }
            }
        }
    }

    #[test]
    fn old_things_sink() {
        for mode in Mode::ALL {
            let theme = default_theme(mode);
            let tones: Vec<f32> = depths().map(|d| luminance(theme.surface(d))).collect();
            assert!(tones.is_sorted_by(|a, b| a > b), "{mode:?}: {tones:?}");
        }
    }

    #[test]
    fn depth_follows_age_in_days() {
        let steps =
            |days: &[i64]| -> Vec<u8> { days.iter().map(|d| Depth::from_days(*d).0).collect() };
        assert_eq!(steps(&[-3, 0, 1, 2, 6, 7, 29]), [0, 0, 1, 2, 2, 3, 3]);
        assert_eq!(steps(&[30, 89, 90, 364, 365, i64::MAX]), [4, 4, 5, 5, 6, 6]);
    }

    #[test]
    fn swatches_stay_tellable_apart_without_tint() {
        let none = |hue| Choice {
            hue,
            tint: Tint::all().next().unwrap(),
        };
        let mut hues = Hue::all();
        let (a, b) = (hues.next().unwrap(), hues.nth(11).unwrap());
        assert_ne!(swatch(none(a)), swatch(none(b)));
        assert_eq!(
            tint_swatch(none(a)),
            tint_swatch(none(b)),
            "no tint is grey"
        );
    }
}
