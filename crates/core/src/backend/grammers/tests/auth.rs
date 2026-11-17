//! The raw login adapter (task 6b): the sent-code mapper for every
//! `auth.SentCodeType`, the auth error table, and `auth.sendCode` over a
//! scripted invoker (request shape, `api_hash`, a login-time `303`).

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use grammers_client::InvocationError;
use grammers_client::sender::{ReadError, RpcError};
use grammers_client::session::Session as _;
use grammers_client::tl::{self, Deserializable as _, Serializable as _};

use crate::api::{ApiCredentials, CodeDelivery, CredentialError, NextDelivery};
use crate::backend::grammers::auth::{
    MappedCode, SignInFailure, auth_error, is_authorized, map_sent_code, send_code, sign_in_failure,
};
use crate::backend::grammers::invoke::Invoke;
use crate::backend::{AuthError, BackendError, RejectReason, SentCode, SessionSnapshot};
use crate::session::AbyssSession;

use tl::enums::auth::SentCodeType as T;
use tl::types::auth as ty;

type Reply = Result<Vec<u8>, InvocationError>;

#[derive(Default)]
struct Mock {
    script: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<Vec<u8>>>,
}

impl Invoke for Arc<Mock> {
    fn invoke<R>(
        &self,
        request: &R,
    ) -> impl Future<Output = Result<R::Return, InvocationError>> + Send
    where
        R: tl::RemoteCall + Send + Sync,
        R::Return: Send,
    {
        self.requests.lock().unwrap().push(request.to_bytes());
        let reply = self.script.lock().unwrap().pop_front();
        async move {
            match reply {
                Some(Ok(bytes)) => R::Return::from_bytes(&bytes)
                    .map_err(|e| InvocationError::from(ReadError::from(e))),
                Some(Err(e)) => Err(e),
                None => Err(InvocationError::Dropped("script exhausted")),
            }
        }
    }
}

fn mock(replies: Vec<Reply>) -> Arc<Mock> {
    Arc::new(Mock {
        script: Mutex::new(replies.into()),
        requests: Mutex::default(),
    })
}

fn rpc(code: i32, name: &str, value: Option<u32>) -> InvocationError {
    InvocationError::Rpc(RpcError {
        code,
        name: name.to_owned(),
        value,
        caused_by: None,
    })
}

fn code(
    r#type: T,
    next: Option<tl::enums::auth::CodeType>,
    timeout: Option<i32>,
) -> tl::enums::auth::SentCode {
    tl::enums::auth::SentCode::Code(ty::SentCode {
        r#type,
        phone_code_hash: "h4sh".into(),
        next_type: next,
        timeout,
    })
}

fn mapped(delivery: CodeDelivery, length: Option<u8>) -> MappedCode {
    MappedCode {
        hash: "h4sh".into(),
        sent: SentCode {
            delivery,
            length,
            next: None,
            timeout: None,
        },
    }
}

#[test]
fn every_sent_code_type_is_mapped() {
    let table: Vec<(T, Result<MappedCode, AuthError>)> = vec![
        (
            T::App(ty::SentCodeTypeApp { length: 5 }),
            Ok(mapped(CodeDelivery::App, Some(5))),
        ),
        (
            T::Sms(ty::SentCodeTypeSms { length: 6 }),
            Ok(mapped(CodeDelivery::Sms, Some(6))),
        ),
        (
            T::Call(ty::SentCodeTypeCall { length: 5 }),
            Ok(mapped(CodeDelivery::Call, Some(5))),
        ),
        (
            T::FlashCall(ty::SentCodeTypeFlashCall {
                pattern: "+44 7700 9*****".into(),
            }),
            Ok(mapped(
                CodeDelivery::FlashCall {
                    pattern: "+44 7700 9*****".into(),
                },
                None,
            )),
        ),
        (
            T::MissedCall(ty::SentCodeTypeMissedCall {
                prefix: "+44 7700".into(),
                length: 4,
            }),
            Ok(mapped(
                CodeDelivery::MissedCall {
                    prefix: "+44 7700".into(),
                },
                Some(4),
            )),
        ),
        (
            T::FragmentSms(ty::SentCodeTypeFragmentSms {
                url: "https://fragment.com/number/888".into(),
                length: 5,
            }),
            Ok(mapped(
                CodeDelivery::Fragment {
                    url: Some("https://fragment.com/number/888".into()),
                },
                Some(5),
            )),
        ),
    ];
    for (t, want) in table {
        assert_eq!(map_sent_code(code(t, None, None)), want);
    }
}

/// Email, Firebase and word/phrase codes need an official app.
#[test]
fn unsupported_sent_code_types_need_an_official_app() {
    let official = Err(AuthError::OfficialAppRequired);
    let table: Vec<(T, Result<MappedCode, AuthError>)> = vec![
        (
            T::EmailCode(ty::SentCodeTypeEmailCode {
                apple_signin_allowed: false,
                google_signin_allowed: false,
                email_pattern: "a***@b".into(),
                length: 6,
                reset_available_period: None,
                reset_pending_date: None,
            }),
            official.clone(),
        ),
        (
            T::SetUpEmailRequired(ty::SentCodeTypeSetUpEmailRequired {
                apple_signin_allowed: false,
                google_signin_allowed: false,
            }),
            official.clone(),
        ),
        (
            T::FirebaseSms(ty::SentCodeTypeFirebaseSms {
                nonce: None,
                play_integrity_project_id: None,
                play_integrity_nonce: None,
                receipt: None,
                push_timeout: None,
                length: 5,
            }),
            official.clone(),
        ),
        (
            T::SmsWord(ty::SentCodeTypeSmsWord { beginning: None }),
            official.clone(),
        ),
        (
            T::SmsPhrase(ty::SentCodeTypeSmsPhrase { beginning: None }),
            official,
        ),
    ];
    for (t, want) in table {
        assert_eq!(map_sent_code(code(t, None, None)), want);
    }
}

#[test]
fn next_type_and_timeout_are_kept_and_bounded() {
    use tl::enums::auth::CodeType as C;
    let app = || T::App(ty::SentCodeTypeApp { length: 5 });
    for (next, want) in [
        (C::Sms, NextDelivery::Sms),
        (C::Call, NextDelivery::Call),
        (C::FlashCall, NextDelivery::FlashCall),
        (C::MissedCall, NextDelivery::MissedCall),
        (C::FragmentSms, NextDelivery::FragmentSms),
    ] {
        let got = map_sent_code(code(app(), Some(next), Some(60))).unwrap();
        assert_eq!(got.sent.next, Some(want));
        assert_eq!(got.sent.timeout, Some(Duration::from_secs(60)));
    }
    let long = map_sent_code(code(app(), None, Some(i32::MAX))).unwrap();
    assert_eq!(long.sent.timeout, Some(Duration::from_hours(24)));
    let negative = map_sent_code(code(app(), None, Some(-5))).unwrap();
    assert_eq!(negative.sent.timeout, None);
    let zero_length = map_sent_code(code(T::Sms(ty::SentCodeTypeSms { length: 0 }), None, None));
    assert_eq!(zero_length.unwrap().sent.length, None);
}

#[test]
fn sent_code_success_and_payment_need_an_official_app() {
    let success = tl::enums::auth::SentCode::Success(ty::SentCodeSuccess {
        authorization: tl::enums::auth::Authorization::SignUpRequired(
            ty::AuthorizationSignUpRequired {
                terms_of_service: None,
            },
        ),
    });
    assert_eq!(map_sent_code(success), Err(AuthError::OfficialAppRequired));
    let paid = tl::enums::auth::SentCode::PaymentRequired(ty::SentCodePaymentRequired {
        store_product: String::new(),
        phone_code_hash: "h".into(),
        support_email_address: String::new(),
        support_email_subject: String::new(),
        premium_days: 0,
        currency: "EUR".into(),
        amount: 0,
    });
    assert_eq!(map_sent_code(paid), Err(AuthError::OfficialAppRequired));
}

#[test]
fn auth_error_table() {
    let table: &[(i32, &str, Option<u32>, AuthError)] = &[
        (
            400,
            "API_ID_INVALID",
            None,
            AuthError::Credentials(CredentialError::ApiIdInvalid),
        ),
        (
            400,
            "API_ID_PUBLISHED_FLOOD",
            None,
            AuthError::Credentials(CredentialError::ApiIdPublishedFlood),
        ),
        (400, "PHONE_NUMBER_INVALID", None, AuthError::PhoneInvalid),
        (400, "PHONE_NUMBER_BANNED", None, AuthError::PhoneBanned),
        (400, "PHONE_NUMBER_FLOOD", None, AuthError::PhoneFlood),
        (400, "PHONE_PASSWORD_FLOOD", None, AuthError::PhoneFlood),
        (
            420,
            "FLOOD_WAIT",
            Some(31),
            AuthError::Backend(BackendError::FloodWait { seconds: 31 }),
        ),
        (
            401,
            "AUTH_KEY_UNREGISTERED",
            None,
            AuthError::Backend(BackendError::Unauthorized),
        ),
        (
            401,
            "SESSION_REVOKED",
            None,
            AuthError::Backend(BackendError::Unauthorized),
        ),
        (
            400,
            "PASSWORD_HASH_INVALID",
            None,
            AuthError::Backend(BackendError::Rejected(RejectReason::Other)),
        ),
        (
            500,
            "INTERNAL",
            None,
            AuthError::Backend(BackendError::Transient),
        ),
    ];
    for (code, name, value, want) in table {
        assert_eq!(auth_error(&rpc(*code, name, *value)), *want, "{name}");
    }
    let io = InvocationError::Io(std::io::Error::other("reset"));
    assert_eq!(auth_error(&io), AuthError::Backend(BackendError::Transient));
}

#[test]
fn sign_in_failures() {
    let table: &[(i32, &str, SignInFailure)] = &[
        (
            401,
            "SESSION_PASSWORD_NEEDED",
            SignInFailure::PasswordNeeded,
        ),
        (400, "PHONE_CODE_INVALID", SignInFailure::InvalidCode),
        (400, "PHONE_CODE_EXPIRED", SignInFailure::InvalidCode),
        (400, "PHONE_CODE_EMPTY", SignInFailure::InvalidCode),
        (
            400,
            "PHONE_NUMBER_UNOCCUPIED",
            SignInFailure::SignUpRequired,
        ),
        (
            420,
            "FLOOD_WAIT",
            SignInFailure::Other(AuthError::Backend(BackendError::FloodWait { seconds: 1 })),
        ),
        (
            401,
            "AUTH_KEY_UNREGISTERED",
            SignInFailure::Other(AuthError::Backend(BackendError::Unauthorized)),
        ),
    ];
    for (code, name, want) in table {
        assert_eq!(sign_in_failure(&rpc(*code, name, Some(1))), *want, "{name}");
    }
}

#[tokio::test]
async fn is_authorized_reads_401_as_not_signed_in() {
    let state = tl::enums::updates::State::State(tl::types::updates::State {
        pts: 1,
        qts: 0,
        date: 0,
        seq: 0,
        unread_count: 0,
    });
    let inv = mock(vec![
        Ok(state.to_bytes()),
        Err(rpc(401, "AUTH_KEY_UNREGISTERED", None)),
        Err(InvocationError::Io(std::io::Error::other("reset"))),
    ]);
    assert_eq!(is_authorized(&inv).await, Ok(true));
    assert_eq!(is_authorized(&inv).await, Ok(false));
    // A failed call is not an answer.
    assert_eq!(is_authorized(&inv).await, Err(BackendError::Transient));
}

fn creds() -> ApiCredentials {
    ApiCredentials {
        api_id: crate::api::ApiId::new(4_242_424).unwrap(),
        api_hash: crate::api::ApiHash::parse("feedfacefeedfacefeedfacefeedface").unwrap(),
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// `auth.sendCode` carries the instance's pair; a `303` moves the home DC
/// and asks once more on it.
#[tokio::test]
async fn send_code_uses_the_pair_and_follows_one_migration() {
    let sent = code(T::Sms(ty::SentCodeTypeSms { length: 5 }), None, Some(30));
    let inv = mock(vec![
        Err(rpc(303, "PHONE_MIGRATE", Some(4))),
        Ok(sent.to_bytes()),
    ]);
    let session = AbyssSession::from_snapshot(SessionSnapshot::detached()).unwrap();
    let old = session.home_dc_id().unwrap();
    let culled = Mutex::new(Vec::new());
    let disconnect = |dc| culled.lock().unwrap().push(dc);
    let got = send_code(&inv, &session, &creds(), "+447700900123", disconnect)
        .await
        .unwrap();
    assert_eq!(got.sent.delivery, CodeDelivery::Sms);
    assert_eq!(got.sent.timeout, Some(Duration::from_secs(30)));
    assert_eq!(session.home_dc_id().unwrap(), 4);
    assert_eq!(*culled.lock().unwrap(), vec![old]);
    let requests = inv.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    for req in &requests {
        assert!(contains(req, b"feedfacefeedfacefeedfacefeedface"));
        assert!(contains(req, &4_242_424_i32.to_le_bytes()));
    }
    assert!(format!("{:?}", got.token).contains("redacted"));
}

#[tokio::test]
async fn send_code_errors_are_typed_and_not_retried() {
    let inv = mock(vec![Err(rpc(400, "API_ID_INVALID", None))]);
    let session = AbyssSession::from_snapshot(SessionSnapshot::detached()).unwrap();
    let res = send_code(&inv, &session, &creds(), "+447700900123", |_| {}).await;
    assert_eq!(
        res.map(|_| ()),
        Err(AuthError::Credentials(CredentialError::ApiIdInvalid))
    );
    assert_eq!(inv.requests.lock().unwrap().len(), 1);
}
