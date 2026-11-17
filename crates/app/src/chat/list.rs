//! The chat list as the UI holds it: core's dialogs in display order (plan
//! section 7). Core owns the order; this mirrors its events.

use std::time::Instant;

use abyss_core::api::UserError;
use abyss_core::model::{ChatId, ChatSummary, Preview};
use abyss_core::store::{MAX_DIALOGS, bump_moves};
use gpui::SharedString;

use super::copy;
use super::retry::Backoff;

/// Rows before the end of the list at which the next page is requested.
const LOAD_MARGIN: usize = 20;
/// Characters of the last message shown in a row.
const PREVIEW_CHARS: usize = 100;

/// What core's `DialogsPaging` said about further pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pages {
    More,
    Done,
}

/// Dialogs pagination, driven by core's `DialogsPaging` rather than by list
/// length: a page whose chats were all filtered (broadcast channels) or
/// already listed leaves the length unchanged, yet more pages may follow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Paging {
    /// Core said more pages exist. Core loads the first page itself.
    has_more: bool,
    /// A `LoadDialogs` was admitted and core has not answered it yet.
    in_flight: bool,
    /// The sidebar needs the next page: its end is on screen, or it shows
    /// no row at all. Kept until a request is admitted, whatever renders.
    wanted: bool,
    /// Refused or failed requests: bounded, spaced retries.
    retry: Backoff,
}

#[derive(Debug, Default)]
pub(crate) struct ChatList {
    chats: Vec<ChatSummary>,
    paging: Paging,
}

/// Whether a `ChatBumped` summary `new` moved its chat in core: core's own
/// rule ([`bump_moves`]), by message id. A summary without a last id is a
/// read-state change, which never moves a chat.
fn is_newer(new: &ChatSummary, old: &ChatSummary) -> bool {
    new.last_id.is_some_and(|n| bump_moves(old.last_id, n))
}

impl ChatList {
    pub(crate) fn as_slice(&self) -> &[ChatSummary] {
        &self.chats
    }

    pub(crate) fn len(&self) -> usize {
        self.chats.len()
    }

    pub(crate) fn get(&self, id: ChatId) -> Option<&ChatSummary> {
        self.chats.iter().find(|c| c.id == id)
    }

    /// Core's whole list (it merges a reloaded first page into later pages).
    pub(crate) fn reset(&mut self, mut chats: Vec<ChatSummary>) {
        chats.truncate(MAX_DIALOGS);
        self.chats = chats;
    }

    /// A later page: listed chats update in place, new ones are appended.
    pub(crate) fn upsert_page(&mut self, chats: Vec<ChatSummary>) {
        for chat in chats {
            if let Some(slot) = self.chats.iter_mut().find(|c| c.id == chat.id) {
                *slot = chat;
            } else if self.chats.len() < MAX_DIALOGS {
                self.chats.push(chat);
            }
        }
    }

    /// A chat's summary changed. A newer last message (by `last_id`) moves
    /// it to the top of its section (pinned chats keep their place), as core
    /// does; anything else (an unread count, a read elsewhere, a repeated
    /// bump) updates it in place.
    pub(crate) fn bump(&mut self, chat: ChatSummary) {
        let Some(pos) = self.chats.iter().position(|c| c.id == chat.id) else {
            if self.chats.len() < MAX_DIALOGS {
                let at = self.section_start(&chat);
                self.chats.insert(at, chat);
            }
            return;
        };
        let moved = self.chats.get(pos).is_some_and(|old| is_newer(&chat, old));
        if !moved || chat.pinned {
            if let Some(slot) = self.chats.get_mut(pos) {
                *slot = chat;
            }
            return;
        }
        self.chats.remove(pos);
        let at = self.section_start(&chat);
        self.chats.insert(at, chat);
    }

    /// Where `chat`'s section (pinned or not) begins.
    fn section_start(&self, chat: &ChatSummary) -> usize {
        if chat.pinned {
            0
        } else {
            self.chats.iter().take_while(|c| c.pinned).count()
        }
    }

    /// Row `last_visible` is on screen: near the end, the sidebar wants the
    /// next page.
    pub(crate) fn seen(&mut self, last_visible: usize) {
        let len = self.chats.len();
        if len == 0 || last_visible.saturating_add(LOAD_MARGIN) >= len {
            self.paging.wanted = true;
        }
    }

    /// Whether to ask core for the next page at `now`: the sidebar wants it,
    /// core said more exist, none is outstanding, no retry is waiting, and
    /// the cap is not reached.
    pub(crate) fn wants_more(&self, now: Instant) -> bool {
        self.could_ask() && self.paging.retry.ready(now)
    }

    fn could_ask(&self) -> bool {
        let p = &self.paging;
        p.wanted && p.has_more && !p.in_flight && self.chats.len() < MAX_DIALOGS
    }

    /// Core admitted a `LoadDialogs`.
    pub(crate) fn requested(&mut self) {
        self.paging.in_flight = true;
        self.paging.wanted = false;
    }

    /// A fresh trigger (the user's Retry, reconnection, activation): a
    /// waiting or exhausted request may be asked again at once.
    pub(crate) fn retry_now(&mut self) {
        self.paging.retry.reset();
    }

    /// Automatic recovery (see [`Backoff::restart_if_exhausted`]).
    pub(crate) fn recover(&mut self) {
        self.paging.retry.restart_if_exhausted();
    }

    /// Whether page requests stopped retrying, wanted or not.
    pub(crate) fn exhausted(&self) -> bool {
        self.paging.retry.gave_up()
    }

    /// Whether failed requests for a page the sidebar wants stopped
    /// retrying (until [`Self::retry_now`]).
    pub(crate) fn gave_up(&self) -> bool {
        self.could_ask() && self.paging.retry.gave_up()
    }

    /// Core did not take a `LoadDialogs`: ask again after a backoff.
    pub(crate) fn refused(&mut self, now: Instant) {
        self.paging.retry.failed(now);
    }

    /// Core applied a dialogs page (`DialogsPaging`). An empty list shows no
    /// row to ask for more, so it wants the next page right away.
    pub(crate) fn paged(&mut self, pages: Pages) {
        let p = &mut self.paging;
        p.has_more = pages == Pages::More;
        p.in_flight = false;
        p.wanted |= self.chats.is_empty();
        p.retry.reset();
    }

    /// The `LoadDialogs` in flight ended without a page
    /// (`Event::LoadEnded`): nothing left to load, or it failed and waits
    /// out a backoff. Errors of other commands never end it.
    pub(crate) fn load_ended(&mut self, error: Option<UserError>, now: Instant) {
        let p = &mut self.paging;
        if !p.in_flight {
            return;
        }
        p.in_flight = false;
        if error.is_some() {
            p.wanted = true;
            p.retry.failed(now);
        } else {
            p.has_more = false;
        }
    }

    /// When a refused or failed request may be asked again, if one waits.
    pub(crate) fn retry_due(&self, now: Instant) -> Option<Instant> {
        self.could_ask()
            .then(|| self.paging.retry.due(now))
            .flatten()
    }
}

/// The row's second line: the last message, media as `[Photo]`.
pub(crate) fn preview_text(preview: Option<&Preview>) -> SharedString {
    match preview {
        Some(Preview::Text(text)) => match text.char_indices().nth(PREVIEW_CHARS) {
            Some((cut, _)) => format!("{}…", text.get(..cut).unwrap_or_default()).into(),
            None => text.clone().into(),
        },
        Some(Preview::Media(kind)) => format!("[{}]", copy::media_label(kind)).into(),
        Some(Preview::Service) => copy::SERVICE_FALLBACK.into(),
        None => SharedString::default(),
    }
}

/// Up to two initials for the avatar placeholder (no images in Stage 1).
pub(crate) fn initials(title: &str) -> SharedString {
    title
        .split_whitespace()
        .filter_map(|w| w.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .flat_map(char::to_uppercase)
        .collect::<String>()
        .into()
}

#[cfg(test)]
mod tests {
    use abyss_core::model::{ChatKind, MediaKind, MsgId, PeerKind};

    use super::*;

    fn id(n: i64) -> ChatId {
        ChatId {
            kind: PeerKind::User,
            id: n,
        }
    }

    fn chat(n: i64) -> ChatSummary {
        ChatSummary {
            id: id(n),
            title: format!("c{n}"),
            kind: ChatKind::Private,
            pinned: false,
            unread: 0,
            unread_mentions: 0,
            last: None,
            last_date: 0,
            last_id: None,
            can_send: true,
        }
    }

    fn pinned(n: i64) -> ChatSummary {
        ChatSummary {
            pinned: true,
            ..chat(n)
        }
    }

    /// A chat whose last message (id `date`) was sent at `date`.
    fn with_message(n: i64, date: i64) -> ChatSummary {
        ChatSummary {
            last: Some(Preview::Text("hi".into())),
            last_date: date,
            last_id: Some(MsgId(i32::try_from(date).unwrap())),
            ..chat(n)
        }
    }

    fn ids(list: &ChatList) -> Vec<i64> {
        list.as_slice().iter().map(|c| c.id.id).collect()
    }

    #[test]
    fn pages_merge_by_id() {
        let mut list = ChatList::default();
        list.reset(vec![pinned(1), chat(2), chat(3)]);
        let mut fresh = chat(3);
        fresh.unread = 7;
        list.upsert_page(vec![fresh, chat(4)]);
        assert_eq!(ids(&list), [1, 2, 3, 4]);
        assert_eq!(list.get(id(3)).unwrap().unread, 7);
        // A reset is core's merged list: it replaces ours wholesale.
        list.reset(vec![chat(4), chat(1)]);
        assert_eq!(ids(&list), [4, 1]);
    }

    #[test]
    fn new_message_moves_but_read_state_does_not() {
        let mut list = ChatList::default();
        list.reset(vec![pinned(1), chat(2), chat(3)]);
        list.bump(with_message(3, 10));
        assert_eq!(ids(&list), [1, 3, 2]);
        let mut read = with_message(3, 10);
        read.unread = 0;
        list.bump(chat(2)); // unchanged last message: in place
        list.bump(read);
        assert_eq!(ids(&list), [1, 3, 2]);
        list.bump(ChatSummary {
            pinned: true,
            ..with_message(1, 20)
        });
        assert_eq!(ids(&list), [1, 3, 2], "pinned chats keep their place");
        list.bump(with_message(9, 30));
        assert_eq!(
            ids(&list),
            [1, 9, 3, 2],
            "unknown chats enter their section"
        );
    }

    /// Same second, same preview: only the message id tells them apart.
    fn with_id(n: i64, msg: i32) -> ChatSummary {
        ChatSummary {
            last_id: Some(MsgId(msg)),
            ..with_message(n, 100)
        }
    }

    #[test]
    fn identical_previews_in_one_second_order_by_id() {
        let mut list = ChatList::default();
        list.reset(vec![with_id(1, 10), with_id(2, 20), with_id(3, 30)]);
        list.bump(with_id(3, 31));
        assert_eq!(ids(&list), [3, 1, 2]);
        list.bump(with_id(2, 21));
        assert_eq!(ids(&list), [2, 3, 1], "same date and preview, newer id");
        let mut stale = with_id(3, 30);
        stale.unread = 5;
        list.bump(stale);
        assert_eq!(ids(&list), [2, 3, 1], "an older id never moves a chat");
        assert_eq!(list.get(id(3)).unwrap().unread, 5, "but updates it");
        list.bump(with_id(2, 21));
        assert_eq!(ids(&list), [2, 3, 1], "a repeated bump is deduped");
    }

    #[test]
    fn pagination_follows_core_paging_not_list_length() {
        let now = Instant::now();
        let mut list = ChatList::default();
        list.seen(0);
        assert!(
            !list.wants_more(now),
            "nothing loaded yet: core loads page 1"
        );
        // A first page made only of broadcast channels: nothing listed, more
        // pages exist. No row renders, yet the list wants the next page.
        list.reset(Vec::new());
        list.paged(Pages::More);
        assert!(list.wants_more(now), "an all-filtered page still paginates");
        list.requested();
        assert!(!list.wants_more(now), "one page at a time");
        list.reset((0..100).map(chat).collect());
        list.paged(Pages::More);
        list.seen(10);
        assert!(!list.wants_more(now), "the end is not on screen");
        list.seen(85);
        assert!(list.wants_more(now));
        list.requested();
        // Core answers a "load more" with a reset when the page reorders
        // rows (4c): paging stays in flight until `DialogsPaging`.
        list.reset((0..100).rev().map(chat).collect());
        list.seen(99);
        assert!(!list.wants_more(now), "the reset alone ends nothing");
        list.paged(Pages::More);
        assert!(list.wants_more(now), "the end stayed on screen");
        list.requested();
        // An overlapping page: the length does not change.
        list.upsert_page((90..100).map(chat).collect());
        list.paged(Pages::More);
        list.seen(99);
        assert!(list.wants_more(now), "the same length asks again");
        list.requested();
        list.load_ended(Some(UserError::Network), now);
        assert!(!list.wants_more(now), "a failed page waits a moment");
        let due = list.retry_due(now).unwrap();
        assert!(list.wants_more(due), "then is asked again");
        list.requested();
        list.load_ended(None, now);
        assert!(!list.wants_more(due), "core has nothing more");
        list.paged(Pages::Done);
        list.seen(99);
        assert!(!list.wants_more(due), "every page loaded");
    }

    #[test]
    fn bounded() {
        let mut list = ChatList::default();
        let max = i64::try_from(MAX_DIALOGS).unwrap();
        list.paged(Pages::More);
        list.reset((0..max + 5).map(chat).collect());
        assert_eq!(list.len(), MAX_DIALOGS);
        list.upsert_page(vec![chat(-1)]);
        list.bump(with_message(-2, 1));
        assert_eq!(list.len(), MAX_DIALOGS);
        list.seen(MAX_DIALOGS - 1);
        assert!(!list.wants_more(Instant::now()), "never past the cap");
    }

    #[test]
    fn row_text() {
        let long = "x".repeat(150);
        assert_eq!(
            preview_text(Some(&Preview::Text(long))).chars().count(),
            101
        );
        assert_eq!(
            preview_text(Some(&Preview::Media(MediaKind::Photo))),
            "[Photo]"
        );
        assert_eq!(initials("ada lovelace"), "AL");
        assert_eq!(initials("  ✨ émile  zola bar"), "ÉZ");
        assert_eq!(initials(""), "");
    }
}
