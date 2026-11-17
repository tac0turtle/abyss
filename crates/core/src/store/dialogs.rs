//! Bounded, ordered chat list.

mod fields;

use std::collections::HashSet;

pub use fields::Guarded;
use fields::{Current, Fields};

use super::{BadgeSeq, ForumStatus};
use crate::model::{ChatId, ChatKind, ChatSummary, Message, MsgId, PeerFolder, Preview};

/// Maximum number of dialogs held.
pub const MAX_DIALOGS: usize = 1000;

/// Ordered chat list: pinned chats first in server order, then unpinned chats by
/// recency. Bounded by [`MAX_DIALOGS`].
///
/// After a refreshed first page, the chats kept from earlier pages are
/// *provisional*: they form the tail of the list (from `confirmed` on) until
/// the restarted traversal reaches them again. Rows the traversal returns are
/// *confirmed*, in server order, ahead of every provisional row, so a chat
/// found below the first page is admitted even at capacity, evicting the
/// lowest-ranked provisional row.
#[derive(Debug, Default)]
pub struct Dialogs {
    list: Vec<ChatSummary>,
    /// `list[..confirmed]` is confirmed, `list[confirmed..]` provisional.
    confirmed: usize,
    /// Bumped by every call that can change which chats are listed, or
    /// their order.
    rev: u64,
    /// Per-field accepted stamps of the rows (`fields`).
    fields: Fields,
}

/// What a later page did to the list.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Appended {
    /// The chats added or changed, in server order.
    pub changed: Vec<ChatSummary>,
    /// The page did more than update rows in place and append at the end
    /// (it placed a chat before provisional rows, or evicted or dropped
    /// one): the caller should resend the whole list.
    pub reordered: bool,
}

/// What a new message did to the chat list.
#[derive(Debug, PartialEq, Eq)]
pub enum Bump {
    /// The chat moved and/or changed; carries its new summary.
    Updated(ChatSummary),
    /// The chat is not in the loaded list. Callers decide whether to fetch it.
    Unknown,
    /// The chat already reflects this message (its `last_id` is not older).
    Reflected,
}

/// The newest message of a chat, as a bump records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Last {
    pub id: MsgId,
    /// Unix seconds.
    pub date: i64,
    pub preview: Preview,
}

/// Whether a new message `new` changes a chat whose newest known message is
/// `known`: only a newer id does (a chat with no known id takes any). An
/// unpinned chat it changes moves to the top of its section. The UI mirrors
/// the order with this same rule (`Event::ChatBumped`).
pub fn bump_moves(known: Option<MsgId>, new: MsgId) -> bool {
    known.is_none_or(|k| new > k)
}

impl Last {
    pub fn of(m: &Message) -> Self {
        Self {
            id: m.id,
            date: m.date,
            preview: Preview::of(m),
        }
    }
}

/// Whether more dialogs pages follow the one being applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rest {
    /// This page ends the server's list.
    Last,
    More,
}

/// Whether a new message counts toward the chat's unread badge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unread {
    Increment,
    Unchanged,
}

impl Dialogs {
    pub fn as_slice(&self) -> &[ChatSummary] {
        &self.list
    }

    pub fn get(&self, id: ChatId) -> Option<&ChatSummary> {
        self.list.iter().find(|c| c.id == id)
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Replaces the list (reconciliation). Duplicates and overflow are dropped.
    pub fn reset(&mut self, chats: Vec<ChatSummary>) {
        self.list.clear();
        self.extend(chats);
        self.confirmed = self.list.len();
        self.prune_fields();
    }

    /// Applies a freshly loaded *first* page (bootstrap or refresh) without
    /// dropping the pages loaded after it. The page becomes the head of the list
    /// in server order. Previously loaded chats it does not contain are kept
    /// after it (unpinned, newest first) when they are not newer than the page's
    /// oldest unpinned chat; a chat that is newer but missing was left or
    /// deleted, so it goes (an archived chat stays listed, in the archive). A page that ends the list replaces it entirely.
    /// Returns how many previously loaded chats were kept.
    pub fn merge_first_page(&mut self, page: Vec<ChatSummary>, rest: Rest) -> usize {
        let old = std::mem::take(&mut self.list);
        self.extend(page);
        self.confirmed = self.list.len();
        if rest == Rest::Last {
            self.prune_fields();
            return 0;
        }
        let head: HashSet<ChatId> = self.list.iter().map(|c| c.id).collect();
        let boundary = self
            .list
            .iter()
            .filter(|c| !c.pinned)
            .map(|c| c.last_date)
            .min();
        let mut tail: Vec<ChatSummary> = old
            .into_iter()
            .filter(|c| !head.contains(&c.id))
            .filter(|c| boundary.is_none_or(|b| c.last_date <= b))
            .map(|mut c| {
                // Not in the pinned head of a fresh first page: no longer pinned.
                c.pinned = false;
                c
            })
            .collect();
        tail.sort_by_key(|c| std::cmp::Reverse(c.last_date));
        let before = self.list.len();
        self.extend(tail);
        self.prune_fields();
        self.list.len() - before
    }

    /// Applies a later page in server order. Confirmed chats are updated in
    /// place; provisional and new ones become confirmed at the end of the
    /// confirmed rows, a new one evicting the lowest-ranked provisional row
    /// when the list is full (it is dropped only when every row is
    /// confirmed). A page that ends the server's list drops the provisional
    /// rows the traversal never reached: they are gone from the server.
    pub fn append(&mut self, chats: Vec<ChatSummary>, rest: Rest) -> Appended {
        self.rev = self.rev.wrapping_add(1);
        let mut out = Appended::default();
        for chat in chats {
            match self.list.iter().position(|c| c.id == chat.id) {
                Some(pos) if pos < self.confirmed => {
                    if let Some(slot) = self.list.get_mut(pos)
                        && *slot != chat
                    {
                        slot.clone_from(&chat);
                        out.changed.push(chat);
                    }
                }
                Some(pos) => {
                    let old = self.list.remove(pos);
                    out.reordered |= pos != self.confirmed;
                    self.admit(chat.clone());
                    if old != chat {
                        out.changed.push(chat);
                    }
                }
                None => {
                    if self.list.len() >= MAX_DIALOGS {
                        if self.confirmed >= self.list.len() {
                            continue; // every row is confirmed: full
                        }
                        self.list.pop();
                        out.reordered = true;
                    }
                    out.reordered |= self.confirmed < self.list.len();
                    self.admit(chat.clone());
                    out.changed.push(chat);
                }
            }
        }
        if rest == Rest::Last && self.confirmed < self.list.len() {
            self.list.truncate(self.confirmed);
            out.reordered = true;
        }
        debug_assert!(self.confirmed <= self.list.len() && self.list.len() <= MAX_DIALOGS);
        self.prune_fields();
        out
    }

    /// Inserts `chat` as the last confirmed row. The caller made room.
    fn admit(&mut self, chat: ChatSummary) {
        self.list.insert(self.confirmed, chat);
        self.confirmed += 1;
    }

    /// Changes whenever the set or order of listed chats may have changed
    /// (not on unread changes or bumps that leave a chat in place).
    pub fn rev(&self) -> u64 {
        self.rev
    }

    fn extend(&mut self, chats: Vec<ChatSummary>) {
        self.rev = self.rev.wrapping_add(1);
        for chat in chats {
            if self.list.len() >= MAX_DIALOGS {
                break;
            }
            if self.get(chat.id).is_none() {
                self.list.push(chat);
            }
        }
    }

    /// Records a new message in `chat`: updates the preview, date, last id and
    /// unread count, and moves an unpinned chat to the top of the unpinned
    /// section. A message not newer than the chat's known `last_id` (already
    /// reflected, by id rather than by timestamp) changes nothing.
    pub fn bump(&mut self, chat: ChatId, last: Last, unread: Unread) -> Bump {
        let Some(pos) = self.list.iter().position(|c| c.id == chat) else {
            return Bump::Unknown;
        };
        if !bump_moves(self.list.get(pos).and_then(|c| c.last_id), last.id) {
            return Bump::Reflected;
        }
        let provisional = pos >= self.confirmed;
        let mut summary = self.list.remove(pos);
        summary.last = Some(last.preview);
        summary.last_date = summary.last_date.max(last.date);
        summary.last_id = Some(last.id);
        if unread == Unread::Increment {
            summary.read.unread = summary.read.unread.saturating_add(1);
        }
        let insert_at = if summary.pinned {
            pos
        } else {
            self.list.iter().take_while(|c| c.pinned).count()
        };
        self.list.insert(insert_at, summary.clone());
        if provisional && insert_at <= self.confirmed {
            // Moved to the top by a new message: its rank is known now.
            self.confirmed += 1;
        }
        if insert_at != pos {
            self.rev = self.rev.wrapping_add(1);
        }
        Bump::Updated(summary)
    }

    /// Guards a dialog snapshot dispatched at `seq` (badge clock; zero for
    /// cached chats) before it is applied: each field keeps its current
    /// value unless the snapshot postdates the field's accepted stamp
    /// (`fields`). `held` names a fenced chat's pre-fence forum status:
    /// no snapshot sets its kind.
    pub fn guard(
        &mut self,
        page: &mut [ChatSummary],
        seq: BadgeSeq,
        held: impl Fn(ChatId) -> Option<ForumStatus>,
    ) -> Guarded {
        let list = &self.list;
        let current = |id: ChatId| {
            list.iter().find(|c| c.id == id).map(|c| Current {
                kind: c.kind,
                folder: c.folder,
                read: c.read,
            })
        };
        self.fields.guard(page, seq, held, current)
    }

    /// Moves a chat into or out of the archive, live, at badge clock `now`.
    /// `None` when the chat is not listed or already there. The order is
    /// unchanged: the archive and the main list are views of one
    /// server-ordered list. The move is remembered, listed or not, so a
    /// snapshot dispatched before it can't revert it.
    pub fn set_folder(
        &mut self,
        chat: ChatId,
        folder: PeerFolder,
        now: BadgeSeq,
    ) -> Option<&ChatSummary> {
        let unlisted = self.get(chat).is_none().then_some(folder);
        self.fields.moved(chat, now, unlisted);
        let summary = self
            .list
            .iter_mut()
            .find(|c| c.id == chat && c.folder != folder)?;
        summary.folder = folder;
        Some(summary)
    }

    /// Applies a fresh summary of a listed chat (`peer_dialogs`), wherever
    /// it sits. A newer top message moves the chat as a bump does; the
    /// rest changes in place, its pinned state included only through
    /// dialogs pages. `None` when the chat isn't listed or nothing changed.
    pub fn refresh(&mut self, fresh: ChatSummary) -> Option<&ChatSummary> {
        let known = self.get(fresh.id)?.last_id;
        if let (Some(id), Some(preview)) = (fresh.last_id, fresh.last.clone())
            && bump_moves(known, id)
        {
            let last = Last {
                id,
                date: fresh.last_date,
                preview,
            };
            self.bump(fresh.id, last, Unread::Unchanged);
        }
        let summary = self.list.iter_mut().find(|c| c.id == fresh.id)?;
        let before = summary.clone();
        summary.title = fresh.title;
        summary.kind = fresh.kind;
        summary.read = fresh.read;
        summary.reading = fresh.reading;
        summary.unread_mentions = fresh.unread_mentions;
        summary.can_send = fresh.can_send;
        summary.muted_until = fresh.muted_until;
        summary.folder = fresh.folder;
        (*summary != before || summary.last_id != known).then_some(&*summary)
    }

    /// Commits a settled forum-flag check: the kind stated by a source
    /// dispatched at `seq`, which no older snapshot can revert. A chat that
    /// isn't listed (a window only) has it remembered for the page that
    /// lists it. `None` when the chat isn't listed or already has it.
    pub fn commit_kind(
        &mut self,
        chat: ChatId,
        kind: ChatKind,
        seq: BadgeSeq,
    ) -> Option<&ChatSummary> {
        let unlisted = self.get(chat).is_none().then_some(kind);
        self.fields.kind_committed(chat, seq, unlisted);
        let summary = self
            .list
            .iter_mut()
            .find(|c| c.id == chat && c.kind != kind)?;
        summary.kind = kind;
        Some(summary)
    }

    /// A chat-level read update from the server (a read here or on another
    /// device), live at badge clock `now`: the count it states, and the
    /// pointer raised to its `max_id`.
    pub fn server_read(
        &mut self,
        chat: ChatId,
        still_unread: u32,
        max_id: MsgId,
        now: BadgeSeq,
    ) -> Option<&ChatSummary> {
        let summary = self.list.iter_mut().find(|c| c.id == chat)?;
        summary.read.unread = still_unread;
        summary.read.read_inbox_max = summary.read.read_inbox_max.max(max_id);
        self.fields.unread_changed(chat, now);
        Some(summary)
    }

    /// A read up to `max_id` succeeded at badge clock `now`: the server's
    /// pointer is at least that high. It proves no count, so nothing is
    /// stamped. A chat that isn't listed (a window only) has it remembered
    /// for the page that lists it. `None` when the chat isn't listed or its
    /// pointer was already there.
    pub fn read_landed(
        &mut self,
        chat: ChatId,
        max_id: MsgId,
        now: BadgeSeq,
    ) -> Option<&ChatSummary> {
        if self.get(chat).is_none() {
            self.fields.read_landed_unlisted(chat, max_id, now);
            return None;
        }
        let summary = self
            .list
            .iter_mut()
            .find(|c| c.id == chat && c.read.read_inbox_max < max_id)?;
        summary.read.read_inbox_max = max_id;
        Some(summary)
    }

    /// Sets the row's read overlay ([`ChatSummary::reading`]) to what the
    /// read intents say now. `None` when the chat isn't listed or it was
    /// already that.
    pub fn set_reading(&mut self, chat: ChatId, reading: Option<MsgId>) -> Option<&ChatSummary> {
        let summary = self
            .list
            .iter_mut()
            .find(|c| c.id == chat && c.reading != reading)?;
        summary.reading = reading;
        Some(summary)
    }

    /// A counted live message changed `chat`'s unread count (Stage 1
    /// rules) at badge clock `now`: an older snapshot can't revert it.
    pub fn unread_counted(&mut self, chat: ChatId, now: BadgeSeq) {
        if self.get(chat).is_some() {
            self.fields.unread_changed(chat, now);
        }
    }

    fn prune_fields(&mut self) {
        let listed: HashSet<ChatId> = self.list.iter().map(|c| c.id).collect();
        self.fields.prune(&listed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChatKind, PeerKind, ReadState};
    use crate::store::Badges;

    fn id(id: i64) -> ChatId {
        ChatId {
            kind: PeerKind::User,
            id,
        }
    }

    fn chat(n: i64) -> ChatSummary {
        ChatSummary {
            id: id(n),
            title: format!("c{n}"),
            kind: ChatKind::Private { contact: false },
            pinned: false,
            read: ReadState::default(),
            unread_mentions: 0,
            last: None,
            last_date: 0,
            last_id: None,
            can_send: true,
            muted_until: 0,
            folder: PeerFolder::Main,
            reading: None,
        }
    }

    fn pinned(n: i64) -> ChatSummary {
        ChatSummary {
            pinned: true,
            ..chat(n)
        }
    }

    fn ids(d: &Dialogs) -> Vec<i64> {
        d.as_slice().iter().map(|c| c.id.id).collect()
    }

    /// A message with id `n` sent at second 0 (dates collide on purpose).
    fn last(n: i32) -> Last {
        Last {
            id: MsgId(n),
            date: 0,
            preview: Preview::Service,
        }
    }

    #[test]
    fn same_second_messages_are_ordered_by_id() {
        let mut d = Dialogs::default();
        d.reset(vec![chat(1), chat(2)]);
        assert!(matches!(
            d.bump(id(2), last(10), Unread::Increment),
            Bump::Updated(ref s) if s.last_id == Some(MsgId(10))
        ));
        // Same timestamp, newer id: a new message, counted.
        assert!(matches!(
            d.bump(id(2), last(11), Unread::Increment),
            Bump::Updated(ref s) if s.read.unread == 2 && s.last_id == Some(MsgId(11))
        ));
        // Already reflected: nothing changes, nothing is counted twice.
        assert_eq!(d.bump(id(2), last(11), Unread::Increment), Bump::Reflected);
        assert_eq!(d.get(id(2)).map(|c| c.read.unread), Some(2));
    }

    #[test]
    fn bump_moves_unpinned_below_pinned() {
        let mut d = Dialogs::default();
        d.reset(vec![pinned(1), pinned(2), chat(3), chat(4)]);
        let r = d.bump(id(4), last(10), Unread::Increment);
        assert!(matches!(r, Bump::Updated(ref s) if s.read.unread == 1));
        assert_eq!(ids(&d), vec![1, 2, 4, 3]);
    }

    #[test]
    fn bump_keeps_pinned_position() {
        let mut d = Dialogs::default();
        d.reset(vec![pinned(1), pinned(2), chat(3)]);
        d.bump(id(2), last(10), Unread::Unchanged);
        assert_eq!(ids(&d), vec![1, 2, 3]);
    }

    #[test]
    fn unknown_chat_is_reported() {
        let mut d = Dialogs::default();
        assert_eq!(d.bump(id(9), last(0), Unread::Increment), Bump::Unknown);
    }

    #[test]
    fn bounded_and_deduplicated() {
        let mut d = Dialogs::default();
        let page: Vec<_> = (0..i64::try_from(MAX_DIALOGS).unwrap() + 50)
            .map(chat)
            .collect();
        d.append(vec![chat(0), chat(0)], Rest::More);
        let added = d.append(page, Rest::More).changed;
        assert_eq!(d.len(), MAX_DIALOGS);
        assert_eq!(added.len(), MAX_DIALOGS - 1);
        assert_eq!(added.first().map(|c| c.id), Some(id(1)));
    }

    fn dated(n: i64, last_date: i64) -> ChatSummary {
        ChatSummary {
            last_date,
            ..chat(n)
        }
    }

    #[test]
    fn first_page_merges_over_loaded_pages() {
        let mut d = Dialogs::default();
        // Loaded: pinned 1, then 2..=6 by recency (dates 60..20).
        d.reset(vec![pinned(1)]);
        d.append((2..=6).map(|n| dated(n, 80 - n * 10)).collect(), Rest::More);
        // Fresh first page: 6 got a new message, 2 was left (newer than the
        // page's oldest, but missing), 1 was unpinned and is not in the page.
        let page = vec![dated(6, 100), dated(3, 50)];
        let kept = d.merge_first_page(page, Rest::More);
        assert_eq!(ids(&d), vec![6, 3, 4, 5, 1]);
        assert_eq!(kept, 3);
        assert!(d.as_slice().iter().all(|c| !c.pinned));
    }

    #[test]
    fn last_first_page_replaces_the_list() {
        let mut d = Dialogs::default();
        d.reset((1..=5).map(chat).collect());
        assert_eq!(d.merge_first_page(vec![chat(9)], Rest::Last), 0);
        assert_eq!(ids(&d), vec![9]);
    }

    #[test]
    fn merged_list_stays_bounded() {
        let mut d = Dialogs::default();
        let max = i64::try_from(MAX_DIALOGS).unwrap();
        d.reset((0..max).map(|n| dated(n, -n)).collect());
        let page: Vec<_> = (max..max + 100).map(|n| dated(n, n)).collect();
        d.merge_first_page(page, Rest::More);
        assert_eq!(d.len(), MAX_DIALOGS);
        assert_eq!(d.as_slice()[100].id, id(0));
    }

    #[test]
    fn later_pages_update_in_place() {
        let mut d = Dialogs::default();
        d.reset(vec![chat(1), chat(2)]);
        let mut fresh = chat(1);
        fresh.read.unread = 3;
        let changed = d
            .append(vec![fresh.clone(), chat(2), chat(3)], Rest::More)
            .changed;
        assert_eq!(changed, vec![fresh, chat(3)]);
        assert_eq!(ids(&d), vec![1, 2, 3]);
        assert_eq!(d.as_slice()[0].read.unread, 3);
    }

    #[test]
    fn unread_saturates() {
        let mut d = Dialogs::default();
        let mut c = chat(1);
        c.read.unread = u32::MAX;
        d.reset(vec![c]);
        d.bump(id(1), last(0), Unread::Increment);
        assert_eq!(d.as_slice()[0].read.unread, u32::MAX);
    }

    #[test]
    fn end_of_traversal_drops_unconfirmed_rows() {
        let mut d = Dialogs::default();
        d.reset((1..=5).map(chat).collect());
        d.merge_first_page(vec![chat(1)], Rest::More);
        let out = d.append(vec![chat(3)], Rest::Last);
        assert!(out.reordered);
        assert_eq!(ids(&d), vec![1, 3]);
    }

    #[test]
    fn a_refresh_changes_the_row_and_moves_it_only_for_a_newer_message() {
        let mut d = Dialogs::default();
        d.reset(vec![chat(1), chat(2), chat(3)]);
        let mut fresh = chat(3);
        fresh.read.unread = 4;
        assert!(d.refresh(fresh.clone()).is_some_and(|c| c.read.unread == 4));
        assert_eq!(ids(&d), vec![1, 2, 3], "in place");
        assert!(d.refresh(fresh.clone()).is_none(), "unchanged");
        fresh.last_id = Some(MsgId(9));
        fresh.last = Some(Preview::Service);
        assert!(d.refresh(fresh).is_some());
        assert_eq!(ids(&d), vec![3, 1, 2], "a newer message moves it");
        assert!(d.refresh(chat(7)).is_none(), "not listed");
    }

    #[test]
    fn archiving_flags_the_chat_in_place() {
        let mut d = Dialogs::default();
        d.reset(vec![chat(1), chat(2)]);
        let rev = d.rev();
        let archive = PeerFolder::Archive;
        assert!(
            d.set_folder(id(2), archive, BadgeSeq::default())
                .is_some_and(|c| c.folder == archive)
        );
        assert!(
            d.set_folder(id(2), archive, BadgeSeq::default()).is_none(),
            "already archived"
        );
        assert!(
            d.set_folder(id(9), archive, BadgeSeq::default()).is_none(),
            "not listed"
        );
        assert_eq!(ids(&d), vec![1, 2], "order unchanged");
        assert_eq!(d.rev(), rev);
        let main = PeerFolder::Main;
        assert!(
            d.set_folder(id(2), main, BadgeSeq::default())
                .is_some_and(|c| c.folder == main)
        );
    }

    /// A snapshot dispatched before a live move keeps the move (and asks
    /// for a refetch); one dispatched after it is authoritative. Once a
    /// newer snapshot set a field, an older one landing late keeps it.
    #[test]
    fn folders_follow_the_newest_source() {
        let archive = PeerFolder::Archive;
        let mut clock = Badges::default();
        let mut d = Dialogs::default();
        d.reset(vec![chat(1), chat(2)]);
        let older = clock.dispatch();
        d.set_folder(id(2), archive, clock.now());
        // Not listed: remembered too, for the page that brings it.
        assert!(d.set_folder(id(3), archive, clock.now()).is_none());
        let mut page = vec![chat(1), chat(2), chat(3)];
        assert_eq!(
            d.guard(&mut page, older, |_| None),
            Guarded::ApplyAndRefetch
        );
        assert_eq!(
            page.iter().map(|c| c.folder).collect::<Vec<_>>(),
            [PeerFolder::Main, archive, archive]
        );
        // A newer source moves it back (the server didn't take the move).
        let newer = clock.dispatch();
        let mut page = vec![chat(2)];
        assert_eq!(d.guard(&mut page, newer, |_| None), Guarded::Apply);
        d.refresh(page.remove(0));
        // The older source landing now can't revert that.
        let mut page = vec![ChatSummary {
            folder: archive,
            ..chat(2)
        }];
        d.guard(&mut page, older, |_| None);
        assert_eq!(page[0].folder, PeerFolder::Main);
    }

    /// Kind and unread follow the same rule; a fenced chat's kind is held,
    /// and a committed kind outranks older snapshots.
    #[test]
    fn kind_and_unread_follow_the_newest_source() {
        let forum = ChatKind::Supergroup {
            forum: true,
            gigagroup: false,
        };
        let group = ChatKind::Supergroup {
            forum: false,
            gigagroup: false,
        };
        let base = ChatSummary {
            kind: group,
            ..chat(1)
        };
        let mut clock = Badges::default();
        let mut d = Dialogs::default();
        d.reset(vec![base.clone()]);
        let older = clock.dispatch();
        let newer = clock.dispatch();
        let fresh = ChatSummary {
            read: ReadState {
                unread: 7,
                read_inbox_max: MsgId(0),
            },
            kind: forum,
            ..base.clone()
        };
        let mut page = vec![fresh.clone()];
        d.guard(&mut page, newer, |_| None);
        d.refresh(page.remove(0));
        let mut stale = vec![base.clone()];
        d.guard(&mut stale, older, |_| None);
        assert_eq!((stale[0].kind, stale[0].read.unread), (forum, 7));
        // A live read outranks every snapshot dispatched before it.
        d.server_read(id(1), 0, MsgId(0), clock.now());
        let mut late = vec![fresh.clone()];
        d.guard(&mut late, newer, |_| None);
        assert_eq!(late[0].read.unread, 0);
        // Fenced: no snapshot sets the kind; the commit's stamp outranks
        // older snapshots, also one newer than the last kind it accepted.
        let middle = clock.dispatch();
        let newest = clock.dispatch();
        let mut held = vec![base];
        d.guard(&mut held, newest, |_| Some(ForumStatus::Forum));
        assert_eq!(held[0].kind, forum);
        d.commit_kind(id(1), group, newest);
        let mut stale = vec![fresh];
        d.guard(&mut stale, middle, |_| None);
        assert_eq!(stale[0].kind, group);
    }

    /// Past the cap the oldest unlisted move goes, and a snapshot dispatched
    /// before it is discarded; one dispatched after it is applied. Stamps
    /// stay bounded, and a snapshot never evicts a record it holds.
    #[test]
    fn evicting_an_unlisted_move_discards_the_snapshots_that_could_need_it() {
        let archive = PeerFolder::Archive;
        let mut clock = Badges::default();
        let mut d = Dialogs::default();
        d.reset(vec![chat(2)]);
        let older = clock.dispatch();
        d.set_folder(id(1), archive, clock.now());
        let others = i64::try_from(fields::MAX_UNLISTED).unwrap();
        for n in 0..others - 1 {
            d.set_folder(id(1000 + n), archive, clock.now());
        }
        let mut page = vec![chat(1), chat(2)];
        assert_eq!(
            d.guard(&mut page, older, |_| None),
            Guarded::ApplyAndRefetch
        );
        // One more: chat 1's move is the oldest, so it goes.
        d.set_folder(id(5000), archive, clock.now());
        let mut page = vec![chat(1), chat(2)];
        assert_eq!(d.guard(&mut page, older, |_| None), Guarded::Discard);
        assert_eq!(page[0].folder, PeerFolder::Main, "untouched");
        let newer = clock.dispatch();
        assert_eq!(d.guard(&mut page, newer, |_| None), Guarded::Apply);
        d.reset(vec![chat(2)]);
        assert!(d.fields.len() <= MAX_DIALOGS + fields::MAX_UNLISTED);

        // The snapshot itself makes the eviction: it lists a chat never
        // seen first, then the oldest remembered one. Room is made before
        // either is guarded, never at the cost of a chat it holds, so the
        // remembered move still wins.
        let mut d = Dialogs::default();
        d.reset(vec![chat(2)]);
        let older = clock.dispatch();
        for n in 0..others {
            d.set_folder(id(1000 + n), archive, clock.now());
        }
        let mut page = vec![chat(9999), chat(1000)];
        assert_eq!(
            d.guard(&mut page, older, |_| None),
            Guarded::ApplyAndRefetch
        );
        assert_eq!(page[1].folder, archive, "not evicted by its own snapshot");
    }
}
