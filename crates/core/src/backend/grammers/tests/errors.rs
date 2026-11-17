//! The RPC error table (plan sections 2, 4 and 9).

use grammers_client::InvocationError;
use grammers_client::sender::RpcError;

use crate::backend::grammers::errors::{backend_error, kind, loggable_name, stream_error};
use crate::backend::{BackendError, RejectReason, StreamError};

fn rpc(code: i32, name: &str, value: Option<u32>) -> InvocationError {
    InvocationError::Rpc(RpcError {
        code,
        name: name.to_owned(),
        value,
        caused_by: None,
    })
}

fn check(err: &InvocationError, backend: BackendError, stream: StreamError) {
    let k = kind(err);
    assert_eq!(backend_error(k), backend, "{}", loggable_name(err));
    assert_eq!(stream_error(k), stream, "{}", loggable_name(err));
}

#[test]
fn rpc_error_table() {
    use BackendError as B;
    use StreamError as S;
    let table: &[(i32, &str, Option<u32>, BackendError, StreamError)] = &[
        (
            420,
            "FLOOD_WAIT",
            Some(31),
            B::FloodWait { seconds: 31 },
            S::Transient,
        ),
        (
            420,
            "FLOOD_PREMIUM_WAIT",
            None,
            B::FloodWait { seconds: 1 },
            S::Transient,
        ),
        (
            400,
            "SLOWMODE_WAIT",
            Some(10),
            B::FloodWait { seconds: 10 },
            S::Transient,
        ),
        (303, "USER_MIGRATE", Some(4), B::Migrated, S::Migrated),
        (303, "NETWORK_MIGRATE", Some(2), B::Migrated, S::Migrated),
        (500, "RPC_CALL_FAIL", None, B::Transient, S::Transient),
        (
            500,
            "INTERDC_CALL_ERROR",
            Some(2),
            B::Transient,
            S::Transient,
        ),
        (-500, "NO_WORKERS_RUNNING", None, B::Transient, S::Transient),
        (
            400,
            "RANDOM_ID_DUPLICATE",
            None,
            B::SentUnconfirmed,
            S::Fatal,
        ),
        (
            400,
            "PEER_ID_INVALID",
            None,
            B::Rejected(RejectReason::Other),
            S::Transient,
        ),
        (
            403,
            "CHAT_WRITE_FORBIDDEN",
            None,
            B::Rejected(RejectReason::Other),
            S::Transient,
        ),
        (
            400,
            "MESSAGE_TOO_LONG",
            None,
            B::Rejected(RejectReason::Other),
            S::Transient,
        ),
    ];
    for &(code, name, value, backend, stream) in table {
        check(&rpc(code, name, value), backend, stream);
    }
}

/// The forum rejections core acts on (Stage 2 plan sections 5 and 6.7).
#[test]
fn forum_rpc_error_table() {
    // `sendMessage` documents the topic errors as 406.
    let table: &[(i32, &str, RejectReason)] = &[
        (406, "TOPIC_CLOSED", RejectReason::TopicClosed),
        (406, "TOPIC_DELETED", RejectReason::TopicDeleted),
        (400, "TOPIC_CLOSED", RejectReason::TopicClosed),
        (400, "CHANNEL_FORUM_MISSING", RejectReason::NotForum),
        // Malformed or unknown ids say nothing about deletion.
        (400, "TOPIC_ID_INVALID", RejectReason::Other),
    ];
    for &(code, name, reason) in table {
        check(
            &rpc(code, name, None),
            BackendError::Rejected(reason),
            StreamError::Transient,
        );
    }
}

/// Auth loss keeps its cause across the stream boundary, so core runs the
/// local logout instead of rebuilding.
#[test]
fn auth_loss_error_table() {
    for (code, name) in [
        (401, "AUTH_KEY_UNREGISTERED"),
        (401, "SESSION_REVOKED"),
        (401, "USER_DEACTIVATED_BAN"),
        (406, "AUTH_KEY_DUPLICATED"),
    ] {
        check(
            &rpc(code, name, None),
            BackendError::Unauthorized,
            StreamError::Unauthorized,
        );
    }
}

#[test]
fn transport_error_table() {
    let io = InvocationError::Io(std::io::Error::other("reset"));
    check(&io, BackendError::Transient, StreamError::Transient);
    let dropped = InvocationError::Dropped("runner gone");
    check(&dropped, BackendError::Transient, StreamError::Fatal);
    check(
        &InvocationError::InvalidDc,
        BackendError::Transient,
        StreamError::Fatal,
    );
}

#[test]
fn only_well_formed_error_names_are_logged() {
    assert_eq!(
        loggable_name(&rpc(400, "PEER_ID_INVALID", None)),
        "PEER_ID_INVALID"
    );
    assert_eq!(
        loggable_name(&rpc(400, "+44 7700 900123", None)),
        "<unprintable rpc error>"
    );
    assert_eq!(
        loggable_name(&rpc(400, &"A".repeat(65), None)),
        "<unprintable rpc error>"
    );
    assert_eq!(
        loggable_name(&InvocationError::Io(std::io::Error::other("secret text"))),
        "io"
    );
}
