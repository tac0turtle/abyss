//! Login, logout and credential types shared by the UI and core (plan
//! sections 4, 5 and 17.2). Re-exported from [`crate::api`].

use std::time::SystemTime;

use zeroize::Zeroize;

use crate::api::UserError;
use crate::cache::EraseFailure;
use crate::secret::{CacheKeyError, StoreError};
use crate::session::PersistError;

/// Capacity of [`PasswordBuf`] in bytes. Matches the app's password field.
pub const PASSWORD_CAPACITY: usize = 256;

/// A 2FA password in a fixed-capacity buffer that never reallocates and is
/// wiped (all [`PASSWORD_CAPACITY`] bytes) on drop (plan section 10).
pub struct PasswordBuf {
    bytes: Box<[u8; PASSWORD_CAPACITY]>,
    len: usize,
}

/// The password does not fit [`PASSWORD_CAPACITY`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PasswordTooLong;

impl PasswordBuf {
    /// An empty buffer. The one allocation happens here.
    pub fn new() -> Self {
        Self {
            bytes: Box::new([0; PASSWORD_CAPACITY]),
            len: 0,
        }
    }

    /// Copies `secret` in. The caller wipes its own copy.
    pub fn from_slice(secret: &[u8]) -> Result<Self, PasswordTooLong> {
        let mut buf = Self::new();
        let dst = buf.bytes.get_mut(..secret.len()).ok_or(PasswordTooLong)?;
        dst.copy_from_slice(secret);
        buf.len = secret.len();
        Ok(buf)
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for PasswordBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PasswordBuf {
    fn drop(&mut self) {
        self.bytes.zeroize();
        self.len = 0;
    }
}

impl std::fmt::Debug for PasswordBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PasswordBuf(<redacted>)")
    }
}

/// A validated `api_hash`: exactly 32 hex digits, lowercase. Never logged;
/// `Debug` redacts it (plan section 17.2).
#[derive(Clone, PartialEq, Eq)]
pub struct ApiHash(String);

/// Why a string is not an `api_hash`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiHashError {
    /// Not exactly 32 characters after trimming.
    Length,
    /// A character other than `0-9a-fA-F`.
    NotHex,
}

impl ApiHash {
    /// Length of an `api_hash` in hex digits.
    pub const LEN: usize = 32;

    /// Trims surrounding whitespace (a paste), checks 32 hex digits, and
    /// normalizes to lowercase.
    pub fn parse(input: &str) -> Result<Self, ApiHashError> {
        let trimmed = input.trim();
        if trimmed.len() != Self::LEN {
            return Err(ApiHashError::Length);
        }
        if !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ApiHashError::NotHex);
        }
        Ok(Self(trimmed.to_ascii_lowercase()))
    }

    /// The hash, for the backend's `request_login_code`. Never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ApiHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiHash(<redacted>)")
    }
}

impl Drop for ApiHash {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Why API credentials are missing or unusable (plan sections 17.2, 17.2a).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialError {
    /// `API_ID_INVALID`: the id/hash pair is unknown.
    ApiIdInvalid,
    /// `API_ID_PUBLISHED_FLOOD`: the id is unusable (for example, it was
    /// published). Not a timed wait: use a different application.
    ApiIdPublishedFlood,
    /// Only one of the two environment variables is set. Never falls through
    /// to the Keychain.
    EnvPartial { missing: EnvVar },
    /// An environment variable is set but malformed.
    EnvMalformed { var: EnvVar },
    /// The Keychain could not be read (locked, denied). Not the same as "no
    /// pair stored": retry, never re-onboard.
    KeychainUnavailable,
    /// The stored pair is unreadable. Entering a new pair replaces it.
    StoredUnreadable,
    /// A submitted `api_id` is not a positive 32-bit number.
    InvalidApiId,
}

impl CredentialError {
    /// Whether the user may enter a new pair while this error is shown.
    pub fn allows_entry(self) -> bool {
        !matches!(
            self,
            Self::EnvPartial { .. } | Self::EnvMalformed { .. } | Self::KeychainUnavailable
        )
    }
}

/// The two credential environment variables (plan section 17.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvVar {
    ApiId,
    ApiHash,
}

impl EnvVar {
    pub const fn name(self) -> &'static str {
        match self {
            Self::ApiId => "ABYSS_API_ID",
            Self::ApiHash => "ABYSS_API_HASH",
        }
    }
}

/// Where the API credentials in use came from (plan section 17.2a).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialSource {
    /// `ABYSS_API_ID` + `ABYSS_API_HASH`: a dev override, never persisted.
    Env,
    /// The Keychain item `dev.abyss.apicreds`.
    Stored,
    /// Compiled in at build time.
    Embedded,
    /// Typed into the onboarding screen this run.
    Entered,
}

/// A validated `api_id`: a positive 32-bit number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApiId(i32);

/// Why a string is not an `api_id`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiIdError {
    Empty,
    /// A character other than `0-9`.
    NotDigits,
    /// More than [`ApiId::MAX_DIGITS`] digits.
    TooLong,
    /// Zero, or above `i32::MAX`.
    OutOfRange,
}

impl ApiId {
    /// The most digits an `api_id` has (`i32::MAX` has 10).
    pub const MAX_DIGITS: usize = 10;

    pub fn new(id: i32) -> Result<Self, ApiIdError> {
        if id > 0 {
            Ok(Self(id))
        } else {
            Err(ApiIdError::OutOfRange)
        }
    }

    /// Trims surrounding whitespace, then expects 1 to 10 ASCII digits.
    pub fn parse(input: &str) -> Result<Self, ApiIdError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(ApiIdError::Empty);
        }
        if !trimmed.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ApiIdError::NotDigits);
        }
        if trimmed.len() > Self::MAX_DIGITS {
            return Err(ApiIdError::TooLong);
        }
        let id = trimmed.parse::<i32>().map_err(|_| ApiIdError::OutOfRange)?;
        Self::new(id)
    }

    pub fn get(self) -> i32 {
        self.0
    }
}

/// A Telegram application's `api_id` and `api_hash`. `Debug` redacts both.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiCredentials {
    pub api_id: ApiId,
    pub api_hash: ApiHash,
}

impl ApiCredentials {
    /// A pair for the fake backend and tests: `api_id` (made positive) and a
    /// fixed hash.
    #[cfg(any(test, feature = "fake"))]
    pub fn fake(api_id: i32) -> Self {
        Self {
            api_id: ApiId(api_id.max(1)),
            api_hash: ApiHash("0123456789abcdef0123456789abcdef".to_owned()),
        }
    }
}

impl std::fmt::Debug for ApiCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiCredentials(<redacted>)")
    }
}

/// The longest login code any delivery takes: a flash call's code is the
/// whole calling number, and E.164 numbers have at most 15 digits.
pub const MAX_CODE_DIGITS: usize = 15;
/// Bound on server-supplied delivery text (a flash-call pattern, a
/// missed-call prefix, a Fragment URL), in bytes.
pub const MAX_DELIVERY_TEXT: usize = 256;

/// How Telegram delivered a login code (`auth.SentCodeType`, the supported
/// subset; plan section 17.2a). Server-supplied text is sanitized by the
/// backend: a pattern or prefix keeps digits, `+`, `*`, spaces and dashes
/// only, a URL must be `https://` and printable ASCII, each at most
/// [`MAX_DELIVERY_TEXT`] bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodeDelivery {
    /// A message in the user's other Telegram sessions.
    App,
    Sms,
    /// A voice call that reads the code.
    Call,
    /// A flash call: the code is the whole calling number, which matches
    /// `pattern` (`*` stands for a digit not given in advance).
    FlashCall {
        pattern: String,
    },
    /// A missed call from a number starting with `prefix`: the code is the
    /// last [`SentCodeInfo::length`] digits of that number.
    MissedCall {
        prefix: String,
    },
    /// An SMS to an anonymous number, read on fragment.com. Core never shows
    /// a code screen for it: it routes to [`AuthState::OfficialAppRequired`]
    /// with this `url`, which is shown as text and never opened by abyss.
    Fragment {
        url: Option<String>,
    },
}

/// How the next code would be delivered after a resend (`auth.CodeType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NextDelivery {
    Sms,
    Call,
    FlashCall,
    MissedCall,
    FragmentSms,
}

/// Where the login code went, for the code screen (plan section 17.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentCodeInfo {
    pub delivery: CodeDelivery,
    /// Digits in the code, when the server said. For a missed call: how many
    /// trailing digits of the calling number to enter.
    pub length: Option<u8>,
    /// What a resend would use, if the server offers one.
    pub next: Option<NextDelivery>,
    /// The server asks not to resend before this time.
    pub resend_at: Option<SystemTime>,
}

impl SentCodeInfo {
    /// The most digits the code entry takes for this delivery: a flash
    /// call's pattern length (the whole calling number), else the stated
    /// length, else [`MAX_CODE_DIGITS`]; never more than that. A pattern
    /// that is a lone `*` accepts a number of any length
    /// (<https://core.telegram.org/api/pattern>), so it gets the full
    /// allowance; `*` among digits stands for one digit.
    pub fn max_code_digits(&self) -> usize {
        let digits = match &self.delivery {
            CodeDelivery::FlashCall { pattern } if pattern.trim() == "*" => None,
            CodeDelivery::FlashCall { pattern } => {
                let n = pattern
                    .chars()
                    .filter(|c| c.is_ascii_digit() || *c == '*')
                    .count();
                (n > 0).then_some(n)
            }
            _ => self.length.map(usize::from).filter(|n| *n > 0),
        };
        digits.map_or(MAX_CODE_DIGITS, |n| n.min(MAX_CODE_DIGITS))
    }
}

/// The Keychain item of entered API credentials, as far as core knows
/// (plan section 17.2a): what the Retry controls act on. A pair is claimed
/// absent only once its deletion succeeded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CredentialItem {
    /// No save or delete is running or failed.
    #[default]
    Settled,
    /// The proven entered pair is being saved.
    Saving,
    /// Saving the proven entered pair failed; it stays in memory.
    /// `Command::RetryCredentialSave` tries again.
    SaveFailed(StorageError),
    /// The stored pair is being deleted; it still counts as stored.
    Deleting,
    /// Deleting the stored pair failed; it may still be in the Keychain.
    /// `Command::ForgetStoredCredentials` tries again.
    DeleteFailed(StorageError),
}

/// What the Done screen shows after a login in this run (plan section 17.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NewLogin {
    /// `Some(false)`: the account has no 2FA password; recommend one.
    /// `None`: unknown (a login confirmed by the probe).
    pub has_password: Option<bool>,
}

/// A device-local storage failure, without OS detail (that may be logged
/// where it occurs; it never carries secrets).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageError {
    /// The Keychain refused or failed the operation.
    Keychain,
    /// A stored item was unreadable.
    Corrupt,
    /// Too many writes queued; this one never reached the Keychain.
    Busy,
    /// The persistence or cache worker stopped.
    Stopped,
    /// The system random number generator failed.
    Random,
    /// The message cache could not be closed; nothing was deleted.
    Cache,
    /// The cache key is gone (the cache is unreadable), but its files remain.
    CacheFiles,
    /// A bug: a poisoned lock or a call from the wrong thread.
    Internal,
}

impl From<&StoreError> for StorageError {
    fn from(e: &StoreError) -> Self {
        match e {
            StoreError::Os { .. } | StoreError::Injected => Self::Keychain,
            StoreError::Malformed => Self::Corrupt,
            StoreError::Stopped => Self::Stopped,
        }
    }
}

impl From<&PersistError> for StorageError {
    fn from(e: &PersistError) -> Self {
        match e {
            PersistError::Store(e) => e.into(),
            PersistError::NotQueued => Self::Busy,
            PersistError::Terminated | PersistError::Stopped => Self::Stopped,
            PersistError::InAsyncContext | PersistError::Poisoned => Self::Internal,
        }
    }
}

impl From<&CacheKeyError> for StorageError {
    fn from(e: &CacheKeyError) -> Self {
        match e {
            CacheKeyError::Store(e) => e.into(),
            CacheKeyError::Malformed => Self::Corrupt,
            CacheKeyError::Random => Self::Random,
        }
    }
}

impl From<&EraseFailure> for StorageError {
    fn from(e: &EraseFailure) -> Self {
        match e {
            EraseFailure::Stopped => Self::Stopped,
            EraseFailure::Close(_) => Self::Cache,
            EraseFailure::DeleteKey(_) => Self::Keychain,
            EraseFailure::Files(_) => Self::CacheFiles,
        }
    }
}

/// `UserError::Storage` for each storage error type.
macro_rules! storage_user_error {
    ($($ty:ty),*) => {$(
        impl From<&$ty> for UserError {
            fn from(e: &$ty) -> Self {
                UserError::Storage(e.into())
            }
        }
    )*};
}

storage_user_error!(StoreError, PersistError, CacheKeyError, EraseFailure);

/// Outcome of the remote half of a logout (`auth.logOut`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteLogout {
    /// The server no longer accepts the session: `auth.logOut` succeeded, or
    /// the server had already revoked it (`AUTH_KEY_UNREGISTERED`,
    /// `SESSION_REVOKED`).
    Revoked,
    /// `auth.logOut` failed; end the session from Active Sessions on another
    /// device.
    Failed(UserError),
    /// `auth.logOut` did not answer within its deadline.
    TimedOut,
    /// There was no remote session to end (a retry of the local deletion
    /// while signed out).
    NotAttempted,
}

/// Outcome of the local half of a logout: the session item, then the cache
/// (crypto-erase).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalDeletion {
    Deleted,
    /// Something may remain on this device. The UI shows "Logout incomplete"
    /// with a retry, never "logged out".
    Failed(StorageError),
}

/// Whether shutdown observed the session saved (plan section 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveStatus {
    Confirmed,
    Unconfirmed,
}

/// A logout running or finished when shutdown began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogoutStatus {
    Done {
        remote: RemoteLogout,
        local: LocalDeletion,
    },
    /// Interrupted: the session key may remain in the Keychain.
    Incomplete,
}

/// Login state (plan sections 4, 17.2 and 17.2a).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthState {
    /// No usable API credentials. `error` says why; `source` says which
    /// source is in control (an env override disables entering a pair;
    /// `Stored` after Back means a pair is still saved).
    NeedCredentials {
        error: Option<CredentialError>,
        source: Option<CredentialSource>,
    },
    Connecting,
    Phone {
        error: Option<UserError>,
    },
    Code {
        /// Masked, e.g. `+44 •••• ••12`; see [`mask_phone`].
        phone_hint: String,
        /// Where the code went.
        sent: Option<SentCodeInfo>,
        error: Option<UserError>,
    },
    Password {
        hint: Option<String>,
        error: Option<UserError>,
    },
    /// The password token was consumed by a failed attempt. The user must
    /// `RetryPasswordParams` before submitting again.
    PasswordTokenUnavailable {
        error: UserError,
    },
    /// Telegram asked to wait. Core returns to `resume` at `until`.
    FloodWait {
        until: SystemTime,
        resume: Box<AuthState>,
    },
    /// A sign-in may have gone through but its answer was lost. Core asks
    /// the server (and retries by itself) before it accepts another attempt.
    /// `error` is why the last check failed.
    Verifying {
        error: Option<UserError>,
    },
    /// The number has no account; create it in an official app.
    SignUpRequired,
    /// Telegram asked for a step abyss does not implement (email setup,
    /// `auth.sentCodeSuccess`, a Fragment code, …): finish it in an official
    /// app, then retry. `url` is a server-supplied page for the step (the
    /// Fragment number's page), sanitized; shown as text, never opened.
    OfficialAppRequired {
        url: Option<String>,
    },
    /// `display_name` is empty until the server has answered once.
    /// `new_login` is set for a login completed in this run.
    SignedIn {
        display_name: String,
        new_login: Option<NewLogin>,
    },
    /// Signed in, but the session could not be saved: the user signs in again
    /// next launch.
    SignedInUnsaved {
        display_name: String,
        new_login: Option<NewLogin>,
    },
    LoggingOut,
    /// A logout was asked for and its marker is being written (the
    /// Keychain may prompt): nothing has changed yet and the account's data
    /// stays, but sends and retries are refused until the write resolves
    /// (review round 25, finding 1). Then `LoggingOut`, or
    /// `LogoutNotStarted`.
    LogoutStarting,
    LoggedOut {
        remote: RemoteLogout,
        local: LocalDeletion,
    },
    /// The logout-pending marker could not be written, so the logout did
    /// not start: nothing was ended or deleted, and the session is intact
    /// (review round 19). `Command::Logout` retries; for a logout the user
    /// asked for, `Command::Back` stays signed in.
    LogoutNotStarted {
        error: StorageError,
        trigger: LogoutTrigger,
    },
    /// Startup could not read the logout-pending marker, so it cannot tell
    /// whether a logout is pending: nothing is loaded or deleted until
    /// `Command::RetryStartup` reads it (review round 19).
    StartupBlocked {
        error: StorageError,
    },
}

/// Who started a logout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogoutTrigger {
    /// The user: they may stay signed in instead.
    User,
    /// The server ended the session, or a repeated migration abandoned it:
    /// the account cannot continue.
    Server,
}

impl AuthState {
    /// Whether the account is signed in (possibly unsaved).
    pub fn is_signed_in(&self) -> bool {
        matches!(self, Self::SignedIn { .. } | Self::SignedInUnsaved { .. })
    }
}

/// Masks a phone number for display: the first three characters and the last
/// two digits, e.g. `+44 •••• ••12`. Input of five characters or fewer is
/// masked entirely.
pub fn mask_phone(phone: &str) -> String {
    let chars: Vec<char> = phone.chars().collect();
    let n = chars.len();
    if n <= 5 {
        return "•".repeat(n);
    }
    let head: String = chars.iter().take(3).collect();
    let tail: String = chars.iter().skip(n - 2).collect();
    let hidden = n - 5;
    let first = hidden.min(4);
    let rest = hidden - first;
    format!("{head} {} {}{tail}", "•".repeat(first), "•".repeat(rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_buf_is_fixed_and_bounded() {
        let pw = PasswordBuf::from_slice(b"hunter2").unwrap();
        assert_eq!(pw.as_bytes(), b"hunter2");
        assert_eq!(pw.len(), 7);
        assert_eq!(format!("{pw:?}"), "PasswordBuf(<redacted>)");
        let full = [b'x'; PASSWORD_CAPACITY];
        assert_eq!(PasswordBuf::from_slice(&full).unwrap().len(), 256);
        let over = [b'x'; PASSWORD_CAPACITY + 1];
        assert_eq!(PasswordBuf::from_slice(&over).unwrap_err(), PasswordTooLong);
        assert!(PasswordBuf::new().is_empty());
    }

    #[test]
    fn api_hash_validation() {
        let ok = format!("  {}\n", "AbCdEf0123456789".repeat(2));
        let hash = ApiHash::parse(&ok).unwrap();
        assert_eq!(hash.expose(), "abcdef0123456789".repeat(2));
        assert_eq!(format!("{hash:?}"), "ApiHash(<redacted>)");
        assert_eq!(ApiHash::parse(&"a".repeat(31)), Err(ApiHashError::Length));
        assert_eq!(ApiHash::parse(&"a".repeat(33)), Err(ApiHashError::Length));
        assert_eq!(ApiHash::parse(""), Err(ApiHashError::Length));
        let bad = format!("{}g", "a".repeat(31));
        assert_eq!(ApiHash::parse(&bad), Err(ApiHashError::NotHex));
        let inner = format!("{} {}", "a".repeat(15), "a".repeat(16));
        assert_eq!(ApiHash::parse(&inner), Err(ApiHashError::NotHex));
        assert_eq!(ApiHash::parse(&"é".repeat(16)), Err(ApiHashError::NotHex));
    }

    #[test]
    fn api_id_validation() {
        assert_eq!(ApiId::parse(" 42 ").unwrap().get(), 42);
        assert_eq!(ApiId::parse("2147483647").unwrap().get(), i32::MAX);
        assert_eq!(ApiId::parse("2147483648"), Err(ApiIdError::OutOfRange));
        assert_eq!(ApiId::parse("0"), Err(ApiIdError::OutOfRange));
        assert_eq!(ApiId::parse("00000000001"), Err(ApiIdError::TooLong));
        assert_eq!(ApiId::parse("-1"), Err(ApiIdError::NotDigits));
        assert_eq!(ApiId::parse("1 2"), Err(ApiIdError::NotDigits));
        assert_eq!(ApiId::parse("١٢"), Err(ApiIdError::NotDigits));
        assert_eq!(ApiId::parse("  "), Err(ApiIdError::Empty));
        assert_eq!(ApiId::new(-5), Err(ApiIdError::OutOfRange));
    }

    #[test]
    fn credentials_debug_redacts_both_halves() {
        let creds = ApiCredentials {
            api_id: ApiId::new(123_456).unwrap(),
            api_hash: ApiHash::parse(&"ab".repeat(16)).unwrap(),
        };
        let shown = format!("{creds:?}");
        assert!(!shown.contains("123456") && !shown.contains("abab"));
    }

    #[test]
    fn env_and_keychain_errors_block_entry() {
        let partial = CredentialError::EnvPartial {
            missing: EnvVar::ApiHash,
        };
        assert!(!partial.allows_entry());
        assert!(!CredentialError::KeychainUnavailable.allows_entry());
        assert!(CredentialError::ApiIdInvalid.allows_entry());
        assert!(CredentialError::StoredUnreadable.allows_entry());
        assert_eq!(EnvVar::ApiHash.name(), "ABYSS_API_HASH");
    }

    #[test]
    fn masks_phone_numbers() {
        assert_eq!(mask_phone("+442079460012"), "+44 •••• ••••12");
        assert_eq!(mask_phone("+15550100"), "+15 •••• 00");
        assert_eq!(mask_phone("+155501"), "+15 •• 01");
        assert_eq!(mask_phone("12345"), "•••••");
        assert_eq!(mask_phone(""), "");
    }

    #[test]
    fn storage_errors_map_without_detail() {
        let os = StoreError::Os {
            code: -25300,
            message: "x".into(),
        };
        assert_eq!(
            UserError::from(&os),
            UserError::Storage(StorageError::Keychain)
        );
        assert_eq!(
            UserError::from(&PersistError::NotQueued),
            UserError::Storage(StorageError::Busy)
        );
        assert_eq!(
            UserError::from(&CacheKeyError::Malformed),
            UserError::Storage(StorageError::Corrupt)
        );
        assert_eq!(
            UserError::from(&EraseFailure::Stopped),
            UserError::Storage(StorageError::Stopped)
        );
    }
}
