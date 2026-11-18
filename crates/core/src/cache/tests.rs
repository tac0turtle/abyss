use super::*;
use crate::model::{
    Body, ChatId, ChatKind, ChatSummary, Message, MsgId, PeerFolder, PeerKind, ReadState, ThreadKey,
};
use crate::rich_text::RichText;

const SECRET: &str = "very-secret-plaintext-marker";

/// Task 6c: `close` processes queued writes, closes the database before it
/// replies, and stops the worker; the data is on disk for the next open.
#[tokio::test]
async fn close_flushes_closes_and_stops_the_worker() {
    let (_t, dir, key) = setup();
    let c = user(1);
    let handle = CacheHandle::spawn(dir.clone(), &key).unwrap();
    handle
        .write(worker::CacheCmd::PutMessages(
            ThreadKey::whole(c),
            vec![msg(c, 1, "a")],
        ))
        .await;
    handle.close().await.unwrap();
    assert!(matches!(handle.chats().await, Err(WorkerError::Stopped)));
    let mut db = Cache::open(&dir, &key).unwrap();
    assert_eq!(ids(&db.tail(ThreadKey::whole(c), 10).unwrap()), vec![1]);
}

/// Exit SIGSEGV fix: a worker is in the census from before its open until
/// its connection is closed, whether it stops by `close`, `erase`, or by
/// every handle being dropped (with no reply anyone could wait on).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_leaves_the_census_only_after_closing() {
    let (_t, dir, key) = setup();
    let none = std::time::Duration::ZERO;
    let enough = std::time::Duration::from_secs(10);
    let census =
        |d: std::path::PathBuf, t| tokio::task::spawn_blocking(move || wait_closed(Some(&d), t));
    assert!(census(dir.clone(), none).await.unwrap());

    let handle = CacheHandle::spawn(dir.clone(), &key).unwrap();
    assert!(
        !census(dir.clone(), none).await.unwrap(),
        "open while a handle lives"
    );
    handle.close().await.unwrap();
    assert!(
        census(dir.clone(), none).await.unwrap(),
        "closed before `close` replies"
    );

    let handle = CacheHandle::spawn(dir.clone(), &key).unwrap();
    let c = user(1);
    handle
        .write(worker::CacheCmd::PutMessages(
            ThreadKey::whole(c),
            vec![msg(c, 1, "a")],
        ))
        .await;
    drop(handle);
    assert!(
        census(dir.clone(), enough).await.unwrap(),
        "dropped handles close"
    );

    let handle = CacheHandle::spawn(dir.clone(), &key).unwrap();
    let erased = handle.erase(key, Box::new(|| Ok(()))).await;
    assert!(erased.is_ok());
    assert!(
        census(dir, none).await.unwrap(),
        "closed before `erase` replies"
    );
}

fn user(id: i64) -> ChatId {
    ChatId {
        kind: PeerKind::User,
        id,
    }
}

fn msg(c: ChatId, id: i32, text: &str) -> Message {
    Message {
        id: MsgId(id),
        thread: ThreadKey {
            chat: c,
            topic: None,
        },
        sender: None,
        date: 0,
        edit_date: None,
        outgoing: false,
        reply_to: None,
        body: Body::Text(RichText::plain(text)),
    }
}

fn summary(c: ChatId, title: &str) -> ChatSummary {
    ChatSummary {
        id: c,
        title: title.into(),
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
        topic_unread: None,
    }
}

fn ids(v: &[Message]) -> Vec<i32> {
    v.iter().map(|m| m.id.0).collect()
}

fn setup() -> (tempfile::TempDir, std::path::PathBuf, CacheKey) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("cache");
    (tmp, dir, CacheKey::generate().unwrap())
}

#[test]
fn roundtrip_survives_reopen() {
    let (_t, dir, key) = setup();
    let c = user(1);
    {
        let mut db = Cache::open(&dir, &key).unwrap();
        db.put_chats(&[summary(user(2), "b"), summary(c, "a")])
            .unwrap();
        db.put_messages(
            ThreadKey::whole(c),
            &[msg(c, 3, "x"), msg(c, 1, "y"), msg(c, 2, "z")],
        )
        .unwrap();
    }
    let mut db = Cache::open(&dir, &key).unwrap();
    let chats = db.chats().unwrap();
    assert_eq!(
        chats.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(),
        ["b", "a"]
    );
    assert_eq!(ids(&db.tail(ThreadKey::whole(c), 2).unwrap()), [2, 3]);
    assert_eq!(
        ids(&db.before(ThreadKey::whole(c), MsgId(3), 10).unwrap()),
        [1, 2]
    );
    assert_eq!(
        ids(&db.after(ThreadKey::whole(c), MsgId(1), 1).unwrap()),
        [2]
    );
}

#[test]
fn wrong_key_is_detected_and_destroy_recovers() {
    let (_t, dir, key) = setup();
    Cache::open(&dir, &key)
        .unwrap()
        .put_messages(ThreadKey::whole(user(1)), &[msg(user(1), 1, "a")])
        .unwrap();
    let other = CacheKey::generate().unwrap();
    assert!(matches!(
        Cache::open(&dir, &other),
        Err(CacheError::WrongKey)
    ));
    destroy_unopened(&dir).unwrap();
    assert!(!dir.exists());
    let mut db = Cache::open(&dir, &other).unwrap();
    assert!(db.tail(ThreadKey::whole(user(1)), 10).unwrap().is_empty());
}

#[test]
fn files_on_disk_are_encrypted() {
    let (_t, dir, key) = setup();
    {
        let mut db = Cache::open(&dir, &key).unwrap();
        db.put_chats(&[summary(user(1), SECRET)]).unwrap();
        db.put_messages(ThreadKey::whole(user(1)), &[msg(user(1), 1, SECRET)])
            .unwrap();
        // Leave the WAL populated: no checkpoint before inspecting.
        for name in std::iter::once("cache.db").chain(SIDE_FILES) {
            let Ok(bytes) = std::fs::read(dir.join(name)) else {
                continue;
            };
            assert!(
                !bytes.starts_with(b"SQLite format 3"),
                "{name} has a plaintext header"
            );
            let found = bytes.windows(SECRET.len()).any(|w| w == SECRET.as_bytes());
            assert!(!found, "{name} contains plaintext");
        }
    }
}

#[test]
fn directory_is_private_and_excluded_from_backup() {
    use std::os::unix::fs::PermissionsExt;
    let (_t, dir, key) = setup();
    Cache::open(&dir, &key).unwrap();
    let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    // Backup exclusion is a Time Machine attribute; only macOS has it.
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("tmutil")
            .arg("isexcluded")
            .arg(&dir)
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("[Excluded]"));
    }
}

/// Task 9b: a symlinked cache directory is refused before anything is
/// created or changed through it.
#[test]
fn a_symlinked_directory_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let (t, dir, key) = setup();
    let target = t.path().join("elsewhere");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(&target, &dir).unwrap();
    assert!(matches!(Cache::open(&dir, &key), Err(CacheError::Io(_))));
    let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755);
    assert!(!target.join(DB_FILE).exists());
}

/// The directory may sit below a symlink (macOS `/var` -> `/private/var`,
/// where every temporary directory lives): only links at the leaf count.
#[test]
fn a_symlink_above_the_directory_is_allowed() {
    let (t, _dir, key) = setup();
    let real = t.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = t.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let dir = link.join("cache");
    let mut db = Cache::open(&dir, &key).unwrap();
    db.put_messages(ThreadKey::whole(user(1)), &[msg(user(1), 1, "a")])
        .unwrap();
    assert!(real.join("cache").join(DB_FILE).exists());
}

#[test]
fn reconcile_removes_messages_deleted_on_server() {
    let (_t, dir, key) = setup();
    let c = user(1);
    let mut db = Cache::open(&dir, &key).unwrap();
    db.put_messages(
        ThreadKey::whole(c),
        &[msg(c, 1, ""), msg(c, 2, ""), msg(c, 3, ""), msg(c, 9, "")],
    )
    .unwrap();
    let gone = db
        .reconcile_range(
            ThreadKey::whole(c),
            MsgId(1),
            MsgId(5),
            &[msg(c, 1, ""), msg(c, 3, "edited"), msg(c, 4, "")],
        )
        .unwrap();
    assert_eq!(gone, [MsgId(2)]);
    let all = db.tail(ThreadKey::whole(c), 100).unwrap();
    assert_eq!(ids(&all), [1, 3, 4, 9]);
    assert_eq!(all[1].body, Body::Text(RichText::plain("edited")));
}

#[test]
fn unscoped_delete_skips_channels() {
    let (_t, dir, key) = setup();
    let a = user(1);
    let ch = ChatId {
        kind: PeerKind::Channel,
        id: 2,
    };
    let mut db = Cache::open(&dir, &key).unwrap();
    db.put_messages(ThreadKey::whole(a), &[msg(a, 5, "")])
        .unwrap();
    db.put_messages(ThreadKey::whole(ch), &[msg(ch, 5, "")])
        .unwrap();
    assert_eq!(
        db.delete_unscoped(&[MsgId(5), MsgId(6)]).unwrap(),
        [(a, vec![MsgId(5)])]
    );
    assert_eq!(ids(&db.tail(ThreadKey::whole(ch), 10).unwrap()), [5]);
}

#[test]
fn messages_for_other_chats_are_ignored_on_put() {
    let (_t, dir, key) = setup();
    let mut db = Cache::open(&dir, &key).unwrap();
    db.put_messages(ThreadKey::whole(user(1)), &[msg(user(2), 1, "")])
        .unwrap();
    assert!(db.tail(ThreadKey::whole(user(1)), 10).unwrap().is_empty());
    assert!(db.tail(ThreadKey::whole(user(2)), 10).unwrap().is_empty());
}

#[test]
fn corrupt_rows_are_deleted_not_returned() {
    let (_t, dir, key) = setup();
    let c = user(1);
    let mut db = Cache::open(&dir, &key).unwrap();
    db.put_messages(ThreadKey::whole(c), &[msg(c, 1, "ok")])
        .unwrap();
    let garbage = vec![0xFFu8; 32];
    let oversized = vec![0u8; MAX_ROW_BYTES + 1];
    for (id, body) in [(2, &garbage), (3, &oversized)] {
        db.conn()
            .execute(
                "INSERT INTO messages (kind, chat, id, topic, size, body) VALUES (0, 1, ?1, 0, 1, ?2)",
                rusqlite::params![id, body],
            )
            .unwrap();
    }
    assert_eq!(ids(&db.tail(ThreadKey::whole(c), 10).unwrap()), [1]);
    let left: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 1);
}

#[test]
fn unknown_schema_is_reported() {
    let (_t, dir, key) = setup();
    Cache::open(&dir, &key)
        .unwrap()
        .conn()
        .execute_batch("PRAGMA user_version = 99")
        .unwrap();
    assert!(matches!(
        Cache::open(&dir, &key),
        Err(CacheError::UnknownSchema(99))
    ));
}

#[test]
fn version_1_messages_are_dropped_on_upgrade() {
    let (_t, dir, key) = setup();
    let c = user(1);
    {
        let mut db = Cache::open(&dir, &key).unwrap();
        db.put_messages(ThreadKey::whole(c), &[msg(c, 1, "old")])
            .unwrap();
        db.conn().execute_batch("PRAGMA user_version = 1").unwrap();
    }
    let mut db = Cache::open(&dir, &key).unwrap();
    assert!(db.tail(ThreadKey::whole(c), 10).unwrap().is_empty());
    assert_eq!(db.total_bytes(), 0);
    let version: i64 = db
        .conn()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 8);
}

/// Version 2 → 3 drops the chat list; the upgrade then continues to
/// version 4, which drops messages (see `version_3_messages_and_chats_are_dropped`),
/// then versions 5 through 8.
#[test]
fn version_2_chat_list_is_dropped_on_upgrade() {
    let (_t, dir, key) = setup();
    let c = user(1);
    {
        let mut db = Cache::open(&dir, &key).unwrap();
        db.put_chats(&[summary(c, "c")]).unwrap();
        db.put_messages(ThreadKey::whole(c), &[msg(c, 1, "dropped by 3 to 4")])
            .unwrap();
        db.conn().execute_batch("PRAGMA user_version = 2").unwrap();
    }
    let mut db = Cache::open(&dir, &key).unwrap();
    assert!(db.chats().unwrap().is_empty());
    assert!(db.tail(ThreadKey::whole(c), 10).unwrap().is_empty());
}

/// Versions 4 through 7 drop the cached chat list on the way to 8
/// (their chat rows can't be decoded) and keep the messages.
#[test]
fn chat_lists_before_version_8_are_dropped_on_upgrade() {
    for from in [4, 5, 6, 7] {
        let (_t, dir, key) = setup();
        let c = user(1);
        {
            let mut db = Cache::open(&dir, &key).unwrap();
            db.put_chats(&[summary(c, "c")]).unwrap();
            db.put_messages(ThreadKey::whole(c), &[msg(c, 1, "kept")])
                .unwrap();
            db.conn()
                .execute_batch(&format!("PRAGMA user_version = {from}"))
                .unwrap();
        }
        let mut db = Cache::open(&dir, &key).unwrap();
        assert!(db.chats().unwrap().is_empty(), "from {from}");
        assert_eq!(ids(&db.tail(ThreadKey::whole(c), 10).unwrap()), [1]);
        let version: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 8);
    }
}

#[test]
fn peer_folder_round_trips() {
    let (_t, dir, key) = setup();
    let c = user(1);
    let archived = ChatSummary {
        folder: PeerFolder::Archive,
        muted_until: i64::from(i32::MAX),
        kind: ChatKind::Private { contact: true },
        ..summary(c, "c")
    };
    let mut db = Cache::open(&dir, &key).unwrap();
    db.put_chats(std::slice::from_ref(&archived)).unwrap();
    assert_eq!(db.chats().unwrap(), std::slice::from_ref(&archived));
    // Confirmed topic counts survive restart; pending reads never do.
    let reading = ChatSummary {
        read: ReadState {
            unread: 3,
            read_inbox_max: MsgId(40),
        },
        reading: Some(MsgId(50)),
        topic_unread: Some(0),
        ..archived
    };
    db.put_chats(std::slice::from_ref(&reading)).unwrap();
    let cached = ChatSummary {
        reading: None,
        ..reading
    };
    assert_eq!(db.chats().unwrap(), [cached]);
}

#[test]
fn per_chat_cap_drops_oldest() {
    let (_t, dir, key) = setup();
    let c = user(1);
    let mut db = Cache::open(&dir, &key).unwrap();
    let cap = i32::try_from(MAX_CACHED_PER_CHAT).unwrap();
    let batch: Vec<_> = (1..=cap + 5).map(|i| msg(c, i, "")).collect();
    db.put_messages(ThreadKey::whole(c), &batch).unwrap();
    let oldest = db.after(ThreadKey::whole(c), MsgId(0), 1).unwrap();
    assert_eq!(ids(&oldest), [6]);
}

#[test]
fn byte_budget_evicts_least_recently_used_chat_first() {
    let (_t, dir, key) = setup();
    let (a, b) = (user(1), user(2));
    let mut db = Cache::open(&dir, &key).unwrap();
    db.put_chats(&[summary(a, "a"), summary(b, "b")]).unwrap();
    let text = "t".repeat(1000);
    let fill = |c| (1..=2000).map(|i| msg(c, i, &text)).collect::<Vec<_>>();
    db.put_messages(ThreadKey::whole(a), &fill(a)).unwrap();
    db.touch(a).unwrap();
    db.touch(b).unwrap(); // b is now most recent
    db.put_messages(ThreadKey::whole(b), &fill(b)).unwrap();
    let per_chat = db.total_bytes() / 2;
    db.set_budget(per_chat + per_chat / 2);
    db.put_messages(ThreadKey::whole(b), &[msg(b, 2001, "")])
        .unwrap();
    assert!(db.total_bytes() <= per_chat + per_chat / 2);
    assert_eq!(db.tail(ThreadKey::whole(b), 1).unwrap().len(), 1);
    let a_left = db.after(ThreadKey::whole(a), MsgId(0), 1).unwrap();
    assert!(
        a_left.first().is_none_or(|m| m.id.0 > 1000),
        "a's oldest evicted first"
    );
    let b_oldest = db.after(ThreadKey::whole(b), MsgId(0), 1).unwrap();
    assert_eq!(ids(&b_oldest), [1], "most recent chat untouched");
}

#[test]
fn byte_accounting_matches_table() {
    let (_t, dir, key) = setup();
    let c = user(1);
    let mut db = Cache::open(&dir, &key).unwrap();
    db.put_messages(
        ThreadKey::whole(c),
        &[msg(c, 1, "aaa"), msg(c, 2, "bbbbbb")],
    )
    .unwrap();
    db.put_messages(ThreadKey::whole(c), &[msg(c, 1, "replaced with longer")])
        .unwrap();
    db.delete_messages(c, &[MsgId(2)]).unwrap();
    let sum: i64 = db
        .conn()
        .query_row("SELECT SUM(size) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(db.total_bytes(), u64::try_from(sum).unwrap());
    drop(db);
    assert_eq!(
        Cache::open(&dir, &key).unwrap().total_bytes(),
        u64::try_from(sum).unwrap()
    );
}

// Regression tests for the cache review (round 1).

fn sql(db: &Cache, s: &str) {
    db.conn().execute_batch(s).unwrap();
}

fn stored_sum(db: &Cache) -> u64 {
    let sum: i64 = db
        .conn()
        .query_row("SELECT COALESCE(SUM(size), 0) FROM messages", [], |r| {
            r.get(0)
        })
        .unwrap();
    u64::try_from(sum).unwrap()
}

#[test]
fn key_v2_is_equivalent_to_pragma_raw_key() {
    let (_t, dir, key) = setup();
    Cache::open(&dir, &key)
        .unwrap()
        .put_messages(ThreadKey::whole(user(1)), &[msg(user(1), 1, "a")])
        .unwrap();
    // Open with the documented PRAGMA form to prove sqlite3_key_v2 used the raw key.
    let conn = rusqlite::Connection::open(dir.join("cache.db")).unwrap();
    let hex: String = key.as_bytes().iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write;
        write!(s, "{b:02x}").unwrap();
        s
    });
    conn.pragma_update(None, "key", format!("x'{hex}'"))
        .unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn rollback_keeps_byte_accounting_exact() {
    let (_t, dir, key) = setup();
    let c = user(1);
    let mut db = Cache::open(&dir, &key).unwrap();
    db.put_messages(ThreadKey::whole(c), &[msg(c, 1, "first")])
        .unwrap();
    sql(
        &db,
        "CREATE TEMP TRIGGER boom BEFORE INSERT ON messages WHEN NEW.id = 3
              BEGIN SELECT RAISE(ABORT, 'boom'); END;",
    );
    assert!(
        db.put_messages(
            ThreadKey::whole(c),
            &[msg(c, 2, "second"), msg(c, 3, "third")]
        )
        .is_err()
    );
    assert_eq!(db.total_bytes(), stored_sum(&db));
    assert_eq!(ids(&db.tail(ThreadKey::whole(c), 10).unwrap()), [1]);
}

#[test]
fn oversized_rows_are_never_materialized_or_stored() {
    let (_t, dir, key) = setup();
    let mut db = Cache::open(&dir, &key).unwrap();
    // A chat whose encoding exceeds the row limit is skipped on write.
    let mut big = summary(user(1), "t");
    big.last = Some(crate::model::Preview::Text("p".repeat(512)));
    let huge_title = "n".repeat(70_000);
    let invalid = summary(user(2), &huge_title); // not clean: over MAX_NAME_BYTES
    db.put_chats(&[big.clone(), invalid]).unwrap();
    assert_eq!(db.chats().unwrap(), vec![big]);
    // An oversized blob injected directly comes back as NULL, not as 8 MiB.
    sql(
        &db,
        "INSERT INTO messages (kind, chat, id, topic, size, body)
         VALUES (0, 1, 7, 0, 1, zeroblob(8388608))",
    );
    assert!(db.tail(ThreadKey::whole(user(1)), 10).unwrap().is_empty());
}

#[test]
fn rows_with_mismatched_identity_or_unclean_fields_are_rejected() {
    let (_t, dir, key) = setup();
    let c = user(1);
    let mut db = Cache::open(&dir, &key).unwrap();
    let mut spoof = msg(c, 999, "x");
    spoof.sender = Some(crate::model::Sender {
        id: c,
        name: "a\u{202E}b".into(),
    });
    let wrong_id = postcard::to_allocvec(&msg(c, 999, "x")).unwrap();
    let wrong_chat = postcard::to_allocvec(&msg(user(2), 3, "x")).unwrap();
    let bidi = postcard::to_allocvec(&spoof).unwrap();
    for (id, body) in [(1, &wrong_id), (3, &wrong_chat), (999, &bidi)] {
        db.conn()
            .execute(
                "INSERT INTO messages (kind, chat, id, topic, size, body) VALUES (0, 1, ?1, 0, 1, ?2)",
                rusqlite::params![id, body],
            )
            .unwrap();
    }
    db.put_messages(ThreadKey::whole(c), &[msg(c, 2, "ok"), spoof])
        .unwrap(); // invalid one skipped on write too
    assert_eq!(ids(&db.tail(ThreadKey::whole(c), 10).unwrap()), [2]);
}

#[test]
fn reconcile_ignores_same_id_from_another_chat() {
    let (_t, dir, key) = setup();
    let (a, b) = (user(1), user(2));
    let mut db = Cache::open(&dir, &key).unwrap();
    db.put_messages(ThreadKey::whole(a), &[msg(a, 5, "")])
        .unwrap();
    let gone = db
        .reconcile_range(ThreadKey::whole(a), MsgId(1), MsgId(9), &[msg(b, 5, "")])
        .unwrap();
    assert_eq!(gone, [MsgId(5)]);
    assert!(db.tail(ThreadKey::whole(a), 10).unwrap().is_empty());
}

#[test]
fn erase_closes_before_deleting_key_and_keeps_files_on_failure() {
    let (_t, dir, key) = setup();
    let db = Cache::open(&dir, &key).unwrap();
    let res = db.erase(&dir, key, || Err::<(), _>("keychain unavailable"));
    assert!(matches!(
        res,
        Err(EraseError::DeleteKey("keychain unavailable"))
    ));
    assert!(
        dir.join("cache.db").exists(),
        "files kept until the key is gone"
    );

    let key = CacheKey::generate().unwrap();
    destroy_unopened(&dir).unwrap();
    let db = Cache::open(&dir, &key).unwrap();
    let mut deleted = false;
    db.erase(&dir, key, || {
        deleted = true;
        Ok::<(), ()>(())
    })
    .unwrap();
    assert!(deleted && !dir.exists());
}

#[test]
fn eviction_uses_index_not_table_scan() {
    let (_t, dir, key) = setup();
    let db = Cache::open(&dir, &key).unwrap();
    let plan: String = db
        .conn()
        .query_row(
            "EXPLAIN QUERY PLAN SELECT kind, chat FROM cached_chats ORDER BY last_access LIMIT 1",
            [],
            |r| r.get(3),
        )
        .unwrap();
    assert!(plan.contains("cached_chats_lru"), "plan: {plan}");
}

/// Task 6c: a cache is served only to the account it is bound to.
#[test]
fn bind_claims_an_empty_cache_and_refuses_another_account() {
    let (_t, dir, key) = setup();
    let mut db = Cache::open(&dir, &key).unwrap();
    assert_eq!(db.bind(None).unwrap(), Binding::Blank);
    assert_eq!(db.bind(Some(7)).unwrap(), Binding::Claimed);
    db.put_messages(ThreadKey::whole(user(1)), &[msg(user(1), 1, "a")])
        .unwrap();
    assert_eq!(db.bind(Some(7)).unwrap(), Binding::Same);
    assert_eq!(db.bind(Some(8)).unwrap(), Binding::Foreign);
    assert_eq!(db.bind(None).unwrap(), Binding::Foreign, "owned");
    drop(db);
    let mut db = Cache::open(&dir, &key).unwrap();
    assert_eq!(db.bind(Some(7)).unwrap(), Binding::Same, "persisted");
}

/// Rows without a recorded owner (a cache from before binding) belong to
/// nobody that can be verified.
#[test]
fn unowned_rows_are_foreign() {
    let (_t, dir, key) = setup();
    let mut db = Cache::open(&dir, &key).unwrap();
    db.put_chats(&[summary(user(1), "x")]).unwrap();
    assert_eq!(db.bind(Some(7)).unwrap(), Binding::Foreign);
    assert_eq!(db.bind(None).unwrap(), Binding::Foreign);
}

mod threads;
