//! The seam between core and a Telegram implementation.
//!
//! A [`Backend`] is a factory of client *instances* (plan section 2): each build
//! yields a [`Client`] for calls, an [`UpdateStream`], a runner future that core
//! supervises, and an [`InstanceControl`] used to retire it. Core owns the
//! lifecycle; a backend never rebuilds itself.
//!
//! The traits use `impl Future + Send` returns rather than `dyn` objects: the actor
//! is generic over one backend chosen at startup, so there is no need for object
//! safety, and static dispatch avoids boxing every call. `BackendChoice` maps the
//! runtime choice onto the concrete type in `start`.
//!
//! There is deliberately no media or download method: Stage 1 never fetches media,
//! and a backend cannot be asked to.

#[cfg(test)]
mod contract;
#[cfg(feature = "fake")]
pub mod fake;
pub mod grammers;
mod seed;

use std::future::Future;
use std::pin::Pin;

pub use seed::{MAX_CARRIED_PEERS, Seed, SessionSeed, SessionSnapshot};

use crate::api::{CodeDelivery, CredentialError, NextDelivery, PasswordBuf};
pub use crate::credentials::CredentialSlot;
use crate::model::{ChatId, ChatSummary, Message, MsgId, ThreadKey};

/// Opaque position in the dialogs list, produced by the backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogsCursor {
    pub(crate) offset_date: i32,
    pub(crate) offset_id: i32,
    pub(crate) offset_peer: Option<ChatId>,
    /// Index for backends that page by position (the fake).
    pub(crate) index: u32,
}

#[derive(Debug)]
pub struct DialogsPage {
    pub chats: Vec<ChatSummary>,
    /// `None` when this was the last page.
    pub next: Option<DialogsCursor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryQuery {
    /// The newest messages.
    Tail,
    /// Messages with id below the given one.
    Before(MsgId),
    /// Messages with id above the given one.
    After(MsgId),
}

#[derive(Debug)]
pub struct HistoryPage {
    /// Ascending by id.
    pub messages: Vec<Message>,
    /// More history exists in the queried direction.
    pub has_more: bool,
}

#[derive(Clone, Debug)]
pub enum Update {
    NewMessage(Message),
    Edited(Message),
    /// `chat` is `None` for private chats and basic groups (account-unique ids).
    Deleted {
        chat: Option<ChatId>,
        ids: Vec<MsgId>,
    },
    /// Read on another device.
    ReadInbox {
        chat: ChatId,
        still_unread: u32,
    },
    /// `updateMessageID`: the server assigned `id` to the send carrying
    /// `random_id` (plan section 9). Correlated with the pending-send table.
    MessageId {
        random_id: i64,
        id: MsgId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendError {
    /// The server rejected the request; retrying with the same input won't help.
    Rejected,
    FloodWait {
        seconds: u32,
    },
    /// Network or transport failure; the outcome of a write is unknown.
    Transient,
    /// The authorization is gone.
    Unauthorized,
    /// An authenticated, non-file request was answered with a `303` migration.
    /// Stage 1 does not move authorizations between DCs (plan section 2, risk 6).
    Migrated,
    /// Sends only: the server accepted the message, but no id mapping is known
    /// (`RANDOM_ID_DUPLICATE` on a resend, or a response without
    /// `updateMessageID`). Never retried (plan section 9).
    SentUnconfirmed,
}

/// One item from an instance's update stream.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "`Baseline` occurs once per stream; boxing would allocate per update"
)]
pub enum StreamItem {
    /// The update baseline (`updates.getState`) is recorded. Yielded at most once
    /// per stream. Snapshots taken after it cannot miss an update the stream
    /// will deliver.
    Baseline,
    Update(Update),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamError {
    /// May clear up; core retries on the same stream with backoff.
    Transient,
    /// The stream is unusable; core rebuilds the instance.
    Fatal,
    /// The authorization is gone (`AUTH_KEY_UNREGISTERED`, `SESSION_REVOKED`):
    /// core runs the local logout instead of rebuilding.
    Unauthorized,
    /// An authenticated `303`: core applies the migration policy (rebuild
    /// once, then ask for a new sign-in; plan section 2, risk 6).
    Migrated,
}

/// Why a login call failed (plan section 4, "Error mapping").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    /// `FLOOD_WAIT`, transport errors, `AUTH_KEY_UNREGISTERED`/`SESSION_REVOKED`
    /// (`Unauthorized`), other RPC errors (`Rejected`).
    Backend(BackendError),
    /// `PHONE_NUMBER_INVALID`.
    PhoneInvalid,
    /// `PHONE_NUMBER_BANNED`.
    PhoneBanned,
    /// `API_ID_INVALID` / `API_ID_PUBLISHED_FLOOD` (plan section 17.2).
    Credentials(CredentialError),
    /// `PHONE_NUMBER_FLOOD` / `PHONE_PASSWORD_FLOOD`: too many attempts for
    /// this number, without a wait time.
    PhoneFlood,
    /// The server asked for a login step abyss does not implement (email
    /// setup, `auth.sentCodeSuccess`, a paid login): finish it in an
    /// official app (plan section 17.2a).
    OfficialAppRequired,
}

/// Where a login code went (`auth.sentCode`, plan section 17.2a).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentCode {
    pub delivery: CodeDelivery,
    pub length: Option<u8>,
    pub next: Option<NextDelivery>,
    /// The server's resend timeout.
    pub timeout: Option<std::time::Duration>,
}

/// A sent login code: the token to sign in with, and where the code went.
#[derive(Debug)]
pub struct CodeRequest<T> {
    pub token: T,
    pub sent: SentCode,
}

impl From<BackendError> for AuthError {
    fn from(e: BackendError) -> Self {
        Self::Backend(e)
    }
}

/// The signed-in account, as far as the UI needs it.
#[derive(Clone, PartialEq, Eq)]
pub struct Account {
    /// Sanitized display name (`crate::sanitize::display_name`).
    pub display_name: String,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Account(<redacted>)")
    }
}

/// A 2FA challenge: the SRP token (`account.getPassword`) and the hint.
pub struct PasswordChallenge<P> {
    pub token: P,
    pub hint: Option<String>,
}

impl<P> std::fmt::Debug for PasswordChallenge<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PasswordChallenge(<redacted>)")
    }
}

/// Outcome of `sign_in` (grammers `auth.rs:339-362`).
#[derive(Debug)]
pub enum SignIn<P> {
    /// Signed in. The backend has cached the self user (`is_self`), which is a
    /// critical, persisted write.
    Done(Account),
    PasswordRequired(PasswordChallenge<P>),
    /// Wrong or expired code (`PHONE_CODE_EXPIRED` folds into this).
    InvalidCode,
    SignUpRequired,
}

/// Outcome of `check_password`. Every other outcome consumed the token.
#[derive(Debug)]
pub enum CheckPassword<P> {
    Done(Account),
    /// Wrong password; carries the server's fresh token.
    InvalidPassword(PasswordChallenge<P>),
}

/// Calls bound to one client instance.
///
/// The login calls (task 6) are never retried by the backend (retry class
/// `Auth`). Their futures must be `'static`: core moves the tokens in and
/// receives them back.
pub trait Client: Send + Sync + 'static {
    /// grammers `LoginToken`: proof of a sent code.
    type LoginToken: Send + Sync + 'static;
    /// grammers `PasswordToken`: one SRP attempt.
    type PasswordToken: Send + Sync + 'static;

    /// Whether this instance's session is authorized (grammers `is_authorized`).
    fn is_authorized(&self) -> impl Future<Output = Result<bool, BackendError>> + Send;

    /// `auth.sendCode` for `phone` with the instance's API credentials,
    /// keeping the full `auth.SentCode`. Handles a login-time `303` itself.
    /// Unsupported flows answer [`AuthError::OfficialAppRequired`].
    fn request_login_code(
        &self,
        phone: &str,
    ) -> impl Future<Output = Result<CodeRequest<Self::LoginToken>, AuthError>> + Send;

    /// `auth.resendCode`: the server-directed resend (its next delivery
    /// type). Returns a fresh token.
    fn resend_login_code(
        &self,
        token: &Self::LoginToken,
    ) -> impl Future<Output = Result<CodeRequest<Self::LoginToken>, AuthError>> + Send;

    fn sign_in(
        &self,
        token: &Self::LoginToken,
        code: &str,
    ) -> impl Future<Output = Result<SignIn<Self::PasswordToken>, AuthError>> + Send;

    /// Consumes `token` and the password buffer.
    fn check_password(
        &self,
        token: Self::PasswordToken,
        password: PasswordBuf,
    ) -> impl Future<Output = Result<CheckPassword<Self::PasswordToken>, AuthError>> + Send;

    /// A fresh challenge (raw `account.getPassword` + `PasswordToken::new`).
    fn password_challenge(
        &self,
    ) -> impl Future<Output = Result<PasswordChallenge<Self::PasswordToken>, AuthError>> + Send;

    /// The self user (raw `users.getUsers([inputUserSelf])`), cached as
    /// `is_self: Some(true)`: a critical write. `Unauthorized` when the
    /// session is not authorized.
    fn fetch_self(&self) -> impl Future<Output = Result<Account, BackendError>> + Send;

    /// `auth.logOut`. The runner must still be running.
    fn sign_out(&self) -> impl Future<Output = Result<(), BackendError>> + Send;

    fn dialogs(
        &self,
        cursor: Option<DialogsCursor>,
        limit: u32,
    ) -> impl Future<Output = Result<DialogsPage, BackendError>> + Send;

    fn history(
        &self,
        thread: ThreadKey,
        query: HistoryQuery,
        limit: u32,
    ) -> impl Future<Output = Result<HistoryPage, BackendError>> + Send;

    /// Sends text with a caller-chosen `random_id`, which must be reused for every
    /// retry of the same logical send so the server can deduplicate.
    fn send_text(
        &self,
        thread: ThreadKey,
        text: String,
        random_id: i64,
    ) -> impl Future<Output = Result<MsgId, BackendError>> + Send;

    /// Marks history read up to and including `max_id`.
    fn read_history(
        &self,
        thread: ThreadKey,
        max_id: MsgId,
    ) -> impl Future<Output = Result<(), BackendError>> + Send;

    /// The chats whose peer data (access hashes) core still needs: pending
    /// sends first, then open windows, then the retained dialogs in list
    /// order; deduplicated and at most `session::MAX_PINNED_PEERS`. Core calls
    /// it, coalesced, whenever that set changes and once per new instance; a
    /// backend keeps exactly these peers pinned. Must not block.
    fn retain_peers(&self, required: &[ChatId]) {
        let _ = required;
    }
}

/// An instance's live updates. The first `next` call must start establishing
/// the baseline before it waits for updates (grammers sends `updates.getState`).
pub trait UpdateStream: Send + 'static {
    fn next(&mut self) -> impl Future<Output = Result<StreamItem, StreamError>> + Send;
}

/// Retirement hooks for one instance.
pub trait InstanceControl: Send + Sync + 'static {
    /// Asks the runner to stop (grammers: `pool.handle.quit()`). Must not block.
    fn quit(&self);

    /// Called once the runner has completed or been cancelled: revokes the
    /// session's write token, so a late write from this instance is dropped, and
    /// returns the live critical snapshot for the next build.
    fn retire(self) -> SessionSnapshot;
}

/// The connection driver of an instance. Its completion, for any reason other
/// than `quit`, means the instance is dead.
pub type Runner = Pin<Box<dyn Future<Output = ()> + Send>>;

/// A freshly built client instance.
pub struct Instance<B: Backend + ?Sized> {
    pub client: B::Client,
    pub updates: B::Updates,
    pub runner: Runner,
    pub control: B::Control,
}

impl<B: Backend + ?Sized> std::fmt::Debug for Instance<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Instance").finish_non_exhaustive()
    }
}

/// Builds client instances.
pub trait Backend: Send + Sync + 'static {
    type Client: Client;
    type Updates: UpdateStream;
    type Control: InstanceControl;

    /// The account's live critical state (task 6). Core calls it before the
    /// first build and again whenever the account generation changes (after a
    /// logout). A build seeded with [`SessionSeed::Stored`] creates its
    /// `AbyssSession` from the latest one (`AbyssSession::from_snapshot`,
    /// which revokes any earlier instance's write token).
    fn use_account(&self, account: crate::session::SessionSnapshot);

    /// The API credentials in use (plan section 17.2a). [`Backend::build`]
    /// waits until the slot holds a pair and builds with it, so no client
    /// exists without credentials.
    fn credentials(&self) -> &CredentialSlot;

    /// Builds a fresh instance from `seed`. Spawns nothing: core spawns the
    /// runner and the update consumer.
    fn build(
        &self,
        seed: Seed,
    ) -> impl Future<Output = Result<Instance<Self>, BackendError>> + Send;
}
