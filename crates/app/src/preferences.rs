//! Secret storage preference, independent of appearance resets. A malformed
//! file blocks real startup instead of silently downgrading Keychain storage.

use std::path::PathBuf;

use abyss_core::StartError;
use abyss_core::secret::{SecretStorageConfig, private_directory, read_private, write_private};
use gpui::{App, AppContext, BorrowAppContext, Global};

#[derive(Default)]
pub(crate) struct Preferences {
    pub(crate) use_keychain: Option<bool>,
    pub(crate) saving: bool,
    pub(crate) failed: bool,
    path: Option<PathBuf>,
}

impl Global for Preferences {}

impl Preferences {
    pub(crate) fn init(cx: &mut App) {
        let path = crate::storage::app_dir().map(|dir| dir.join("keychain-preference"));
        let loaded = path.as_deref().map_or_else(
            || Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            |path| {
                private_directory(path.parent().ok_or(std::io::ErrorKind::InvalidInput)?)?;
                read_private(path).and_then(|bytes| parse(bytes.as_deref().map(Vec::as_slice)))
            },
        );
        let valid = loaded.is_ok();
        cx.set_global(Self {
            use_keychain: loaded.ok(),
            failed: !valid,
            path,
            saving: false,
        });
    }

    pub(crate) fn config(cx: &App) -> Result<SecretStorageConfig, StartError> {
        let prefs = cx.try_global::<Self>().ok_or(StartError::SecretStorage)?;
        Ok(SecretStorageConfig {
            dir: crate::storage::app_dir()
                .ok_or(StartError::SecretStorage)?
                .join("private"),
            use_keychain: prefs.use_keychain.ok_or(StartError::SecretStorage)?,
        })
    }

    /// Save first, then display the new value. Failure leaves the previous
    /// choice visible and offers a retry through the same control.
    pub(crate) fn toggle(cx: &mut App) {
        let Some(prefs) = cx.try_global::<Self>() else {
            return;
        };
        if prefs.saving {
            return;
        }
        let Some(path) = prefs.path.clone() else {
            return;
        };
        let enabled = !prefs.use_keychain.unwrap_or(false);
        cx.update_global::<Self, _>(|prefs, _| prefs.saving = true);
        cx.spawn(async move |cx| {
            let result = cx
                .background_spawn(async move {
                    write_private(&path, if enabled { b"1\n" } else { b"0\n" })
                })
                .await;
            cx.update(|cx| {
                cx.update_global::<Self, _>(|prefs, _| {
                    prefs.saving = false;
                    prefs.failed = result.is_err();
                    if result.is_ok() {
                        prefs.use_keychain = Some(enabled);
                    }
                });
                cx.refresh_windows();
            });
        })
        .detach();
        cx.refresh_windows();
    }
}

fn parse(bytes: Option<&[u8]>) -> std::io::Result<bool> {
    match bytes {
        None | Some(b"0\n") => Ok(false),
        Some(b"1\n") => Ok(true),
        Some(_) => Err(std::io::ErrorKind::InvalidData.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_is_opt_in_and_damaged_preferences_are_not_a_downgrade() {
        assert!(!parse(None).unwrap());
        assert!(!parse(Some(b"0\n")).unwrap());
        assert!(parse(Some(b"1\n")).unwrap());
        for bytes in [b"".as_slice(), b"true", b"1\nextra"] {
            assert!(parse(Some(bytes)).is_err());
        }
    }

    #[gpui::test]
    fn toggling_persists_and_appearance_reset_keeps_the_choice(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keychain-preference");
        cx.update(|cx| {
            cx.set_global(Preferences {
                use_keychain: Some(false),
                path: Some(path.clone()),
                ..Preferences::default()
            });
            Preferences::toggle(cx);
        });
        cx.run_until_parked();
        assert_eq!(std::fs::read(&path).unwrap(), b"1\n");
        cx.update(|cx| {
            assert_eq!(cx.global::<Preferences>().use_keychain, Some(true));
            crate::ui::theme::Themes::set(crate::ui::theme::Settings::default(), cx);
            assert_eq!(cx.global::<Preferences>().use_keychain, Some(true));
            Preferences::toggle(cx);
        });
        cx.run_until_parked();
        assert_eq!(std::fs::read(path).unwrap(), b"0\n");
    }

    #[gpui::test]
    fn a_failed_save_keeps_the_previous_choice(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Preferences {
                use_keychain: Some(false),
                path: Some(dir.path().join("missing/keychain-preference")),
                ..Preferences::default()
            });
            Preferences::toggle(cx);
        });
        cx.run_until_parked();
        cx.update(|cx| {
            let prefs = cx.global::<Preferences>();
            assert_eq!(prefs.use_keychain, Some(false));
            assert!(prefs.failed);
            assert!(!prefs.saving);
        });
    }
}
