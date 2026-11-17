//! Bounded, ordered chat list.

use std::collections::{HashMap, HashSet};

use crate::model::{ChatId, ChatSummary, Message, MsgId, PeerFolder, Preview};

/// Maximum number of dialogs held.
pub const MAX_DIALOGS: usize = 1000;

/// Live folder moves remembered for crossing dialogs responses. Beyond it
/// the oldest move is forgotten, and every response that could still need
/// it is discarded ([`Guarded::Discard`]).
const MAX_FOLDER_MOVES: usize = MAX_DIALOGS;

/// What [`Dialogs::guard_folders`] made of a dialogs response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Guarded {
    /// Apply the page: no move it could predate disagrees with it.
    Apply,
    /// Apply the page (live moves kept over it), then fetch the first page
    /// again: the page predates a move it disagreed with.
    ApplyAndRefetch,
    /// Drop the page and fetch the first page again: it was dispatched
    /// before a move whose record was evicted, so it can't be guarded.
    Discard,
}

/// One remembered live folder move.
#[derive(Clone, Copy, Debug)]
struct Move {
    folder: PeerFolder,
    /// The dispatch seq current when the move was made.
    at: DialogsSeq,
    /// Move order (monotonic), for eviction oldest first.
    order: u64,
}

/// The dialogs requests' logical clock. Every dialogs request takes the next
/// value when it is *dispatched*, so a response knows which live folder
/// moves it cannot have seen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DialogsSeq(u64);

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
    /// The seq of the last dispatched dialogs request.
    dispatched: DialogsSeq,
    /// Live folder moves (`set_folder`) with the seq current when each was
    /// made. A response dispatched at or before that seq may predate the
    /// move, so the move wins over it ([`Dialogs::guard_folders`]).
    moves: HashMap<ChatId, Move>,
    /// Source of [`Move::order`].
    move_order: u64,
    /// Responses dispatched at or before this seq may predate an evicted
    /// move: they are discarded.
    evicted_through: DialogsSeq,
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
            summary.unread = summary.unread.saturating_add(1);
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

    /// Takes the seq of a dialogs request being dispatched.
    pub fn dispatch(&mut self) -> DialogsSeq {
        self.dispatched = DialogsSeq(self.dispatched.0.saturating_add(1));
        self.dispatched
    }

    /// Keeps live folder moves over a dialogs response dispatched at `seq`,
    /// before the page is applied: a chat moved at or after that dispatch
    /// takes its live folder. A move older than the dispatch is settled by
    /// the response, which is authoritative for it. A response dispatched
    /// before an evicted move is discarded whole, untouched.
    ///
    /// Every refetch this asks for is dispatched after every move recorded
    /// so far, so it is authoritative for all of them: only a new move
    /// crossing it can ask for another.
    pub fn guard_folders(&mut self, page: &mut [ChatSummary], seq: DialogsSeq) -> Guarded {
        if seq <= self.evicted_through {
            return Guarded::Discard;
        }
        let mut conflict = false;
        for chat in page {
            let Some(&Move { folder, at, .. }) = self.moves.get(&chat.id) else {
                continue;
            };
            if seq <= at {
                conflict |= chat.folder != folder;
                chat.folder = folder;
            } else {
                self.moves.remove(&chat.id);
            }
        }
        if conflict {
            Guarded::ApplyAndRefetch
        } else {
            Guarded::Apply
        }
    }

    /// Moves a chat into or out of the archive. `None` when the chat is not
    /// listed or already there. The order is unchanged: the archive and the
    /// main list are views of one server-ordered list. The move is
    /// remembered, listed or not, for responses already in flight.
    pub fn set_folder(&mut self, chat: ChatId, folder: PeerFolder) -> Option<&ChatSummary> {
        if self.moves.len() >= MAX_FOLDER_MOVES
            && !self.moves.contains_key(&chat)
            && let Some((&oldest, _)) = self.moves.iter().min_by_key(|(_, m)| m.order)
            && let Some(evicted) = self.moves.remove(&oldest)
        {
            // A response dispatched at or before the evicted move could
            // need it; none dispatched later can.
            self.evicted_through = self.evicted_through.max(evicted.at);
        }
        self.move_order = self.move_order.wrapping_add(1);
        let record = Move {
            folder,
            at: self.dispatched,
            order: self.move_order,
        };
        self.moves.insert(chat, record);
        let summary = self
            .list
            .iter_mut()
            .find(|c| c.id == chat && c.folder != folder)?;
        summary.folder = folder;
        Some(summary)
    }

    /// Sets the unread count (local read or read on another device).
    pub fn set_unread(&mut self, chat: ChatId, unread: u32) -> Option<&ChatSummary> {
        let summary = self.list.iter_mut().find(|c| c.id == chat)?;
        summary.unread = unread;
        Some(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChatKind, PeerKind};

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
            Bump::Updated(ref s) if s.unread == 2 && s.last_id == Some(MsgId(11))
        ));
        // Already reflected: nothing changes, nothing is counted twice.
        assert_eq!(d.bump(id(2), last(11), Unread::Increment), Bump::Reflected);
        assert_eq!(d.get(id(2)).map(|c| c.unread), Some(2));
    }

    #[test]
    fn bump_moves_unpinned_below_pinned() {
        let mut d = Dialogs::default();
        d.reset(vec![pinned(1), pinned(2), chat(3), chat(4)]);
        let r = d.bump(id(4), last(10), Unread::Increment);
        assert!(matches!(r, Bump::Updated(ref s) if s.unread == 1));
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
        fresh.unread = 3;
        let changed = d
            .append(vec![fresh.clone(), chat(2), chat(3)], Rest::More)
            .changed;
        assert_eq!(changed, vec![fresh, chat(3)]);
        assert_eq!(ids(&d), vec![1, 2, 3]);
        assert_eq!(d.as_slice()[0].unread, 3);
    }

    #[test]
    fn unread_saturates() {
        let mut d = Dialogs::default();
        let mut c = chat(1);
        c.unread = u32::MAX;
        d.reset(vec![c]);
        d.bump(id(1), last(0), Unread::Increment);
        assert_eq!(d.as_slice()[0].unread, u32::MAX);
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
    fn archiving_flags_the_chat_in_place() {
        let mut d = Dialogs::default();
        d.reset(vec![chat(1), chat(2)]);
        let rev = d.rev();
        let archive = PeerFolder::Archive;
        assert!(
            d.set_folder(id(2), archive)
                .is_some_and(|c| c.folder == archive)
        );
        assert!(d.set_folder(id(2), archive).is_none(), "already archived");
        assert!(d.set_folder(id(9), archive).is_none(), "not listed");
        assert_eq!(ids(&d), vec![1, 2], "order unchanged");
        assert_eq!(d.rev(), rev);
        let main = PeerFolder::Main;
        assert!(d.set_folder(id(2), main).is_some_and(|c| c.folder == main));
    }

    /// A response dispatched before a live move keeps the move (and asks for
    /// a refetch); one dispatched after it is authoritative and settles it.
    #[test]
    fn folder_moves_win_over_older_responses_only() {
        let archive = PeerFolder::Archive;
        let mut d = Dialogs::default();
        d.reset(vec![chat(1), chat(2)]);
        let older = d.dispatch();
        d.set_folder(id(2), archive);
        // Not listed: remembered too, for the page that brings it.
        assert!(d.set_folder(id(3), archive).is_none());
        let mut page = vec![chat(1), chat(2), chat(3)];
        assert_eq!(d.guard_folders(&mut page, older), Guarded::ApplyAndRefetch);
        assert_eq!(
            page.iter().map(|c| c.folder).collect::<Vec<_>>(),
            [PeerFolder::Main, archive, archive]
        );
        let newer = d.dispatch();
        let mut page = vec![chat(2)];
        assert_eq!(
            d.guard_folders(&mut page, newer),
            Guarded::Apply,
            "authoritative"
        );
        assert_eq!(page[0].folder, PeerFolder::Main);
        let mut page = vec![chat(2)];
        assert_eq!(
            d.guard_folders(&mut page, older),
            Guarded::Apply,
            "settled: forgotten"
        );
    }

    /// Past the cap the oldest move goes first, in move order (not by
    /// seq: these share one), and a response dispatched before it is
    /// discarded; a response dispatched after it is applied.
    #[test]
    fn evicting_a_move_discards_the_responses_that_could_need_it() {
        let archive = PeerFolder::Archive;
        let mut d = Dialogs::default();
        d.reset(vec![chat(1), chat(2)]);
        let older = d.dispatch();
        d.set_folder(id(1), archive);
        let others = i64::try_from(MAX_FOLDER_MOVES).unwrap();
        for n in 0..others - 1 {
            d.set_folder(id(1000 + n), archive);
        }
        let mut page = vec![chat(1), chat(2)];
        assert_eq!(d.guard_folders(&mut page, older), Guarded::ApplyAndRefetch);
        // One more: chat 1's move is the oldest, so it goes.
        d.set_folder(id(5000), archive);
        assert!(!d.moves.contains_key(&id(1)));
        assert!(d.moves.contains_key(&id(1000)));
        let mut page = vec![chat(1), chat(2)];
        assert_eq!(d.guard_folders(&mut page, older), Guarded::Discard);
        assert_eq!(page[0].folder, PeerFolder::Main, "untouched");
        let newer = d.dispatch();
        assert_eq!(d.guard_folders(&mut page, newer), Guarded::Apply);
    }
}
