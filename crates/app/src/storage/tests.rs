//! Cache location and directory preparation (tasks 9b, 9c). Every test takes
//! the Application Support root as a parameter, so the container's layout is
//! tested without a sandboxed test runner (Stage 3 plan section 7).

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::*;

/// The container's Application Support, as a sandboxed run sees it.
const CONTAINER_SUPPORT: &str =
    "/Users/someone/Library/Containers/dev.abyss/Data/Library/Application Support";

/// A temporary Application Support directory (an existing, empty root).
fn support() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn real(support: &Path) -> CacheLocation {
    CacheLocation::real(Some(support.to_path_buf())).unwrap()
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn the_real_cache_is_in_the_given_root() {
    // The real tree, unsandboxed.
    let plain = Path::new("/Users/someone/Library/Application Support");
    let location = CacheLocation::real(Some(plain.into())).unwrap();
    assert_eq!(location.dir(), plain.join("dev.abyss/cache"));
    // The container's tree, sandboxed: the same layout below another root.
    let location = CacheLocation::real(Some(CONTAINER_SUPPORT.into())).unwrap();
    assert_eq!(
        location.dir(),
        Path::new(CONTAINER_SUPPORT).join("dev.abyss/cache")
    );
}

#[test]
fn an_unknown_or_relative_root_has_no_cache() {
    assert!(matches!(
        CacheLocation::real(None),
        Err(StorageError::NoRoot)
    ));
    for bad in ["", "relative/support", "~"] {
        assert!(matches!(
            CacheLocation::real(Some(bad.into())),
            Err(StorageError::NoRoot)
        ));
    }
    assert!(config(CacheLocation::real(None)).is_none());
}

#[test]
fn prepare_creates_private_directories() {
    let support = support();
    let location = real(support.path());
    let dir = prepare(&location).unwrap();
    assert_eq!(dir, location.dir());
    let app = support.path().join("dev.abyss");
    assert_eq!(mode(&app), 0o700);
    assert_eq!(mode(&dir), 0o700);
    // Idempotent: a second launch finds them in place.
    assert_eq!(prepare(&location).unwrap(), dir);
    let config = config(Ok(location)).unwrap();
    assert_eq!(config.dir, dir);
}

#[test]
fn prepare_narrows_a_wider_mode() {
    let support = support();
    let app = support.path().join("dev.abyss");
    std::fs::create_dir(&app).unwrap();
    std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir(app.join("cache")).unwrap();
    std::fs::set_permissions(app.join("cache"), std::fs::Permissions::from_mode(0o777)).unwrap();
    let dir = prepare(&real(support.path())).unwrap();
    assert_eq!(mode(&app), 0o700);
    assert_eq!(mode(&dir), 0o700);
}

#[test]
fn a_symlinked_component_is_refused() {
    let support = support();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::set_permissions(elsewhere.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let app = support.path().join("dev.abyss");
    // The cache directory is a symlink to a real directory.
    std::fs::create_dir(&app).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), app.join("cache")).unwrap();
    assert!(matches!(
        prepare(&real(support.path())),
        Err(StorageError::Symlink("cache"))
    ));
    // The target's mode was not touched through the link.
    assert_eq!(mode(elsewhere.path()), 0o755);
    assert!(config(Ok(real(support.path()))).is_none());

    // The app directory itself is a symlink.
    let support = self::support();
    let app = support.path().join("dev.abyss");
    std::os::unix::fs::symlink(elsewhere.path(), &app).unwrap();
    assert!(matches!(
        prepare(&real(support.path())),
        Err(StorageError::Symlink("dev.abyss"))
    ));
    assert!(!elsewhere.path().join("cache").exists());
}

#[test]
fn a_file_in_place_or_a_missing_root_has_no_cache() {
    let support = support();
    let app = support.path().join("dev.abyss");
    std::fs::write(&app, b"").unwrap();
    assert!(matches!(
        prepare(&real(support.path())),
        Err(StorageError::NotADirectory("dev.abyss"))
    ));
    // No root: nothing is created above it.
    let parent = tempfile::tempdir().unwrap();
    let missing = parent.path().join("Library/Application Support");
    assert!(matches!(
        prepare(&real(&missing)),
        Err(StorageError::Root(_))
    ));
    assert!(!parent.path().join("Library").exists());
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

    fn fake(tmpdir: &Path, support: &Path) -> CacheLocation {
        let tmp = Some(tmpdir.as_os_str().to_owned());
        CacheLocation::fake(Some("1".as_ref()), tmp, Some(support.to_path_buf())).unwrap()
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
        let support = support();
        // Even with TMPDIR pointed at Application Support itself.
        let fake = fake(support.path(), support.path());
        let real = real(support.path());
        assert_ne!(fake.dir(), real.dir());
        assert!(!fake.dir().starts_with(real.dir()));
        let dir = prepare(&fake).unwrap();
        assert!(dir.starts_with(support.path().join("dev.abyss.fake")));
        assert_eq!(mode(&dir), 0o700);
        assert!(!real.dir().exists(), "the fake touched the real cache");
    }

    /// The overlap refusal works against the root it is given, the
    /// container's included.
    #[test]
    fn a_fake_root_inside_the_real_app_directory_is_refused() {
        let support = support();
        let app = support.path().join("dev.abyss");
        std::fs::create_dir_all(app.join("nested")).unwrap();
        for tmpdir in [app.clone(), app.join("nested")] {
            let location = fake(&tmpdir, support.path());
            assert!(matches!(prepare(&location), Err(StorageError::Overlap)));
            assert!(!app.join("dev.abyss.fake").exists());
        }
        // A symlink into it resolves inside it and is refused too.
        let link = tempfile::tempdir().unwrap();
        let link = link.path().join("tmp");
        std::os::unix::fs::symlink(&app, &link).unwrap();
        let location = fake(&link, support.path());
        assert!(matches!(prepare(&location), Err(StorageError::Overlap)));
        // A sibling is fine.
        let sibling = tempfile::tempdir().unwrap();
        assert!(prepare(&fake(sibling.path(), support.path())).is_ok());
    }
}

/// Foundation's answer is an absolute Application Support directory: the
/// real one here (the test runner is not sandboxed), the container's in the
/// bundle. Whether `HOME` is ignored is checked on the built bundle.
#[cfg(target_os = "macos")]
#[test]
fn foundation_gives_an_absolute_application_support() {
    let support = support_dir().unwrap();
    assert!(support.is_absolute());
    assert!(
        support.ends_with("Library/Application Support"),
        "{support:?}"
    );
    assert_eq!(app_dir().unwrap(), support.join("dev.abyss"));
}
