//! Onboarding copy (plan section 17.3): the welcome, the my.telegram.org
//! guide, credential entry, and the login screens' extra text.
//! `docs/connecting.md` mirrors this text; [`DOC_PHRASES`] are checked
//! against it by a unit test, so the two cannot drift apart.

use abyss_core::api::{
    ApiIdError, CodeDelivery, CredentialError, CredentialItem, CredentialSource, EnvVar,
    NextDelivery,
};
use gpui::SharedString;

/// The only URL onboarding opens directly (plan section 17.3).
pub(crate) const MY_TELEGRAM_URL: &str = "https://my.telegram.org/apps";

pub(crate) const BACK: &str = "Back";
pub(crate) const RETRY: &str = "Retry";

pub(crate) const WELCOME_TITLE: &str = "Welcome to abyss";
pub(crate) const WELCOME_LINES: [&str; 4] = [
    "abyss is a small, auditable Telegram client for macOS.",
    "It reduces the client-side attack surface: less code that handles what \
     strangers send you.",
    "Cloud chats are still readable by Telegram's servers.",
    "There are no secret chats, calls, stickers, browser or Mini Apps in this version.",
];
pub(crate) const CONNECT: &str = "Connect to Telegram";

pub(crate) const GUIDE_TITLE: &str = "Get your API credentials";
pub(crate) const GUIDE_INTRO: &str = "abyss connects with your own Telegram API credentials. \
                                      Getting them takes a few minutes on my.telegram.org.";
pub(crate) const OPEN_SITE: &str = "Open my.telegram.org";
/// The numbered steps; step 0 is the prerequisite.
pub(crate) const GUIDE_STEPS: [&str; 5] = [
    "0. You need an existing Telegram account signed in on an official app (phone or \
     desktop), and that session must stay available: login codes for my.telegram.org \
     and for abyss arrive there.",
    "1. Log in with your phone number. The login code arrives in your Telegram app, not \
     by SMS.",
    "2. Open \"API development tools\".",
    "3. Fill App title \"abyss\", a Short name of 5–32 letters or digits, and Platform \
     \"Desktop\". Leave URL empty.",
    "4. Copy api_id and api_hash.",
];
pub(crate) const TROUBLE_TITLE: &str = "If something goes wrong";
pub(crate) const TROUBLE: [&str; 4] = [
    "If the form says \"ERROR\", retry later with VPN and ad blockers off and a \
     letters-only short name.",
    "There is one app per phone number. If you already registered one, copy its \
     existing api_id and api_hash; don't try to create a second.",
    "Never share your api_hash.",
    "New api_ids can draw extra anti-spam scrutiny from Telegram.",
];
pub(crate) const HAVE_CREDENTIALS: &str = "I have my api_id and api_hash";

pub(crate) const ENTRY_TITLE: &str = "Enter your API credentials";
pub(crate) const ENTRY_DETAIL: &str = "Paste them from my.telegram.org. They are saved on this Mac \
                                       once Telegram has accepted them.";
pub(crate) const API_ID_PLACEHOLDER: &str = "api_id (digits)";
pub(crate) const API_HASH_PLACEHOLDER: &str = "api_hash (32 characters)";
pub(crate) const REVEAL: &str = "Show";
pub(crate) const HIDE: &str = "Hide";
pub(crate) const STORED_NOTE: &str = "A pair is saved on this Mac. Use it, enter a new \
                                      pair to replace it, or forget it.";
pub(crate) const USE_SAVED: &str = "Use saved pair";
pub(crate) const FORGET_SAVED: &str = "Forget saved pair";
pub(crate) const HASH_INVALID: &str = "The api_hash is 32 characters: digits and the letters a–f.";

pub(crate) fn api_id_invalid(error: ApiIdError) -> &'static str {
    match error {
        ApiIdError::Empty => "Enter the api_id.",
        ApiIdError::NotDigits => "The api_id is a number.",
        ApiIdError::TooLong | ApiIdError::OutOfRange => {
            "That api_id is too large. Copy it again from my.telegram.org."
        }
    }
}

pub(crate) const ENV_TITLE: &str = "Credentials from the environment";
pub(crate) const KEYCHAIN_TITLE: &str = "Local storage unavailable";
pub(crate) const KEYCHAIN_DETAIL: &str = "abyss couldn't read your saved API credentials. \
                                          Check local storage access, then retry.";

/// Why the env override is unusable, and how to fix it.
pub(crate) fn env_detail(error: CredentialError) -> Vec<SharedString> {
    let first: SharedString = match error {
        CredentialError::EnvPartial { missing } => {
            format!("{} is not set; both variables are needed.", missing.name()).into()
        }
        CredentialError::EnvMalformed { var: EnvVar::ApiId } => {
            "ABYSS_API_ID must be the positive number shown on my.telegram.org.".into()
        }
        CredentialError::EnvMalformed {
            var: EnvVar::ApiHash,
        } => "ABYSS_API_HASH must be the 32-character hash shown on my.telegram.org.".into(),
        other => credential_error(other, Some(CredentialSource::Env)),
    };
    vec![
        first,
        "ABYSS_API_ID and ABYSS_API_HASH override the saved credentials. Fix them, or \
         unset both and restart abyss to enter credentials here."
            .into(),
    ]
}

/// A credential error, with the source that produced it.
pub(crate) fn credential_error(
    error: CredentialError,
    source: Option<CredentialSource>,
) -> SharedString {
    match error {
        CredentialError::ApiIdInvalid if source == Some(CredentialSource::Env) => {
            "Telegram doesn't recognize the ABYSS_API_ID / ABYSS_API_HASH pair.".into()
        }
        CredentialError::ApiIdInvalid => "Telegram doesn't recognize this api_id and api_hash. \
                                          Copy both again from my.telegram.org."
            .into(),
        CredentialError::ApiIdPublishedFlood => "Telegram blocked this api_id (for example, \
                                                 because it was published). Create or use a \
                                                 different application."
            .into(),
        CredentialError::EnvPartial { missing } => format!("{} is not set.", missing.name()).into(),
        CredentialError::EnvMalformed { var } => format!("{} is not valid.", var.name()).into(),
        CredentialError::KeychainUnavailable => KEYCHAIN_DETAIL.into(),
        CredentialError::StoredUnreadable => "The saved credentials are unreadable. Enter them \
                                              again to replace them."
            .into(),
        CredentialError::InvalidApiId => "The api_id must be a positive number.".into(),
    }
}

pub(crate) const PHONE_DETAIL: &str = "Your phone number, in international format with a \
                                       leading +. The code is sent to your other Telegram \
                                       sessions first.";

/// The fixed warning on the code screen (plan section 17.3).
pub(crate) const CODE_WARNING: &str =
    "Telegram and abyss will never ask for this code anywhere else.";
pub(crate) const NEW_LOGIN_NOTICE: &str =
    "Your other devices will show a \"new login\" notice. That's expected.";
pub(crate) const RESEND: &str = "Send the code another way";

/// Where the code went. A flash call's number pattern and a missed call's
/// prefix and digit count come from core already sanitized and bounded.
pub(crate) fn code_detail(
    phone_hint: &str,
    delivery: Option<&CodeDelivery>,
    length: Option<u8>,
) -> SharedString {
    let place = match delivery {
        None | Some(CodeDelivery::App) => "in your Telegram app on another device".to_owned(),
        Some(CodeDelivery::Sms) => "by SMS".to_owned(),
        Some(CodeDelivery::Call) => "by a phone call that reads it out".to_owned(),
        Some(CodeDelivery::FlashCall { pattern }) if !pattern.is_empty() => {
            format!("as a call: enter the calling number (it matches {pattern})")
        }
        Some(CodeDelivery::FlashCall { .. }) => "as a call: enter the calling number".to_owned(),
        Some(CodeDelivery::MissedCall { prefix }) => missed_call(prefix, length),
        Some(CodeDelivery::Fragment { .. }) => "by SMS through fragment.com".to_owned(),
    };
    format!("Telegram sent a code for {phone_hint} {place}.").into()
}

fn missed_call(prefix: &str, length: Option<u8>) -> String {
    let digits = match length {
        Some(n) => format!("the last {n} digits"),
        None => "the last digits".to_owned(),
    };
    if prefix.is_empty() {
        format!("as a missed call: enter {digits} of the calling number")
    } else {
        format!("as a missed call from a number starting {prefix}: enter {digits} of that number")
    }
}

/// The stored credential item's save or delete, while it needs saying.
/// A failed delete never claims the pair is gone.
pub(crate) fn credential_item(item: CredentialItem) -> Option<SharedString> {
    let text = match item {
        CredentialItem::Settled | CredentialItem::Saving => return None,
        CredentialItem::SaveFailed(e) => format!(
            "Your API credentials weren't saved, so you'll need to enter them again next \
             time. {}",
            super::storage_error(e)
        ),
        CredentialItem::Deleting => "Removing the saved API credentials…".to_owned(),
        CredentialItem::DeleteFailed(e) => format!(
            "The saved API credentials couldn't be removed and are still stored. {}",
            super::storage_error(e)
        ),
    };
    Some(text.into())
}

/// A Fragment number's address, shown as text: abyss never opens it.
pub(crate) fn official_app_url(url: &str) -> SharedString {
    format!("Fragment address (abyss does not open it): {url}").into()
}

pub(crate) fn resend_detail(next: NextDelivery, seconds_left: Option<u64>) -> SharedString {
    let how = match next {
        NextDelivery::Sms => "by SMS",
        NextDelivery::Call => "by a phone call",
        NextDelivery::FlashCall => "as a flash call",
        NextDelivery::MissedCall => "as a missed call",
        NextDelivery::FragmentSms => "by SMS through fragment.com",
    };
    match seconds_left {
        Some(s) => format!("You can ask for the code {how} in {s} s.").into(),
        None => format!("You can ask for the code {how}.").into(),
    }
}

pub(crate) const VERIFYING_TITLE: &str = "Checking your sign-in";
pub(crate) const VERIFYING_DETAIL: &str = "The last answer from Telegram was lost. abyss is \
                                           asking whether the sign-in went through before it \
                                           accepts another attempt.";

pub(crate) const SIGN_UP_TITLE: &str = "No Telegram account";
pub(crate) const SIGN_UP_DETAIL: &str = "This number has no Telegram account. Create one in \
                                         an official Telegram app, then sign in here.";
pub(crate) const OFFICIAL_APP_TITLE: &str = "Finish in an official app";
pub(crate) const OFFICIAL_APP_DETAIL: &str =
    "Finish this step in an official Telegram app, then try again.";

pub(crate) const LOGGING_OUT_TITLE: &str = "Signing out…";
pub(crate) const LOGGING_OUT_DETAIL: &str = "Ending the session with Telegram, then deleting \
                                             this account's data from this Mac.";
pub(crate) const LOGOUT: &str = "Log out";

pub(crate) const DONE_TITLE: &str = "You're signed in";
pub(crate) const DONE_TIPS: [&str; 1] = [
    "The session shows as \"abyss\" under Settings › Devices in official apps, where you \
     can end it remotely.",
];
pub(crate) const RECOMMEND_2FA: &str = "Your account has no two-step verification password. \
                                        Turn it on in an official Telegram app (Settings › \
                                        Privacy and Security); abyss doesn't set it.";
pub(crate) const UNSAVED_TITLE: &str = "Signed in, but not saved";
pub(crate) const OPEN_CHATS: &str = "Open chats";

/// Each 17.3 step's key phrase. Each appears in the copy above and in
/// `docs/connecting.md`.
#[cfg(test)]
pub(crate) const DOC_PHRASES: [&str; 12] = [
    "small, auditable Telegram client",
    "Cloud chats are still readable by Telegram's servers",
    "https://my.telegram.org/apps",
    "existing Telegram account signed in on an official app",
    "arrives in your Telegram app, not by SMS",
    "API development tools",
    "Leave URL empty",
    "one app per phone number",
    "Never share your api_hash",
    "Telegram and abyss will never ask for this code anywhere else.",
    "Settings › Devices",
    "Finish this step in an official Telegram app, then try again.",
];

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/connecting.md"
    ));

    fn all_copy() -> String {
        let mut parts: Vec<&str> = Vec::new();
        parts.extend(WELCOME_LINES);
        parts.extend(GUIDE_STEPS);
        parts.extend(TROUBLE);
        parts.extend(DONE_TIPS);
        parts.extend([
            MY_TELEGRAM_URL,
            CODE_WARNING,
            OFFICIAL_APP_DETAIL,
            PHONE_DETAIL,
        ]);
        parts.join("\n")
    }

    #[test]
    fn every_key_phrase_is_in_the_copy_and_the_doc() {
        let copy = all_copy();
        // The doc wraps lines; compare with whitespace collapsed.
        let doc = DOC.split_whitespace().collect::<Vec<_>>().join(" ");
        for phrase in DOC_PHRASES {
            assert!(copy.contains(phrase), "copy lacks {phrase:?}");
            assert!(doc.contains(phrase), "docs/connecting.md lacks {phrase:?}");
        }
    }

    #[test]
    fn steps_are_numbered_from_the_prerequisite() {
        for (i, step) in GUIDE_STEPS.iter().enumerate() {
            assert!(step.starts_with(&format!("{i}. ")), "{step}");
        }
    }
}
