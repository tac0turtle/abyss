//! The topic-list memory budget (plan section 9): **retained payload
//! capacity**, worst case.
//!
//! Counted:
//! - every container's allocated capacity (not its length): the entries
//!   `Vec`, every title and preview `String`, tombstones, the in-flight id
//!   list, and the outer `Topics` map (whose buckets hold the lists inline)
//!   and LRU;
//! - B-tree storage (discovery, badge stamps, kind-check marks) at its
//!   worst-case node count and size;
//! - [`ALLOC_OVERHEAD`] for every heap allocation (each string, vector and
//!   B-tree node).
//!
//! Not counted: allocator fragmentation and memory the allocator keeps
//! after a free, and transient peaks inside one operation (a first-page
//! merge briefly holds the old and the new entries).
//!
//! The measured test drives every list to its worst case (paging to the cap,
//! first-page refreshes over a full list, discovery and by-id inserts at the
//! cap, open-topic churn) with maximal titles and previews whose strings
//! carry spare capacity, and checks the measured capacities against the
//! bound from the constants. The bound for all lists, the badge stamps and
//! the kind-check FIFO must leave at least 10% of 4 MiB free.

use std::mem::size_of;
use std::time::Instant;

use super::*;
use crate::model::{ChatId, MediaKind};
use crate::sanitize::MAX_PREVIEW_BYTES;
use crate::session::MAX_PINNED_PEERS;
use crate::store::topics::entry::TopicEntry;
use crate::store::topics::{MAX_DISCOVERY, MAX_ENTRIES, MAX_TOMBSTONES, MAX_TOPICS_BY_ID, Paging};
use crate::store::{MAX_DIALOGS, MAX_TOPIC_LISTS, Topics};

/// Bytes charged per heap allocation: glibc's malloc adds an 8-byte chunk
/// header and rounds to 16, so at most 23; 32 is charged.
const ALLOC_OVERHEAD: usize = 32;
/// The budget: 4 MiB with a 10% margin.
const BUDGET: usize = 4 * 1024 * 1024 * 9 / 10;

/// A `BTreeMap`/`BTreeSet` of `len` elements of `kv` bytes, worst case: a
/// leaf holds 11 elements and a 16-byte header, an internal node also 12
/// child pointers; every node but the root holds at least 5 elements, so an
/// internal node (but the root) has at least 6 children.
fn btree_bytes(len: usize, kv: usize) -> usize {
    let leaves = len.div_ceil(5) + 1;
    let internal = leaves.div_ceil(5) + 1;
    let leaf = 11 * kv + 16 + ALLOC_OVERHEAD;
    let node = leaf + 12 * size_of::<usize>();
    leaves * leaf + internal * node
}

/// A string's (or vector's) allocated bytes, with the allocation overhead.
fn heap(capacity: usize) -> usize {
    if capacity == 0 {
        0
    } else {
        capacity + ALLOC_OVERHEAD
    }
}

/// The outer `Topics` containers: hashbrown buckets (a power of two at
/// least 8/7 of the capacity, at least 4) each holding a chat id and a
/// whole list, one control byte per bucket plus a 16-byte group, and the
/// LRU vector.
fn outer(map_capacity: usize, lru_capacity: usize) -> usize {
    let buckets = (map_capacity * 8 / 7 + 1).next_power_of_two().max(4);
    heap(buckets * (size_of::<ChatId>() + size_of::<TopicList>()) + buckets + 16)
        + heap(lru_capacity * size_of::<ChatId>())
}

/// A string of `len` ASCII bytes with four times that capacity.
fn padded(c: char, len: usize) -> String {
    let mut s = String::with_capacity(4 * len);
    s.extend(std::iter::repeat_n(c, len));
    s
}

/// Topic `n` with a maximal title and preview, both over-allocated.
fn heavy(n: i32) -> TopicSnapshot {
    snap(Topic {
        title: padded('t', MAX_NAME_BYTES),
        last: Some(Preview::Text(padded('p', MAX_PREVIEW_BYTES))),
        ..topic(n)
    })
}

fn heavy_page(ns: impl Iterator<Item = i32>, rest: Rest) -> PageSnapshot {
    page(TopicOrder::LastActivity, ns.map(heavy).collect(), rest)
}

/// Drives one list through the worst case.
fn churn(l: &mut TopicList, base: i32, now: Instant) {
    let mut kind = PageKind::First;
    let mut top = base + 2_000;
    while l.paging() == Paging::More {
        load(
            l,
            kind,
            heavy_page((0..50).map(|i| top - i), Rest::More),
            now,
        );
        top -= 50;
        kind = PageKind::Next;
    }
    // First-page refreshes over a full list, with new topics at the top.
    for round in 0..3 {
        let fresh = (0..50).map(|i| base + 3_000 + round * 50 - i);
        load(l, PageKind::First, heavy_page(fresh, Rest::More), now);
    }
    // Discovery and by-id inserts at the cap, and open-topic churn.
    for round in 0..5 {
        let open = tid(base + 5_000 + round);
        l.set_open(Some(open));
        for i in 0..i32::try_from(MAX_DISCOVERY).unwrap() {
            l.discover(tid(base + 6_000 + round * 100 + i));
        }
        while let Some(call) = l.dispatch_by_id(now) {
            let found = call.ids.iter().map(|id| heavy(id.get())).collect();
            l.apply_by_id(call.seq, by_id(found, vec![]), now).unwrap();
        }
        l.set_open(None);
    }
    l.request_page(PageKind::First);
    l.dispatch_page();
    l.discover(tid(base + 9_000));
    l.dispatch_by_id(now);
}

/// Heap bytes a list retains beyond its inline struct (which the outer map
/// holds), from its actual capacities.
fn measured(l: &TopicList) -> usize {
    let strings: usize = l
        .entries
        .iter()
        .map(|e| {
            let preview = match &e.topic.last {
                Some(Preview::Text(s) | Preview::Media(MediaKind::Document { name: s, .. })) => {
                    heap(s.capacity())
                }
                _ => 0,
            };
            heap(e.topic.title.capacity()) + preview
        })
        .sum();
    let in_flight = l
        .by_id
        .as_ref()
        .map_or(0, |c| heap(c.ids.capacity() * size_of::<TopicId>()));
    heap(l.entries.capacity() * size_of::<TopicEntry>())
        + strings
        + heap(l.tombstones.capacity() * size_of::<(TopicId, FetchSeq)>())
        + btree_bytes(l.discovery.len(), size_of::<TopicId>())
        + in_flight
}

/// The most a list can retain beyond its inline struct, from the constants.
fn list_bound() -> usize {
    heap(MAX_ENTRIES * size_of::<TopicEntry>())
        + MAX_ENTRIES * (heap(MAX_NAME_BYTES) + heap(MAX_PREVIEW_BYTES))
        + heap(MAX_TOMBSTONES * size_of::<(TopicId, FetchSeq)>())
        + btree_bytes(MAX_DISCOVERY, size_of::<TopicId>())
        + heap(MAX_TOPICS_BY_ID * size_of::<TopicId>())
}

/// The worst case for the whole account: the outer containers, every list,
/// the badge stamps (one per retained dialog, plus the pane's chat when it
/// isn't listed: `Badges::track_extra`) and the kind-check FIFO with
/// its per-chat marks (bounded by the required-peer set; task 8 builds them,
/// so they are charged generously: a FIFO of twice the bound, and 16 bytes
/// of marks per chat).
fn account_bound() -> usize {
    let badges = btree_bytes(
        MAX_DIALOGS + 1,
        size_of::<ChatId>() + crate::store::badge::CHAT_BADGE_BYTES,
    );
    let kind_fifo = heap(2 * MAX_PINNED_PEERS * size_of::<ChatId>());
    let kind_marks = btree_bytes(MAX_PINNED_PEERS, size_of::<ChatId>() + 16);
    outer(MAX_TOPIC_LISTS, MAX_TOPIC_LISTS + 1)
        + MAX_TOPIC_LISTS * list_bound()
        + badges
        + kind_fifo
        + kind_marks
}

#[test]
fn retained_capacity_stays_within_the_bound() {
    let now = Instant::now();
    let mut topics = Topics::default();
    let bound = list_bound();
    // One more forum than retained: the least recently used list is evicted.
    for f in 0..=i64::try_from(MAX_TOPIC_LISTS).unwrap() {
        let forum = ChatId {
            kind: PeerKind::Channel,
            id: f,
        };
        let l = topics.open(forum);
        churn(l, i32::try_from(f).unwrap() * 10_000 + 1, now);
        check_invariants(l).unwrap();
        assert!(
            l.entries.capacity() <= MAX_ENTRIES,
            "{} entries allocated",
            l.entries.capacity()
        );
        assert!(l.tombstones.capacity() <= MAX_TOMBSTONES);
        assert!(l.discovery.len() <= MAX_DISCOVERY);
        let bytes = measured(l);
        assert!(bytes <= bound, "list {f}: {bytes} > {bound}");
    }
    let (map, lru) = topics.capacities();
    assert_eq!(
        (map, lru),
        Topics::default().capacities(),
        "the outer containers never grow"
    );
    assert_eq!(topics.chats().len(), MAX_TOPIC_LISTS);
    let lists: usize = topics
        .chats()
        .iter()
        .filter_map(|c| topics.get(*c))
        .map(measured)
        .sum();
    let total = outer(map, lru) + lists;
    assert!(total <= outer(MAX_TOPIC_LISTS, MAX_TOPIC_LISTS + 1) + MAX_TOPIC_LISTS * bound);
}

#[test]
fn the_account_bound_leaves_a_tenth_of_four_mib() {
    let total = account_bound();
    assert!(total <= BUDGET, "{total} bytes > {BUDGET}");
}
