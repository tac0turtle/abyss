//! Cache location and directory preparation (tasks 9b, 9c).

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::*;

/// A temporary home with `Library/Application Support`, as on macOS.
fn home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("Library/Application Support")).unwrap();
    home
}

fn real(home: &Path) -> CacheLocation {
    CacheLocation::real(Some(home.to_path_buf())).unwrap()
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn the_real_cache_is_in_application_support() {
    let location = CacheLocation::real(Some("/Users/someone".into())).unwrap();
    assert_eq!(
        location.dir(),
        Path::new("/Users/someone/Library/Application Support/dev.abyss/cache")
    );
}

#[test]
fn an_unknown_or_relative_home_has_no_cache() {
    assert!(matches!(
        CacheLocation::real(None),
        Err(StorageError::NoHome)
    ));
    for bad in ["", "relative/home", "~"] {
        assert!(matches!(
            CacheLocation::real(Some(bad.into())),
            Err(StorageError::NoHome)
        ));
    }
    assert!(config(CacheLocation::real(None)).is_none());
}

#[test]
fn prepare_creates_private_directories() {
    let home = home();
    let location = real(home.path());
    let dir = prepare(&location).unwrap();
    assert_eq!(dir, location.dir());
    let app = home.path().join("Library/Application Support/dev.abyss");
    assert_eq!(mode(&app), 0o700);
    assert_eq!(mode(&dir), 0o700);
    // Idempotent: a second launch finds them in place.
    assert_eq!(prepare(&location).unwrap(), dir);
    let config = config(Ok(location)).unwrap();
    assert_eq!(config.dir, dir);
}

#[test]
fn prepare_narrows_a_wider_mode() {
    let home = home();
    let app = home.path().join("Library/Application Support/dev.abyss");
    std::fs::create_dir(&app).unwrap();
    std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir(app.join("cache")).unwrap();
    std::fs::set_permissions(app.join("cache"), std::fs::Permissions::from_mode(0o777)).unwrap();
    let dir = prepare(&real(home.path())).unwrap();
    assert_eq!(mode(&app), 0o700);
    assert_eq!(mode(&dir), 0o700);
}

#[test]
fn a_symlinked_component_is_refused() {
    let home = home();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::set_permissions(elsewhere.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let app = home.path().join("Library/Application Support/dev.abyss");
    // The cache directory is a symlink to a real directory.
    std::fs::create_dir(&app).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), app.join("cache")).unwrap();
    assert!(matches!(
        prepare(&real(home.path())),
        Err(StorageError::Symlink("cache"))
    ));
    // The target's mode was not touched through the link.
    assert_eq!(mode(elsewhere.path()), 0o755);
    assert!(config(Ok(real(home.path()))).is_none());

    // The app directory itself is a symlink.
    let home = self::home();
    let app = home.path().join("Library/Application Support/dev.abyss");
    std::os::unix::fs::symlink(elsewhere.path(), &app).unwrap();
    assert!(matches!(
        prepare(&real(home.path())),
        Err(StorageError::Symlink("dev.abyss"))
    ));
    assert!(!elsewhere.path().join("cache").exists());
}

#[test]
fn a_file_in_place_or_a_missing_root_has_no_cache() {
    let home = home();
    let app = home.path().join("Library/Application Support/dev.abyss");
    std::fs::write(&app, b"").unwrap();
    assert!(matches!(
        prepare(&real(home.path())),
        Err(StorageError::NotADirectory("dev.abyss"))
    ));
    // No `Library/Application Support`: nothing is created above the root.
    let bare = tempfile::tempdir().unwrap();
    assert!(matches!(
        prepare(&real(bare.path())),
        Err(StorageError::Root(_))
    ));
    assert!(!bare.path().join("Library").exists());
}

#[test]
fn errors_never_name_the_path() {
    let error = StorageError::Io("cache", std::io::Error::other("boom"));
    assert_eq!(error.to_string(), "\"cache\": boom");
}

#[cfg(feature = "fake")]
mod fake {
    use std::ffi::OsString;

    use super::*;

    fn fake(tmpdir: &Path, home: &Path) -> CacheLocation {
        let tmp = Some(tmpdir.as_os_str().to_owned());
        CacheLocation::fake(Some("1".as_ref()), tmp, Some(home.to_path_buf())).unwrap()
    }

    #[test]
    fn the_fake_has_no_cache_unless_asked() {
        let tmp = Some(OsString::from("/tmp/x"));
        assert_eq!(CacheLocation::fake(None, tmp.clone(), None), None);
        for value in ["", "0", "true", "yes", " 1"] {
            assert_eq!(
                CacheLocation::fake(Some(value.as_ref()), tmp.clone(), None),
                None,
                "{value:?}"
            );
        }
        // Asked, but without an absolute TMPDIR.
        assert_eq!(CacheLocation::fake(Some("1".as_ref()), None, None), None);
        let relative = Some("rel".into());
        assert_eq!(
            CacheLocation::fake(Some("1".as_ref()), relative, None),
            None
        );
    }

    #[test]
    fn the_fake_cache_is_never_the_real_one() {
        let home = home();
        let support = home.path().join("Library/Application Support");
        // Even with TMPDIR pointed at Application Support.
        let fake = fake(&support, home.path());
        let real = real(home.path());
        assert_ne!(fake.dir(), real.dir());
        assert!(!fake.dir().starts_with(real.dir()));
        let dir = prepare(&fake).unwrap();
        assert!(dir.starts_with(support.join("dev.abyss.fake")));
        assert_eq!(mode(&dir), 0o700);
        assert!(!real.dir().exists(), "the fake touched the real cache");
    }
}
