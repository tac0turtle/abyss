//! The grammers (`MTProto`) backend (plan sections 2–9, task 11).
//!
//! One build is one client instance: a fresh [`AbyssSession`] over the
//! account's critical cell, a `SenderPool` with `NoRetries` and a 1,024-update
//! channel, a `Client`, and an update stream with `catch_up: false`. Nothing is
//! spawned here: core spawns and supervises the runner and the update consumer,
//! so a panic in grammers (known sites: plan section 1) is contained by core's
//! `JoinHandle`s and becomes a rebuild or `Error(Internal)`. The mappers in
//! [`map`] are panic-free by construction (the panicking lints are denied).
//!
//! Logging: grammers' own events are dropped by the app's subscriber
//! (`grammers*` filtered out, plan section 11). This module logs only RPC
//! error names and codes, counts and ids, never a TL value: no `Debug` of a
//! request, response, peer or update, which could carry text or a phone.
//!
//! # Auth (task 6b)
//!
//! The login calls run over raw TL in [`auth`] (plan sections 4 and 17.2a):
//! `auth.sendCode` keeping the full `auth.SentCode` with the instance's
//! `api_hash`, `auth.resendCode`, `auth.signIn`, `auth.checkPassword` with an
//! `account.getPassword` refetch, `users.getUsers([inputUserSelf])` for the
//! self user, and `auth.logOut`. All use [`invoke::RetryClass::Never`].
//! A build waits for the [`CredentialSlot`] to hold a pair, so no instance
//! exists without credentials; core never bootstraps while signed out.

mod auth;
mod calls;
mod errors;
mod invoke;
pub(crate) mod map;
mod pins;
mod stream;
#[cfg(test)]
mod tests;

use std::num::NonZeroUsize;
use std::sync::Arc;

use grammers_client::client::ClientConfiguration;
use grammers_client::sender::{
    ConnectionParams, NoRetries, SenderPool, SenderPoolHandle, UpdatesConfiguration,
};

pub use auth::{GrammersLoginToken, GrammersPasswordToken};
pub use stream::GrammersUpdates;

use super::{
    Account, AuthError, Backend, BackendError, CheckPassword, Client, CodeRequest, CredentialSlot,
    DialogsCursor, DialogsPage, HistoryPage, HistoryQuery, Instance, InstanceControl,
    PasswordChallenge, Seed, SessionSeed, SessionSnapshot, SignIn, TopicsById, TopicsCursor,
    TopicsPage,
};
use crate::api::{ApiCredentials, PasswordBuf};
use crate::model::{
    ChatFolder, ChatId, ChatSummary, FolderEdit, MsgId, PeerFolder, ThreadKey, TopicId,
};
use crate::session::{AbyssSession, PersistHandle};
use calls::Calls;

/// Update channel between the runner and the stream (plan section 2; grammers'
/// default is 100).
pub const UPDATES_CHANNEL_CAPACITY: NonZeroUsize = match NonZeroUsize::new(1024) {
    Some(n) => n,
    None => NonZeroUsize::MIN,
};

fn connection_params() -> ConnectionParams {
    ConnectionParams {
        // Every retry is decided by the adapter's classes (`invoke`).
        retry_policy: Box::new(NoRetries),
        updates_channel_capacity: UPDATES_CHANNEL_CAPACITY,
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        ..ConnectionParams::default()
    }
}

/// Builds grammers client instances for the API credentials in its slot.
pub struct GrammersBackend {
    /// The pair in use; each build takes the current one.
    slot: CredentialSlot,
    /// Loads the persisted session for `SessionSeed::Stored` when core has
    /// not provided the account (`use_account`).
    persist: PersistHandle,
    /// The account core loaded at startup or created after a logout.
    account: std::sync::Mutex<Option<SessionSnapshot>>,
}

impl std::fmt::Debug for GrammersBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrammersBackend").finish_non_exhaustive()
    }
}

impl GrammersBackend {
    pub fn new(slot: CredentialSlot, persist: PersistHandle) -> Self {
        Self {
            slot,
            persist,
            account: std::sync::Mutex::new(None),
        }
    }

    fn account(&self) -> Option<SessionSnapshot> {
        self.account
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn snapshot(&self, seed: SessionSeed) -> SessionSnapshot {
        match seed {
            SessionSeed::Live(snapshot) => snapshot,
            SessionSeed::Stored => {
                if let Some(account) = self.account() {
                    return account;
                }
                if let Ok(loaded) = self.persist.load_session().await {
                    loaded.snapshot
                } else {
                    // Unreadable store: start logged out (task 5 wiring, step 1).
                    tracing::warn!("session store unreadable; starting signed out");
                    self.persist.fresh_session()
                }
            }
        }
    }
}

impl Backend for GrammersBackend {
    type Client = GrammersClient;
    type Updates = GrammersUpdates;
    type Control = GrammersControl;

    fn use_account(&self, account: SessionSnapshot) {
        *self
            .account
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(account);
    }

    fn credentials(&self) -> &CredentialSlot {
        &self.slot
    }

    async fn build(&self, seed: Seed) -> Result<Instance<Self>, BackendError> {
        // The retired instance's pins (core's last required set) arrive in
        // the snapshot and stay pinned until core's first `retain_peers` on
        // this instance.
        let creds = self.slot.wait().await;
        let snapshot = self.snapshot(seed.session).await;
        let session =
            Arc::new(AbyssSession::from_snapshot(snapshot).map_err(|_| BackendError::Transient)?);
        let SenderPool {
            runner,
            handle,
            updates,
        } = SenderPool::with_configuration(
            Arc::clone(&session),
            creds.api_id.get(),
            connection_params(),
        );
        let client = grammers_client::Client::with_configuration(
            handle.clone(),
            ClientConfiguration {
                auto_cache_peers: true,
            },
        );
        let stream = client
            .stream_updates(updates, UpdatesConfiguration { catch_up: false })
            .await
            .map_err(|_| BackendError::Transient)?;
        Ok(Instance {
            client: GrammersClient {
                calls: Calls::new(client, Arc::clone(&session)),
                creds,
                pool: handle.thin.clone(),
            },
            updates: GrammersUpdates::new(stream, session.baseline_established()),
            runner: Box::pin(runner.run()),
            control: GrammersControl {
                handle: handle.thin,
                session,
            },
        })
    }
}

/// Calls of one grammers instance.
pub struct GrammersClient {
    calls: Calls<grammers_client::Client>,
    /// The pair this instance was built with (`auth.sendCode` needs the hash).
    creds: Arc<ApiCredentials>,
    /// For a login-time `303`: cull the old DC's connection.
    pool: SenderPoolHandle,
}

impl std::fmt::Debug for GrammersClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrammersClient").finish_non_exhaustive()
    }
}

impl Client for GrammersClient {
    type LoginToken = GrammersLoginToken;
    type PasswordToken = GrammersPasswordToken;

    async fn is_authorized(&self) -> Result<bool, BackendError> {
        auth::is_authorized(self.calls.invoker()).await
    }

    async fn request_login_code(
        &self,
        phone: &str,
    ) -> Result<CodeRequest<GrammersLoginToken>, AuthError> {
        let pool = &self.pool;
        let disconnect = |dc| {
            pool.disconnect_from_dc(dc);
        };
        let session = self.calls.session();
        auth::send_code(
            self.calls.invoker(),
            session,
            &self.creds,
            phone,
            disconnect,
        )
        .await
    }

    async fn resend_login_code(
        &self,
        token: &GrammersLoginToken,
    ) -> Result<CodeRequest<GrammersLoginToken>, AuthError> {
        auth::resend_code(self.calls.invoker(), token).await
    }

    async fn sign_in(
        &self,
        token: &GrammersLoginToken,
        code: &str,
    ) -> Result<SignIn<GrammersPasswordToken>, AuthError> {
        auth::sign_in(self.calls.invoker(), self.calls.session(), token, code).await
    }

    async fn check_password(
        &self,
        token: GrammersPasswordToken,
        password: PasswordBuf,
    ) -> Result<CheckPassword<GrammersPasswordToken>, AuthError> {
        let session = self.calls.session();
        auth::check_password(self.calls.invoker(), session, token, password).await
    }

    async fn password_challenge(
        &self,
    ) -> Result<PasswordChallenge<GrammersPasswordToken>, AuthError> {
        auth::password_challenge(self.calls.invoker()).await
    }

    async fn fetch_self(&self) -> Result<Account, BackendError> {
        auth::fetch_self(self.calls.invoker(), self.calls.session()).await
    }

    async fn sign_out(&self) -> Result<(), BackendError> {
        auth::sign_out(self.calls.invoker()).await
    }

    async fn dialogs(
        &self,
        cursor: Option<DialogsCursor>,
        limit: u32,
    ) -> Result<DialogsPage, BackendError> {
        self.calls.dialogs(cursor, limit).await
    }

    async fn chat_folders(&self) -> Result<Vec<ChatFolder>, BackendError> {
        self.calls.chat_folders().await
    }

    async fn set_peer_folder(&self, chat: ChatId, folder: PeerFolder) -> Result<(), BackendError> {
        self.calls.set_peer_folder(chat, folder).await
    }

    async fn edit_chat_folder(&self, edit: FolderEdit) -> Result<(), BackendError> {
        self.calls.edit_chat_folder(edit).await
    }

    async fn history(
        &self,
        thread: ThreadKey,
        query: HistoryQuery,
        limit: u32,
    ) -> Result<HistoryPage, BackendError> {
        self.calls.history(thread, query, limit).await
    }

    async fn send_text(
        &self,
        thread: ThreadKey,
        text: String,
        random_id: i64,
    ) -> Result<MsgId, BackendError> {
        self.calls.send_text(thread, text, random_id).await
    }

    async fn read_history(&self, thread: ThreadKey, max_id: MsgId) -> Result<(), BackendError> {
        self.calls.read_history(thread, max_id).await
    }

    async fn topics(
        &self,
        chat: ChatId,
        cursor: Option<TopicsCursor>,
        limit: u32,
    ) -> Result<TopicsPage, BackendError> {
        self.calls.topics(chat, cursor, limit).await
    }

    async fn topics_by_id(
        &self,
        chat: ChatId,
        ids: &[TopicId],
    ) -> Result<TopicsById, BackendError> {
        self.calls.topics_by_id(chat, ids).await
    }

    async fn peer_dialogs(&self, chats: &[ChatId]) -> Result<Vec<ChatSummary>, BackendError> {
        self.calls.peer_dialogs(chats).await
    }

    fn retain_peers(&self, required: &[ChatId]) {
        self.calls.retain(required);
    }
}

/// Retirement hooks of one grammers instance.
pub struct GrammersControl {
    handle: SenderPoolHandle,
    session: Arc<AbyssSession>,
}

impl std::fmt::Debug for GrammersControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrammersControl").finish_non_exhaustive()
    }
}

impl InstanceControl for GrammersControl {
    fn quit(&self) {
        // `false` means the runner is already gone, which is what we want.
        let _ = self.handle.quit();
    }

    fn retire(self) -> SessionSnapshot {
        // Revoke before snapshotting (plan section 2, rebuild steps 4–5): a
        // late write through this instance must not reach the next one.
        if self.session.revoke().is_err() {
            tracing::warn!("retired session was poisoned");
        }
        self.session.snapshot()
    }
}
