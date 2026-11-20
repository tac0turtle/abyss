//! The theme: a [`Colourway`] and, under abyss, the user's hues
//! (`docs/brand/brand.md`, `docs/plans/colourways.md`).
//!
//! **abyss** is tonal: one hue per mode, no accent color, and surfaces that
//! darken with age. The user chooses a hue and a tint amount for night and
//! for day ([`Settings`]). Lightness is fixed here and is never theirs to
//! change: that is what keeps every text pair readable whatever they pick.
//!
//! **Ultramarine** and **Sky** are flat (`flat.rs`): one surface, a colored
//! fill for the user's own messages, and an accent.
//!
//! Views paint with the theme's roles, never with derivations of their own,
//! and the tests check every pair the roles paint, for every colourway and
//! every abyss choice on offer.

pub(crate) mod colourway;
mod flat;
pub(crate) mod settings;

use std::path::{Path, PathBuf};

use gpui::{App, AppContext, BorrowAppContext, Global, Rgba, Task, Window, WindowAppearance};

pub(crate) use self::colourway::Colourway;
use self::flat::Flat;
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
const QUIET: f32 = 0.74;
/// Opacity of a hairline drawn in the text color.
const LINE: f32 = 0.24;
/// Opacity of the wash behind inline code, in the text's own color.
pub(crate) const CODE_WASH: f32 = 0.1;
/// Opacity of the wash behind a group message's sender name.
pub(crate) const HEADER_WASH: f32 = 0.06;
/// Opacity of the disc behind an avatar's initials or a topic's glyph.
pub(crate) const DISC: f32 = 0.14;
/// Opacity of the wash under a hovered tab.
pub(crate) const TAB_HOVER: f32 = 0.06;
/// Opacity of the wash under a hovered menu entry.
pub(crate) const MENU_HOVER: f32 = 0.08;
/// Opacity of an input's placeholder: a hint, so it needs 3:1, not 4.5:1.
pub(crate) const PLACEHOLDER: f32 = 0.5;
/// Opacity of the wash behind selected text in an input.
pub(crate) const SELECTION: f32 = 0.2;

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
    /// A hidden spoiler on an incoming message: far enough from the wash it
    /// sits on to show at every depth (3:1).
    spoiler: Tone,
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
    spoiler: tone(0.65, 0.030),
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
    spoiler: tone(0.55, 0.030),
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

/// A hidden spoiler on the user's own message: an opaque block between the
/// two text colors.
const HIDDEN: Tone = tone(0.60, 0.030);

/// The colors of one mode, for one choice of hue and tint.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Theme {
    surfaces: [Rgba; STEPS],
    flip: usize,
    /// One surface at every depth, and marks in the accent (every colourway
    /// but abyss); otherwise the strata, and marks in each row's own ink (abyss).
    flat: bool,
    increased_contrast: bool,
    /// Text on the surface. In abyss it is also the source of the `own` and
    /// `accent` fills; views paint those through their roles.
    pub(crate) text: Rgba,
    /// Text on a `text` fill: abyss's `on_own` and `on_accent`. Private, so
    /// no view paints it outside a role.
    on_text: Rgba,
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
    /// The theme `colourway` paints in `mode` with the user's `settings`:
    /// abyss takes the mode's hue and tint, the others the accent hue.
    pub(crate) fn new(mode: Mode, colourway: Colourway, settings: Settings) -> Self {
        let accent = settings.accent.filter(|_| colourway.takes_accent());
        match colourway {
            Colourway::Abyss => Self::abyss(mode, settings.get(mode)),
            Colourway::Ultramarine => flat::theme(Flat::Ultramarine, mode, accent),
            Colourway::Sky => flat::theme(Flat::Sky, mode, accent),
            Colourway::Fern => flat::theme(Flat::Fern, mode, accent),
            Colourway::Violet => flat::theme(Flat::Violet, mode, accent),
            Colourway::Ember => flat::theme(Flat::Ember, mode, accent),
            Colourway::Graphite => flat::theme(Flat::Graphite, mode, accent),
        }
    }

    fn abyss(mode: Mode, choice: Choice) -> Self {
        let spec = mode.spec();
        let color = |t: Tone| oklch(t, choice.hue.degrees(), choice.tint.factor());
        let (text, on_text) = (color(spec.text), color(spec.on_text));
        // abyss has no accent: the user's own words, buttons and marks all
        // take the text color, and everything is told apart by contrast.
        Self {
            surfaces: spec.surfaces.map(color),
            flip: spec.flip,
            flat: false,
            increased_contrast: false,
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
            spoilers: [color(spec.spoiler), color(HIDDEN)],
            quiet: [QUIET; 2],
        }
    }

    /// A variant of this colourway with opaque rules and quiet text. It
    /// keeps the colourway's own inks and fills, including custom hues.
    pub(crate) fn increased_contrast(mut self) -> Self {
        self.increased_contrast = true;
        self.quiet = [1.; 2];
        self
    }

    pub(crate) fn has_increased_contrast(&self) -> bool {
        self.increased_contrast
    }

    pub(crate) fn quiet_opacity(&self) -> f32 {
        if self.increased_contrast { 1. } else { QUIET }
    }

    pub(crate) fn quiet(&self, ink: Rgba) -> Rgba {
        faded(ink, self.quiet_opacity())
    }

    pub(crate) fn line(&self, ink: Rgba) -> Rgba {
        self.rule(ink, LINE)
    }

    pub(crate) fn rule(&self, ink: Rgba, opacity: f32) -> Rgba {
        faded(ink, if self.increased_contrast { 1. } else { opacity })
    }

    /// Focus on a surface uses that depth's contrasting ink.
    pub(crate) fn focus_on(&self, depth: Depth) -> Rgba {
        self.text_on(depth)
    }

    /// Pick the stronger side of the black/white contrast crossover for a
    /// solid colour sample. The weaker maximum still clears 4.5:1.
    pub(crate) fn sample_outline(fill: Rgba) -> Rgba {
        gpui::rgb(if luminance(fill) > 0.179 {
            0x00_00_00
        } else {
            0xFF_FF_FF
        })
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
        if self.flat {
            self.accent
        } else {
            self.text_on(depth)
        }
    }

    /// An unread badge's fill and text on the surface at `depth`. The unread
    /// dot is the fill alone.
    pub(crate) fn badge(&self, depth: Depth) -> (Rgba, Rgba) {
        if self.flat {
            (self.accent, self.on_accent)
        } else {
            (self.text_on(depth), self.surface(depth))
        }
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

/// Where the choices are kept. `None` keeps that choice for this run only.
#[derive(Clone, Debug, Default)]
pub(crate) struct Stores {
    /// abyss's hues (`settings.rs`).
    pub(crate) theme: Option<PathBuf>,
    /// The colourway (`colourway.rs`).
    pub(crate) colourway: Option<PathBuf>,
}

/// One file to write: each choice is saved only when it changes, so picking
/// a colourway never touches the saved hues (colourways plan, section 3).
#[derive(Clone, Debug)]
enum Save {
    Hues(PathBuf, Settings),
    Colourway(PathBuf, Colourway),
}

/// The user's choices and the two themes built from them; an app global.
pub(crate) struct Themes {
    colourway: Colourway,
    settings: Settings,
    night: Theme,
    day: Theme,
    stores: Stores,
    /// The save in flight. Each save waits for the one before, so the last
    /// choice made is the last one written.
    saving: Option<Task<()>>,
}

impl Global for Themes {}

impl Themes {
    /// Themes for `colourway` and `settings`, saved to `stores` when they
    /// change.
    pub(crate) fn new(colourway: Colourway, settings: Settings, stores: Stores) -> Self {
        Self {
            colourway,
            settings,
            night: Theme::new(Mode::Night, colourway, settings),
            day: Theme::new(Mode::Day, colourway, settings),
            stores,
            saving: None,
        }
    }

    /// Installs the saved choices, or the defaults. Call once at startup.
    /// Nothing is written here.
    pub(crate) fn init(cx: &mut App) {
        if let Some(app_dir) = crate::storage::app_dir() {
            Self::init_in(&app_dir, cx);
        } else {
            tracing::warn!("the home directory is unknown; theme choices last this run");
            let defaults = Self::new(Colourway::DEFAULT, Settings::default(), Stores::default());
            cx.set_global(defaults);
        }
    }

    /// [`Themes::init`] with the app's directory at `app_dir`.
    pub(crate) fn init_in(app_dir: &Path, cx: &mut App) {
        let stores = Stores {
            theme: Some(settings::path_in(app_dir)),
            colourway: Some(colourway::path_in(app_dir)),
        };
        let settings = stores
            .theme
            .as_deref()
            .map_or_else(Settings::default, settings::load);
        let colourway = stores
            .colourway
            .as_deref()
            .map_or(Colourway::DEFAULT, |path| colourway::load(path).colourway());
        cx.set_global(Self::new(colourway, settings, stores));
    }

    /// The theme for the window's current system appearance. Views that can
    /// render before [`Themes::init`] (tests) get the defaults.
    pub(crate) fn of(window: &Window, cx: &App) -> Theme {
        let mode = Mode::of(window);
        let theme = cx.try_global::<Self>().map_or_else(
            || Theme::new(mode, Colourway::DEFAULT, Settings::default()),
            |t| t.theme(mode),
        );
        if super::display::DisplayPreferences::of(cx).increase_contrast {
            theme.increased_contrast()
        } else {
            theme
        }
    }

    /// abyss's hues, or the defaults before [`Themes::init`].
    pub(crate) fn settings(cx: &App) -> Settings {
        cx.try_global::<Self>()
            .map_or_else(Settings::default, |t| t.settings)
    }

    /// The colourway showing, or the default before [`Themes::init`].
    pub(crate) fn colourway(cx: &App) -> Colourway {
        cx.try_global::<Self>()
            .map_or(Colourway::DEFAULT, |t| t.colourway)
    }

    fn theme(&self, mode: Mode) -> Theme {
        match mode {
            Mode::Night => self.night,
            Mode::Day => self.day,
        }
    }

    /// Replaces abyss's hues, repaints every window and saves them.
    pub(crate) fn set(settings: Settings, cx: &mut App) {
        Self::change(cx, |themes| {
            (themes.settings != settings).then(|| {
                themes.settings = settings;
                themes.stores.theme.clone().map(|p| Save::Hues(p, settings))
            })
        });
    }

    /// Reset in the picker: puts back the defaults of what the showing
    /// colourway lets the user choose (abyss's hues, or the accent), and
    /// leaves the rest. Never changes the colourway.
    pub(crate) fn reset_colours(cx: &mut App) {
        let settings = Self::settings(cx);
        let defaults = Settings::default();
        let reset = if Self::colourway(cx) == Colourway::Abyss {
            Settings {
                accent: settings.accent,
                ..defaults
            }
        } else {
            settings.with_accent(defaults.accent)
        };
        Self::set(reset, cx);
    }

    /// Switches the colourway, repaints every window and saves it. The saved
    /// hues are left alone.
    pub(crate) fn set_colourway(colourway: Colourway, cx: &mut App) {
        Self::change(cx, |themes| {
            (themes.colourway != colourway).then(|| {
                themes.colourway = colourway;
                themes
                    .stores
                    .colourway
                    .clone()
                    .map(|p| Save::Colourway(p, colourway))
            })
        });
    }

    /// Applies `update` (which returns `None` when nothing changed, and the
    /// file to write if there is one), rebuilds both themes and saves.
    fn change(cx: &mut App, update: impl FnOnce(&mut Self) -> Option<Option<Save>>) {
        if !cx.has_global::<Self>() {
            let defaults = Self::new(Colourway::DEFAULT, Settings::default(), Stores::default());
            cx.set_global(defaults);
        }
        let changed = cx.update_global::<Self, _>(|themes, cx| {
            let Some(save) = update(themes) else {
                return false;
            };
            themes.night = Theme::new(Mode::Night, themes.colourway, themes.settings);
            themes.day = Theme::new(Mode::Day, themes.colourway, themes.settings);
            if let Some(save) = save {
                themes.save(save, cx);
            }
            true
        });
        if changed {
            cx.refresh_windows();
        }
    }

    fn save(&mut self, save: Save, cx: &App) {
        let before = self.saving.take();
        self.saving = Some(cx.background_spawn(async move {
            if let Some(before) = before {
                before.await;
            }
            let result = match &save {
                Save::Hues(path, settings) => settings::save(path, *settings),
                Save::Colourway(path, colourway) => colourway::save(path, *colourway),
            };
            if let Err(error) = result {
                // The path holds the user's name; the kind is enough.
                tracing::warn!(kind = ?error.kind(), "couldn't save the theme choice");
            }
        }));
    }
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
/// WCAG relative luminance of an opaque sRGB color.
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

fn encode(linear: f32) -> f32 {
    let linear = linear.clamp(0., 1.);
    if linear <= 0.003_130_8 {
        12.92 * linear
    } else {
        1.055 * linear.powf(1. / 2.4) - 0.055
    }
}

#[cfg(test)]
mod persist_tests;
#[cfg(test)]
mod tests;
