//! User-facing strings for the chat list and message view. Kept apart from
//! `ui::copy` (auth and shutdown) so the two can change independently.

use std::time::Instant;

use abyss_core::api::{ConnState, SendState};
use abyss_core::model::{ChatKind, MediaKind};
use gpui::SharedString;

use super::sends::{Echo, Phase};

/// The wordmark above the chat list: lowercase, no article.
pub(crate) const WORDMARK: &str = "abyss";
pub(crate) const SELECT_CHAT: &str = "Select a chat";
pub(crate) const NO_CHATS: &str = "No chats yet";
pub(crate) const NO_ARCHIVED: &str = "No archived chats";
pub(crate) const ARCHIVE: &str = "Archive";
/// The archive view's title, which leads back to the main list.
pub(crate) const ARCHIVE_BACK: &str = "‹ Archive";
pub(crate) const LOADING_HISTORY: &str = "Loading…";
pub(crate) const START_OF_HISTORY: &str = "Start of the conversation";
pub(crate) const EMPTY_CHAT: &str = "No messages yet";
pub(crate) const EDITED: &str = "edited";
pub(crate) const UNSUPPORTED: &str = "This message type isn't supported yet.";
pub(crate) const SERVICE_FALLBACK: &str = "Service message";
pub(crate) const READ_ONLY: &str = "Only admins can post in this group.";
pub(crate) const JUMP_TO_LATEST: &str = "Jump to latest";
pub(crate) const RETRY: &str = "Retry";
/// Shown once automatic retries of history loads gave up.
pub(crate) const RETRY_HISTORY: &str = "Couldn't load messages · Retry";
/// Shown once automatic retries of chat list pages gave up.
pub(crate) const RETRY_CHATS: &str = "Couldn't load more chats · Retry";
pub(crate) const DISCARD: &str = "Discard";
/// A send core refused and holds nothing for (task 6j).
pub(crate) const RETURNED: &str = "Not sent and not kept";
pub(crate) const PUT_BACK: &str = "Put back in the box";
/// Core stopped before confirming a send (task 6k): it may have been sent.
pub(crate) const STRANDED: &str = "Unknown: abyss stopped before confirming. It may have been sent, so check before sending it again.";
/// Core stopped with this send failed: it was not sent.
pub(crate) const STRANDED_UNSENT: &str = "Not sent: abyss stopped before it could be retried.";
pub(crate) const COPY_TEXT: &str = "Copy text";
/// A chat switch refused because a draft would need a slot past the bound.
pub(crate) const TOO_MANY_DRAFTS: &str = "Too many unsent drafts: send or clear one first.";
/// Enter while core is stopped.
pub(crate) const CORE_STOPPED_COPY: &str = "abyss stopped: copy anything you need, then restart.";
/// The recovery view's banner after a core stop behind "Logging out"
/// (task 6m): the marker write may or may not have finished.
pub(crate) const STOPPED_SIGNING_OUT: &str = "abyss stopped while signing out. Sign-out may not have finished. Copy anything you need, then restart.";
/// The same behind "Couldn't start logout".
pub(crate) const STOPPED_SIGN_OUT_NOT_STARTED: &str =
    "abyss stopped. Sign-out did not start. Copy anything you need, then restart.";
/// A core stop with no account state kept on screen.
pub(crate) const STOPPED_RESTART: &str = "abyss stopped. Restart it to continue.";
/// A chat opened after a core stop whose history was never loaded.
pub(crate) const HISTORY_UNAVAILABLE: &str = "History unavailable: abyss stopped.";
pub(crate) const TEXT_COPIED: &str = "Text copied.";
/// Put back from a chat other than the send's.
pub(crate) const OTHER_CHAT: &str = "Open the chat this message was for to put it back.";
pub(crate) const DISMISS: &str = "Dismiss";
/// Put back while the message box has text.
pub(crate) const BOX_NOT_EMPTY: &str =
    "Clear the message box first: the text goes back into an empty box.";
/// Shown once text is back in the message box.
pub(crate) const PUT_BACK_DONE: &str = "Not sent and not kept. Your text is back in the box.";
pub(crate) const LINK_COPIED: &str = "Link copied. abyss never opens links from messages.";
pub(crate) const FORUM_TOPICS: &str = "Topics arrive in a later version.";

pub(crate) fn conn_label(conn: Option<ConnState>) -> SharedString {
    match conn {
        None | Some(ConnState::Connecting) => "Connecting…".into(),
        Some(ConnState::Online) => "Online".into(),
        Some(ConnState::Rebuilding { attempt }) => {
            format!("Reconnecting (attempt {attempt})…").into()
        }
        Some(ConnState::Offline) => "Offline. Retrying…".into(),
    }
}

/// The kind tag next to a chat title; private chats have none.
pub(crate) fn kind_label(kind: ChatKind) -> Option<&'static str> {
    match kind {
        ChatKind::Private => None,
        ChatKind::Bot => Some("bot"),
        ChatKind::Group
        | ChatKind::Supergroup {
            forum: false,
            gigagroup: false,
        } => Some("group"),
        ChatKind::Supergroup {
            gigagroup: true, ..
        } => Some("broadcast group"),
        ChatKind::Supergroup { forum: true, .. } => Some("forum"),
    }
}

/// `[Photo]`, `[Document: report.pdf]`, ... for previews and media chips.
pub(crate) fn media_label(kind: &MediaKind) -> String {
    let name = match kind {
        MediaKind::Photo => "Photo",
        MediaKind::Video => "Video",
        MediaKind::Voice => "Voice message",
        MediaKind::Audio => "Audio",
        MediaKind::Document { name, size } => {
            return format!("Document: {name} ({})", byte_size(*size));
        }
        MediaKind::Sticker => "Sticker",
        MediaKind::Gif => "GIF",
        MediaKind::Poll => "Poll",
        MediaKind::Location => "Location",
        MediaKind::Contact => "Contact",
        MediaKind::Other => "Attachment",
    };
    name.to_owned()
}

/// `512 B`, `1.5 KB`, `12.0 MB`, `2.3 GB`.
pub(crate) fn byte_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut unit = 0;
    let mut tenths = bytes.saturating_mul(10);
    while tenths >= 10_240 && unit + 1 < UNITS.len() {
        tenths /= 1024;
        unit += 1;
    }
    let name = UNITS.get(unit).copied().unwrap_or("B");
    if unit == 0 {
        format!("{bytes} {name}")
    } else {
        format!("{}.{} {name}", tenths / 10, tenths % 10)
    }
}

/// The status line under an outgoing message that has no server copy yet.
/// A flood-waiting send counts down to its resend.
pub(crate) fn send_label(echo: &Echo, now: Instant) -> SharedString {
    match echo.phase {
        Phase::Returned => return RETURNED.into(),
        Phase::Stranded if echo.state == SendState::Failed => return STRANDED_UNSENT.into(),
        Phase::Stranded => return STRANDED.into(),
        Phase::Discarding => return "Discarding…".into(),
        Phase::Submitted | Phase::Kept => {}
    }
    match echo.state {
        SendState::FloodWait { .. } => match echo.wait_left(now) {
            Some(0) => "Resending…".into(),
            Some(left) => format!("Waiting {left} s (Telegram rate limit)").into(),
            None => "Waiting (Telegram rate limit)".into(),
        },
        state => state_label(state),
    }
}

fn state_label(state: SendState) -> SharedString {
    match state {
        SendState::Sending => "Sending…".into(),
        SendState::Sent { .. } => "Sent".into(),
        SendState::SentUnconfirmed => "Sent, unconfirmed".into(),
        SendState::Unknown { attempts } => {
            format!("Not sure it was sent ({attempts} attempts)").into()
        }
        SendState::Failed => "Not sent".into(),
        SendState::FloodWait { seconds } => {
            format!("Waiting {seconds} s (Telegram rate limit)").into()
        }
    }
}

#[cfg(test)]
mod send_label_tests {
    use std::time::Duration;

    use abyss_core::model::{ChatId, LocalId, PeerKind, ThreadKey};

    use super::*;
    use crate::chat::sends::Echoes;

    #[test]
    fn flood_wait_counts_down_to_the_resend() {
        let chat = ChatId {
            kind: PeerKind::User,
            id: 1,
        };
        let mut echoes = Echoes::default();
        echoes
            .push(Echo::new(LocalId(1), ThreadKey::whole(chat), "x".into(), 0))
            .unwrap();
        echoes.admitted(LocalId(1));
        let now = Instant::now();
        echoes.set_state(LocalId(1), SendState::FloodWait { seconds: 30 }, now);
        let e = echoes.get(LocalId(1)).unwrap();
        assert_eq!(send_label(e, now), "Waiting 30 s (Telegram rate limit)");
        let later = now + Duration::from_secs(12);
        assert_eq!(send_label(e, later), "Waiting 18 s (Telegram rate limit)");
        let done = now + Duration::from_secs(31);
        assert_eq!(send_label(e, done), "Resending…");
        assert!(e.can_discard() && !e.can_retry());
    }
}

/// The archive entry's second line: how many archived chats are loaded (the
/// list may hold only some of them), how many of those are unread.
pub(crate) fn archive_summary(chats: usize, unread: usize) -> SharedString {
    let noun = if chats == 1 { "chat" } else { "chats" };
    match unread {
        0 => format!("{chats} {noun} loaded").into(),
        n => format!("{chats} {noun} loaded · {n} unread").into(),
    }
}

pub(crate) fn unread_badge(unread: u32) -> SharedString {
    if unread > 999 {
        "999+".into()
    } else {
        unread.to_string().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(byte_size(0), "0 B");
        assert_eq!(byte_size(1023), "1023 B");
        assert_eq!(byte_size(1536), "1.5 KB");
        assert_eq!(byte_size(12 * 1024 * 1024), "12.0 MB");
        assert!(
            byte_size(u64::MAX).ends_with(" GB"),
            "saturates, no overflow"
        );
    }

    #[test]
    fn archive_counts_say_loaded() {
        assert_eq!(archive_summary(1, 0), "1 chat loaded");
        assert_eq!(archive_summary(12, 3), "12 chats loaded · 3 unread");
    }
}
