//! The fake server's login half: codes, 2FA, expiry, consumed password
//! tokens, revocation, and the self-user write a real login makes.

use grammers_session::Session as _;
use grammers_session::types::{PeerAuth, PeerInfo};

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{
    Account, AuthError, BackendError, CheckPassword, CodeDelivery, CodeRequest,
    FAKE_INVALID_API_ID, FAKE_PUBLISHED_API_ID, FakeClient, FakeHandle, FakeLogin, Fault,
    NextDelivery, Op, PasswordBuf, PasswordChallenge, RejectReason, SentCode, SignIn,
    injected_panic,
};
use crate::api::CredentialError;

/// The fake account's user id and access hash.
pub const FAKE_SELF_ID: i64 = 777_000;
pub const FAKE_SELF_HASH: i64 = 0x5e1f;
/// Shortest phone number the fake accepts (digits).
const MIN_PHONE_DIGITS: usize = 5;

/// Proof of a sent code: the id of the code request.
#[derive(Debug)]
pub struct FakeLoginToken(u64);

/// One SRP attempt.
#[derive(Debug)]
pub struct FakePasswordToken(u64);

/// Server-side login state, shared by every instance.
#[derive(Debug)]
pub(super) struct ServerAuth {
    login: FakeLogin,
    /// Whether the server accepts the session's auth key. Starts `true`, so a
    /// stored session is valid; a code request resets it.
    pub(super) authorized: bool,
    next_token: u64,
    /// The only code request whose code is still valid.
    code_token: Option<u64>,
    /// The only password token not consumed yet.
    password_token: Option<u64>,
    signed_out: usize,
}

impl Default for ServerAuth {
    fn default() -> Self {
        Self::new(FakeLogin::default())
    }
}

/// What `sign_in` decided on the server.
enum SignInVerdict {
    InvalidCode,
    SignUpRequired,
    Password(u64, Option<String>),
    Done(String),
}

/// What `check_password` decided on the server.
enum PasswordVerdict {
    /// Stale or consumed token: grammers' `SignInError::Other`.
    Consumed,
    Invalid(u64, Option<String>),
    Done(String),
}

impl ServerAuth {
    pub(super) fn new(login: FakeLogin) -> Self {
        Self {
            login,
            authorized: true,
            next_token: 1,
            code_token: None,
            password_token: None,
            signed_out: 0,
        }
    }

    fn token(&mut self) -> u64 {
        let id = self.next_token;
        self.next_token = self.next_token.saturating_add(1);
        id
    }

    fn send_code(&mut self) -> u64 {
        self.authorized = false;
        let id = self.token();
        self.code_token = Some(id);
        id
    }

    fn sign_in(&mut self, token: u64, code: &str) -> SignInVerdict {
        if self.code_token != Some(token) || code != self.login.code {
            return SignInVerdict::InvalidCode;
        }
        if self.login.sign_up_required {
            return SignInVerdict::SignUpRequired;
        }
        self.code_token = None;
        if self.login.password.is_some() {
            let id = self.token();
            self.password_token = Some(id);
            return SignInVerdict::Password(id, self.login.hint.clone());
        }
        self.authorized = true;
        SignInVerdict::Done(self.login.display_name.clone())
    }

    fn check_password(&mut self, token: u64, password: &[u8]) -> PasswordVerdict {
        if self.password_token.take() != Some(token) {
            return PasswordVerdict::Consumed;
        }
        let expected = self.login.password.as_deref().unwrap_or_default();
        if password != expected.as_bytes() {
            let id = self.token();
            self.password_token = Some(id);
            return PasswordVerdict::Invalid(id, self.login.hint.clone());
        }
        self.authorized = true;
        PasswordVerdict::Done(self.login.display_name.clone())
    }

    fn challenge(&mut self) -> (u64, Option<String>) {
        let id = self.token();
        self.password_token = Some(id);
        (id, self.login.hint.clone())
    }
}

/// A flash call's pattern in the fake: a 12-digit number whose code is the
/// whole number (`--fake --onboarding`'s flash-call screen).
pub(crate) const FAKE_FLASH_PATTERN: &str = "+44 7700 9*****";

/// The delivery a resend (or a scripted number) takes in the fake.
fn delivery_of(next: Option<NextDelivery>) -> CodeDelivery {
    match next {
        Some(NextDelivery::Call) => CodeDelivery::Call,
        Some(NextDelivery::FlashCall) => CodeDelivery::FlashCall {
            pattern: FAKE_FLASH_PATTERN.into(),
        },
        Some(NextDelivery::MissedCall) => CodeDelivery::MissedCall {
            prefix: "+44 7700 90".into(),
        },
        Some(NextDelivery::FragmentSms) => CodeDelivery::Fragment {
            url: Some("https://fragment.com/number/888".into()),
        },
        Some(NextDelivery::Sms) | None => CodeDelivery::Sms,
    }
}

/// Where the fake "sends" a code. Numbers starting `+999` script the
/// unsupported email-setup flow; `+998` to `+994` an SMS, a call, a flash
/// call, a missed call and a Fragment code, so `--fake --onboarding` can
/// show each screen.
fn delivery_for(digits: &str, scripted: Option<SentCode>) -> Option<SentCode> {
    let with = |next| SentCode {
        delivery: delivery_of(Some(next)),
        length: Some(5),
        next: Some(NextDelivery::Call),
        timeout: Some(Duration::from_secs(90)),
    };
    match digits.get(..3) {
        Some("999") => None,
        Some("998") => Some(with(NextDelivery::Sms)),
        Some("997") => Some(with(NextDelivery::Call)),
        Some("996") => Some(with(NextDelivery::FlashCall)),
        Some("995") => Some(with(NextDelivery::MissedCall)),
        Some("994") => Some(with(NextDelivery::FragmentSms)),
        _ => scripted,
    }
}

/// Whether a call returns its answer or the answer is lost.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    Deliver,
    Lose,
    /// Applied by the server after this delay, detached from the caller
    /// ([`Fault::Queued`]).
    Queued(Duration),
}

fn to_backend(e: AuthError) -> BackendError {
    match e {
        AuthError::Backend(e) => e,
        _ => BackendError::Rejected(RejectReason::Other),
    }
}

fn account(display_name: &str) -> Account {
    Account {
        display_name: crate::sanitize::display_name(display_name),
    }
}

impl FakeClient {
    /// Counts the call, applies latency, then any injected fault.
    async fn auth_enter(&self, op: Op) -> Result<Answer, AuthError> {
        let fault = self.take_fault(op);
        if !self.shared.latency.is_zero() {
            tokio::time::sleep(self.shared.latency).await;
        }
        match fault {
            None => Ok(Answer::Deliver),
            Some(Fault::Delay(d)) => {
                tokio::time::sleep(d).await;
                Ok(Answer::Deliver)
            }
            Some(Fault::Error(e)) => Err(e.into()),
            Some(Fault::Auth(e)) => Err(e),
            Some(Fault::Hang) => std::future::pending().await,
            Some(Fault::CompleteThenHang) => Ok(Answer::Lose),
            Some(Fault::Queued(d)) => Ok(Answer::Queued(d)),
            Some(Fault::Panic) => injected_panic("login call"),
        }
    }

    /// Caches the self user like grammers' `complete_login`: a critical write.
    async fn cache_self(&self) -> Result<(), AuthError> {
        let session = &self.session;
        let id = self.shared.with(|s| s.auth.login.self_id);
        let info = PeerInfo::User {
            id,
            auth: Some(PeerAuth::from_hash(FAKE_SELF_HASH)),
            bot: Some(false),
            is_self: Some(true),
        };
        session
            .cache_peer(info)
            .await
            .map_err(|_| AuthError::Backend(BackendError::Rejected(RejectReason::Other)))
    }

    async fn answer<T>(answer: Answer, value: T) -> T {
        if answer == Answer::Lose {
            std::future::pending::<()>().await;
        }
        value
    }

    /// Applies `apply` to the server's login state: now, or for a queued
    /// request, after its delay on a detached task (unless the instance was
    /// retired by then, which drops it), whether or not this caller waits.
    async fn on_server<T: Send + 'static>(
        &self,
        answer: Answer,
        apply: impl FnOnce(&mut ServerAuth) -> T + Send + 'static,
    ) -> T {
        let Answer::Queued(delay) = answer else {
            return self.shared.with(|s| apply(&mut s.auth));
        };
        let shared = Arc::clone(&self.shared);
        let retired = Arc::clone(&self.retired);
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            if retired.load(Ordering::SeqCst) {
                return;
            }
            let _ = tx.send(shared.with(|s| apply(&mut s.auth)));
        });
        match rx.await {
            Ok(v) => v,
            Err(_) => std::future::pending().await,
        }
    }

    pub(super) async fn auth_is_authorized(&self) -> Result<bool, BackendError> {
        let answer = self
            .auth_enter(Op::IsAuthorized)
            .await
            .map_err(to_backend)?;
        let authorized = self.shared.with(|s| s.auth.authorized);
        Self::answer(answer, Ok(authorized)).await
    }

    pub(super) async fn auth_request_code(
        &self,
        phone: &str,
    ) -> Result<CodeRequest<FakeLoginToken>, AuthError> {
        let answer = self.auth_enter(Op::RequestCode).await?;
        match self.creds.api_id.get() {
            FAKE_INVALID_API_ID => {
                return Err(AuthError::Credentials(CredentialError::ApiIdInvalid));
            }
            FAKE_PUBLISHED_API_ID => {
                return Err(AuthError::Credentials(CredentialError::ApiIdPublishedFlood));
            }
            _ => {}
        }
        let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
        if digits.len() < MIN_PHONE_DIGITS {
            return Err(AuthError::PhoneInvalid);
        }
        let scripted = self.shared.with(|s| s.auth.login.sent.clone());
        let sent = delivery_for(&digits, scripted).ok_or(AuthError::OfficialAppRequired)?;
        let id = self.shared.with(|s| s.auth.send_code());
        let request = CodeRequest {
            token: FakeLoginToken(id),
            sent,
        };
        Self::answer(answer, Ok(request)).await
    }

    /// `auth.resendCode`: the next delivery type, a fresh code request.
    pub(super) async fn auth_resend_code(
        &self,
        token: &FakeLoginToken,
    ) -> Result<CodeRequest<FakeLoginToken>, AuthError> {
        let answer = self.auth_enter(Op::ResendCode).await?;
        let resent = self.shared.with(|s| {
            (s.auth.code_token == Some(token.0))
                .then(|| (s.auth.send_code(), s.auth.login.sent.clone()))
        });
        // A stale or expired code request: Telegram's PHONE_CODE_EXPIRED.
        let (id, sent) = resent.ok_or(AuthError::Backend(BackendError::Rejected(
            RejectReason::Other,
        )))?;
        let delivery = delivery_of(sent.and_then(|s| s.next));

        let request = CodeRequest {
            token: FakeLoginToken(id),
            sent: SentCode {
                delivery,
                length: Some(5),
                next: None,
                timeout: None,
            },
        };
        Self::answer(answer, Ok(request)).await
    }

    pub(super) async fn auth_sign_in(
        &self,
        token: &FakeLoginToken,
        code: &str,
    ) -> Result<SignIn<FakePasswordToken>, AuthError> {
        let answer = self.auth_enter(Op::SignIn).await?;
        let (id, code) = (token.0, code.to_owned());
        let verdict = self
            .on_server(answer, move |auth| auth.sign_in(id, &code))
            .await;
        let out = match verdict {
            SignInVerdict::InvalidCode => SignIn::InvalidCode,
            SignInVerdict::SignUpRequired => SignIn::SignUpRequired,
            SignInVerdict::Password(id, hint) => SignIn::PasswordRequired(PasswordChallenge {
                token: FakePasswordToken(id),
                hint,
            }),
            SignInVerdict::Done(name) => {
                self.cache_self().await?;
                SignIn::Done(account(&name))
            }
        };
        Self::answer(answer, Ok(out)).await
    }

    pub(super) async fn auth_check_password(
        &self,
        token: FakePasswordToken,
        password: PasswordBuf,
    ) -> Result<CheckPassword<FakePasswordToken>, AuthError> {
        let answer = self.auth_enter(Op::CheckPassword).await?;
        let id = token.0;
        let verdict = self
            .on_server(answer, move |auth| {
                auth.check_password(id, password.as_bytes())
            })
            .await;

        let out = match verdict {
            PasswordVerdict::Consumed => {
                return Err(AuthError::Backend(BackendError::Rejected(
                    RejectReason::Other,
                )));
            }
            PasswordVerdict::Invalid(id, hint) => {
                CheckPassword::InvalidPassword(PasswordChallenge {
                    token: FakePasswordToken(id),
                    hint,
                })
            }
            PasswordVerdict::Done(name) => {
                self.cache_self().await?;
                CheckPassword::Done(account(&name))
            }
        };
        Self::answer(answer, Ok(out)).await
    }

    pub(super) async fn auth_challenge(
        &self,
    ) -> Result<PasswordChallenge<FakePasswordToken>, AuthError> {
        let answer = self.auth_enter(Op::PasswordChallenge).await?;
        let (id, hint) = self.shared.with(|s| s.auth.challenge());
        let challenge = PasswordChallenge {
            token: FakePasswordToken(id),
            hint,
        };
        Self::answer(answer, Ok(challenge)).await
    }

    pub(super) async fn auth_fetch_self(&self) -> Result<Account, BackendError> {
        let answer = self.auth_enter(Op::FetchSelf).await.map_err(to_backend)?;
        let (authorized, name) = self
            .shared
            .with(|s| (s.auth.authorized, s.auth.login.display_name.clone()));
        if !authorized {
            return Err(BackendError::Unauthorized);
        }
        self.cache_self().await.map_err(to_backend)?;
        Self::answer(answer, Ok(account(&name))).await
    }

    pub(super) async fn auth_sign_out(&self) -> Result<(), BackendError> {
        let answer = self.auth_enter(Op::SignOut).await.map_err(to_backend)?;
        self.shared.with(|s| {
            s.auth.authorized = false;
            s.auth.signed_out = s.auth.signed_out.saturating_add(1);
        });
        Self::answer(answer, Ok(())).await
    }
}

impl FakeHandle {
    /// The server revokes the session (`AUTH_KEY_UNREGISTERED` from now on).
    pub fn revoke_session(&self) {
        self.shared.with(|s| s.auth.authorized = false);
    }

    /// The next login signs in to another account: the self user `self_id`.
    pub fn sign_in_as(&self, self_id: i64) {
        self.shared.with(|s| s.auth.login.self_id = self_id);
    }

    /// The last code expires (grammers folds `PHONE_CODE_EXPIRED` into
    /// `InvalidCode`); a new code request issues a fresh one.
    pub fn expire_code(&self) {
        self.shared.with(|s| s.auth.code_token = None);
    }

    /// Whether the server accepts the session.
    pub fn authorized(&self) -> bool {
        self.shared.with(|s| s.auth.authorized)
    }

    /// Successful `auth.logOut` calls.
    pub fn signed_out(&self) -> usize {
        self.shared.with(|s| s.auth.signed_out)
    }
}
