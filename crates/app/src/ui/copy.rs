//! User-facing strings for the startup, auth and shutdown screens, in one
//! place for review and later localization. Nothing here may include user
//! data beyond what the caller passes in for display (a masked phone hint, a
//! server-provided password hint).

use abyss_core::StartError;
use abyss_core::api::{LocalDeletion, RemoteLogout, StorageError, UserError};
use gpui::SharedString;

use crate::launch::LaunchError;

pub(crate) mod onboarding;

pub(crate) use onboarding::PHONE_DETAIL;

/// The tagline, shown with the wordmark on the sign-in screens.
pub(crate) const TAGLINE: &str = "Look in. Nothing looks back.";
/// What abyss is, in the form Telegram's API terms require: "Telegram" in a
/// description only after "unofficial", and a plain statement that the app
/// uses the Telegram API, shown where the user first meets it.
pub(crate) const DESCRIPTOR: &str =
    "abyss is an unofficial Telegram client. It uses the Telegram API.";

pub(crate) const APPEARANCE: &str = "Appearance";
pub(crate) const APPEARANCE_MENU: &str = "Settings…";
pub(crate) const SETTINGS: &str = "Settings";
pub(crate) const KEYCHAIN_STORAGE: &str = "Store secrets in Keychain";
pub(crate) const KEYCHAIN_STORAGE_DETAIL: &str = "Off by default. When off, session secrets stay unencrypted in a private local file accessible to your macOS account. Changes apply next launch.";
pub(crate) const KEYCHAIN_MIGRATION_NOTE: &str = "Moving existing secrets out of Keychain may ask for access, even when this is off. Once the move finishes, launches with Keychain off do not access it. With Keychain on, macOS controls access prompts.";
pub(crate) const PREFERENCE_FAILED: &str =
    "Couldn't read or save this setting. Click to retry, then restart abyss.";
pub(crate) const APPEARANCE_NOTE: &str =
    "Pick a hue and how much of it. Lightness is fixed, so text stays readable in any hue.";
pub(crate) const NIGHT: &str = "Night";
pub(crate) const DAY: &str = "Day";
pub(crate) const IN_USE: &str = "in use now";
pub(crate) const HUE: &str = "Hue";
pub(crate) const TINT: &str = "Tint";
pub(crate) const RESET: &str = "Reset";
pub(crate) const DONE: &str = "Done";

pub(crate) const CONNECTING: &str = "Connecting…";
pub(crate) const CONTINUE: &str = "Continue";

pub(crate) const PHONE_TITLE: &str = "Sign in to Telegram";
pub(crate) const PHONE_PLACEHOLDER: &str = "Phone number, e.g. +44 20 7946 0000";

pub(crate) const CODE_TITLE: &str = "Enter the code";
pub(crate) const CODE_PLACEHOLDER: &str = "Code";

pub(crate) const PASSWORD_TITLE: &str = "Two-step verification";
pub(crate) const PASSWORD_PLACEHOLDER: &str = "Password";
/// `--fake`: the demo account's login code, shown when the code screen opens.
#[cfg(feature = "fake")]
pub(crate) const FAKE_CODE_HINT: &str = "Demo account: the code is 12345.";
/// The password token was used up; Enter fetches new sign-in parameters.
pub(crate) const PASSWORD_PARAMS: &str =
    "Press Enter to get new sign-in parameters, then enter your password again.";
pub(crate) const SIGNED_IN_UNSAVED: &str =
    "Signed in, but the session couldn't be saved. You'll need to sign in again next time.";

/// The result of a logout, as a notice.
pub(crate) fn logged_out(remote: RemoteLogout, local: LocalDeletion) -> SharedString {
    let remote = match remote {
        RemoteLogout::Revoked | RemoteLogout::NotAttempted => "",
        RemoteLogout::Failed(_) | RemoteLogout::TimedOut => {
            " Telegram didn't confirm it: end the session from Active Sessions on another device."
        }
    };
    match local {
        LocalDeletion::Deleted => format!("Signed out.{remote}").into(),
        LocalDeletion::Failed(_) => format!("{LOGOUT_INCOMPLETE_TITLE}.{remote}").into(),
    }
}

/// A logout whose local deletion failed (plan section 5).
pub(crate) const LOGOUT_INCOMPLETE_TITLE: &str = "Logout incomplete";
pub(crate) const LOGOUT_INCOMPLETE_DETAIL: &str = "Some of this account's data couldn't be \
     deleted from this Mac, and the session key may remain in local storage. Retry to finish \
     before signing in again.";
/// The remote half of an incomplete logout failed too.
pub(crate) const LOGOUT_REMOTE_UNCONFIRMED: &str = "Telegram didn't confirm the sign-out: end \
     the session from Active Sessions on another device.";

/// The logout-pending marker could not be written, so nothing was done
/// (review round 19).
pub(crate) const LOGOUT_NOT_STARTED_TITLE: &str = "Couldn't start logout";
pub(crate) const LOGOUT_NOT_STARTED_USER: &str = "abyss couldn't record the logout in local storage, so nothing was changed: you're still signed in on this Mac. Check local storage access and retry.";
pub(crate) const LOGOUT_NOT_STARTED_SERVER: &str = "Telegram ended this session, but abyss \
     couldn't record the logout in local storage, so nothing was deleted from this Mac yet. \
     Check local storage access and retry.";
pub(crate) const STAY_SIGNED_IN: &str = "Stay signed in";

/// Startup couldn't read the logout-pending marker (review round 19). This
/// is not a logout: nothing is deleted on it.
pub(crate) const STARTUP_BLOCKED_TITLE: &str = "Couldn't read local storage";
pub(crate) const STARTUP_BLOCKED_DETAIL: &str = "abyss couldn't read its saved secrets, so it \
     can't tell whether a logout is unfinished. Nothing was loaded or deleted. Check local storage access and retry.";

/// Logout asks first: it deletes local data (plan section 17.3, task 6c).
pub(crate) const LOGOUT_CONFIRM_TITLE: &str = "Log out?";
pub(crate) const LOGOUT_CONFIRM_DETAIL: &str = "This ends the session with Telegram and deletes \
     this account's session and cached messages from this Mac.";
pub(crate) const CANCEL: &str = "Cancel";

pub(crate) fn password_detail(hint: Option<&str>) -> SharedString {
    match hint {
        Some(h) => format!("Enter your cloud password (hint: {h}).").into(),
        None => "Enter your cloud password.".into(),
    }
}

pub(crate) const FLOOD_TITLE: &str = "Too many attempts";

pub(crate) fn flood_detail(seconds_left: u64) -> SharedString {
    format!("Telegram asks us to wait. Try again in {seconds_left} s.").into()
}

pub(crate) const COMPOSER_PLACEHOLDER: &str = "Write a message…";
pub(crate) const CORE_STOPPED: &str = "abyss's core stopped unexpectedly.";
pub(crate) const CORE_GONE: &str = "abyss's core has stopped.";

pub(crate) const CLOSING: &str = "Closing…";
pub(crate) const UNCONFIRMED_TITLE: &str = "Couldn't confirm the session was saved";
pub(crate) const UNCONFIRMED_DETAIL: &str = "You may need to sign in again next time.";
pub(crate) const LOGOUT_UNFINISHED_TITLE: &str = LOGOUT_INCOMPLETE_TITLE;
pub(crate) const LOGOUT_UNFINISHED_DETAIL: &str = "Some of this account's data may still be on \
     this Mac. Open abyss again and sign out to finish deleting it.";
pub(crate) const QUIT: &str = "Quit";

/// A sentence for a core error.
pub(crate) fn user_error(error: UserError) -> SharedString {
    match error {
        UserError::Busy => "Busy right now. Try again in a moment.".into(),
        UserError::Network => "Network problem. Reconnecting…".into(),
        UserError::FloodWait { seconds } => {
            format!("Too many attempts. Try again in {seconds} s.").into()
        }
        UserError::Rejected => "That wasn't accepted. Check it and try again.".into(),
        UserError::NotFound => "Not found.".into(),
        UserError::ShuttingDown => "abyss is closing.".into(),
        UserError::Unsupported => "Not available in this build yet.".into(),
        UserError::Internal => "Something went wrong inside abyss.".into(),
        UserError::FolderChanged => "That folder changed in the meantime. Try again.".into(),
        UserError::FolderLimit => "That would make the folder too big for Telegram.".into(),
        UserError::FolderWritesCancelled => "Pending folder changes were cancelled.".into(),
        UserError::TopicClosed => "This topic is closed. Only its admins can post.".into(),
        UserError::TopicDeleted => "This topic was deleted.".into(),
        UserError::AccountMigrated => {
            "Telegram moved this account to another data center. Sign in again.".into()
        }
        UserError::PhoneInvalid => "That phone number isn't valid. Use international format, \
                                    e.g. +44 20 7946 0000."
            .into(),
        UserError::PhoneBanned => "Telegram has banned this phone number.".into(),
        UserError::InvalidCode => {
            "That code is wrong or expired. Check it, or ask for a new one.".into()
        }
        UserError::InvalidPassword => "That password is wrong.".into(),
        UserError::SignUpRequired => "This number has no Telegram account. Create one in an \
                                      official Telegram app first."
            .into(),
        UserError::SessionRevoked => {
            "This session was ended (signed out elsewhere). Sign in again.".into()
        }
        UserError::Credentials(e) => onboarding::credential_error(e, None),
        UserError::PhoneFlood => {
            "Too many sign-in attempts for this number. Try again later.".into()
        }
        UserError::CredentialsNotSaved(e) => format!(
            "Your API credentials couldn't be saved to local storage ({}). abyss retries \
             after the next sign-in step.",
            storage_error(e)
        )
        .into(),
        UserError::CredentialsNotForgotten(e) => format!(
            "The saved API credentials couldn't be deleted ({}).",
            storage_error(e)
        )
        .into(),
        UserError::SignOutFirst => "Sign out first to change the API credentials.".into(),
        UserError::Storage(e) => storage_error(e),
    }
}

pub(crate) fn storage_error(error: StorageError) -> SharedString {
    match error {
        StorageError::Local => "The private local storage file could not be accessed.".into(),
        StorageError::Keychain => "The Keychain refused the request.".into(),
        StorageError::Corrupt => "Saved data could not be read.".into(),
        StorageError::Busy => "Too many saves at once; one was dropped.".into(),
        StorageError::Stopped => "Local storage stopped.".into(),
        StorageError::Random => "The system random number generator failed.".into(),
        StorageError::Cache => "The message cache couldn't be closed; nothing was deleted.".into(),
        StorageError::CacheFiles => {
            "The message cache is unreadable, but its files couldn't be deleted.".into()
        }
        StorageError::Internal => "Something went wrong with local storage.".into(),
    }
}

/// Title and paragraphs for a launch problem.
pub(crate) fn launch_error(error: &LaunchError) -> (&'static str, Vec<SharedString>) {
    match error {
        LaunchError::FakeNotBuilt => (
            "This build has no demo backend",
            vec!["--fake needs a build with the `fake` feature. Use `cargo dev`.".into()],
        ),
    }
}

/// Title and paragraphs for a core that failed to start.
pub(crate) fn start_error(error: &StartError) -> (&'static str, Vec<SharedString>) {
    ("abyss couldn't start", vec![error.to_string().into()])
}
