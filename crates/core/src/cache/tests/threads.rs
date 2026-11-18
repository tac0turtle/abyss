//! Schema version 4 (plan stage 2, sections 10 and 11): rows carry their
//! topic, pages and reconciles are scoped to one thread, a gap replaces only
//! that thread's rows, and rows whose key disagrees with their body are
//! deleted.

use std::collections::BTreeMap;

use proptest::prelude::*;

use super::{ids, msg, setup, summary, user};
use crate::cache::db::Page;
use crate::cache::worker::{CacheCmd, reconcile_tail};
use crate::cache::{Cache, CacheHandle};
use crate::model::{ChatId, Message, MsgId, PeerKind, ThreadKey, TopicId};

/// A forum: forums are channels, whose message ids are unique per chat only.
fn forum() -> ChatId {
    ChatId {
        kind: PeerKind::Channel,
        id: 9,
    }
}

fn topic(id: i32) -> TopicId {
    TopicId::try_from(id).unwrap()
}

fn key(chat: ChatId, t: i32) -> ThreadKey {
    ThreadKey {
        chat,
        topic: Some(topic(t)),
    }
}

/// The forum's thread `t`: its whole-chat thread for 0, otherwise topic `t`.
fn thread(t: i32) -> ThreadKey {
    ThreadKey {
        chat: forum(),
        topic: (t > 0).then(|| topic(t)),
    }
}

/// Message `id` of the forum, in thread `t` (see [`thread`]).
fn post(id: i32, t: i32) -> Message {
    let mut m = msg(forum(), id, "");
    m.thread = thread(t);
    m
}

fn posts(t: i32, ids: impl IntoIterator<Item = i32>) -> Vec<Message> {
    ids.into_iter().map(|id| post(id, t)).collect()
}

/// Every stored row of the forum as `id -> topic` column (0 for none), read
/// straight from the table: no thread key addresses every row.
fn stored(db: &mut Cache) -> BTreeMap<i32, i32> {
    let mut stmt = db
        .conn()
        .prepare("SELECT id, topic FROM messages WHERE kind = 2 AND chat = ?1")
        .unwrap();
    stmt.query_map([forum().id], |r| {
        Ok((r.get(0)?, i32::try_from(r.get::<_, i64>(1)?).unwrap()))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

fn row_count(db: &Cache) -> i64 {
    db.conn()
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap()
}

/// Section 11: thread pages return only their thread.
#[test]
fn thread_pages_return_only_their_thread() {
    let (_t, dir, key_) = setup();
    let mut db = Cache::open(&dir, &key_).unwrap();
    let (a, b) = (key(forum(), 2), key(forum(), 3));
    db.put_messages(a, &posts(2, [1, 3, 5, 7])).unwrap();
    db.put_messages(b, &posts(3, [2, 4, 6, 8])).unwrap();
    assert_eq!(ids(&db.tail(a, 10).unwrap()), [1, 3, 5, 7]);
    assert_eq!(ids(&db.tail(a, 2).unwrap()), [5, 7]);
    assert_eq!(ids(&db.tail(b, 2).unwrap()), [6, 8]);
    assert_eq!(ids(&db.before(a, MsgId(6), 2).unwrap()), [3, 5]);
    assert_eq!(ids(&db.after(b, MsgId(3), 2).unwrap()), [4, 6]);
    assert!(db.tail(key(forum(), 4), 10).unwrap().is_empty());
    assert!(
        db.tail(key(user(1), 2), 10).unwrap().is_empty(),
        "another chat"
    );
}

/// A whole-chat key addresses exactly the thread `{ chat, topic: None }`:
/// the rows with topic 0 (plan stage 2, task 7). Core files a forum's
/// messages under their topics, so writes through a whole-chat key skip
/// them, its pages never return them, and its reconciles and replaces never
/// touch them. A non-forum chat only has topic-0 rows.
#[test]
fn a_whole_chat_key_addresses_only_topic_0() {
    let (_t, dir, key_) = setup();
    let mut db = Cache::open(&dir, &key_).unwrap();
    let whole = ThreadKey::whole(forum());
    let mut mixed = posts(2, [1, 3]);
    mixed.extend(posts(3, [2, 4]));
    mixed.push(post(5, 0));
    db.put_messages(whole, &mixed).unwrap();
    assert_eq!(stored(&mut db), BTreeMap::from([(5, 0)]), "topics skipped");
    db.put_messages(key(forum(), 2), &posts(2, [1, 3])).unwrap();
    db.put_messages(key(forum(), 3), &posts(3, [2, 4])).unwrap();
    assert_eq!(ids(&db.tail(whole, 10).unwrap()), [5]);
    assert!(db.before(whole, MsgId(5), 10).unwrap().is_empty());
    assert_eq!(ids(&db.after(whole, MsgId(0), 10).unwrap()), [5]);

    // Overlap with the whole thread's own newest row (5), not the chat's.
    let gone = reconcile_tail(&mut db, whole, &posts(0, [5, 6]), Some(MsgId(5))).unwrap();
    assert!(gone.is_empty());
    assert_eq!(
        stored(&mut db),
        BTreeMap::from([(1, 2), (2, 3), (3, 2), (4, 3), (5, 0), (6, 0)])
    );
    // A gap replaces the whole thread's rows only; the topics' rows stay.
    let gone = reconcile_tail(&mut db, whole, &posts(0, [9]), Some(MsgId(9))).unwrap();
    assert_eq!(gone, [MsgId(5), MsgId(6)]);
    assert_eq!(
        stored(&mut db),
        BTreeMap::from([(1, 2), (2, 3), (3, 2), (4, 3), (9, 0)])
    );
}

/// `put_messages` writes only the messages its thread addresses.
#[test]
fn put_messages_skips_other_threads() {
    let (_t, dir, key_) = setup();
    let mut db = Cache::open(&dir, &key_).unwrap();
    let mut batch = posts(2, [1, 2]);
    batch.extend(posts(3, [3]));
    batch.push(msg(forum(), 4, "whole chat"));
    batch.push(msg(user(1), 5, "another chat"));
    db.put_messages(key(forum(), 2), &batch).unwrap();
    assert_eq!(stored(&mut db), BTreeMap::from([(1, 2), (2, 2)]));
    assert!(db.tail(ThreadKey::whole(user(1)), 10).unwrap().is_empty());
}

/// Section 11: `reconcile_range` on one thread never deletes another's rows,
/// even where their ids interleave inside the range.
#[test]
fn reconcile_range_spares_sibling_topics() {
    let (_t, dir, key_) = setup();
    let mut db = Cache::open(&dir, &key_).unwrap();
    db.put_messages(key(forum(), 2), &posts(2, [1, 3, 5]))
        .unwrap();
    db.put_messages(key(forum(), 3), &posts(3, [2, 4, 6]))
        .unwrap();
    let gone = db
        .reconcile_range(key(forum(), 2), MsgId(1), MsgId(6), &posts(2, [5]))
        .unwrap();
    assert_eq!(gone, [MsgId(1), MsgId(3)]);
    assert_eq!(
        stored(&mut db),
        BTreeMap::from([(2, 3), (4, 3), (5, 2), (6, 3)])
    );
}

/// Section 6.6: a forum flag change deletes every cached message of the
/// chat, whatever its thread, and no other chat's.
#[test]
fn deleting_a_chat_takes_every_thread_and_spares_other_chats() {
    let (_t, dir, key_) = setup();
    let mut db = Cache::open(&dir, &key_).unwrap();
    db.put_messages(key(forum(), 2), &posts(2, [1, 3])).unwrap();
    db.put_messages(key(forum(), 3), &posts(3, [2])).unwrap();
    db.put_messages(ThreadKey::whole(user(1)), &[msg(user(1), 5, "kept")])
        .unwrap();
    db.delete_chat_messages(forum()).unwrap();
    assert!(stored(&mut db).is_empty());
    assert_eq!(row_count(&db), 1);
}

/// Section 11: a `ReconcileTail` that overlaps its thread's cache reconciles
/// only that thread's ids in the tail's range.
#[tokio::test]
async fn reconcile_tail_with_overlap_only_touches_its_thread() {
    let (_t, dir, key_) = setup();
    let handle = CacheHandle::spawn(dir, &key_).unwrap();
    let (a, b) = (key(forum(), 2), key(forum(), 3));
    handle
        .write(CacheCmd::PutMessages(a, posts(2, [10, 20, 30, 40])))
        .await;
    handle
        .write(CacheCmd::PutMessages(b, posts(3, [15, 25, 35, 45])))
        .await;
    // A's server tail starts at 20 (cached), lacks 30 and adds 50.
    handle
        .write(CacheCmd::ReconcileTail {
            thread: a,
            messages: posts(2, [20, 40, 50]),
            from: Some(MsgId(20)),
        })
        .await;
    assert_eq!(ids(&handle.tail(a, 10).await.unwrap()), [10, 20, 40, 50]);
    assert_eq!(ids(&handle.tail(b, 10).await.unwrap()), [15, 25, 35, 45]);
}

/// The section 10 example. Topic A's cache ends at 100; 101–200 arrived
/// while A had no window and were not cached. On reopen the server tail is
/// 150–250. Overlap is judged against A's own newest row (100), not the
/// chat's (B's 160), so the tail is a gap: A's rows are replaced, B's rows
/// (inside and outside that range) are kept, and nothing between 100 and
/// 150 is served for A.
#[tokio::test]
async fn reconcile_tail_gap_replaces_only_its_thread() {
    let (_t, dir, key_) = setup();
    let handle = CacheHandle::spawn(dir, &key_).unwrap();
    let (a, b) = (key(forum(), 2), key(forum(), 3));
    handle
        .write(CacheCmd::PutMessages(a, posts(2, [60, 80, 100])))
        .await;
    handle
        .write(CacheCmd::PutMessages(b, posts(3, [90, 120, 160, 300])))
        .await;
    handle
        .write(CacheCmd::ReconcileTail {
            thread: a,
            messages: posts(2, [150, 200, 250]),
            from: Some(MsgId(150)),
        })
        .await;
    assert_eq!(ids(&handle.tail(a, 10).await.unwrap()), [150, 200, 250]);
    assert!(handle.before(a, MsgId(150), 10).await.unwrap().is_empty());
    assert_eq!(ids(&handle.tail(b, 10).await.unwrap()), [90, 120, 160, 300]);
}

/// Section 11: migration 3 → 4 drops messages and their LRU rows and adds
/// the topic column. It keeps the chat list, but the upgrade continues to
/// version 5, which drops it (`ChatSummary::folder`, folders plan part A),
/// and on to version 6.
#[test]
fn version_3_messages_and_chats_are_dropped() {
    let (_t, dir, key_) = setup();
    let c = user(1);
    {
        let mut db = Cache::open(&dir, &key_).unwrap();
        db.put_chats(&[summary(c, "dropped by 4 to 5")]).unwrap();
        db.touch(c).unwrap();
        let body = postcard::to_allocvec(&msg(c, 1, "v3")).unwrap();
        // The version 3 layout: no topic column, no thread index.
        db.conn()
            .execute_batch(
                "DROP TABLE messages;
                 CREATE TABLE messages (
                     kind INTEGER NOT NULL, chat INTEGER NOT NULL, id INTEGER NOT NULL,
                     size INTEGER NOT NULL, body BLOB NOT NULL,
                     PRIMARY KEY (kind, chat, id)) WITHOUT ROWID;
                 CREATE INDEX messages_account_id ON messages (id) WHERE kind IN (0, 1);
                 PRAGMA user_version = 3;",
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO messages (kind, chat, id, size, body) VALUES (0, 1, 1, ?1, ?2)",
                rusqlite::params![i64::try_from(body.len()).unwrap(), body],
            )
            .unwrap();
    }
    let mut db = Cache::open(&dir, &key_).unwrap();
    assert!(db.chats().unwrap().is_empty());
    assert_eq!(row_count(&db), 0);
    assert_eq!(db.total_bytes(), 0);
    let lru: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM cached_chats", [], |r| r.get(0))
        .unwrap();
    assert_eq!(lru, 0);
    let version: i64 = db
        .conn()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 8);
    db.put_messages(key(forum(), 2), &posts(2, [1])).unwrap();
    assert_eq!(ids(&db.tail(key(forum(), 2), 10).unwrap()), [1]);
}

/// `m` encoded with its topic id replaced by `raw`, which `TopicId` refuses
/// for `raw <= 0`. Postcard writes fields in order, so the topic follows the
/// id and chat: an `Option` tag (1) and the zigzag varint of the id.
fn body_with_topic(m: &Message, raw: i8) -> Vec<u8> {
    let mut body = postcard::to_allocvec(m).unwrap();
    let at = postcard::to_allocvec(&(m.id, m.thread.chat)).unwrap().len();
    let stored = m.thread.topic.unwrap().get();
    assert!((1..64).contains(&stored), "a one-byte varint");
    assert_eq!(body[at..at + 2], [1, u8::try_from(stored * 2).unwrap()]);
    body[at + 1] = u8::try_from((i16::from(raw) << 1) ^ (i16::from(raw) >> 15)).unwrap();
    body
}

/// Section 11: rows whose stored topic disagrees with the body, whose column
/// is negative, or whose body holds a non-positive topic id are bad rows:
/// deleted and skipped by the reads that select them.
#[test]
fn rows_with_a_bad_topic_are_deleted() {
    let (_t, dir, key_) = setup();
    let mut db = Cache::open(&dir, &key_).unwrap();
    let good = key(forum(), 3);
    db.put_messages(good, &posts(3, [10])).unwrap();
    let encode = |m: &Message| postcard::to_allocvec(m).unwrap();
    let rows: [(i32, i64, Vec<u8>); 7] = [
        (1, 2, encode(&post(1, 3))),              // column 2, body topic 3
        (2, 0, encode(&post(2, 3))),              // column 0, body topic 3
        (3, 3, encode(&msg(forum(), 3, ""))),     // column 3, body no topic
        (4, -1, encode(&msg(forum(), 4, ""))),    // negative column
        (5, -3, encode(&post(5, 3))),             // negative column
        (6, 3, body_with_topic(&post(6, 3), 0)),  // body `TopicId(0)`
        (7, 3, body_with_topic(&post(7, 3), -1)), // body `TopicId(-1)`
    ];
    for (id, topic_col, body) in rows {
        db.conn()
            .execute(
                "INSERT INTO messages (kind, chat, id, topic, size, body)
                 VALUES (2, 9, ?1, ?2, 1, ?3)",
                rusqlite::params![id, topic_col, body],
            )
            .unwrap();
    }
    // The topic page selects the rows filed under topic 3 and deletes the
    // bad ones among them.
    assert_eq!(ids(&db.tail(good, 10).unwrap()), [10]);
    assert_eq!(row_count(&db), 5, "ids 3, 6 and 7 deleted");
    // The whole-chat page selects the topic-0 rows, and topic 2's page the
    // row filed under 2: both disagree with their bodies.
    assert!(db.tail(ThreadKey::whole(forum()), 10).unwrap().is_empty());
    assert!(db.tail(key(forum(), 2), 10).unwrap().is_empty());
    // A negative column matches no thread: never served, left to the
    // chat's trimming and eviction.
    assert_eq!(stored(&mut db), BTreeMap::from([(4, -1), (5, -3), (10, 3)]));
}

/// `messages_thread` serves every production topic page without scanning
/// the chat or sorting, and a whole-chat page keeps the primary key. The
/// plan is SQLite's for the exact SQL and parameters `Cache::page` runs.
#[test]
fn topic_pages_use_the_thread_index() {
    let (_t, dir, key_) = setup();
    let db = Cache::open(&dir, &key_).unwrap();
    for page in [Page::Tail, Page::Before, Page::After] {
        let plan = db.page_plan(key(forum(), 3), page).unwrap();
        assert!(
            plan.contains("USING INDEX messages_thread"),
            "{page:?}: {plan}"
        );
        assert!(!plan.contains("TEMP B-TREE"), "{page:?} sorts: {plan}");
        let plan = db.page_plan(ThreadKey::whole(forum()), page).unwrap();
        assert!(plan.contains("USING PRIMARY KEY"), "{page:?} whole: {plan}");
        assert!(
            !plan.contains("TEMP B-TREE"),
            "{page:?} whole sorts: {plan}"
        );
    }
}

/// A forum's history: each id's thread (0 for the whole-chat thread, else
/// topics 1–3), and whether it is cached.
fn history() -> impl Strategy<Value = Vec<(i32, i32, bool)>> {
    prop::collection::vec((0..=3i32, any::<bool>()), 1..40).prop_map(|v| {
        v.into_iter()
            .zip(1..)
            .map(|((t, cached), id)| (id, t, cached))
            .collect()
    })
}

fn cached(history: &[(i32, i32, bool)]) -> BTreeMap<i32, i32> {
    history
        .iter()
        .filter(|(_, _, c)| *c)
        .map(|&(id, t, _)| (id, t))
        .collect()
}

fn fill(db: &mut Cache, rows: &BTreeMap<i32, i32>) {
    for t in 0..=3 {
        let ids = rows.iter().filter(|(_, rt)| **rt == t).map(|(id, _)| *id);
        db.put_messages(thread(t), &posts(t, ids)).unwrap();
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Section 11: over interleaved topics, `reconcile_range` on one thread
    /// leaves every other thread's rows as they were, and makes its own
    /// rows in the range equal the server page. Server messages of other
    /// threads are ignored. A whole-chat key (thread 0) is one more
    /// thread: it addresses only topic-0 rows.
    #[test]
    fn reconcile_range_never_touches_another_thread(
        history in history(),
        t in 0..=3i32,
        server in prop::collection::vec(any::<bool>(), 40),
        (lo, hi) in (0..=41i32, 0..=41i32),
    ) {
        let (_t, dir, key_) = setup();
        let mut db = Cache::open(&dir, &key_).unwrap();
        let before = cached(&history);
        fill(&mut db, &before);
        let ours = |rt: i32| rt == t;
        let in_range = |id: i32| (lo..=hi).contains(&id);
        let page: Vec<Message> = history
            .iter()
            .zip(&server)
            .filter(|(_, s)| **s)
            .map(|(&(id, t, _), _)| post(id, t))
            .collect();
        let gone = db
            .reconcile_range(thread(t), MsgId(lo), MsgId(hi), &page)
            .unwrap();

        let served: BTreeMap<i32, i32> = page
            .iter()
            .map(|m| (m.id.0, m.thread.topic.map_or(0, TopicId::get)))
            .filter(|&(id, rt)| ours(rt) && in_range(id))
            .collect();
        let mut expected: BTreeMap<i32, i32> = before
            .iter()
            .filter(|&(&id, &rt)| !(ours(rt) && in_range(id)))
            .map(|(&id, &rt)| (id, rt))
            .collect();
        expected.extend(&served);
        prop_assert_eq!(stored(&mut db), expected);
        let expected_gone: Vec<MsgId> = before
            .iter()
            .filter(|&(&id, &rt)| ours(rt) && in_range(id) && !served.contains_key(&id))
            .map(|(&id, _)| MsgId(id))
            .collect();
        prop_assert_eq!(gone, expected_gone);
    }

    /// Section 11: `ReconcileTail` on thread `t` (0: the whole-chat
    /// thread), with or without overlap, never changes a sibling's rows. Its own rows from the tail's
    /// start on equal the tail; below it they are kept only when the tail
    /// overlaps `t`'s own newest cached row, so a gap is never bridged.
    #[test]
    fn reconcile_tail_never_touches_another_thread(
        history in history(),
        t in 0..=3i32,
        tail in prop::collection::vec(any::<bool>(), 40),
        first in prop::option::of(1..=41i32),
    ) {
        let (_t, dir, key_) = setup();
        let mut db = Cache::open(&dir, &key_).unwrap();
        let before = cached(&history);
        fill(&mut db, &before);
        let start = first.unwrap_or(i32::MIN);
        let page: Vec<Message> = history
            .iter()
            .zip(&tail)
            .filter(|&(&(id, ht, _), &s)| s && ht == t && id >= start)
            .map(|(&(id, ht, _), _)| post(id, ht))
            .collect();
        reconcile_tail(&mut db, thread(t), &page, first.map(MsgId)).unwrap();

        let newest = before.iter().filter(|(_, bt)| **bt == t).map(|(id, _)| *id).max();
        let overlap = first.is_some() && newest.is_none_or(|n| start <= n);
        let mut expected: BTreeMap<i32, i32> = before
            .iter()
            .filter(|&(&id, &bt)| bt != t || (overlap && id < start))
            .map(|(&id, &bt)| (id, bt))
            .collect();
        expected.extend(page.iter().map(|m| (m.id.0, t)));
        prop_assert_eq!(stored(&mut db), expected);
    }
}
