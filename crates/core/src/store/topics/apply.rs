//! Applying page and by-id results through the merge rule (plan section 6.4).

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::time::Instant;

use super::entry::{Merged, Source, TopicEntry};
use super::order::{self, Key, rank};
use super::stamp::{FetchSeq, Verdict};
use super::{
    ByIdSnapshot, FirstPage, ListChange, PageKind, PageSnapshot, Paging, Replay, TopicList,
};
use crate::model::{Topic, TopicGone, TopicId, TopicOrder, TopicSnapshot, Visibility};
use crate::store::{Rest, Stale};

/// What happens to retained topics a first page doesn't contain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unlisted {
    /// Kept after the page (a refresh in the same order).
    Keep,
    /// Dropped, except the open topic (an order change resets the list).
    Drop,
}

/// Whether a later page brought a topic the list didn't hold (an empty page
/// counts as news: it ends paging by its own `rest`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum News {
    None,
    Some,
}

/// What a result did to the emitted list, in the order it happened, so a
/// consumer applying it in sequence repeats the store's steps.
#[derive(Debug, Default)]
struct Emitted(Vec<ListChange>);

impl Emitted {
    fn removed(&mut self, id: TopicId, reason: TopicGone) {
        self.0.push(ListChange::Removed(id, reason));
    }

    fn appended(&mut self, topic: Topic) {
        if let Some(ListChange::Appended(topics)) = self.0.last_mut() {
            topics.push(topic);
        } else {
            self.0.push(ListChange::Appended(vec![topic]));
        }
    }
}

impl TopicList {
    /// Applies a page dispatched as `seq`. A page whose order differs from
    /// the list's resets it when it is a first page, and is discarded (the
    /// first page is wanted again, nothing else changes) when it is a later one.
    pub fn apply_page(
        &mut self,
        seq: FetchSeq,
        page: PageSnapshot,
        now: Instant,
    ) -> Result<Vec<ListChange>, Stale> {
        let call = self.page.take_if(|p| p.seq == seq).ok_or(Stale)?;
        let changes = match call.kind {
            PageKind::Next if page.order != self.order => {
                self.first_page = FirstPage::Needed;
                Vec::new()
            }
            PageKind::Next => {
                self.set_post_closed(seq, page.post_closed);
                self.apply_next(seq, page, now)
            }
            PageKind::First => {
                self.set_post_closed(seq, page.post_closed);
                let unlisted = if page.order == self.order {
                    Unlisted::Keep
                } else {
                    self.order = page.order;
                    Unlisted::Drop
                };
                self.apply_first(seq, page, unlisted, now)
            }
        };
        self.prune_tombstones();
        Ok(changes)
    }

    fn apply_first(
        &mut self,
        seq: FetchSeq,
        page: PageSnapshot,
        unlisted: Unlisted,
        now: Instant,
    ) -> Vec<ListChange> {
        let mut old = std::mem::take(&mut self.entries);
        let mut list: Vec<TopicEntry> =
            Vec::with_capacity(page.topics.len().min(super::MAX_ENTRIES));
        // Kept entries whose activity the page couldn't set: their rank
        // comes from newer live activity, not from the page's position.
        let mut keep_rank = Vec::new();
        // Topics the page hid (or an open one it brought in hidden): the
        // reset alone would not say why they left.
        let mut hidden = Emitted::default();
        for snap in page.topics {
            let id = snap.topic.id;
            if list.iter().any(|e| e.id() == id) {
                continue;
            }
            if let Some(at) = old.iter().position(|e| e.id() == id) {
                let mut entry = old.remove(at);
                let was_shown = entry.shown();
                let merged = entry.merge(snap, seq, Source::Page);
                if was_shown && !entry.shown() {
                    hidden.removed(id, TopicGone::Hidden);
                }
                if merged.conflict() {
                    entry.owe(now);
                }
                if merged.activity != Verdict::Apply {
                    keep_rank.push(id);
                }
                list.push(entry);
            } else if let Some(entry) = self.admit_new(snap, seq, now) {
                self.note_new_hidden(&entry, &mut hidden);
                list.push(entry);
            }
        }
        if unlisted == Unlisted::Keep {
            mark_missed_first(&mut old, &list, self.order, page.rest, now);
            // Omitted entries whose activity is newer than the page (a live
            // change since its dispatch, or a later-dispatched snapshot
            // already applied) keep the rank it earned, not a place after
            // the page, so the cap can't cut them for being misplaced.
            keep_rank.extend(
                old.iter()
                    .filter(|e| e.activity.judge(seq) != Verdict::Apply)
                    .map(TopicEntry::id),
            );
        }
        order::pinned_first(&mut list);
        self.entries = list;
        match unlisted {
            Unlisted::Keep => self.entries.extend(old),
            Unlisted::Drop => {
                if let Some(open) = old.into_iter().find(|e| Some(e.id()) == self.open) {
                    order::insert_ranked(&mut self.entries, self.order, open);
                }
            }
        }
        order::pinned_first(&mut self.entries);
        if self.order == TopicOrder::LastActivity {
            for id in keep_rank {
                if let Some(at) = self.position(id) {
                    order::replace(&mut self.entries, self.order, at);
                }
            }
        }
        self.paging = paging_after(page.rest);
        self.enforce_cap();
        // Absence from the reset says nothing more: no deletion is inferred.
        hidden.0.push(self.reset());
        hidden.0
    }

    fn apply_next(&mut self, seq: FetchSeq, page: PageSnapshot, now: Instant) -> Vec<ListChange> {
        let before = self.shown_keys();
        let mut out = Emitted::default();
        let mut in_page: Vec<Topic> = Vec::with_capacity(page.topics.len());
        // A non-empty page made only of topics the list already holds ends
        // paging: an inflated server `count` can't drive redundant requests.
        let mut news = if page.topics.is_empty() {
            News::Some
        } else {
            News::None
        };
        for snap in page.topics {
            let id = snap.topic.id;
            if in_page.iter().any(|t| t.id == id) {
                continue;
            }
            if self.position(id).is_none() {
                news = News::Some;
            }
            if self.position(id).is_some() {
                let merged = self.merge_known(seq, snap, Source::Page, &mut out);
                if merged.is_some_and(Merged::conflict) {
                    self.owe(id, now);
                }
                in_page.extend(self.get(id).cloned());
            } else if self.capped_len() < super::MAX_TOPICS_PER_FORUM {
                in_page.push(snap.topic.clone());
                if let Some(entry) = self.admit_new(snap, seq, now) {
                    if entry.shown() {
                        out.appended(entry.topic.clone());
                    }
                    self.note_new_hidden(&entry, &mut out);
                    self.entries.push(entry);
                }
            } else {
                self.paging = Paging::Capped;
            }
        }
        self.mark_missed_between(&in_page, now);
        order::pinned_first(&mut self.entries);
        if self.paging != Paging::Capped {
            self.paging = match news {
                News::Some => paging_after(page.rest),
                News::None => Paging::Complete,
            };
        }
        for id in self.enforce_cap() {
            out.removed(id, TopicGone::Evicted);
        }
        // A later page is placed in server order, which the consumer's rank
        // rule may not reproduce: a deliberate reset then.
        self.checked(&before, out.0, Replay::ServerOrder)
    }

    /// Merges a snapshot into a listed topic and moves it if its pinned
    /// flag or rank key changed, or it became shown (the [`ListChange`]
    /// placement rule).
    fn merge_known(
        &mut self,
        seq: FetchSeq,
        snap: TopicSnapshot,
        source: Source,
        out: &mut Emitted,
    ) -> Option<Merged> {
        let order = self.order;
        let at = self.position(snap.topic.id)?;
        let entry = self.entries.get_mut(at)?;
        let (was, was_vis) = (entry.topic.clone(), entry.visibility);
        let merged = entry.merge(snap, seq, source);
        let shown_now = was_vis != Visibility::Shown && entry.shown();
        if shown_now || !Key::of(&entry.topic).places_like(&Key::of(&was), order) {
            order::replace(&mut self.entries, order, at);
        }
        classify(self.entry(was.id), &was, was_vis, out);
        Some(merged)
    }

    /// Topics ranking strictly inside a later page's range that it didn't
    /// contain may be gone: check them by id.
    fn mark_missed_between(&mut self, in_page: &[Topic], now: Instant) {
        let mut unpinned = in_page.iter().filter(|t| !t.pinned);
        let (Some(first), Some(last)) = (unpinned.next(), in_page.iter().rfind(|t| !t.pinned))
        else {
            return;
        };
        let order = self.order;
        for e in &mut self.entries {
            let missed = !e.topic.pinned
                && !in_page.iter().any(|t| t.id == e.id())
                && rank(order, first, &e.topic) == Ordering::Less
                && rank(order, &e.topic, last) == Ordering::Less;
            if missed {
                e.owe(now);
            }
        }
    }

    /// Applies a by-id result dispatched as `seq`. Only the requested ids
    /// count. A fresh `deleted` (one that passes the meta group's merge rule)
    /// removes the topic and records a tombstone.
    pub fn apply_by_id(
        &mut self,
        seq: FetchSeq,
        result: ByIdSnapshot,
        now: Instant,
    ) -> Result<Vec<ListChange>, Stale> {
        let call = self.by_id.take_if(|c| c.seq == seq).ok_or(Stale)?;
        self.set_post_closed(seq, result.post_closed);
        let before = self.shown_keys();
        let mut unanswered: BTreeSet<TopicId> = call.ids.into_iter().collect();
        let mut out = Emitted::default();
        for snap in result.found {
            let id = snap.topic.id;
            if unanswered.remove(&id) {
                self.discovery.remove(&id);
                self.apply_found(seq, snap, now, &mut out);
            }
        }
        for id in result.deleted {
            if unanswered.remove(&id) {
                self.discovery.remove(&id);
                self.apply_deleted(seq, id, now, &mut out);
            }
        }
        for id in unanswered {
            if Some(id) == self.unlisted_open() {
                self.open_retry.back_off(now);
            } else if self.is_due(id) {
                self.back_off(id, now);
            }
        }
        self.prune_tombstones();
        Ok(self.checked(&before, out.0, Replay::Exact))
    }

    fn apply_found(&mut self, seq: FetchSeq, snap: TopicSnapshot, now: Instant, out: &mut Emitted) {
        let id = snap.topic.id;
        if self.position(id).is_some() {
            self.merge_known(seq, snap, Source::ById, out);
            self.settle_or_back_off(id, seq, now);
            return;
        }
        if let Some(entry) = self.admit_new(snap, seq, now) {
            if entry.shown() {
                out.appended(entry.topic.clone());
            }
            self.note_new_hidden(&entry, out);
            order::insert_ranked(&mut self.entries, self.order, entry);
            for gone in self.enforce_cap() {
                out.removed(gone, TopicGone::Evicted);
            }
        }
    }

    fn apply_deleted(&mut self, seq: FetchSeq, id: TopicId, now: Instant, out: &mut Emitted) {
        let Some(at) = self.position(id) else {
            // Nothing newer is known about it: block older results. Unlisted
            // (never listed, or just evicted by this result) it is still
            // gone, which the consumer's other state needs to know.
            self.tombstone(id, seq);
            out.removed(id, TopicGone::Deleted);
            return;
        };
        match self.entries.get(at).map(|e| e.meta.judge(seq)) {
            Some(Verdict::Apply) => {
                self.entries.remove(at);
                self.tombstone(id, seq);
                out.removed(id, TopicGone::Deleted);
            }
            Some(Verdict::Stale | Verdict::Conflict) => self.settle_or_back_off(id, seq, now),
            None => {}
        }
    }

    /// After a by-id result dispatched as `seq` answered `id`: it stays due
    /// (backing off) unless every group's last snapshot postdates its last
    /// live change and no deletion is suspected.
    fn settle_or_back_off(&mut self, id: TopicId, seq: FetchSeq, now: Instant) {
        if self.entry(id).is_some_and(TopicEntry::settled) {
            self.settle(id, seq);
        } else {
            self.back_off(id, now);
        }
    }

    /// A topic new to the list that arrives hidden (General) and is the
    /// open one: the consumer showing it must learn it is hidden, though
    /// it never saw it shown.
    fn note_new_hidden(&self, entry: &TopicEntry, out: &mut Emitted) {
        if !entry.shown() && self.open == Some(entry.id()) {
            out.removed(entry.id(), TopicGone::Hidden);
        }
    }

    /// An entry for a topic new to the list, unless a tombstone newer than
    /// `seq` blocks it. A result dispatched before a live event that touched
    /// an unlisted topic may predate that event for this one, so its groups
    /// start changed and the topic due.
    fn admit_new(
        &mut self,
        snap: TopicSnapshot,
        seq: FetchSeq,
        now: Instant,
    ) -> Option<TopicEntry> {
        let id = snap.topic.id;
        if self.tombstones.blocks(id, seq) {
            return None;
        }
        self.tombstones.forget(id);
        self.discovery.remove(&id);
        let mut entry = TopicEntry::new(snap, seq);
        if seq <= self.unlisted_change {
            for stamp in [&mut entry.meta, &mut entry.unread, &mut entry.activity] {
                stamp.set_changed(self.unlisted_change);
            }
            entry.owe(now);
        }
        Some(entry)
    }
}

/// Retained topics a same-order first page should have contained become
/// due: pinned ones (pinned topics lead every first page), every one when
/// the page ends the list, and unpinned ones ranking above its last.
fn mark_missed_first(
    old: &mut [TopicEntry],
    page: &[TopicEntry],
    order: TopicOrder,
    rest: Rest,
    now: Instant,
) {
    let boundary = page
        .iter()
        .rev()
        .find(|e| !e.topic.pinned)
        .map(|e| &e.topic);
    for entry in old {
        let missed = entry.topic.pinned
            || rest == Rest::Last
            || boundary.is_some_and(|b| rank(order, &entry.topic, b) == Ordering::Less);
        if missed {
            entry.owe(now);
        }
    }
}

/// Sorts a merged entry into touched (shown and changed, or newly shown) or
/// removed (newly hidden).
fn classify(entry: Option<&TopicEntry>, was: &Topic, was_vis: Visibility, out: &mut Emitted) {
    let Some(entry) = entry else {
        return;
    };
    match (was_vis == Visibility::Shown, entry.shown()) {
        (true, false) => out.removed(entry.id(), TopicGone::Hidden),
        (false, true) => out.appended(entry.topic.clone()),
        (true, true) if entry.topic != *was => out.appended(entry.topic.clone()),
        _ => {}
    }
}

fn paging_after(rest: Rest) -> Paging {
    match rest {
        Rest::More => Paging::More,
        Rest::Last => Paging::Complete,
    }
}
