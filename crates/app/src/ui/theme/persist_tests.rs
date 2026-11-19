//! What `Themes` reads and writes: nothing at startup, and each file saved
//! only when its own choice changes.

use std::fs;
use std::path::{Path, PathBuf};

use gpui::TestAppContext;

use super::colourway::{self, Saved};
use super::{Choice, Colourway, Hue, Mode, Settings, Themes, Tint, settings};

/// A trusted parent (standing in for Application Support) and the app's
/// directory in it, not created yet.
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

fn init(app: &Path, cx: &mut TestAppContext) -> Colourway {
    cx.update(|cx| {
        Themes::init_in(app, cx);
        Themes::colourway(cx)
    })
}

fn saved(app: &Path) -> Saved {
    colourway::load(&colourway::path_in(app))
}

#[gpui::test]
fn startup_shows_ultramarine_and_writes_nothing(cx: &mut TestAppContext) {
    let (_parent, app) = new_install();
    assert_eq!(init(&app, cx), Colourway::Ultramarine);
    cx.run_until_parked();
    assert!(!app.exists(), "nothing created");
}

/// A damaged file, or one that can't be read, means the default and is
/// left alone until the user picks.
#[gpui::test]
fn an_unreadable_or_invalid_file_means_the_default_unwritten(cx: &mut TestAppContext) {
    let (parent, app) = new_install();
    fs::create_dir(&app).unwrap();
    let path = colourway::path_in(&app);
    fs::create_dir(&path).unwrap();
    assert_eq!(init(&app, cx), Colourway::DEFAULT);
    assert!(path.is_dir(), "a read error is not rewritten");
    fs::remove_dir(&path).unwrap();
    // A rename would replace this link, so it shows any write.
    std::os::unix::fs::symlink(parent.path().join("nowhere"), &path).unwrap();
    assert_eq!(init(&app, cx), Colourway::DEFAULT);
    assert!(path.symlink_metadata().unwrap().file_type().is_symlink());
    fs::remove_file(&path).unwrap();
    fs::write(&path, "abyss-colourway 1\nneon\n").unwrap();
    assert_eq!(init(&app, cx), Colourway::DEFAULT);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "abyss-colourway 1\nneon\n"
    );
}

/// Both saved choices come back at the next launch.
#[gpui::test]
fn saved_choices_load_back(cx: &mut TestAppContext) {
    let (_parent, app) = new_install();
    settings::save(&settings::path_in(&app), custom()).unwrap();
    for colourway in Colourway::ALL {
        colourway::save(&colourway::path_in(&app), colourway).unwrap();
        assert_eq!(init(&app, cx), colourway);
        assert_eq!(cx.update(|cx| Themes::settings(cx)), custom());
    }
}

/// Picking a colourway writes only `colourway`; Reset writes only `theme`,
/// keeps abyss, and the hues survive every colourway.
#[gpui::test]
fn each_choice_saves_only_its_own_file(cx: &mut TestAppContext) {
    let (_parent, app) = new_install();
    assert_eq!(init(&app, cx), Colourway::Ultramarine);
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
        Themes::reset_colours(cx);
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
    init(&app, cx);
    cx.update(|cx| {
        Themes::set_colourway(Colourway::Abyss, cx);
        Themes::set(custom(), cx);
        Themes::set_colourway(Colourway::Sky, cx);
        Themes::reset_colours(cx);
        Themes::set(custom(), cx);
        Themes::set_colourway(Colourway::Ultramarine, cx);
        Themes::set_colourway(Colourway::Abyss, cx);
    });
    cx.run_until_parked();
    assert_eq!(saved(&app), Saved::Chosen(Colourway::Abyss));
    assert_eq!(settings::load(&settings::path_in(&app)), custom());
}
