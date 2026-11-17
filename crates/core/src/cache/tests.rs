use std::time::Duration;

use super::*;
use crate::model::{Body, ChatId, ChatKind, ChatSummary, Message, MsgId, PeerKind, ThreadKey};
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
        .write(worker::CacheCmd::PutMessages(c, vec![msg(c, 1, "a")]))
        .await;
    handle.close().await.unwrap();
    assert!(matches!(handle.chats().await, Err(WorkerError::Stopped)));
    let mut db = Cache::open(&dir, &key).unwrap();
    assert_eq!(ids(&db.tail(c, 10).unwrap()), vec![1]);
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
        .write(worker::CacheCmd::PutMessages(c, vec![msg(c, 1, "a")]))
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

/// Round 23, finding 4: a worker that panics while handling a command
/// closes its connection before it leaves the census. The probe runs
/// between the two (it is declared between them in `Cache`): by then the
/// WAL is gone, so the connection has closed, and the census still lists
/// the cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_panic_closes_the_connection_before_leaving_the_census() {
    let (_t, dir, key) = setup();
    let wal = dir.join("cache.db-wal");
    let handle = CacheHandle::spawn(dir.clone(), &key).unwrap();
    let c = user(1);
    handle
        .write(worker::CacheCmd::PutMessages(c, vec![msg(c, 1, "a")]))
        .await;
    handle.barrier().await.unwrap();
    assert!(wal.exists(), "the connection is open");

    let (tx, rx) = std::sync::mpsc::channel();
    let d = dir.clone();
    let probe = db::DropProbe::new(move || {
        let _ = tx.send((d.join("cache.db-wal").exists(), census::is_open(&d)));
    });
    handle.panic_worker(probe).await;
    let seen = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(10)));
    let (wal_left, listed) = seen.await.unwrap().unwrap();
    assert!(
        !wal_left,
        "the connection closed before the entry was released"
    );
    assert!(listed, "the census still listed the cache while it closed");
    let d = dir.clone();
    let drained =
        tokio::task::spawn_blocking(move || wait_closed(Some(&d), Duration::from_secs(10)));
    assert!(drained.await.unwrap());
    assert!(matches!(handle.chats().await, Err(WorkerError::Stopped)));
}

/// Round 23, finding 2: `open_blocking` takes its census entry when it
/// schedules the open, so an open still queued for a blocking thread is
/// counted (a drain waits for it), and closed admission refuses it before
/// it is scheduled.
#[test]
fn a_queued_blocking_open_is_counted_before_it_starts() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (_t, dir, key) = setup();
    let empty = |d: &std::path::Path| wait_closed(Some(d), Duration::ZERO);
    rt.block_on(async {
        let (release, wait) = std::sync::mpsc::channel::<()>();
        // Occupies the only blocking thread, so the open queues.
        let blocker = tokio::task::spawn_blocking(move || wait.recv());
        let opening = open_blocking(dir.clone(), key);
        assert!(!empty(&dir), "a queued open is counted");
        assert!(!dir.join("cache.db").exists(), "and has not started");
        release.send(()).unwrap();
        blocker.await.unwrap().unwrap();
        let (handle, _key) = opening.await.unwrap();
        handle.close().await.unwrap();
    });
    assert!(empty(&dir));

    let closed = close_admission(&dir);
    let key = CacheKey::generate().unwrap();
    assert!(rt.block_on(open_blocking(dir.clone(), key)).is_none());
    assert!(empty(&dir), "a refused open takes no entry");
    drop(closed);
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
        db.put_messages(c, &[msg(c, 3, "x"), msg(c, 1, "y"), msg(c, 2, "z")])
            .unwrap();
    }
    let mut db = Cache::open(&dir, &key).unwrap();
    let chats = db.chats().unwrap();
    assert_eq!(
        chats.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(),
        ["b", "a"]
    );
    assert_eq!(ids(&db.tail(c, 2).unwrap()), [2, 3]);
    assert_eq!(ids(&db.before(c, MsgId(3), 10).unwrap()), [1, 2]);
    assert_eq!(ids(&db.after(c, MsgId(1), 1).unwrap()), [2]);
}

#[test]
fn wrong_key_is_detected_and_destroy_recovers() {
    let (_t, dir, key) = setup();
    Cache::open(&dir, &key)
        .unwrap()
        .put_messages(user(1), &[msg(user(1), 1, "a")])
        .unwrap();
    let other = CacheKey::generate().unwrap();
    assert!(matches!(
        Cache::open(&dir, &other),
        Err(CacheError::WrongKey)
    ));
    destroy_unopened(&dir).unwrap();
    assert!(!dir.exists());
    let mut db = Cache::open(&dir, &other).unwrap();
    assert!(db.tail(user(1), 10).unwrap().is_empty());
}

#[test]
fn files_on_disk_are_encrypted() {
    let (_t, dir, key) = setup();
    {
        let mut db = Cache::open(&dir, &key).unwrap();
        db.put_chats(&[summary(user(1), SECRET)]).unwrap();
        db.put_messages(user(1), &[msg(user(1), 1, SECRET)])
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

/// Round 26, finding 2: a `cache.db` that is a symlink (to another cache's
/// database) is refused, and the target is neither opened nor changed.
#[test]
fn a_symlinked_database_file_is_refused() {
    let (t, dir, key) = setup();
    let other = t.path().join("other");
    {
        let mut db = Cache::open(&other, &key).unwrap();
        db.put_messages(user(1), &[msg(user(1), 1, "a")]).unwrap();
    }
    let target = other.join(DB_FILE);
    let before = std::fs::read(&target).unwrap();
    std::fs::create_dir(&dir).unwrap();
    std::os::unix::fs::symlink(&target, dir.join(DB_FILE)).unwrap();
    assert!(Cache::open(&dir, &key).is_err(), "opened through the link");
    assert_eq!(std::fs::read(&target).unwrap(), before);
}

/// Round 26, finding 2: linked side files (`-wal`, `-shm`) are never
/// followed: the open or the first write fails, and the targets are
/// untouched.
#[test]
fn symlinked_side_files_are_never_followed() {
    for side in ["-wal", "-shm"] {
        let (t, dir, key) = setup();
        drop(Cache::open(&dir, &key).unwrap());
        let victim = t.path().join("victim");
        std::fs::write(&victim, b"untouched").unwrap();
        let name = format!("{DB_FILE}{side}");
        let _ = std::fs::remove_file(dir.join(&name));
        std::os::unix::fs::symlink(&victim, dir.join(&name)).unwrap();
        let wrote = Cache::open(&dir, &key)
            .and_then(|mut db| db.put_messages(user(1), &[msg(user(1), 1, "a")]));
        assert!(wrote.is_err(), "{side}: wrote through the link");
        assert_eq!(std::fs::read(&victim).unwrap(), b"untouched", "{side}");
    }
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
    db.put_messages(user(1), &[msg(user(1), 1, "a")]).unwrap();
    assert!(real.join("cache").join(DB_FILE).exists());
}

/// Round 29, finding 3: SQLite names the file in a failed open's message;
/// `CacheError` keeps the result codes and drops the message, so neither
/// `Display` nor `Debug` shows the path.
#[test]
fn sql_errors_never_show_the_path() {
    const SENTINEL: &str = "SENTINEL-user-name-7f3a";
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join(SENTINEL);
    // A directory where the database file should be: the open fails.
    std::fs::create_dir_all(dir.join(DB_FILE)).unwrap();
    let key = CacheKey::generate().unwrap();
    let Err(error) = Cache::open(&dir, &key) else {
        panic!("opened a directory as the database");
    };
    assert!(matches!(error, CacheError::Sql(_)), "{error:?}");
    // The message rusqlite carries does name the file.
    let raw = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
        Some(format!("unable to open database file: {}", dir.display())),
    );
    assert!(raw.to_string().contains(SENTINEL));
    let redacted = CacheError::from(raw);
    for shown in [
        error.to_string(),
        format!("{error:?}"),
        redacted.to_string(),
        format!("{redacted:?}"),
    ] {
        assert!(!shown.contains(SENTINEL), "{shown}");
    }
    assert_eq!(redacted.to_string(), "cache sql: CannotOpen (14)");
}

#[test]
fn reconcile_removes_messages_deleted_on_server() {
    let (_t, dir, key) = setup();
    let c = user(1);
    let mut db = Cache::open(&dir, &key).unwrap();
    db.put_messages(
        c,
        &[msg(c, 1, ""), msg(c, 2, ""), msg(c, 3, ""), msg(c, 9, "")],
    )
    .unwrap();
    let gone = db
        .reconcile_range(
            c,
            MsgId(1),
            MsgId(5),
            &[msg(c, 1, ""), msg(c, 3, "edited"), msg(c, 4, "")],
        )
        .unwrap();
    assert_eq!(gone, [MsgId(2)]);
    let all = db.tail(c, 100).unwrap();
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
    db.put_messages(a, &[msg(a, 5, "")]).unwrap();
    db.put_messages(ch, &[msg(ch, 5, "")]).unwrap();
    assert_eq!(
        db.delete_unscoped(&[MsgId(5), MsgId(6)]).unwrap(),
        [(a, vec![MsgId(5)])]
    );
    assert_eq!(ids(&db.tail(ch, 10).unwrap()), [5]);
}

#[test]
fn messages_for_other_chats_are_ignored_on_put() {
    let (_t, dir, key) = setup();
    let mut db = Cache::open(&dir, &key).unwrap();
    db.put_messages(user(1), &[msg(user(2), 1, "")]).unwrap();
    assert!(db.tail(user(1), 10).unwrap().is_empty());
    assert!(db.tail(user(2), 10).unwrap().is_empty());
}

#[test]
fn corrupt_rows_are_deleted_not_returned() {
    let (_t, dir, key) = setup();
    let c = user(1);
    let mut db = Cache::open(&dir, &key).unwrap();
    db.put_messages(c, &[msg(c, 1, "ok")]).unwrap();
    let garbage = vec![0xFFu8; 32];
    let oversized = vec![0u8; MAX_ROW_BYTES + 1];
    for (id, body) in [(2, &garbage), (3, &oversized)] {
        db.conn()
            .execute(
                "INSERT INTO messages (kind, chat, id, size, body) VALUES (0, 1, ?1, 1, ?2)",
                rusqlite::params![id, body],
            )
            .unwrap();
    }
    assert_eq!(ids(&db.tail(c, 10).unwrap()), [1]);
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
        db.put_messages(c, &[msg(c, 1, "old")]).unwrap();
        db.conn().execute_batch("PRAGMA user_version = 1").unwrap();
    }
    let mut db = Cache::open(&dir, &key).unwrap();
    assert!(db.tail(c, 10).unwrap().is_empty());
    assert_eq!(db.total_bytes(), 0);
    let version: i64 = db
        .conn()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 3);
}

#[test]
fn version_2_chat_list_is_dropped_on_upgrade() {
    let (_t, dir, key) = setup();
    let c = user(1);
    {
        let mut db = Cache::open(&dir, &key).unwrap();
        db.put_chats(&[summary(c, "c")]).unwrap();
        db.put_messages(c, &[msg(c, 1, "kept")]).unwrap();
        db.conn().execute_batch("PRAGMA user_version = 2").unwrap();
    }
    let mut db = Cache::open(&dir, &key).unwrap();
    assert!(db.chats().unwrap().is_empty());
    assert_eq!(ids(&db.tail(c, 10).unwrap()), [1]);
}

#[test]
fn per_chat_cap_drops_oldest() {
    let (_t, dir, key) = setup();
    let c = user(1);
    let mut db = Cache::open(&dir, &key).unwrap();
    let cap = i32::try_from(MAX_CACHED_PER_CHAT).unwrap();
    let batch: Vec<_> = (1..=cap + 5).map(|i| msg(c, i, "")).collect();
    db.put_messages(c, &batch).unwrap();
    let oldest = db.after(c, MsgId(0), 1).unwrap();
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
    db.put_messages(a, &fill(a)).unwrap();
    db.touch(a).unwrap();
    db.touch(b).unwrap(); // b is now most recent
    db.put_messages(b, &fill(b)).unwrap();
    let per_chat = db.total_bytes() / 2;
    db.set_budget(per_chat + per_chat / 2);
    db.put_messages(b, &[msg(b, 2001, "")]).unwrap();
    assert!(db.total_bytes() <= per_chat + per_chat / 2);
    assert_eq!(db.tail(b, 1).unwrap().len(), 1);
    let a_left = db.after(a, MsgId(0), 1).unwrap();
    assert!(
        a_left.first().is_none_or(|m| m.id.0 > 1000),
        "a's oldest evicted first"
    );
    let b_oldest = db.after(b, MsgId(0), 1).unwrap();
    assert_eq!(ids(&b_oldest), [1], "most recent chat untouched");
}

#[test]
fn byte_accounting_matches_table() {
    let (_t, dir, key) = setup();
    let c = user(1);
    let mut db = Cache::open(&dir, &key).unwrap();
    db.put_messages(c, &[msg(c, 1, "aaa"), msg(c, 2, "bbbbbb")])
        .unwrap();
    db.put_messages(c, &[msg(c, 1, "replaced with longer")])
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
        .put_messages(user(1), &[msg(user(1), 1, "a")])
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
    db.put_messages(c, &[msg(c, 1, "first")]).unwrap();
    sql(
        &db,
        "CREATE TEMP TRIGGER boom BEFORE INSERT ON messages WHEN NEW.id = 3
              BEGIN SELECT RAISE(ABORT, 'boom'); END;",
    );
    assert!(
        db.put_messages(c, &[msg(c, 2, "second"), msg(c, 3, "third")])
            .is_err()
    );
    assert_eq!(db.total_bytes(), stored_sum(&db));
    assert_eq!(ids(&db.tail(c, 10).unwrap()), [1]);
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
        "INSERT INTO messages (kind, chat, id, size, body) VALUES (0, 1, 7, 1, zeroblob(8388608))",
    );
    assert!(db.tail(user(1), 10).unwrap().is_empty());
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
                "INSERT INTO messages (kind, chat, id, size, body) VALUES (0, 1, ?1, 1, ?2)",
                rusqlite::params![id, body],
            )
            .unwrap();
    }
    db.put_messages(c, &[msg(c, 2, "ok"), spoof]).unwrap(); // invalid one skipped on write too
    assert_eq!(ids(&db.tail(c, 10).unwrap()), [2]);
}

#[test]
fn reconcile_ignores_same_id_from_another_chat() {
    let (_t, dir, key) = setup();
    let (a, b) = (user(1), user(2));
    let mut db = Cache::open(&dir, &key).unwrap();
    db.put_messages(a, &[msg(a, 5, "")]).unwrap();
    let gone = db
        .reconcile_range(a, MsgId(1), MsgId(9), &[msg(b, 5, "")])
        .unwrap();
    assert_eq!(gone, [MsgId(5)]);
    assert!(db.tail(a, 10).unwrap().is_empty());
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
    db.put_messages(user(1), &[msg(user(1), 1, "a")]).unwrap();
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
