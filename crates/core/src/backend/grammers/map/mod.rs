//! Pure mapping from grammers' TL types to the domain model (plan section 6).
//!
//! Nothing here touches the network, the session or a `Client`, so every
//! function is tested with hand-built `tl::types` values. Server strings are
//! sanitized (`sanitize`), formatting goes through
//! [`RichText::from_entities`](crate::rich_text::RichText::from_entities), and
//! every server id is range-checked before use.

pub(crate) mod folders;
pub(crate) mod lists;
pub(crate) mod message;
pub(crate) mod peers;
pub(crate) mod topics;

pub(crate) use lists::{
    Oversized, dialogs, dialogs_parts, history, messages_parts, sent_id, update,
};
pub(crate) use peers::{Directory, MAX_DIRECTORY, input_channel, input_peer, peer_id};
