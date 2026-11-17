//! What to start: the fake backend (`--fake`, optionally `--onboarding`) or
//! the real one (plan sections 12 and 17.1). The real backend resolves its
//! API credentials in core: the env pair (`ABYSS_API_ID`/`ABYSS_API_HASH`)
//! is only read here and handed over as an [`EnvResolution`]; a missing pair
//! is not a launch error any more, it is the onboarding. Credentials are
//! never logged.

use std::ffi::{OsStr, OsString};

use abyss_core::credentials::{EnvResolution, from_env};

use crate::storage::{self, CacheLocation, StorageError};

pub(crate) const FAKE_FLAG: &str = "--fake";
/// With `--fake`: start from `NeedCredentials`, as on a first run.
#[cfg(feature = "fake")]
pub(crate) const ONBOARDING_FLAG: &str = "--onboarding";

/// Where the fake starts.
#[cfg(feature = "fake")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FakeStart {
    /// Signed in to the demo account, with an env-like pair.
    Demo,
    /// No credentials: the onboarding from the welcome screen.
    Onboarding,
}

#[derive(Debug)]
pub(crate) enum Launch {
    /// The in-process fake backend with a demo account, and any faults asked
    /// for in [`abyss_core::FAKE_FAULTS_VAR`].
    #[cfg(feature = "fake")]
    Fake {
        faults: abyss_core::FakeFaults,
        start: FakeStart,
        /// `None` (the default) runs without a disk cache; see `storage`.
        cache: Option<CacheLocation>,
    },
    Telegram {
        env: EnvResolution,
        /// The real cache's location (task 9b); an error runs without one.
        cache: Result<CacheLocation, StorageError>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LaunchError {
    /// `--fake` was passed to a build without the `fake` feature.
    #[cfg_attr(
        all(feature = "fake", not(test)),
        expect(
            dead_code,
            reason = "only builds without the fake backend reject --fake"
        )
    )]
    FakeNotBuilt,
}

/// Decides what to start from the command line and an environment lookup.
/// The home directory comes from the user database (`storage::home_dir`),
/// not `HOME`.
pub(crate) fn resolve(
    args: impl IntoIterator<Item = OsString>,
    var: impl Fn(&str) -> Option<OsString>,
) -> Result<Launch, LaunchError> {
    resolve_in(args, var, storage::home_dir())
}

/// [`resolve`] with the home directory given.
#[cfg_attr(
    feature = "fake",
    expect(
        clippy::unnecessary_wraps,
        reason = "only builds without the fake backend fail (`FakeNotBuilt`)"
    )
)]
pub(crate) fn resolve_in(
    args: impl IntoIterator<Item = OsString>,
    var: impl Fn(&str) -> Option<OsString>,
    home: Option<std::path::PathBuf>,
) -> Result<Launch, LaunchError> {
    // Skip argv[0]; ignore anything else (macOS may add `-psn_…`).
    let args: Vec<OsString> = args.into_iter().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == OsStr::new(flag));
    if has(FAKE_FLAG) {
        #[cfg(feature = "fake")]
        return Ok(Launch::Fake {
            faults: var(abyss_core::FAKE_FAULTS_VAR)
                .and_then(|v| v.into_string().ok())
                .map(|v| abyss_core::FakeFaults::parse(&v))
                .unwrap_or_default(),
            start: if has(ONBOARDING_FLAG) {
                FakeStart::Onboarding
            } else {
                FakeStart::Demo
            },
            cache: CacheLocation::fake(
                var(storage::FAKE_CACHE_VAR).as_deref(),
                var(storage::TMPDIR_VAR),
                home,
            ),
        });
        #[cfg(not(feature = "fake"))]
        return Err(LaunchError::FakeNotBuilt);
    }
    // A non-UTF-8 value counts as malformed: it is set, but unusable.
    let env = from_env(|name| {
        var(name).map(|v| v.into_string().unwrap_or_else(|_| "\u{fffd}".to_owned()))
    });
    Ok(Launch::Telegram {
        env,
        cache: CacheLocation::real(home),
    })
}

#[cfg(test)]
mod tests {
    use abyss_core::api::EnvVar;

    use super::*;

    const API_ID_VAR: &str = "ABYSS_API_ID";
    const API_HASH_VAR: &str = "ABYSS_API_HASH";

    fn args(list: &[&str]) -> Vec<OsString> {
        std::iter::once("abyss")
            .chain(list.iter().copied())
            .map(OsString::from)
            .collect()
    }

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| OsString::from(v))
        }
    }

    fn telegram(launch: Result<Launch, LaunchError>) -> EnvResolution {
        match launch.unwrap() {
            Launch::Telegram { env, .. } => env,
            #[cfg(feature = "fake")]
            Launch::Fake { .. } => panic!("expected the real backend"),
        }
    }

    const HASH: &str = "0123456789ABCDEF0123456789abcdef";

    #[test]
    fn no_env_pair_is_the_onboarding_not_an_error() {
        assert_eq!(
            telegram(resolve(args(&[]), env(&[]))),
            EnvResolution::Absent
        );
    }

    #[test]
    fn a_partial_env_pair_names_the_missing_variable() {
        assert_eq!(
            telegram(resolve(
                args(&[]),
                env(&[(API_ID_VAR, "1"), (API_HASH_VAR, "")])
            )),
            EnvResolution::Partial {
                missing: EnvVar::ApiHash
            }
        );
    }

    #[test]
    fn malformed_env_values() {
        for bad in ["abc", "-1", "0", "99999999999"] {
            assert_eq!(
                telegram(resolve(
                    args(&[]),
                    env(&[(API_ID_VAR, bad), (API_HASH_VAR, HASH)])
                )),
                EnvResolution::Malformed { var: EnvVar::ApiId }
            );
        }
        assert_eq!(
            telegram(resolve(
                args(&[]),
                env(&[(API_ID_VAR, "1"), (API_HASH_VAR, "xyz")])
            )),
            EnvResolution::Malformed {
                var: EnvVar::ApiHash
            }
        );
    }

    #[test]
    fn credentials_from_env() {
        let env = telegram(resolve(
            args(&["-psn_0_1"]),
            env(&[(API_ID_VAR, " 42 "), (API_HASH_VAR, HASH)]),
        ));
        let EnvResolution::Pair(c) = env else {
            panic!("expected a pair");
        };
        assert_eq!(c.api_id.get(), 42);
        assert_eq!(c.api_hash.expose(), HASH.to_lowercase());
        assert!(!format!("{c:?}").contains("0123"), "the hash is redacted");
    }

    #[test]
    fn fake_flag_skips_credentials() {
        let launch = resolve(args(&[FAKE_FLAG]), env(&[]));
        #[cfg(feature = "fake")]
        assert!(matches!(
            launch,
            Ok(Launch::Fake { faults, start: FakeStart::Demo, cache: None })
                if faults == abyss_core::FakeFaults::default()
        ));
        #[cfg(not(feature = "fake"))]
        assert_eq!(launch.unwrap_err(), LaunchError::FakeNotBuilt);
    }

    #[cfg(feature = "fake")]
    #[test]
    fn onboarding_flag_starts_the_fake_without_credentials() {
        let launch = resolve(args(&[ONBOARDING_FLAG, FAKE_FLAG]), env(&[]));
        assert!(matches!(
            launch,
            Ok(Launch::Fake {
                start: FakeStart::Onboarding,
                ..
            })
        ));
    }

    #[cfg(feature = "fake")]
    #[test]
    fn fake_faults_from_env() {
        let launch = resolve(
            args(&[FAKE_FLAG]),
            env(&[(abyss_core::FAKE_FAULTS_VAR, "store-stall")]),
        );
        let Ok(Launch::Fake { faults, .. }) = launch else {
            panic!("expected the fake");
        };
        assert!(faults.store_stall);
    }

    /// Task 9b: the real backend gets the Application Support cache.
    #[test]
    fn the_real_backend_gets_the_real_cache() {
        let home = Some("/Users/someone".into());
        // `HOME` is ignored: the home directory is the user database's.
        let launch = resolve_in(args(&[]), env(&[("HOME", "/elsewhere")]), home);
        let Ok(Launch::Telegram { cache, .. }) = launch else {
            panic!("expected the real backend");
        };
        assert_eq!(
            cache.unwrap().dir(),
            std::path::Path::new("/Users/someone/Library/Application Support/dev.abyss/cache")
        );
        let Ok(Launch::Telegram { cache, .. }) = resolve_in(args(&[]), env(&[]), None) else {
            panic!("expected the real backend");
        };
        assert!(cache.is_err(), "no home, no cache");
    }

    /// Task 9b: `--fake` never gets the real cache, and a cache only when
    /// asked for.
    #[cfg(feature = "fake")]
    #[test]
    fn the_fake_gets_a_cache_only_when_asked() {
        let home = || Some("/Users/someone".into());
        let vars = [("TMPDIR", "/var/folders/t/")];
        let Ok(Launch::Fake { cache, .. }) = resolve_in(args(&[FAKE_FLAG]), env(&vars), home())
        else {
            panic!("expected the fake");
        };
        assert_eq!(cache, None);
        let asked = [
            ("TMPDIR", "/var/folders/t/"),
            (crate::storage::FAKE_CACHE_VAR, "1"),
        ];
        let Ok(Launch::Fake { cache, .. }) = resolve_in(args(&[FAKE_FLAG]), env(&asked), home())
        else {
            panic!("expected the fake");
        };
        let dir = cache.unwrap().dir();
        // A fresh run directory each time: `.../dev.abyss.fake/run-*/cache`.
        assert!(dir.starts_with("/var/folders/t/dev.abyss.fake"), "{dir:?}");
        assert!(dir.ends_with("cache"));
        assert!(
            dir.parent()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n.to_string_lossy().starts_with("run-"))
        );
    }
}
