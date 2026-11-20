//! Onboarding and login actions of the session (plan section 17.3): the
//! UI-only welcome and guide steps, credential entry, Back, resend, the Done
//! screen and Logout (with its confirmation and Retry). Core decides
//! everything else. A command's UI change is made only once core admitted it.

use abyss_core::api::{ApiHash, ApiId, Command, CredentialItem};
use gpui::{Context, Window};

use super::Root;
use super::auth::{AuthView, Back, Onboarding, Stored, back};
use super::session::Session;
use crate::ui::copy::onboarding as copy;
use crate::ui::password::{PasswordInput, Reveal};

/// What Retry does for the credential item: save the pair again, or delete
/// it again. Nothing while it is settled or in progress.
pub(crate) fn credential_retry(item: CredentialItem) -> Option<Command> {
    match item {
        CredentialItem::SaveFailed(_) => Some(Command::RetryCredentialSave),
        CredentialItem::DeleteFailed(_) => Some(Command::ForgetStoredCredentials),
        CredentialItem::Settled | CredentialItem::Saving | CredentialItem::Deleting => None,
    }
}

/// What a credential entry holds, checked like core checks it.
pub(crate) fn check_entry(api_id: &str, api_hash: &str) -> Result<(ApiId, ApiHash), &'static str> {
    let id = ApiId::parse(api_id).map_err(copy::api_id_invalid)?;
    let hash = ApiHash::parse(api_hash).map_err(|_| copy::HASH_INVALID)?;
    Ok((id, hash))
}

impl Session {
    fn show_step(&mut self, step: Onboarding, window: &mut Window, cx: &mut Context<'_, Root>) {
        self.entry_error = None;
        self.set_auth(AuthView::Onboarding(step), window, cx);
    }

    /// Welcome → the my.telegram.org guide.
    pub(super) fn connect(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if self.auth == AuthView::Onboarding(Onboarding::Welcome) {
            self.show_step(Onboarding::Guide, window, cx);
        }
    }

    /// Guide → credential entry.
    pub(super) fn have_credentials(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if self.auth == AuthView::Onboarding(Onboarding::Guide) {
            let step = Onboarding::Entry {
                error: None,
                stored: Stored::No,
            };
            self.show_step(step, window, cx);
        }
    }

    /// Opens the hardcoded my.telegram.org page in the system browser: the
    /// only URL onboarding opens directly.
    pub(super) fn open_site(cx: &mut Context<'_, Root>) {
        cx.open_url(copy::MY_TELEGRAM_URL);
    }

    pub(super) fn toggle_hash_mask(&mut self, cx: &mut Context<'_, Root>) {
        self.api_hash.update(cx, |input, cx| {
            let next = match input.reveal() {
                Reveal::Hidden => Reveal::Shown,
                Reveal::Shown => Reveal::Hidden,
            };
            input.set_reveal(next, cx);
        });
        cx.notify();
    }

    /// Validates inline, then sends `SubmitCredentials`. The fields are
    /// wiped once core admitted it; refused, they keep their text.
    pub(super) fn submit_credentials(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if !matches!(self.auth, AuthView::Onboarding(Onboarding::Entry { .. })) {
            return;
        }
        let id = self.api_id.read(cx).text().to_owned();
        // A zeroizing copy; the field keeps its fixed buffer until admitted.
        let hash = self.api_hash.read(cx).secret();
        let checked = check_entry(&id, std::str::from_utf8(&hash).unwrap_or_default());
        drop(hash);
        match checked {
            Ok((api_id, api_hash)) => {
                self.entry_error = None;
                let api_id = api_id.get();
                let cmd = Command::SubmitCredentials { api_id, api_hash };
                if self.admit_auth(cmd, window, cx).is_ok() {
                    self.api_id.update(cx, |input, cx| input.clear(window, cx));
                    self.api_hash.update(cx, PasswordInput::clear);
                }
            }
            Err(problem) => self.entry_error = Some(problem.into()),
        }
        cx.notify();
    }

    /// The saved pair: use it (reload) or forget it.
    pub(super) fn use_saved(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        self.send(Command::ReloadCredentials, window, cx);
    }

    pub(super) fn forget_saved(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        self.send(Command::ForgetStoredCredentials, window, cx);
    }

    /// Retry on a failed save or delete of the credential item. The banner
    /// stays until core reports the item again.
    pub(super) fn retry_credential_item(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        if let Some(cmd) = credential_retry(self.cred_item) {
            self.send(cmd, window, cx);
        }
    }

    pub(super) fn back(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        match back(&self.auth, self.source) {
            Back::None => {}
            Back::Local(view) => {
                self.entry_error = None;
                self.set_auth(view, window, cx);
            }
            Back::Core => self.send(Command::Back, window, cx),
        }
    }

    pub(super) fn resend_code(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if matches!(self.auth, AuthView::Code { .. }) {
            self.send(Command::ResendCode, window, cx);
        }
    }

    /// Done → the chats. The Done screen already said whether the session
    /// was saved.
    pub(super) fn open_chats(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if matches!(self.auth, AuthView::Done { .. }) {
            self.set_auth(AuthView::SignedIn, window, cx);
        }
    }

    /// Log out deletes local data, so it asks first.
    pub(super) fn logout(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if matches!(self.auth, AuthView::SignedIn | AuthView::Done { .. }) {
            self.close_menu(window, cx);
            self.logout_focus.open(window, cx);
            self.open_logout_prompt();
            cx.notify();
        }
    }

    pub(super) fn cancel_logout(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        self.close_logout_prompt(window, cx);
    }

    /// Confirmed: the prompt closes once core admitted the logout; refused,
    /// it stays up for another try.
    pub(super) fn confirm_logout(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if !self.confirming_logout() {
            // A prompt that outlived its screen closes the normal way,
            // releasing its focus (review N55); none open: nothing to do.
            self.close_logout_prompt(window, cx);
            return cx.notify();
        }
        if self.admit_auth(Command::Logout, window, cx).is_ok() {
            self.cancel_logout(window, cx);
        }
        cx.notify();
    }

    /// "Logout incomplete": Retry runs the local deletion again. "Couldn't
    /// start logout": Retry writes the marker again; the screen stays until
    /// core says the logout started (or failed to again).
    pub(super) fn retry_logout(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        match self.auth {
            AuthView::LogoutIncomplete { .. } => {
                if self.admit_auth(Command::Logout, window, cx).is_ok() {
                    self.set_auth(AuthView::LoggingOut, window, cx);
                }
            }
            AuthView::LogoutNotStarted { .. } => {
                let _ = self.admit_auth(Command::Logout, window, cx);
            }
            _ => {}
        }
    }

    /// "Couldn't read Keychain" at startup: core reads the marker again.
    pub(super) fn retry_startup(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if self.auth == AuthView::StartupBlocked {
            let _ = self.admit_auth(Command::RetryStartup, window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_is_checked_like_core() {
        let hash = "0123456789ABCDEF0123456789abcdef";
        let (id, h) = check_entry(" 42 ", hash).unwrap();
        assert_eq!(id.get(), 42);
        assert_eq!(h.expose(), hash.to_lowercase());
        assert_eq!(
            check_entry("", hash).unwrap_err(),
            copy::api_id_invalid(abyss_core::api::ApiIdError::Empty)
        );
        assert_eq!(
            check_entry("99999999999", hash).unwrap_err(),
            copy::api_id_invalid(abyss_core::api::ApiIdError::TooLong)
        );
        assert_eq!(check_entry("42", "abc").unwrap_err(), copy::HASH_INVALID);
    }
}
