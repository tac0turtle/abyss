//! The user's theme choice and the small file that keeps it.
//!
//! The file is not secret and not precious: three lines of text in
//! Application Support, readable only by the user (`ui::small_file`).
//! Anything unexpected in it (wrong header, a value that is not on offer,
//! extra bytes) means the defaults, never an error the user has to deal
//! with.

use std::io;
use std::path::{Path, PathBuf};

use super::Mode;
use crate::ui::small_file;

const HUE_STEP: u16 = 15;
/// Offsets the grid so both default hues (200 and 215) are on it.
const HUE_OFFSET: u16 = 5;
const HUE_COUNT: u16 = 24;
const TINT_PERCENTS: [u16; 5] = [0, 50, 100, 150, 200];

const HEADER: &str = "abyss-theme 1";
const FILE: &str = "theme";

/// A hue on offer, in degrees: 24 steps around the wheel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Hue(u16);

impl Hue {
    const NIGHT: Self = Self(215);
    const DAY: Self = Self(200);

    pub(crate) fn all() -> impl Iterator<Item = Self> {
        (0..HUE_COUNT).map(|i| Self(HUE_OFFSET + i * HUE_STEP))
    }

    fn from_degrees(degrees: u16) -> Option<Self> {
        (degrees < 360 && degrees % HUE_STEP == HUE_OFFSET).then_some(Self(degrees))
    }

    pub(crate) fn degrees(self) -> f32 {
        f32::from(self.0)
    }
}

/// A tint amount on offer, in percent of the designed chroma: 0 is grey.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tint(u16);

impl Tint {
    pub(crate) const DEFAULT: Self = Self(100);

    pub(crate) fn all() -> impl Iterator<Item = Self> {
        TINT_PERCENTS.into_iter().map(Self)
    }

    fn from_percent(percent: u16) -> Option<Self> {
        TINT_PERCENTS.contains(&percent).then_some(Self(percent))
    }

    pub(crate) fn factor(self) -> f32 {
        f32::from(self.0) / 100.
    }
}

/// The two numbers a mode is built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Choice {
    pub(crate) hue: Hue,
    pub(crate) tint: Tint,
}

/// The user's choice for each mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) night: Choice,
    pub(crate) day: Choice,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            night: Choice {
                hue: Hue::NIGHT,
                tint: Tint::DEFAULT,
            },
            day: Choice {
                hue: Hue::DAY,
                tint: Tint::DEFAULT,
            },
        }
    }
}

impl Settings {
    pub(crate) fn get(self, mode: Mode) -> Choice {
        match mode {
            Mode::Night => self.night,
            Mode::Day => self.day,
        }
    }

    /// These settings with `mode` changed to `choice`.
    #[must_use]
    pub(crate) fn with(self, mode: Mode, choice: Choice) -> Self {
        match mode {
            Mode::Night => Self {
                night: choice,
                ..self
            },
            Mode::Day => Self {
                day: choice,
                ..self
            },
        }
    }
}

fn format(settings: Settings) -> String {
    let line = |name: &str, c: Choice| format!("{name} {} {}\n", c.hue.0, c.tint.0);
    format!(
        "{HEADER}\n{}{}",
        line("night", settings.night),
        line("day", settings.day)
    )
}

/// Exactly the three lines [`format`] writes, or nothing.
fn parse(text: &str) -> Option<Settings> {
    let mut lines = text.lines();
    if lines.next()? != HEADER {
        return None;
    }
    let night = parse_choice(lines.next()?, "night")?;
    let day = parse_choice(lines.next()?, "day")?;
    lines.next().is_none().then_some(Settings { night, day })
}

fn parse_choice(line: &str, name: &str) -> Option<Choice> {
    let mut words = line.split(' ');
    if words.next()? != name {
        return None;
    }
    let hue = Hue::from_degrees(words.next()?.parse().ok()?)?;
    let tint = Tint::from_percent(words.next()?.parse().ok()?)?;
    words.next().is_none().then_some(Choice { hue, tint })
}

/// Where the choice is kept, if the home directory is known.
pub(crate) fn default_path() -> Option<PathBuf> {
    small_file::path(FILE)
}

/// The saved choice, or the defaults if there is none or it is not valid.
pub(crate) fn load(path: &Path) -> Settings {
    match small_file::read(path) {
        Ok(text) => parse(&text).unwrap_or_else(|| {
            tracing::warn!("the saved theme choice is not valid; using the defaults");
            Settings::default()
        }),
        Err(error) => {
            if error.kind() != io::ErrorKind::NotFound {
                // The path holds the user's name; the kind is enough.
                tracing::warn!(kind = ?error.kind(), "couldn't read the theme choice");
            }
            Settings::default()
        }
    }
}

/// Saves the choice (see [`small_file::write`]).
pub(crate) fn save(path: &Path, settings: Settings) -> io::Result<()> {
    small_file::write(path, &format(settings))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn every_choice() -> Vec<Choice> {
        Hue::all()
            .flat_map(|hue| Tint::all().map(move |tint| Choice { hue, tint }))
            .collect()
    }

    /// A directory of this test's own under the system temp directory.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("abyss-theme-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_defaults_are_on_offer() {
        let defaults = Settings::default();
        assert!(every_choice().contains(&defaults.night));
        assert!(every_choice().contains(&defaults.day));
        assert_eq!(every_choice().len(), 24 * 5);
    }

    #[test]
    fn every_choice_survives_the_file_format() {
        for night in every_choice() {
            let settings = Settings::default().with(Mode::Night, night);
            assert_eq!(parse(&format(settings)), Some(settings));
        }
        let day = every_choice()[37];
        let settings = Settings::default().with(Mode::Day, day);
        assert_eq!(parse(&format(settings)), Some(settings));
        assert_eq!(settings.get(Mode::Day), day);
        assert_eq!(settings.get(Mode::Night), Settings::default().night);
    }

    #[test]
    fn anything_unexpected_is_rejected() {
        let good = format(Settings::default());
        assert_eq!(good, "abyss-theme 1\nnight 215 100\nday 200 100\n");
        for bad in [
            "",
            "abyss-theme 2\nnight 215 100\nday 200 100\n",
            "abyss-theme 1\nnight 210 100\nday 200 100\n", // a hue not on offer
            "abyss-theme 1\nnight 365 100\nday 200 100\n",
            "abyss-theme 1\nnight 215 75\nday 200 100\n", // a tint not on offer
            "abyss-theme 1\nnight 215 100 1\nday 200 100\n",
            "abyss-theme 1\nnight 215  100\nday 200 100\n",
            "abyss-theme 1\nnight -215 100\nday 200 100\n",
            "abyss-theme 1\nday 200 100\nnight 215 100\n",
            "abyss-theme 1\nnight 215 100\n",
            "abyss-theme 1\nnight 215 100\nday 200 100\nextra\n",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn saves_privately_and_loads_back() {
        let dir = scratch("roundtrip");
        let path = dir.join("nested").join(FILE);
        let settings = Settings::default().with(Mode::Day, every_choice()[3]);
        save(&path, settings).unwrap();
        assert_eq!(load(&path), settings);
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert!(!path.with_extension("tmp").exists());
        // Saving again replaces the file.
        save(&path, Settings::default()).unwrap();
        assert_eq!(load(&path), Settings::default());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_or_damaged_file_means_the_defaults() {
        let dir = scratch("damaged");
        let path = dir.join(FILE);
        assert_eq!(load(&path), Settings::default(), "missing");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "not a theme").unwrap();
        assert_eq!(load(&path), Settings::default(), "damaged");
        // Only the first bytes are read, so a huge file is just a damaged one.
        let padded = format!("{}{}", format(Settings::default()), "x".repeat(4096));
        fs::write(&path, padded).unwrap();
        assert_eq!(load(&path), Settings::default(), "oversized");
        fs::remove_dir_all(&dir).unwrap();
    }
}
