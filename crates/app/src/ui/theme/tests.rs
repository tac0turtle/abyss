//! Every pair the theme roles paint, for every colourway and every abyss
//! choice on offer (colourways plan, section 6).

use gpui::rgb;

use super::*;

/// WCAG AA for body text.
const AA: f32 = 4.5;
/// WCAG AA for marks that aren't text (rules, badges, control edges), and
/// for hints (placeholders).
const NON_TEXT: f32 = 3.;

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

/// Text in `ink` at `opacity` on an opaque `ground`.
fn text(ink: Rgba, opacity: f32, ground: Rgba) -> f32 {
    contrast(over(faded(ink, opacity), ground), ground)
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

fn abyss(mode: Mode) -> Theme {
    Theme::new(mode, Colourway::Abyss, Settings::default())
}

/// Every colourway but abyss.
fn flat_colourways() -> impl Iterator<Item = Colourway> {
    Colourway::ALL
        .into_iter()
        .filter(|c| *c != Colourway::Abyss)
}

/// Every theme a user can see: each flat colourway with its own accent and
/// every accent on offer, and abyss for every choice, in both modes.
fn every_theme() -> Vec<(String, Mode, Theme)> {
    let mut themes = Vec::new();
    for mode in Mode::ALL {
        for colourway in flat_colourways() {
            for accent in Hue::all().map(Some).chain([None]) {
                let settings = Settings::default().with_accent(accent);
                let theme = Theme::new(mode, colourway, settings);
                themes.push((format!("{colourway:?} {mode:?} {accent:?}"), mode, theme));
            }
        }
        for hue in Hue::all() {
            for tint in Tint::all() {
                let choice = Choice { hue, tint };
                let settings = Settings::default().with(mode, choice);
                let theme = Theme::new(mode, Colourway::Abyss, settings);
                themes.push((format!("Abyss {mode:?} {hue:?} {tint:?}"), mode, theme));
            }
        }
    }
    themes
}

/// One measured pair: what it is, its contrast, and the least it needs.
struct Pair {
    what: String,
    ratio: f32,
    floor: f32,
}

fn pair(what: impl Into<String>, ratio: f32, floor: f32) -> Pair {
    Pair {
        what: what.into(),
        ratio,
        floor,
    }
}

/// The weakest bubble against the surface beneath it, per mode: abyss's own
/// floor (measured in task 1), which every colourway's bubbles must clear.
fn separation_floor(mode: Mode) -> f32 {
    match mode {
        Mode::Day => 1.068,
        Mode::Night => 1.175,
    }
}

/// What sits on the surface at `depth`: text, the row mark, badges, discs
/// and a floating control's edge.
fn on_surface(t: &Theme, d: Depth, out: &mut Vec<Pair>) {
    let s = t.surface(d);
    let ink = t.text_on(d);
    let (fill, badge_ink) = t.badge(d);
    out.extend([
        pair(format!("text {d:?}"), text(ink, 1., s), AA),
        pair(
            format!("quiet text {d:?}"),
            text(ink, t.quiet_opacity(), s),
            AA,
        ),
        pair(
            format!("row focus ring {d:?}"),
            contrast(t.focus_on(d), s),
            NON_TEXT,
        ),
        pair(
            format!("row mark {d:?}"),
            contrast(t.row_mark(d), s),
            NON_TEXT,
        ),
        pair(format!("badge text {d:?}"), contrast(badge_ink, fill), AA),
        pair(format!("badge fill {d:?}"), contrast(fill, s), NON_TEXT),
        pair(
            format!("disc {d:?}"),
            text(ink, 1., over(faded(ink, DISC), s)),
            AA,
        ),
        pair(format!("floating over {d:?}"), edge(t, s), NON_TEXT),
    ]);
}

/// A floating control's edge over `ground`: its fill or its ring.
fn edge(t: &Theme, ground: Rgba) -> f32 {
    contrast(t.accent, ground).max(contrast(t.on_accent, ground))
}

/// What sits in a bubble on `side` over the surface at `depth`.
fn in_bubble(t: &Theme, mode: Mode, side: Side, d: Depth, out: &mut Vec<Pair>) {
    let s = t.surface(d);
    let g = over(t.bubble(side), s);
    let ink = t.bubble_ink(side);
    let at = format!("{side:?} {d:?}");
    out.extend([
        pair(format!("text {at}"), text(ink, 1., g), AA),
        pair(format!("quiet {at}"), text(ink, t.quiet_in(side), g), AA),
        pair(
            format!("code {at}"),
            text(ink, 1., over(faded(ink, CODE_WASH), g)),
            AA,
        ),
        pair(
            format!("header {at}"),
            text(ink, 1., over(faded(ink, HEADER_WASH), g)),
            AA,
        ),
        pair(
            format!("spoiler {at}"),
            // An opaque block, so nothing under it shows through.
            if t.spoiler(side).a < 1. {
                0.
            } else {
                contrast(t.spoiler(side), g)
            },
            NON_TEXT,
        ),
        pair(
            format!("bubble {at}"),
            contrast(g, s),
            separation_floor(mode),
        ),
        pair(format!("floating over {at}"), edge(t, g), NON_TEXT),
    ]);
}

/// The chrome on the newest surface: buttons, tabs, menus and inputs.
fn chrome(t: &Theme, out: &mut Vec<Pair>) {
    let n = t.surface(Depth::NOW);
    let tab = over(faded(t.text, TAB_HOVER), n);
    let menu = over(faded(t.text, MENU_HOVER), n);
    let field = over(t.incoming, n);
    let input = t.on_incoming;
    out.extend([
        pair("button text", contrast(t.on_accent, t.accent), AA),
        pair("button", contrast(t.accent, n), NON_TEXT),
        pair(
            "primary focus ring",
            contrast(t.on_accent, t.accent),
            NON_TEXT,
        ),
        pair("secondary focus ring", contrast(t.text, n), NON_TEXT),
        pair("field focus ring", contrast(input, field), NON_TEXT),
        pair("hovered tab", text(t.text, 1., tab), AA),
        pair(
            "hovered quiet tab",
            text(t.text, t.quiet_opacity(), tab),
            AA,
        ),
        pair("rail rule", contrast(t.accent, n), NON_TEXT),
        pair("hovered rail rule", contrast(t.accent, tab), NON_TEXT),
        pair("hovered menu entry", text(t.text, 1., menu), AA),
        pair("hovered menu focus ring", contrast(t.text, menu), NON_TEXT),
        pair("hovered tab focus ring", contrast(t.text, tab), NON_TEXT),
        pair(
            "focused pill text and fill",
            contrast(t.accent, t.on_accent),
            AA,
        ),
        pair("input text", text(input, 1., field), AA),
        pair(
            "input placeholder",
            text(input, PLACEHOLDER, field),
            NON_TEXT,
        ),
        pair("input counter", text(input, t.quiet_opacity(), field), AA),
        pair(
            "selected input text",
            text(input, 1., over(faded(input, SELECTION), field)),
            AA,
        ),
    ]);
}

fn increased_contrast_rules(t: &Theme, out: &mut Vec<Pair>) {
    let n = t.surface(Depth::NOW);
    let field = over(t.incoming, n);
    out.extend([
        pair(
            "settings and menu outline",
            contrast(over(t.line(t.text), n), n),
            NON_TEXT,
        ),
        pair(
            "rail outline",
            contrast(over(t.rule(t.text, 0.08), n), n),
            NON_TEXT,
        ),
        pair(
            "archive outline",
            contrast(over(t.rule(t.text, 0.4), n), n),
            NON_TEXT,
        ),
        pair(
            "input outline",
            contrast(over(t.line(t.on_incoming), field), field),
            NON_TEXT,
        ),
    ]);
    for depth in depths() {
        let ground = t.surface(depth);
        let ink = t.text_on(depth);
        out.push(pair(
            "unread rule",
            contrast(over(t.line(ink), ground), ground),
            NON_TEXT,
        ));
        for side in sides() {
            let bubble = over(t.bubble(side), ground);
            let ink = t.bubble_ink(side);
            let header = over(faded(ink, HEADER_WASH), bubble);
            out.push(pair(
                "media outline",
                contrast(over(t.line(ink), bubble), bubble),
                NON_TEXT,
            ));
            out.push(pair(
                "sender outline",
                contrast(over(t.line(ink), header), header),
                NON_TEXT,
            ));
        }
    }
}

fn every_pair(t: &Theme, mode: Mode) -> Vec<Pair> {
    let mut out = Vec::new();
    for d in depths() {
        on_surface(t, d, &mut out);
        for side in sides() {
            in_bubble(t, mode, side, d, &mut out);
        }
    }
    chrome(t, &mut out);
    if t.has_increased_contrast() {
        increased_contrast_rules(t, &mut out);
    }
    out
}

/// The property every colourway and the abyss picker rely on: nothing on
/// offer can make text unreadable or a mark invisible.
#[test]
fn every_pair_reads_in_every_theme_on_offer() {
    let mut failures = Vec::new();
    for (name, mode, theme) in every_theme() {
        for variant in [theme, theme.increased_contrast()] {
            for p in every_pair(&variant, mode) {
                if p.ratio < p.floor {
                    let increased = variant.has_increased_contrast();
                    failures.push(format!(
                        "{name} (increase contrast {increased}): {} {:.3} < {}",
                        p.what, p.ratio, p.floor
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn settings_sample_outlines_contrast_with_every_fill() {
    let tints = Tint::all().collect::<Vec<_>>();
    let mut fills = every_theme()
        .into_iter()
        .map(|(_, _, theme)| theme.accent)
        .collect::<Vec<_>>();
    for hue in Hue::all() {
        for &tint in &tints {
            let choice = Choice { hue, tint };
            fills.extend([swatch(choice), tint_swatch(choice)]);
        }
    }
    for fill in fills {
        assert!(contrast(Theme::sample_outline(fill), fill) >= NON_TEXT);
    }
}

/// The roles are named for what they paint, and under abyss each one is the
/// color that call site painted before roles existed (colourways plan, task
/// 1), except the incoming spoiler, retuned to show at every depth (task 2).
#[test]
fn abyss_roles_are_the_colors_they_replace() {
    for (name, _, theme) in every_theme()
        .into_iter()
        .filter(|(n, ..)| n.starts_with("Abyss"))
    {
        assert_eq!(theme.accent, theme.text, "{name}");
        assert_eq!(theme.on_accent, theme.on_text, "{name}");
        assert_eq!(theme.bubble(Side::Outgoing), theme.text, "{name}");
        assert_eq!(theme.bubble_ink(Side::Outgoing), theme.on_text, "{name}");
        assert_eq!(theme.bubble(Side::Incoming), theme.incoming, "{name}");
        assert_eq!(theme.bubble_ink(Side::Incoming), theme.text, "{name}");
        for side in sides() {
            assert_eq!(theme.quiet_in(side).to_bits(), QUIET.to_bits(), "{name}");
        }
        for d in depths() {
            assert_eq!(theme.row_mark(d), theme.text_on(d), "{name} {d:?}");
            assert_eq!(
                theme.badge(d),
                (theme.text_on(d), theme.surface(d)),
                "{name}"
            );
        }
    }
    let theme = abyss(Mode::Night);
    let hidden = oklch(HIDDEN, 215., 1.);
    assert_eq!(theme.spoiler(Side::Outgoing), hidden);
    assert_ne!(theme.spoiler(Side::Incoming), hidden, "retuned in task 2");
}

#[test]
fn flat_colourways_are_flat() {
    for mode in Mode::ALL {
        for colourway in flat_colourways() {
            let theme = Theme::new(mode, colourway, Settings::default());
            for d in depths() {
                assert_eq!(theme.surface(d), theme.surface(Depth::NOW), "{colourway:?}");
                assert_eq!(theme.text_on(d), theme.text, "{colourway:?}");
                assert_eq!(theme.row_mark(d), theme.accent, "{colourway:?}");
                assert_eq!(
                    theme.badge(d),
                    (theme.accent, theme.on_accent),
                    "{colourway:?}"
                );
            }
            // The mark keeps the public ramp, whatever the hues say.
            let other = Settings::default().with(
                mode,
                Choice {
                    hue: Hue::all().next().unwrap(),
                    tint: Tint::DEFAULT,
                },
            );
            let retinted = Theme::new(mode, colourway, other);
            assert_eq!(theme, retinted, "{colourway:?} ignores the hues");
            assert!(close(theme.mark[0], rgb(0xdd_ebee)));
        }
    }
}

#[test]
fn abyss_defaults_match_the_brand_sheet() {
    let night = abyss(Mode::Night);
    assert!(close(night.surface(Depth::NOW), rgb(0x0c_333c)));
    assert!(close(night.surface(Depth(DEEPEST)), rgb(0x00_0e13)));
    assert!(close(night.text, rgb(0xdd_ebee)));
    let day = abyss(Mode::Day);
    assert!(close(day.surface(Depth::NOW), rgb(0xf1_f6f7)));
    assert!(close(day.surface(Depth(DEEPEST)), rgb(0x0b_2f31)));
    assert!(close(day.text, rgb(0x0b_2425)));
    // The mark at the night default is the public ramp, surface to floor.
    assert!(close(night.mark[0], rgb(0xdd_ebee)));
    assert!(close(night.mark[5], rgb(0x00_1318)));
}

#[test]
fn old_things_sink_in_abyss() {
    for mode in Mode::ALL {
        let theme = abyss(mode);
        let tones: Vec<f32> = depths().map(|d| luminance(theme.surface(d))).collect();
        assert!(tones.is_sorted_by(|a, b| a > b), "{mode:?}: {tones:?}");
    }
}

#[test]
fn depth_follows_age_in_days() {
    let steps = |days: &[i64]| -> Vec<u8> { days.iter().map(|d| Depth::from_days(*d).0).collect() };
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
