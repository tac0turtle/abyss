//! Which colourway is showing, and the small file that keeps it
//! (colourways plan, section 3).
//!
//! The file is `abyss-colourway 1` and one name. It sits beside the `theme`
//! file, which keeps abyss's hues in its own unchanged format, so an older
//! build (which never reads this file) still shows abyss with the user's
//! hues.
//!
//! The first real launch without the file decides once and writes the
//! choice ([`decide`]): an existing install keeps abyss, a new one gets
//! Ultramarine.

use std::io;
use std::path::{Path, PathBuf};

use crate::ui::small_file;

const HEADER: &str = "abyss-colourway 1";
const FILE: &str = "colourway";

/// A colourway: the structure and colors every theme role takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Colourway {
    /// The tonal strata: one user-chosen hue per mode, no accent, surfaces
    /// that darken with age.
    Abyss,
    /// Flat neutrals and a saturated blue for the user's own messages.
    Ultramarine,
    /// Flat, slightly cool surfaces and a sky blue.
    Sky,
}

impl Colourway {
    /// The default for a new install, and whenever the file can't be
    /// trusted.
    pub(crate) const DEFAULT: Self = Self::Ultramarine;

    pub(crate) const ALL: [Self; 3] = [Self::Ultramarine, Self::Sky, Self::Abyss];

    /// Its name in the file and, capitalized, in the picker.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Abyss => "abyss",
            Self::Ultramarine => "ultramarine",
            Self::Sky => "sky",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.key() == key)
    }
}

/// What the file says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Saved {
    /// A valid choice.
    Chosen(Colourway),
    /// No file: nothing has been chosen yet.
    Missing,
    /// Anything else: a damaged file, an unknown name, a symlink, an error
    /// reading it. The default shows and the file is left alone until the
    /// user picks something.
    Invalid,
}

impl Saved {
    /// The colourway to show.
    pub(crate) fn colourway(self) -> Colourway {
        match self {
            Self::Chosen(colourway) => colourway,
            Self::Missing | Self::Invalid => Colourway::DEFAULT,
        }
    }
}

fn format(colourway: Colourway) -> String {
    format!("{HEADER}\n{}\n", colourway.key())
}

/// Exactly the two lines [`format`] writes, or nothing.
fn parse(text: &str) -> Option<Colourway> {
    let mut lines = text.lines();
    if lines.next()? != HEADER {
        return None;
    }
    let colourway = Colourway::from_key(lines.next()?)?;
    (lines.next().is_none() && text.ends_with('\n')).then_some(colourway)
}

/// Where the choice is kept in `app_dir`.
pub(crate) fn path_in(app_dir: &Path) -> PathBuf {
    app_dir.join(FILE)
}

/// What the file at `path` says. Only a missing file is [`Saved::Missing`].
pub(crate) fn load(path: &Path) -> Saved {
    match small_file::read(path) {
        Ok(text) => parse(&text).map_or_else(
            || {
                tracing::warn!("the saved colourway is not valid; using the default");
                Saved::Invalid
            },
            Saved::Chosen,
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Saved::Missing,
        Err(error) => {
            // The path holds the user's name; the kind is enough.
            tracing::warn!(kind = ?error.kind(), "couldn't read the colourway");
            Saved::Invalid
        }
    }
}

/// Saves the choice (see [`small_file::write`]).
pub(crate) fn save(path: &Path, colourway: Colourway) -> io::Result<()> {
    small_file::write(path, &format(colourway))
}

/// Whether a launch may write its first-launch decision. Only a real
/// Telegram launch persists; a fake or failed one keeps it in memory, so a
/// development run can never pin the real install.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FirstLaunch {
    Persist,
    MemoryOnly,
}

/// The colourway to show at startup, from the file in `app_dir`. Only a
/// missing file is a first launch: under [`FirstLaunch::Persist`] an
/// existing install (a `theme` file or the cache directory is there) keeps
/// abyss and a new one gets Ultramarine, and the choice is written before
/// this returns, so the window (which creates the cache) can't sway the
/// next launch. A failed write keeps the choice for this run and logs it.
pub(crate) fn decide(app_dir: &Path, policy: FirstLaunch) -> Colourway {
    let path = path_in(app_dir);
    let saved = load(&path);
    if saved != Saved::Missing || policy == FirstLaunch::MemoryOnly {
        return saved.colourway();
    }
    let chosen = if existing_install(app_dir) {
        Colourway::Abyss
    } else {
        Colourway::DEFAULT
    };
    if let Err(error) = save(&path, chosen) {
        // The path holds the user's name; the kind is enough.
        tracing::warn!(kind = ?error.kind(), "couldn't save the first colourway");
    }
    chosen
}

/// Anything at the `theme` file's or the cache's path: an earlier build has
/// run here. Links are not followed; an entry is enough.
fn existing_install(app_dir: &Path) -> bool {
    [
        super::settings::path_in(app_dir),
        crate::storage::cache_dir_in(app_dir),
    ]
    .iter()
    .any(|path| path.symlink_metadata().is_ok())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn every_colourway_survives_the_file_format() {
        for colourway in Colourway::ALL {
            assert_eq!(parse(&format(colourway)), Some(colourway));
        }
        assert_eq!(format(Colourway::Sky), "abyss-colourway 1\nsky\n");
    }

    #[test]
    fn anything_unexpected_is_rejected() {
        for bad in [
            "",
            "abyss-colourway 1\n",
            "abyss-colourway 1\nsky",
            "abyss-colourway 2\nsky\n",
            "abyss-colourway 1\nSky\n",
            "abyss-colourway 1\nneon\n",
            "abyss-colourway 1\nsky \n",
            "abyss-colourway 1\nsky\nabyss\n",
            "abyss-theme 1\nnight 215 100\nday 200 100\n",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn only_a_missing_file_is_missing() {
        let parent = tempfile::tempdir().unwrap();
        let app = parent.path().join("dev.abyss");
        let path = path_in(&app);
        assert_eq!(load(&path), Saved::Missing);
        save(&path, Colourway::Ultramarine).unwrap();
        assert_eq!(load(&path), Saved::Chosen(Colourway::Ultramarine));
        fs::write(&path, "abyss-colourway 1\nneon\n").unwrap();
        assert_eq!(load(&path), Saved::Invalid);
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(parent.path().join("nowhere"), &path).unwrap();
        assert_eq!(load(&path), Saved::Invalid, "a dangling symlink");
        assert_eq!(Saved::Invalid.colourway(), Colourway::DEFAULT);
        assert_eq!(Saved::Missing.colourway(), Colourway::DEFAULT);
    }
}
