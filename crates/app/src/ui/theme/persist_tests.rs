//! What `Themes` reads and writes (colourways plan, sections 3 and 6): the
//! first-launch decision, its policy, and each file saved only when its own
//! choice changes.

use std::fs;
use std::path::{Path, PathBuf};

use gpui::TestAppContext;

use super::colourway::{self, FirstLaunch, Saved};
use super::{Choice, Colourway, Hue, Mode, Settings, Themes, Tint, settings};

/// A trusted parent (standing in for Application Support) and the app's
/// directory in it, not created yet: a new install.
fn new_install() -> (tempfile::TempDir, PathBuf) {
    let parent = tempfile::tempdir().unwrap();
    let app = parent.path().join("dev.abyss");
    (parent, app)
}

/// Hues that differ from the defaults in both modes.
fn custom() -> Settings {
    let choice = Choice {
        hue: Hue::all().next().unwrap(),
        tint: Tint::all().last().unwrap(),
    };
    Settings::default()
        .with(Mode::Night, choice)
        .with(Mode::Day, choice)
}

fn init(app: &Path, policy: FirstLaunch, cx: &mut TestAppContext) -> Colourway {
    cx.update(|cx| {
        Themes::init_in(app, policy, cx);
        Themes::colourway(cx)
    })
}

fn saved(app: &Path) -> Saved {
    colourway::load(&colourway::path_in(app))
}

#[gpui::test]
fn a_new_install_gets_ultramarine_written_before_init_returns(cx: &mut TestAppContext) {
    let (_parent, app) = new_install();
    assert_eq!(init(&app, FirstLaunch::Persist, cx), Colourway::Ultramarine);
    // No executor has run: the write happened inside `init_in`.
    assert_eq!(saved(&app), Saved::Chosen(Colourway::Ultramarine));
    assert!(!settings::path_in(&app).exists(), "no theme file");
}

#[gpui::test]
fn a_theme_file_means_an_existing_install(cx: &mut TestAppContext) {
    let (_parent, app) = new_install();
    settings::save(&settings::path_in(&app), custom()).unwrap();
    assert_eq!(init(&app, FirstLaunch::Persist, cx), Colourway::Abyss);
    assert_eq!(saved(&app), Saved::Chosen(Colourway::Abyss));
    assert_eq!(cx.update(|cx| Themes::settings(cx)), custom());
}

#[gpui::test]
fn a_cache_directory_means_an_existing_install(cx: &mut TestAppContext) {
    let (_parent, app) = new_install();
    fs::create_dir_all(crate::storage::cache_dir_in(&app)).unwrap();
    assert_eq!(init(&app, FirstLaunch::Persist, cx), Colourway::Abyss);
    assert_eq!(saved(&app), Saved::Chosen(Colourway::Abyss));
}

/// A fake or failed launch writes nothing, so the next real launch still
/// decides.
#[gpui::test]
fn memory_only_writes_nothing_and_the_real_launch_still_decides(cx: &mut TestAppContext) {
    let (_parent, app) = new_install();
    assert_eq!(init(&app, FirstLaunch::MemoryOnly, cx), Colourway::DEFAULT);
    cx.run_until_parked();
    assert!(!app.exists(), "nothing created");
    // The real launch comes after the session made the cache.
    fs::create_dir_all(crate::storage::cache_dir_in(&app)).unwrap();
    assert_eq!(init(&app, FirstLaunch::Persist, cx), Colourway::Abyss);
    assert_eq!(saved(&app), Saved::Chosen(Colourway::Abyss));
}

/// Only a missing file is a first launch: a damaged one, or one that can't
/// be read, means the default and is left alone.
#[gpui::test]
fn an_unreadable_or_invalid_file_means_the_default_unwritten(cx: &mut TestAppContext) {
    let (_parent, app) = new_install();
    fs::create_dir(&app).unwrap();
    let path = colourway::path_in(&app);
    fs::create_dir(&path).unwrap();
    assert_eq!(init(&app, FirstLaunch::Persist, cx), Colourway::DEFAULT);
    assert!(path.is_dir(), "a read error is not rewritten");
    fs::remove_dir(&path).unwrap();
    fs::write(&path, "abyss-colourway 1\nneon\n").unwrap();
    assert_eq!(init(&app, FirstLaunch::Persist, cx), Colourway::DEFAULT);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "abyss-colourway 1\nneon\n"
    );
}

/// Picking a colourway writes only `colourway`; Reset writes only `theme`,
/// keeps abyss, and the hues survive every colourway.
#[gpui::test]
fn each_choice_saves_only_its_own_file(cx: &mut TestAppContext) {
    let (_parent, app) = new_install();
    assert_eq!(init(&app, FirstLaunch::Persist, cx), Colourway::Ultramarine);
    cx.update(|cx| Themes::set_colourway(Colourway::Sky, cx));
    cx.run_until_parked();
    assert_eq!(saved(&app), Saved::Chosen(Colourway::Sky));
    assert!(
        !settings::path_in(&app).exists(),
        "picking never writes hues"
    );

    cx.update(|cx| {
        Themes::set_colourway(Colourway::Abyss, cx);
        Themes::set(custom(), cx);
    });
    cx.run_until_parked();
    let theme = settings::path_in(&app);
    assert_eq!(settings::load(&theme), custom());

    cx.update(|cx| {
        Themes::reset_hues(cx);
        assert_eq!(Themes::colourway(cx), Colourway::Abyss);
    });
    cx.run_until_parked();
    assert_eq!(settings::load(&theme), Settings::default());
    assert_eq!(saved(&app), Saved::Chosen(Colourway::Abyss));
}

/// Changes made faster than they save land on disk in the order made.
#[gpui::test]
fn the_last_choice_is_the_one_on_disk(cx: &mut TestAppContext) {
    let (_parent, app) = new_install();
    init(&app, FirstLaunch::Persist, cx);
    cx.update(|cx| {
        Themes::set_colourway(Colourway::Abyss, cx);
        Themes::set(custom(), cx);
        Themes::set_colourway(Colourway::Sky, cx);
        Themes::reset_hues(cx);
        Themes::set(custom(), cx);
        Themes::set_colourway(Colourway::Ultramarine, cx);
        Themes::set_colourway(Colourway::Abyss, cx);
    });
    cx.run_until_parked();
    assert_eq!(saved(&app), Saved::Chosen(Colourway::Abyss));
    assert_eq!(settings::load(&settings::path_in(&app)), custom());
}

/// The `theme` file keeps its v1 bytes under every colourway, so an older
/// build reads it back, and its save reads back here unchanged.
#[gpui::test]
fn the_theme_file_survives_every_colourway_and_a_downgrade(cx: &mut TestAppContext) {
    let (_parent, app) = new_install();
    let theme = settings::path_in(&app);
    settings::save(&theme, custom()).unwrap();
    let v1 = fs::read(&theme).unwrap();
    for colourway in Colourway::ALL {
        colourway::save(&colourway::path_in(&app), colourway).unwrap();
        assert_eq!(init(&app, FirstLaunch::Persist, cx), colourway);
        assert_eq!(cx.update(|cx| Themes::settings(cx)), custom());
        cx.update(|cx| Themes::set_colourway(Colourway::Sky, cx));
        cx.run_until_parked();
        assert_eq!(
            fs::read(&theme).unwrap(),
            v1,
            "untouched under {colourway:?}"
        );
    }
    // An older build saves hues in the same format and never reads
    // `colourway`; the upgrade reads both back.
    settings::save(&theme, Settings::default()).unwrap();
    assert_eq!(init(&app, FirstLaunch::Persist, cx), Colourway::Sky);
    assert_eq!(cx.update(|cx| Themes::settings(cx)), Settings::default());
}
