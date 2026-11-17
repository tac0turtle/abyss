//! The chat list as the UI holds it: core's dialogs in display order (plan
//! section 7). Core owns the order; this mirrors its events.

use std::time::Instant;

use abyss_core::api::{Event, UserError};
use abyss_core::model::{ChatFolder, ChatId, ChatKind, ChatSummary, PeerFolder, Preview, TopicId};
use abyss_core::store::{MAX_DIALOGS, bump_moves};
use gpui::SharedString;

use super::copy;
use super::retry::Backoff;

/// Rows before the end of the list at which the next page is requested.
const LOAD_MARGIN: usize = 20;
/// Characters of the last message shown in a row.
const PREVIEW_CHARS: usize = 100;
/// Seconds after which a folder view rechecks its chats' mutes against the
/// clock (plan section 17.7): an expired mute stops counting within a
/// minute of the next redraw.
const MUTE_RECHECK: i64 = 60;

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

/// A tab of the sidebar's strip (plan section 17.2): a built-in rule over
/// the main peer folder, or a chat folder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Tab {
    /// Every chat of the main peer folder, led by the archive's entry.
    #[default]
    All,
    /// One-to-one chats: people and bots (D9).
    Private,
    /// Basic groups and supergroups that aren't forums.
    Groups,
    Forums,
    /// A chat folder, by id: its chats from both peer folders.
    Folder(i32),
}

impl Tab {
    /// The built-in tabs, in the strip's order; the folders follow them.
    pub(crate) const BUILT_IN: [Self; 4] = [Self::All, Self::Private, Self::Groups, Self::Forums];

    /// The one built-in tab besides All whose rule takes `chat` in (D9).
    /// Every chat has exactly one, so Private, Groups and Forums partition
    /// All.
    pub(crate) fn of_kind(chat: &ChatSummary) -> Self {
        match chat.kind {
            ChatKind::Private { .. } | ChatKind::Bot => Self::Private,
            ChatKind::Group | ChatKind::Supergroup { forum: false, .. } => Self::Groups,
            ChatKind::Supergroup { forum: true, .. } => Self::Forums,
        }
    }
}

/// Which view of the chat list the sidebar shows. Core keeps one list
/// holding both of Telegram's peer folders, in server order; every view is
/// a filter over it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shown {
    Tab(Tab),
    /// The archived chats; "‹ Archive" leads back to All.
    Archive,
}

impl Default for Shown {
    fn default() -> Self {
        Self::Tab(Tab::All)
    }
}

/// The sidebar's view: what it shows, and whether only unread rows show
/// (the filter of plan section 17.4, applied from task 15).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct View {
    pub(crate) shown: Shown,
    pub(crate) unread_only: bool,
}

/// Which way Ctrl+Tab moves along the strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Next,
    Previous,
}

/// The main list's entry for the archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ArchiveEntry {
    /// Archived chats loaded so far.
    pub(crate) chats: usize,
    /// How many of them have unread messages.
    pub(crate) unread: usize,
}

/// One row of the sidebar (plan section 17.3): an owned descriptor, kept
/// in the row cache and resolved against the chat list and the shown
/// forum's topics when it renders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SidebarRow {
    Archive(ArchiveEntry),
    Chat(ChatId),
    /// A topic of the pane's forum, under the forum's row.
    Topic {
        forum: ChatId,
        topic: TopicId,
    },
    /// The pane's forum holds more topics than its rows show.
    MoreTopics {
        forum: ChatId,
    },
    /// The pane's forum has no topic copy yet.
    TopicsPending {
        forum: ChatId,
    },
}

impl SidebarRow {
    /// Whether the user selects something with it: a chat or a topic, not
    /// the archive's entry, More or a placeholder.
    pub(crate) fn selectable(self) -> bool {
        matches!(self, Self::Chat(_) | Self::Topic { .. })
    }
}

/// Rows the chat shell places right after a forum's row: its inline topics
/// (plan section 17.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Beneath {
    pub(crate) forum: ChatId,
    pub(crate) rows: Vec<SidebarRow>,
}

#[derive(Debug, Default)]
pub(crate) struct ChatList {
    chats: Vec<ChatSummary>,
    paging: Paging,
    view: View,
    /// A folder tab restored from the sidebar file, shown once the
    /// account's folders arrive if it is still among them.
    restoring: Option<i32>,
    /// The account's chat folders, in server order.
    folders: Vec<ChatFolder>,
    /// The shown view's rows without inline topics, recomputed when the
    /// list, the folders or the view change.
    base: Vec<SidebarRow>,
    /// When `base` was computed (unix seconds): folder mutes are checked
    /// against it.
    computed: i64,
    /// The rows under the pane's forum, from the chat shell.
    beneath: Option<Beneath>,
    /// The sidebar's rows: `base` with `beneath` after its forum's row, if
    /// the view shows that row.
    rows: Vec<SidebarRow>,
}

/// Whether a `ChatBumped` summary `new` moved its chat in core: core's own
/// rule ([`bump_moves`]), by message id. A summary without a last id is a
/// read-state change, which never moves a chat.
fn is_newer(new: &ChatSummary, old: &ChatSummary) -> bool {
    new.last_id.is_some_and(|n| bump_moves(old.last_id, n))
}

impl ChatList {
    #[cfg(test)]
    pub(crate) fn as_slice(&self) -> &[ChatSummary] {
        &self.chats
    }

    pub(crate) fn len(&self) -> usize {
        self.chats.len()
    }

    pub(crate) fn get(&self, id: ChatId) -> Option<&ChatSummary> {
        self.chats.iter().find(|c| c.id == id)
    }

    pub(crate) fn shown(&self) -> Shown {
        self.view.shown
    }

    pub(crate) fn view(&self) -> View {
        self.view
    }

    /// The view as the sidebar file keeps it: never the archive, which
    /// leads back to All, and a folder tab still being restored is still
    /// the choice.
    pub(crate) fn kept_view(&self) -> View {
        let shown = match (self.restoring, self.view.shown) {
            (Some(folder), _) => Shown::Tab(Tab::Folder(folder)),
            (None, Shown::Archive) => Shown::default(),
            (None, shown) => shown,
        };
        View { shown, ..self.view }
    }

    /// The view the sidebar file kept, before anything is listed (core
    /// loads the first page itself). A folder tab waits for the account's
    /// folders ([`Self::set_folders`]); All shows meanwhile.
    pub(crate) fn restore(&mut self, view: View) {
        self.view.unread_only = view.unread_only;
        match view.shown {
            Shown::Tab(Tab::Folder(folder)) => self.restoring = Some(folder),
            shown => self.view.shown = shown,
        }
        self.recompute();
    }

    /// Switches the sidebar to another view. The user's choice replaces a
    /// folder tab still being restored. A view with no row renders nothing
    /// that could ask for more, so it wants the next page right away (its
    /// chats may lie further down).
    pub(crate) fn show(&mut self, shown: Shown) {
        self.restoring = None;
        self.view.shown = shown;
        self.recompute();
        self.paging.wanted |= self.row_count() == 0;
    }

    /// The strip's tabs: the built-in ones, then the folders in server
    /// order.
    pub(crate) fn tabs(&self) -> impl Iterator<Item = Tab> + '_ {
        let folders = self.folders.iter().map(|f| Tab::Folder(f.id));
        Tab::BUILT_IN.into_iter().chain(folders)
    }

    /// Moves to the next or previous tab of the strip, wrapping. Nothing
    /// changes in the archive, which has no tabs. Returns whether the view
    /// changed.
    pub(crate) fn step_tab(&mut self, step: Step) -> bool {
        let Shown::Tab(current) = self.view.shown else {
            return false;
        };
        let tabs: Vec<Tab> = self.tabs().collect();
        let at = tabs.iter().position(|&t| t == current).unwrap_or(0);
        let next = match step {
            Step::Next => (at + 1) % tabs.len(),
            Step::Previous => (at + tabs.len() - 1) % tabs.len(),
        };
        let Some(&tab) = tabs.get(next) else {
            return false;
        };
        self.show(Shown::Tab(tab));
        true
    }

    pub(crate) fn folders(&self) -> &[ChatFolder] {
        &self.folders
    }

    /// Core's chat folders. A folder tab being restored is shown if the
    /// folder exists, and dropped either way. A shown folder that is gone
    /// falls back to All.
    pub(crate) fn set_folders(&mut self, folders: Vec<ChatFolder>) {
        self.folders = folders;
        let exists = |id| self.folders.iter().any(|f| f.id == id);
        let mut shown = self.view.shown;
        if let Some(folder) = self.restoring.take()
            && exists(folder)
        {
            shown = Shown::Tab(Tab::Folder(folder));
        }
        if let Shown::Tab(Tab::Folder(id)) = shown
            && !exists(id)
        {
            shown = Shown::default();
        }
        if shown == self.view.shown {
            // The shown folder's rules may have changed.
            self.recompute();
        } else {
            self.show(shown);
        }
    }

    /// The archive's entry in the main list, once an archived chat is loaded.
    fn archive_entry(&self) -> Option<ArchiveEntry> {
        let (chats, unread) = self
            .chats
            .iter()
            .filter(|c| c.folder == PeerFolder::Archive)
            .fold((0, 0), |(n, u), c| (n + 1, u + usize::from(c.unread > 0)));
        (chats > 0).then_some(ArchiveEntry { chats, unread })
    }

    /// The chats of the shown view: a built-in tab's or the archive's in
    /// core's order, or a chat folder's matching chats with its pinned ones
    /// first, in its order. Mutes are checked against `now`.
    fn shown_chats(&self, now: i64) -> Vec<&ChatSummary> {
        let main = |c: &&ChatSummary| c.folder == PeerFolder::Main;
        match self.view.shown {
            Shown::Archive => self
                .chats
                .iter()
                .filter(|c| c.folder == PeerFolder::Archive)
                .collect(),
            Shown::Tab(Tab::All) => self.chats.iter().filter(main).collect(),
            Shown::Tab(Tab::Folder(id)) => {
                let Some(folder) = self.folders.iter().find(|f| f.id == id) else {
                    return Vec::new();
                };
                let pinned = folder.pinned.iter().filter_map(|&p| self.get(p));
                let rest = self
                    .chats
                    .iter()
                    .filter(|c| !folder.pinned.contains(&c.id) && folder.contains(c, now));
                pinned.chain(rest).collect()
            }
            Shown::Tab(tab) => self
                .chats
                .iter()
                .filter(main)
                .filter(|c| Tab::of_kind(c) == tab)
                .collect(),
        }
    }

    /// Whether the shown view is led by the archive's entry: All only.
    fn leads_with_archive(&self) -> bool {
        self.view.shown == Shown::Tab(Tab::All)
    }

    /// The sidebar's rows: All starts with the archive's entry, and the
    /// pane's forum is followed by its inline topics.
    pub(crate) fn rows(&self) -> &[SidebarRow] {
        &self.rows
    }

    pub(crate) fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Recomputes the shown view's rows (plan section 17.7): called by
    /// every change of the list, the folders or the view.
    fn recompute(&mut self) {
        let now = super::format::now();
        let entry = self
            .leads_with_archive()
            .then(|| self.archive_entry())
            .flatten();
        let chats = self.shown_chats(now).into_iter().map(|c| c.id);
        self.base = entry
            .map(SidebarRow::Archive)
            .into_iter()
            .chain(chats.map(SidebarRow::Chat))
            .collect();
        self.computed = now;
        self.place_beneath();
    }

    /// Places the rows beneath their forum's row, if the view shows it:
    /// they never add a chat to a view.
    fn place_beneath(&mut self) {
        let placed = self.beneath.as_ref().and_then(|beneath| {
            let at = self
                .base
                .iter()
                .position(|r| *r == SidebarRow::Chat(beneath.forum))?;
            let (head, tail) = self.base.split_at(at + 1);
            Some([head, &beneath.rows, tail].concat())
        });
        self.rows = placed.unwrap_or_else(|| self.base.clone());
    }

    /// The rows to show under a forum's row (none: no forum is expanded).
    pub(crate) fn set_beneath(&mut self, beneath: Option<Beneath>) {
        if self.beneath != beneath {
            self.beneath = beneath;
            self.place_beneath();
        }
    }

    /// A redraw at `now`: a folder view rechecks its chats' mutes, at most
    /// once every [`MUTE_RECHECK`] seconds.
    pub(crate) fn recheck(&mut self, now: i64) {
        let folder = matches!(self.view.shown, Shown::Tab(Tab::Folder(_)));
        if folder && now.saturating_sub(self.computed) >= MUTE_RECHECK {
            self.recompute();
        }
    }

    /// Applies a chat-list event from core; other events are not its own.
    pub(crate) fn apply(&mut self, event: Event) {
        match event {
            Event::DialogsReset(chats) => self.reset(chats),
            Event::DialogsAppended(chats) => self.upsert_page(chats),
            Event::DialogsPaging { has_more } => {
                self.paged(if has_more { Pages::More } else { Pages::Done });
            }
            Event::ChatBumped(chat) => self.bump(chat),
            Event::ChatFolders(folders) => self.set_folders(folders),
            _ => {}
        }
    }

    /// Core's whole list (it merges a reloaded first page into later pages).
    pub(crate) fn reset(&mut self, mut chats: Vec<ChatSummary>) {
        chats.truncate(MAX_DIALOGS);
        self.chats = chats;
        self.recompute();
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
        self.recompute();
    }

    /// A chat's summary changed. A newer last message (by `last_id`) moves
    /// it to the top of its section (pinned chats keep their place), as core
    /// does; anything else (an unread count, a read elsewhere, a repeated
    /// bump) updates it in place.
    pub(crate) fn bump(&mut self, chat: ChatSummary) {
        self.place(chat);
        self.recompute();
    }

    fn place(&mut self, chat: ChatSummary) {
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

    /// Row `last_visible` of the shown view is on screen: near the view's
    /// end, the sidebar wants the next page (which may hold more of either
    /// view: the server lists both folders in one traversal).
    pub(crate) fn seen(&mut self, last_visible: usize) {
        let len = self.row_count();
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
        let empty = self.row_count() == 0;
        let p = &mut self.paging;
        p.has_more = pages == Pages::More;
        p.in_flight = false;
        p.wanted |= empty;
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
    use abyss_core::model::{ChatKind, FolderRules, MediaKind, MsgId, PeerKind};

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
            kind: ChatKind::Private { contact: false },
            pinned: false,
            unread: 0,
            unread_mentions: 0,
            last: None,
            last_date: 0,
            last_id: None,
            can_send: true,
            muted_until: 0,
            folder: PeerFolder::Main,
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

    fn archived(n: i64) -> ChatSummary {
        ChatSummary {
            folder: PeerFolder::Archive,
            ..chat(n)
        }
    }

    fn row_ids(list: &ChatList) -> Vec<i64> {
        list.rows()
            .iter()
            .map(|r| match r {
                SidebarRow::Chat(c) => c.id,
                _ => 0,
            })
            .collect()
    }

    #[test]
    fn the_archive_is_a_view_of_the_one_list() {
        let mut list = ChatList::default();
        list.reset(vec![pinned(1), chat(2), chat(4)]);
        assert_eq!(row_ids(&list), [1, 2, 4], "no archive loaded, no entry");
        list.reset(vec![pinned(1), chat(2), archived(3), chat(4), archived(5)]);
        assert_eq!(
            row_ids(&list),
            [0, 1, 2, 4],
            "the entry leads the main list"
        );
        assert_eq!(
            list.rows()[0],
            SidebarRow::Archive(ArchiveEntry {
                chats: 2,
                unread: 0
            })
        );
        list.show(Shown::Archive);
        assert_eq!(row_ids(&list), [3, 5], "server order, no entry");
        // Unarchived elsewhere: the chat changes view in place.
        list.bump(chat(3));
        assert_eq!(row_ids(&list), [5]);
        list.show(Shown::default());
        assert_eq!(row_ids(&list), [0, 1, 2, 3, 4]);
        let mut unread = archived(5);
        unread.unread = 2;
        list.bump(unread);
        assert!(matches!(
            list.rows()[0],
            SidebarRow::Archive(ArchiveEntry {
                chats: 1,
                unread: 1
            })
        ));
    }

    fn folder(id: i32, rules: FolderRules) -> ChatFolder {
        ChatFolder {
            id,
            title: format!("f{id}"),
            rules,
            pinned: Vec::new(),
            include: Vec::new(),
            exclude: Vec::new(),
            shared: false,
        }
    }

    #[test]
    fn a_folder_shows_its_chats_from_both_peer_folders() {
        let mut list = ChatList::default();
        let group = |n| ChatSummary {
            kind: ChatKind::Group,
            ..chat(n)
        };
        let muted = |n| ChatSummary {
            muted_until: i64::from(i32::MAX),
            ..group(n)
        };
        list.reset(vec![chat(1), group(2), archived(3), muted(4), group(5)]);
        let mut groups = folder(7, FolderRules::GROUPS.with(FolderRules::EXCLUDE_MUTED));
        groups.pinned = vec![id(5)];
        groups.include = vec![id(3)];
        groups.exclude = vec![id(2)];
        list.set_folders(vec![groups]);
        list.show(Shown::Tab(Tab::Folder(7)));
        assert_eq!(
            row_ids(&list),
            [5, 3],
            "pinned first, then included (archived too); excluded and muted out"
        );
        // The folder is deleted elsewhere: back to the main list.
        list.set_folders(Vec::new());
        assert_eq!(list.shown(), Shown::Tab(Tab::All));
    }

    #[test]
    fn folder_rules_follow_telegram() {
        let contact = ChatSummary {
            kind: ChatKind::Private { contact: true },
            ..chat(1)
        };
        let stranger = chat(2);
        let bot = ChatSummary {
            kind: ChatKind::Bot,
            ..chat(3)
        };
        let read = FolderRules::CONTACTS.with(FolderRules::EXCLUDE_READ);
        let f = folder(1, FolderRules::CONTACTS.with(FolderRules::BOTS));
        assert!(f.contains(&contact, 0) && f.contains(&bot, 0) && !f.contains(&stranger, 0));
        let unread = ChatSummary {
            unread: 1,
            ..contact.clone()
        };
        assert!(!folder(1, read).contains(&contact, 0) && folder(1, read).contains(&unread, 0));
        let archive = FolderRules::NON_CONTACTS.with(FolderRules::EXCLUDE_ARCHIVED);
        assert!(!folder(1, archive).contains(&archived(2), 0));
        let mut both = folder(1, FolderRules::NON_CONTACTS);
        both.include = vec![id(2)];
        both.exclude = vec![id(2)];
        assert!(!both.contains(&stranger, 0), "an exclusion wins");
    }

    /// "Exclude muted" keeps a muted chat with an unread mention, unless it
    /// is archived (Telegram Desktop's `ChatFilter::contains`); a mute that
    /// has expired no longer counts.
    #[test]
    fn muted_chats_with_mentions_and_expired_mutes() {
        let f = folder(
            1,
            FolderRules::NON_CONTACTS.with(FolderRules::EXCLUDE_MUTED),
        );
        let muted = ChatSummary {
            muted_until: 1_000,
            ..chat(1)
        };
        let mentioned = ChatSummary {
            unread_mentions: 1,
            unread: 1,
            ..muted.clone()
        };
        let archived_mention = ChatSummary {
            folder: PeerFolder::Archive,
            ..mentioned.clone()
        };
        assert!(!f.contains(&muted, 500), "muted, no mention: out");
        assert!(f.contains(&mentioned, 500), "muted with a mention: in");
        assert!(
            !f.contains(&archived_mention, 500),
            "archived: no exception"
        );
        assert!(f.contains(&muted, 1_000), "the mute has expired");
    }

    /// Forums are supergroups, so a "groups" folder takes them in, as
    /// Telegram Desktop does (megagroups count as groups), archived or
    /// not. Broadcast channels never reach the list, so a folder naming one
    /// shows nothing for it.
    #[test]
    fn forums_are_groups_and_broadcasts_never_show() {
        let forum = |n| ChatSummary {
            id: ChatId {
                kind: PeerKind::Channel,
                id: n,
            },
            kind: ChatKind::Supergroup {
                forum: true,
                gigagroup: false,
            },
            ..chat(n)
        };
        let groups = folder(7, FolderRules::GROUPS);
        assert!(groups.contains(&forum(1), 0));
        let archived_forum = ChatSummary {
            folder: PeerFolder::Archive,
            ..forum(2)
        };
        assert!(groups.contains(&archived_forum, 0));
        let mut list = ChatList::default();
        list.reset(vec![forum(1), archived_forum, chat(3)]);
        let broadcast = ChatId {
            kind: PeerKind::Channel,
            id: 50,
        };
        let mut named = folder(8, FolderRules::default());
        named.pinned = vec![broadcast];
        named.include = vec![broadcast, id(3)];
        list.set_folders(vec![groups, named]);
        list.show(Shown::Tab(Tab::Folder(7)));
        assert_eq!(row_ids(&list), [1, 2]);
        list.show(Shown::Tab(Tab::Folder(8)));
        assert_eq!(row_ids(&list), [3], "the unlisted broadcast is not shown");
    }

    fn kind_of(n: u8) -> ChatKind {
        match n % 6 {
            0 => ChatKind::Private { contact: true },
            1 => ChatKind::Private { contact: false },
            2 => ChatKind::Bot,
            3 => ChatKind::Group,
            forum => ChatKind::Supergroup {
                forum: forum == 5,
                gigagroup: n >= 6,
            },
        }
    }

    proptest::proptest! {
        /// D9: Private, Groups and Forums split All's chats exactly, each
        /// in core's order, and leave the archive out.
        #[test]
        fn built_in_tabs_partition_all(
            rows in proptest::collection::vec((0u8..12, proptest::bool::ANY, proptest::bool::ANY), 0..60),
        ) {
            let mut list = ChatList::default();
            list.reset(
                rows.iter()
                    .zip(0..)
                    .map(|(&(kind, archived, pinned), n)| ChatSummary {
                        kind: kind_of(kind),
                        pinned,
                        folder: if archived { PeerFolder::Archive } else { PeerFolder::Main },
                        ..chat(n)
                    })
                    .collect(),
            );
            let chats = |list: &ChatList| -> Vec<i64> {
                list.rows()
                    .iter()
                    .filter_map(|r| match r {
                        SidebarRow::Chat(c) => Some(c.id),
                        _ => None,
                    })
                    .collect()
            };
            let all = chats(&list);
            let main: Vec<i64> = list
                .as_slice()
                .iter()
                .filter(|c| c.folder == PeerFolder::Main)
                .map(|c| c.id.id)
                .collect();
            proptest::prop_assert_eq!(&all, &main);
            let mut seen = Vec::new();
            for tab in [Tab::Private, Tab::Groups, Tab::Forums] {
                list.show(Shown::Tab(tab));
                let shown = chats(&list);
                let in_order: Vec<i64> = all
                    .iter()
                    .copied()
                    .filter(|id| shown.contains(id))
                    .collect();
                proptest::prop_assert_eq!(&shown, &in_order, "core's order");
                proptest::prop_assert!(list.rows().iter().all(|r| matches!(r, SidebarRow::Chat(_))));
                seen.extend(shown);
            }
            seen.sort_unstable();
            let mut all = all;
            all.sort_unstable();
            proptest::prop_assert_eq!(seen, all, "each chat in exactly one tab");
        }
    }

    #[test]
    fn the_strip_lists_built_in_tabs_then_folders_and_wraps() {
        let mut list = ChatList::default();
        list.set_folders(vec![
            folder(9, FolderRules::GROUPS),
            folder(3, FolderRules::BOTS),
        ]);
        let strip = [
            Tab::All,
            Tab::Private,
            Tab::Groups,
            Tab::Forums,
            Tab::Folder(9),
            Tab::Folder(3),
        ];
        assert!(list.tabs().eq(strip), "server order after the built-ins");
        for &tab in strip.iter().skip(1).chain([&Tab::All]) {
            assert!(list.step_tab(Step::Next));
            assert_eq!(list.shown(), Shown::Tab(tab));
        }
        assert!(list.step_tab(Step::Previous));
        assert_eq!(list.shown(), Shown::Tab(Tab::Folder(3)), "wraps back");
        list.show(Shown::Archive);
        assert!(!list.step_tab(Step::Next), "the archive has no tabs");
        assert_eq!(list.shown(), Shown::Archive);
        assert_eq!(
            list.kept_view(),
            View::default(),
            "the archive is never kept"
        );
    }

    #[test]
    fn a_kept_folder_tab_waits_for_the_folders() {
        let kept = |folder| View {
            shown: Shown::Tab(Tab::Folder(folder)),
            unread_only: true,
        };
        let mut list = ChatList::default();
        list.restore(kept(4));
        assert_eq!(list.shown(), Shown::Tab(Tab::All), "All until folders load");
        assert_eq!(list.kept_view(), kept(4), "still the choice");
        list.set_folders(vec![folder(4, FolderRules::GROUPS)]);
        assert_eq!(list.view(), kept(4));
        // Restored once: a later folder list doesn't bring it back.
        list.show(Shown::Tab(Tab::Groups));
        list.set_folders(vec![folder(4, FolderRules::GROUPS)]);
        assert_eq!(list.shown(), Shown::Tab(Tab::Groups));

        let mut list = ChatList::default();
        list.restore(kept(5));
        list.set_folders(vec![folder(4, FolderRules::GROUPS)]);
        assert_eq!(list.shown(), Shown::Tab(Tab::All), "gone: All");
        assert_eq!(list.kept_view().shown, Shown::Tab(Tab::All), "and dropped");
        list.set_folders(vec![folder(5, FolderRules::GROUPS)]);
        assert_eq!(list.shown(), Shown::Tab(Tab::All));
    }

    #[test]
    fn paging_follows_the_shown_view() {
        let now = Instant::now();
        let mut list = ChatList::default();
        // 100 chats, 90 of them archived: the main view ends early.
        list.reset(
            (0..100)
                .map(|n| if n < 10 { chat(n) } else { archived(n) })
                .collect(),
        );
        list.paged(Pages::More);
        list.seen(5);
        assert!(list.wants_more(now), "the main view's end is on screen");
        list.requested();
        list.paged(Pages::More);
        list.show(Shown::Archive);
        list.seen(10);
        assert!(!list.wants_more(now), "the archive's end is not");
        // An archive view with no row asks for the next page by itself.
        list.reset((0..10).map(chat).collect());
        list.show(Shown::Archive);
        assert!(list.wants_more(now));
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
