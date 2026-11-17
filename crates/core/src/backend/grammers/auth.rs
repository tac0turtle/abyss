//! Login over raw TL (plan sections 4 and 17.2a, task 6b).
//!
//! grammers' high-level login discards the sent-code type, timeout and next
//! type (`auth.rs:290`), panics on `auth.sentCodeSuccess` and on odd 2FA
//! parameters (`current_algo.clone().unwrap()`), and caches the self user
//! through its own path. Here every request is raw, under
//! [`RetryClass::Never`] (auth calls are never retried), and every response
//! goes through a pure mapper:
//! - [`map_sent_code`]: the full `auth.SentCode`; unsupported flows (email
//!   setup, `auth.sentCodeSuccess`, paid logins, Firebase, word and phrase
//!   codes) become [`AuthError::OfficialAppRequired`].
//! - [`auth_error`]: `API_ID_*`, `PHONE_NUMBER_*`, floods and `401`.
//! - [`sign_in_failure`]: `SESSION_PASSWORD_NEEDED`, `PHONE_CODE_*`,
//!   `PHONE_NUMBER_UNOCCUPIED`.
//!
//! Nothing here logs a phone number, code, password, hash or user name: the
//! invocation layer logs only RPC error names and codes.

use std::time::Duration;

use grammers_client::InvocationError;
use grammers_client::session::Session as _;
use grammers_client::session::types::{PeerAuth, PeerInfo, UpdateState, UpdatesState};
use grammers_client::tl;
use zeroize::Zeroizing;

use super::errors::{backend_error, kind};
use super::invoke::{Invoke, RetryClass, call};
use crate::api::{
    ApiCredentials, CodeDelivery, CredentialError, MAX_DELIVERY_TEXT, NextDelivery, PasswordBuf,
};
use crate::backend::{
    Account, AuthError, BackendError, CheckPassword, CodeRequest, PasswordChallenge, SentCode,
    SignIn,
};
use crate::session::AbyssSession;

/// The phone number and `phone_code_hash` of one code request.
pub struct GrammersLoginToken {
    phone: Zeroizing<String>,
    hash: String,
}

impl std::fmt::Debug for GrammersLoginToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GrammersLoginToken(<redacted>)")
    }
}

/// One SRP attempt: the `account.password` it was computed from.
pub struct GrammersPasswordToken(Box<tl::types::account::Password>);

impl std::fmt::Debug for GrammersPasswordToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GrammersPasswordToken(<redacted>)")
    }
}

/// A mapped `auth.sentCode`.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct MappedCode {
    /// `phone_code_hash`: login material, never logged.
    pub(crate) hash: String,
    pub(crate) sent: SentCode,
}

impl std::fmt::Debug for MappedCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedCode")
            .field("hash", &"<redacted>")
            .field("sent", &self.sent)
            .finish()
    }
}

/// A flash-call pattern or missed-call prefix as shown: digits, `+`, `*`,
/// spaces and dashes only, bounded.
pub(crate) fn number_text(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_digit() || matches!(c, '+' | '*' | ' ' | '-'))
        .take(MAX_DELIVERY_TEXT)
        .collect()
}

/// The Fragment host: its number pages are `https://fragment.com/number/…`.
const FRAGMENT_HOST: &str = "fragment.com";
/// The longest DNS label.
const MAX_LABEL: usize = 63;

/// A server-supplied Fragment URL, kept only if it is unambiguously on
/// Fragment (review round 19, finding 6): bounded printable ASCII (no
/// spaces, controls or bidi marks), `https://`, no userinfo, no port but
/// the default, and a host that is `fragment.com` or a lowercase subdomain
/// of it (no punycode labels). Anything else is dropped, so the screen
/// shows no address rather than a misleading one. Shown as text; nothing
/// opens it. A small parser on purpose: no URL crate for one field.
///
/// Telegram documents `fragment.com` as the host (`auth.sentCodeTypeFragmentSms`
/// carries "the URL to open on Fragment"). Whether it ever sends a subdomain,
/// and what its path looks like, is unverified (no live sample): subdomains
/// are allowed only as plain lowercase labels under it, and the path is not
/// checked beyond being printable ASCII (review round 25).
pub(crate) fn safe_url(raw: &str) -> Option<String> {
    if raw.len() > MAX_DELIVERY_TEXT || !raw.bytes().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    let rest = raw.strip_prefix("https://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = rest.get(..end)?;
    // Userinfo (`fragment.com@evil.example`) is never allowed.
    if authority.contains('@') {
        return None;
    }
    let host = match authority.split_once(':') {
        Some((host, "443")) => host,
        Some(_) => return None,
        None => authority,
    };
    fragment_host(host).then(|| raw.to_owned())
}

/// `fragment.com`, or a subdomain of it made of plain lowercase labels.
fn fragment_host(host: &str) -> bool {
    let Some(sub) = host.strip_suffix(FRAGMENT_HOST) else {
        return false;
    };
    if sub.is_empty() {
        return true;
    }
    // `evilfragment.com` is not a subdomain.
    let Some(labels) = sub.strip_suffix('.') else {
        return false;
    };
    labels.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= MAX_LABEL
            && !label.starts_with('-')
            && !label.ends_with('-')
            && !label.starts_with("xn--")
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    })
}

/// The supported delivery of a sent-code type, with its code length.
fn delivery(t: &tl::enums::auth::SentCodeType) -> Option<(CodeDelivery, Option<u8>)> {
    use tl::enums::auth::SentCodeType as T;
    let len = |n: i32| u8::try_from(n).ok().filter(|n| *n > 0);
    Some(match t {
        T::App(x) => (CodeDelivery::App, len(x.length)),
        T::Sms(x) => (CodeDelivery::Sms, len(x.length)),
        T::Call(x) => (CodeDelivery::Call, len(x.length)),
        T::FlashCall(x) => {
            let pattern = number_text(&x.pattern);
            (CodeDelivery::FlashCall { pattern }, None)
        }
        T::MissedCall(x) => {
            let prefix = number_text(&x.prefix);
            (CodeDelivery::MissedCall { prefix }, len(x.length))
        }
        // Core routes this to the official-app screen with the URL as text.
        T::FragmentSms(x) => {
            let url = safe_url(&x.url);
            (CodeDelivery::Fragment { url }, len(x.length))
        }
        // Codes abyss cannot take (email, Firebase, a word or phrase), and the
        // email setup it does not implement.
        T::EmailCode(_)
        | T::SetUpEmailRequired(_)
        | T::FirebaseSms(_)
        | T::SmsWord(_)
        | T::SmsPhrase(_) => return None,
    })
}

fn next_delivery(t: &tl::enums::auth::CodeType) -> NextDelivery {
    use tl::enums::auth::CodeType as T;
    match t {
        T::Sms => NextDelivery::Sms,
        T::Call => NextDelivery::Call,
        T::FlashCall => NextDelivery::FlashCall,
        T::MissedCall => NextDelivery::MissedCall,
        T::FragmentSms => NextDelivery::FragmentSms,
    }
}

/// Keeps the full `auth.SentCode` (plan section 17.2a). The resend timeout
/// is bounded to a day; a negative one is dropped.
pub(crate) fn map_sent_code(sent: tl::enums::auth::SentCode) -> Result<MappedCode, AuthError> {
    const MAX_TIMEOUT: u64 = 24 * 60 * 60;
    let code = match sent {
        tl::enums::auth::SentCode::Code(code) => code,
        tl::enums::auth::SentCode::Success(_) | tl::enums::auth::SentCode::PaymentRequired(_) => {
            return Err(AuthError::OfficialAppRequired);
        }
    };
    let (delivery, length) = delivery(&code.r#type).ok_or(AuthError::OfficialAppRequired)?;
    let timeout = code
        .timeout
        .and_then(|t| u64::try_from(t).ok())
        .map(|t| Duration::from_secs(t.min(MAX_TIMEOUT)));
    Ok(MappedCode {
        hash: code.phone_code_hash,
        sent: SentCode {
            delivery,
            length,
            next: code.next_type.as_ref().map(next_delivery),
            timeout,
        },
    })
}

/// A login call's error (plan sections 4 and 17.2): typed where the user can
/// act on it, else the shared [`BackendError`] mapping (floods, `401`,
/// transport).
pub(crate) fn auth_error(e: &InvocationError) -> AuthError {
    if let InvocationError::Rpc(rpc) = e {
        match rpc.name.as_str() {
            "API_ID_INVALID" => return AuthError::Credentials(CredentialError::ApiIdInvalid),
            // The id is unusable, not rate-limited: no countdown.
            "API_ID_PUBLISHED_FLOOD" => {
                return AuthError::Credentials(CredentialError::ApiIdPublishedFlood);
            }
            "PHONE_NUMBER_INVALID" => return AuthError::PhoneInvalid,
            "PHONE_NUMBER_BANNED" => return AuthError::PhoneBanned,
            "PHONE_NUMBER_FLOOD" | "PHONE_PASSWORD_FLOOD" => return AuthError::PhoneFlood,
            _ => {}
        }
    }
    AuthError::Backend(backend_error(kind(e)))
}

/// How an `auth.signIn` error continues the login.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SignInFailure {
    /// `SESSION_PASSWORD_NEEDED`: fetch the 2FA parameters.
    PasswordNeeded,
    /// `PHONE_CODE_*` (invalid, expired, empty, hash empty).
    InvalidCode,
    /// `PHONE_NUMBER_UNOCCUPIED` (older layers).
    SignUpRequired,
    Other(AuthError),
}

pub(crate) fn sign_in_failure(e: &InvocationError) -> SignInFailure {
    if e.is("SESSION_PASSWORD_NEEDED") {
        SignInFailure::PasswordNeeded
    } else if e.is("PHONE_CODE_*") {
        SignInFailure::InvalidCode
    } else if e.is("PHONE_NUMBER_UNOCCUPIED") {
        SignInFailure::SignUpRequired
    } else {
        SignInFailure::Other(auth_error(e))
    }
}

/// `updates.getState` as grammers' `is_authorized` (`auth.rs:105-111`): any
/// `401` means "not authorized", which is not the same as a failed call.
pub(crate) async fn is_authorized<I: Invoke>(inv: &I) -> Result<bool, BackendError> {
    match call(inv, RetryClass::Never, &tl::functions::updates::GetState {}).await {
        Ok(_) => Ok(true),
        Err(InvocationError::Rpc(e)) if e.code == 401 => Ok(false),
        Err(e) => Err(backend_error(kind(&e))),
    }
}

fn send_code_request(phone: &str, creds: &ApiCredentials) -> tl::functions::auth::SendCode {
    tl::functions::auth::SendCode {
        phone_number: phone.to_owned(),
        api_id: creds.api_id.get(),
        api_hash: creds.api_hash.expose().to_owned(),
        settings: tl::types::CodeSettings {
            allow_flashcall: false,
            current_number: false,
            allow_app_hash: false,
            allow_missed_call: false,
            allow_firebase: false,
            unknown_number: false,
            logout_tokens: None,
            token: None,
            app_sandbox: None,
        }
        .into(),
    }
}

/// The DC a login-time `303` points at.
fn migrate_to(e: &InvocationError) -> Option<i32> {
    match e {
        InvocationError::Rpc(rpc) if rpc.code == 303 => {
            rpc.value.and_then(|v| i32::try_from(v).ok())
        }
        _ => None,
    }
}

/// `auth.sendCode` with the instance's credentials. A login-time `303`
/// moves the home DC and asks once more, as grammers does
/// (`auth.rs:262-275`); `disconnect` culls the old DC's connection.
pub(crate) async fn send_code<I: Invoke>(
    inv: &I,
    session: &AbyssSession,
    creds: &ApiCredentials,
    phone: &str,
    disconnect: impl Fn(i32),
) -> Result<CodeRequest<GrammersLoginToken>, AuthError> {
    let request = send_code_request(phone, creds);
    let sent = match call(inv, RetryClass::Never, &request).await {
        Ok(sent) => sent,
        Err(e) => {
            let Some(dc) = migrate_to(&e) else {
                return Err(auth_error(&e));
            };
            let old = session
                .home_dc_id()
                .map_err(|_| AuthError::Backend(BackendError::Transient))?;
            disconnect(old);
            session
                .set_home_dc_id(dc)
                .await
                .map_err(|_| AuthError::Backend(BackendError::Transient))?;
            call(inv, RetryClass::Never, &request)
                .await
                .map_err(|e| auth_error(&e))?
        }
    };
    let mapped = map_sent_code(sent)?;
    Ok(CodeRequest {
        token: GrammersLoginToken {
            phone: Zeroizing::new(phone.to_owned()),
            hash: mapped.hash,
        },
        sent: mapped.sent,
    })
}

/// `auth.resendCode`: the server's next delivery type.
pub(crate) async fn resend_code<I: Invoke>(
    inv: &I,
    token: &GrammersLoginToken,
) -> Result<CodeRequest<GrammersLoginToken>, AuthError> {
    let request = tl::functions::auth::ResendCode {
        phone_number: token.phone.as_str().to_owned(),
        phone_code_hash: token.hash.clone(),
        reason: None,
    };
    let sent =
        call(inv, RetryClass::Never, &request)
            .await
            .map_err(|e| match sign_in_failure(&e) {
                // An expired code request: ask for a new code from the phone step.
                SignInFailure::InvalidCode => AuthError::Backend(BackendError::Rejected),
                SignInFailure::Other(e) => e,
                SignInFailure::PasswordNeeded | SignInFailure::SignUpRequired => {
                    AuthError::Backend(BackendError::Rejected)
                }
            })?;
    let mapped = map_sent_code(sent)?;
    Ok(CodeRequest {
        token: GrammersLoginToken {
            phone: token.phone.clone(),
            hash: mapped.hash,
        },
        sent: mapped.sent,
    })
}

/// `auth.signIn`.
pub(crate) async fn sign_in<I: Invoke>(
    inv: &I,
    session: &AbyssSession,
    token: &GrammersLoginToken,
    code: &str,
) -> Result<SignIn<GrammersPasswordToken>, AuthError> {
    let request = tl::functions::auth::SignIn {
        phone_number: token.phone.as_str().to_owned(),
        phone_code_hash: token.hash.clone(),
        phone_code: Some(code.to_owned()),
        email_verification: None,
    };
    match call(inv, RetryClass::Never, &request).await {
        Ok(tl::enums::auth::Authorization::Authorization(auth)) => {
            complete_login(inv, session, auth).await.map(SignIn::Done)
        }
        Ok(tl::enums::auth::Authorization::SignUpRequired(_)) => Ok(SignIn::SignUpRequired),
        Err(e) => match sign_in_failure(&e) {
            SignInFailure::PasswordNeeded => {
                password_challenge(inv).await.map(SignIn::PasswordRequired)
            }
            SignInFailure::InvalidCode => Ok(SignIn::InvalidCode),
            SignInFailure::SignUpRequired => Ok(SignIn::SignUpRequired),
            SignInFailure::Other(e) => Err(e),
        },
    }
}

/// A fresh challenge: raw `account.getPassword` (plan section 4).
pub(crate) async fn password_challenge<I: Invoke>(
    inv: &I,
) -> Result<PasswordChallenge<GrammersPasswordToken>, AuthError> {
    let tl::enums::account::Password::Password(password) = call(
        inv,
        RetryClass::Never,
        &tl::functions::account::GetPassword {},
    )
    .await
    .map_err(|e| auth_error(&e))?;
    let hint = password.hint.clone();
    Ok(PasswordChallenge {
        token: GrammersPasswordToken(Box::new(password)),
        hint,
    })
}

/// The SRP inputs of a challenge, if usable: salts, `p`, `g`, `srp_B`,
/// `srp_id`. `None` for an unknown algorithm or missing fields (grammers
/// panics on these, `auth.rs:416` and `utils.rs:57`).
fn srp_params(
    password: &tl::types::account::Password,
) -> Option<(
    &tl::types::PasswordKdfAlgoSha256Sha256Pbkdf2Hmacsha512iter100000Sha256ModPow,
    &Vec<u8>,
    i64,
)> {
    let algo = match password.current_algo.as_ref()? {
        tl::enums::PasswordKdfAlgo::Sha256Sha256Pbkdf2Hmacsha512iter100000Sha256ModPow(a) => a,
        tl::enums::PasswordKdfAlgo::Unknown => return None,
    };
    Some((algo, password.srp_b.as_ref()?, password.srp_id?))
}

/// The SRP check for `password` against `token`'s parameters. Bad `p`/`g`
/// refetch the parameters once, as grammers does; still bad is `Rejected`.
async fn srp_check<I: Invoke>(
    inv: &I,
    token: GrammersPasswordToken,
    password: &PasswordBuf,
) -> Result<tl::functions::auth::CheckPassword, AuthError> {
    use grammers_crypto::two_factor_auth::{calculate_2fa, check_p_and_g};
    let usable = |pw: &tl::types::account::Password| {
        srp_params(pw).is_some_and(|(a, _, _)| check_p_and_g(&a.p, &a.g))
    };
    let mut info = token.0;
    if !usable(&info) {
        info = password_challenge(inv).await?.token.0;
    }
    if !usable(&info) {
        return Err(AuthError::Backend(BackendError::Rejected));
    }
    let (algo, srp_b, srp_id) =
        srp_params(&info).ok_or(AuthError::Backend(BackendError::Rejected))?;
    let (m1, g_a) = calculate_2fa(
        &algo.salt1,
        &algo.salt2,
        &algo.p,
        &algo.g,
        srp_b.clone(),
        info.secure_random.clone(),
        password.as_bytes(),
    );
    Ok(tl::functions::auth::CheckPassword {
        password: tl::enums::InputCheckPasswordSrp::Srp(tl::types::InputCheckPasswordSrp {
            srp_id,
            a: g_a.to_vec(),
            m1: m1.to_vec(),
        }),
    })
}

/// `auth.checkPassword`. A wrong password fetches a fresh challenge (SRP
/// parameters are single-use); every other failure consumed the token.
pub(crate) async fn check_password<I: Invoke>(
    inv: &I,
    session: &AbyssSession,
    token: GrammersPasswordToken,
    password: PasswordBuf,
) -> Result<CheckPassword<GrammersPasswordToken>, AuthError> {
    let request = srp_check(inv, token, &password).await;
    drop(password);
    match call(inv, RetryClass::Never, &request?).await {
        Ok(tl::enums::auth::Authorization::Authorization(auth)) => {
            complete_login(inv, session, auth)
                .await
                .map(CheckPassword::Done)
        }
        Ok(tl::enums::auth::Authorization::SignUpRequired(_)) => {
            Err(AuthError::Backend(BackendError::Rejected))
        }
        Err(e) if e.is("PASSWORD_HASH_INVALID") => password_challenge(inv)
            .await
            .map(CheckPassword::InvalidPassword),
        Err(e) => Err(auth_error(&e)),
    }
}

fn display_name(user: &tl::types::User) -> String {
    let first = user.first_name.as_deref().unwrap_or_default();
    let last = user.last_name.as_deref().unwrap_or_default();
    let joined = if last.is_empty() {
        first.to_owned()
    } else {
        format!("{first} {last}")
    };
    crate::sanitize::display_name(&joined)
}

/// Caches `user` as the self user: a critical, persisted write.
async fn cache_self(session: &AbyssSession, user: &tl::types::User) -> Result<(), BackendError> {
    let info = PeerInfo::User {
        id: user.id,
        auth: user.access_hash.map(PeerAuth::from_hash),
        bot: Some(user.bot),
        is_self: Some(true),
    };
    session
        .cache_peer(info)
        .await
        .map_err(|_| BackendError::Transient)
}

/// The update state after an authorization (round 11, finding 3). Only an
/// explicitly transient failure (transport, flood wait) is best-effort
/// (`None`: the stream establishes its own baseline). `401` means the key
/// was revoked between the authorization and now: propagated, so the login
/// tears the session down. Any other failure is reported as an ambiguous
/// transport error: the sign-in went through, so core reconciles with the
/// probe rather than showing a definite failure.
pub(crate) fn login_state(
    res: Result<tl::enums::updates::State, InvocationError>,
) -> Result<Option<tl::enums::updates::State>, AuthError> {
    match res {
        Ok(state) => Ok(Some(state)),
        Err(e) => match backend_error(kind(&e)) {
            BackendError::Transient | BackendError::FloodWait { .. } => Ok(None),
            BackendError::Unauthorized => Err(AuthError::Backend(BackendError::Unauthorized)),
            _ => Err(AuthError::Backend(BackendError::Transient)),
        },
    }
}

/// After a successful `auth.signIn`/`auth.checkPassword`, as grammers'
/// `complete_login` (`auth.rs:113-150`): record the update state, cache the
/// self user.
async fn complete_login<I: Invoke>(
    inv: &I,
    session: &AbyssSession,
    auth: tl::types::auth::Authorization,
) -> Result<Account, AuthError> {
    let state =
        login_state(call(inv, RetryClass::Never, &tl::functions::updates::GetState {}).await)?;
    let tl::enums::User::User(user) = auth.user else {
        return Err(AuthError::Backend(BackendError::Rejected));
    };
    cache_self(session, &user).await?;
    if let Some(tl::enums::updates::State::State(s)) = state {
        let all = UpdatesState {
            pts: s.pts,
            qts: s.qts,
            date: s.date,
            seq: s.seq,
            channels: Vec::new(),
        };
        let _ = session.set_update_state(UpdateState::All(all)).await;
    }
    Ok(Account {
        display_name: display_name(&user),
    })
}

/// The self user: raw `users.getUsers([inputUserSelf])`, cached as
/// `is_self` (plan section 4). `Unauthorized` when the session is not.
pub(crate) async fn fetch_self<I: Invoke>(
    inv: &I,
    session: &AbyssSession,
) -> Result<Account, BackendError> {
    let request = tl::functions::users::GetUsers {
        id: vec![tl::enums::InputUser::UserSelf],
    };
    let users = call(inv, RetryClass::Never, &request)
        .await
        .map_err(|e| backend_error(kind(&e)))?;
    let Some(tl::enums::User::User(user)) = users.into_iter().next() else {
        return Err(BackendError::Rejected);
    };
    cache_self(session, &user).await?;
    Ok(Account {
        display_name: display_name(&user),
    })
}

/// `auth.logOut`.
pub(crate) async fn sign_out<I: Invoke>(inv: &I) -> Result<(), BackendError> {
    call(inv, RetryClass::Never, &tl::functions::auth::LogOut {})
        .await
        .map(drop)
        .map_err(|e| backend_error(kind(&e)))
}
