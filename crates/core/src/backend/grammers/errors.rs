//! grammers errors to [`BackendError`] and [`StreamError`] (plan sections 2, 4
//! and 9), and what may be logged about them.
//!
//! Only the RPC error *name* and code are ever logged, never the request,
//! response or `Display` of an error (which can embed payloads).

use grammers_client::InvocationError;
use grammers_client::sender::RpcError;

use crate::backend::{BackendError, RejectReason, StreamError};

/// Flood waits without a value are treated as this many seconds.
const DEFAULT_FLOOD_SECONDS: u32 = 1;

/// What an invocation error means for a caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ErrorKind {
    /// `FLOOD_WAIT_X` and friends: retry after `seconds`.
    Flood { seconds: u32 },
    /// The connection failed; the request may or may not have run.
    Io,
    /// The request never ran and never will on this instance (runner gone).
    Dropped,
    /// An authenticated `303`: the account lives on another DC.
    Migrated,
    /// The authorization is gone (`AUTH_KEY_UNREGISTERED`, `SESSION_REVOKED`...).
    Unauthorized,
    /// The server failed internally (`5xx`); the outcome is unknown.
    Server,
    /// `RANDOM_ID_DUPLICATE`: a send with this `random_id` already went through.
    DuplicateRandomId,
    /// A definite rejection of this request.
    Rejected,
    /// Local trouble: the session store, an unknown DC, key generation.
    Local,
}

pub(crate) fn kind(e: &InvocationError) -> ErrorKind {
    match e {
        InvocationError::Rpc(rpc) => rpc_kind(rpc),
        InvocationError::Io(_) | InvocationError::Transport(_) => ErrorKind::Io,
        // A response that does not parse: whether the request ran is unknown.
        InvocationError::Deserialize(_) => ErrorKind::Server,
        InvocationError::Dropped(_) => ErrorKind::Dropped,
        InvocationError::Session(_)
        | InvocationError::InvalidDc
        | InvocationError::Authentication(_) => ErrorKind::Local,
    }
}

pub(crate) fn rpc_kind(rpc: &RpcError) -> ErrorKind {
    let name = rpc.name.as_str();
    match rpc.code {
        303 => ErrorKind::Migrated,
        401 => ErrorKind::Unauthorized,
        420 => ErrorKind::Flood {
            seconds: rpc.value.unwrap_or(DEFAULT_FLOOD_SECONDS),
        },
        406 if name == "AUTH_KEY_DUPLICATED" => ErrorKind::Unauthorized,
        code if code >= 500 || code == -500 => ErrorKind::Server,
        _ if name == "RANDOM_ID_DUPLICATE" => ErrorKind::DuplicateRandomId,
        // Some flood errors arrive as 400 (e.g. `SLOWMODE_WAIT_X`).
        _ if name.ends_with("_WAIT") && rpc.value.is_some() => ErrorKind::Flood {
            seconds: rpc.value.unwrap_or(DEFAULT_FLOOD_SECONDS),
        },
        _ => ErrorKind::Rejected,
    }
}

/// The error a [`crate::backend::Client`] call reports.
pub(crate) fn backend_error(kind: ErrorKind) -> BackendError {
    match kind {
        ErrorKind::Flood { seconds } => BackendError::FloodWait { seconds },
        ErrorKind::Migrated => BackendError::Migrated,
        ErrorKind::Unauthorized => BackendError::Unauthorized,
        ErrorKind::DuplicateRandomId => BackendError::SentUnconfirmed,
        ErrorKind::Rejected => BackendError::Rejected(RejectReason::Other),
        // Unknown outcome: core may retry (sends with the same `random_id`).
        ErrorKind::Io | ErrorKind::Dropped | ErrorKind::Server | ErrorKind::Local => {
            BackendError::Transient
        }
    }
}

/// How the update stream's error affects the instance: transient errors are
/// retried on the same stream; fatal ones rebuild it; auth loss and migration
/// keep their cause so core runs the local logout or the migration policy.
pub(crate) fn stream_error(kind: ErrorKind) -> StreamError {
    match kind {
        ErrorKind::Flood { .. } | ErrorKind::Io | ErrorKind::Server | ErrorKind::Rejected => {
            StreamError::Transient
        }
        ErrorKind::Migrated => StreamError::Migrated,
        ErrorKind::Unauthorized => StreamError::Unauthorized,
        ErrorKind::Dropped | ErrorKind::DuplicateRandomId | ErrorKind::Local => StreamError::Fatal,
    }
}

/// An RPC error name as it may appear in our logs: Telegram's names are
/// upper-case ASCII, so anything else is not echoed.
pub(crate) fn loggable_name(e: &InvocationError) -> &str {
    match e {
        InvocationError::Rpc(rpc)
            if rpc.name.len() <= 64
                && rpc
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') =>
        {
            &rpc.name
        }
        InvocationError::Rpc(_) => "<unprintable rpc error>",
        InvocationError::Io(_) => "io",
        InvocationError::Transport(_) => "transport",
        InvocationError::Deserialize(_) => "deserialize",
        InvocationError::Dropped(_) => "dropped",
        InvocationError::Session(_) => "session",
        InvocationError::InvalidDc => "invalid dc",
        InvocationError::Authentication(_) => "authentication",
    }
}

pub(crate) fn rpc_code(e: &InvocationError) -> i32 {
    match e {
        InvocationError::Rpc(rpc) => rpc.code,
        _ => 0,
    }
}
