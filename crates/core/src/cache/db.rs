//! SQLCipher-backed storage. All methods are blocking; core runs them on a
//! dedicated worker so the async runtime never waits on disk.
//!
//! Byte accounting is transaction-local: every mutating method computes the new
//! total alongside its transaction and assigns it only after a successful commit.

use std::collections::HashSet;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, ffi, params};

use super::census::{self, Open};
use super::{CacheError, CacheKey, MAX_CACHE_BYTES, MAX_CACHED_PER_CHAT, MAX_ROW_BYTES};
use crate::model::{ChatId, ChatSummary, Message, MsgId, PeerKind, ThreadKey, group_by_chat};
use crate::store::MAX_DIALOGS;

const SCHEMA_VERSION: i64 = 3;
/// Version 2 replaced `Message::edited` with `edit_date`; version 1 message rows
/// cannot be decoded, so they are dropped (the cache is disposable).
const MIGRATE_1_TO_2: &str =
    "BEGIN; DELETE FROM messages; DELETE FROM cached_chats; PRAGMA user_version = 2; COMMIT;";
/// Version 3 added `ChatSummary::last_id`; version 2 chat rows cannot be
/// decoded, so the cached chat list is dropped (messages are unchanged).
const MIGRATE_2_TO_3: &str = "BEGIN; DELETE FROM chats; PRAGMA user_version = 3; COMMIT;";
/// Messages deleted per eviction step.
const EVICT_BATCH: i64 = 1000;
/// Upper bound on eviction steps per call; exceeding it is reported, not hidden.
const MAX_EVICT_STEPS: usize = 1024;

const SCHEMA: &str = "
CREATE TABLE meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE chats (
    kind INTEGER NOT NULL, id INTEGER NOT NULL, ord INTEGER NOT NULL, body BLOB NOT NULL,
    PRIMARY KEY (kind, id)) WITHOUT ROWID;
CREATE TABLE messages (
    kind INTEGER NOT NULL, chat INTEGER NOT NULL, id INTEGER NOT NULL,
    size INTEGER NOT NULL, body BLOB NOT NULL,
    PRIMARY KEY (kind, chat, id)) WITHOUT ROWID;
CREATE INDEX messages_account_id ON messages (id) WHERE kind IN (0, 1);
-- One row per chat that has (or had) cached messages; drives LRU eviction
-- without scanning `messages`. last_access 0 = never opened.
CREATE TABLE cached_chats (
    kind INTEGER NOT NULL, chat INTEGER NOT NULL, last_access INTEGER NOT NULL,
    PRIMARY KEY (kind, chat)) WITHOUT ROWID;
CREATE INDEX cached_chats_lru ON cached_chats (last_access);
INSERT INTO meta (key, value) VALUES ('clock', 0);
";

/// SQL expression selecting `body` only if it is a blob within the row limit, so
/// oversized values are never copied into Rust memory. Parameter `?{param}` must
/// be bound to `MAX_ROW_BYTES`.
fn bounded_body(param: u8) -> String {
    format!("CASE WHEN typeof(body) = 'blob' AND length(body) <= ?{param} THEN body END")
}

/// An open cache database. Fields drop in declaration order: the connection
/// closes first and the census entry is released last, on every path,
/// unwinding included, so nothing waiting on the census (shutdown, process
/// exit) ever sees the cache gone while its connection is still closing.
#[derive(Debug)]
pub struct Cache {
    conn: Connection,
    /// Sum of `messages.size`, as committed.
    bytes: u64,
    budget: u64,
    /// Tests: runs between the connection's close and the entry's release.
    #[cfg(test)]
    probe: Option<DropProbe>,
    /// Census entry (see `census`). Must stay the last field.
    _entry: Open,
}

/// Tests: a callback run when it is dropped.
#[cfg(test)]
pub(crate) struct DropProbe(Option<Box<dyn FnOnce() + Send>>);

#[cfg(test)]
impl DropProbe {
    pub(crate) fn new(f: impl FnOnce() + Send + 'static) -> Self {
        Self(Some(Box::new(f)))
    }
}

#[cfg(test)]
impl std::fmt::Debug for DropProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DropProbe")
    }
}

#[cfg(test)]
impl Drop for DropProbe {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

fn kind_code(kind: PeerKind) -> i64 {
    match kind {
        PeerKind::User => 0,
        PeerKind::Chat => 1,
        PeerKind::Channel => 2,
    }
}

fn kind_from_code(code: i64) -> Option<PeerKind> {
    match code {
        0 => Some(PeerKind::User),
        1 => Some(PeerKind::Chat),
        2 => Some(PeerKind::Channel),
        _ => None,
    }
}

fn row_limit() -> i64 {
    i64::try_from(MAX_ROW_BYTES).unwrap_or(i64::MAX)
}

fn to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// Serializes `value` unless it exceeds the row limit.
fn encode<T: serde::Serialize>(value: &T) -> Result<Option<Vec<u8>>, CacheError> {
    let body = postcard::to_allocvec(value)?;
    Ok((body.len() <= MAX_ROW_BYTES).then_some(body))
}

fn decode_message(chat: ChatId, id: i32, body: Option<Vec<u8>>) -> Option<Message> {
    let m: Message = postcard::from_bytes(&body?).ok()?;
    (m.id == MsgId(id) && m.thread.chat == chat && m.is_valid()).then_some(m)
}

/// The chat a raw `(kind, id)` SQL key names, if `kind` is a known code.
fn chat_id(kind: i64, id: i64) -> Option<ChatId> {
    Some(ChatId {
        kind: kind_from_code(kind)?,
        id,
    })
}

fn decode_chat(kind: i64, id: i64, body: Option<Vec<u8>>) -> Option<ChatSummary> {
    let c: ChatSummary = postcard::from_bytes(&body?).ok()?;
    (Some(c.id) == chat_id(kind, id) && c.is_valid()).then_some(c)
}

impl Cache {
    /// Opens or creates the cache in `dir`.
    ///
    /// Refused before SQLCipher or the disk is touched until SQLCipher is
    /// initialized with the exit hook in place (`census::init`), and once
    /// admission to `dir` is closed (core shutdown, process exit). The
    /// census entry is taken before the connection opens; on an error the
    /// connection, declared after it, closes first.
    pub fn open(dir: &Path, key: &CacheKey) -> Result<Self, CacheError> {
        census::init().map_err(CacheError::Init)?;
        let entry = Open::enter(dir).map_err(CacheError::Refused)?;
        super::prepare_dir(dir)?;
        let conn = Self::connect(dir)?;
        Self::unlock(&conn, key)?;
        Self::migrate(&conn)?;
        let bytes: i64 =
            conn.query_row("SELECT COALESCE(SUM(size), 0) FROM messages", [], |r| {
                r.get(0)
            })?;
        Ok(Self {
            conn,
            bytes: to_u64(bytes),
            budget: MAX_CACHE_BYTES,
            #[cfg(test)]
            probe: None,
            _entry: entry,
        })
    }

    /// Opens the database file without following a symlink (round 26,
    /// finding 2). The directory is resolved first, so a symlink above it
    /// (macOS's `/var` -> `/private/var`) is allowed, while a `cache.db`
    /// that is itself a link is refused (`SQLITE_OPEN_NOFOLLOW` fails on any
    /// link in the name it is given). The side files (`-wal`, `-shm`,
    /// `-journal`) are named after that resolved path, and SQLite's unix VFS
    /// opens every file with `O_NOFOLLOW` (SQLCipher 4.20.0, `unixOpen` and
    /// `unixOpenSharedMemory`), so a linked side file fails to open too.
    fn connect(dir: &Path) -> Result<Connection, CacheError> {
        let dir = std::fs::canonicalize(dir)?;
        let flags = rusqlite::OpenFlags::default() | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW;
        Ok(Connection::open_with_flags(super::db_path(&dir), flags)?)
    }

    fn unlock(conn: &Connection, key: &CacheKey) -> Result<(), CacheError> {
        // Enable SQLCipher's memory wiping before it ever sees the key.
        conn.execute_batch("PRAGMA cipher_memory_security = ON;")?;
        let literal = key.hex_literal();
        let len = std::ffi::c_int::try_from(literal.len()).unwrap_or(0);
        // SAFETY: the raw handle is only passed to `sqlite3_key_v2` below, which
        // neither closes it nor outlives `conn`.
        let db = unsafe { conn.handle() };
        // SAFETY: `db` is valid for the lifetime of `conn`; "main" is a
        // NUL-terminated C string; `literal` is a live buffer of `len` bytes.
        // sqlite3_key_v2 copies the key, so no SQL string ever holds it.
        let rc = unsafe { ffi::sqlite3_key_v2(db, c"main".as_ptr(), literal.as_ptr().cast(), len) };
        drop(literal);
        if rc != ffi::SQLITE_OK {
            return Err(rusqlite::Error::SqliteFailure(ffi::Error::new(rc), None).into());
        }
        // Any read fails with NOTADB when the key is wrong.
        let probe: Result<i64, _> =
            conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0));
        match probe {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::NotADatabase =>
            {
                return Err(CacheError::WrongKey);
            }
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }
        let provider: String = conn.query_row("PRAGMA cipher_provider", [], |r| r.get(0))?;
        if cfg!(target_os = "macos") && provider != "commoncrypto" {
            return Err(CacheError::WrongProvider(provider));
        }
        conn.execute_batch(
            "PRAGMA temp_store = MEMORY;
             PRAGMA secure_delete = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;",
        )?;
        Ok(())
    }

    fn migrate(conn: &Connection) -> Result<(), CacheError> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        match version {
            0 => {
                conn.execute_batch(&format!(
                    "BEGIN; {SCHEMA} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
                ))?;
                Ok(())
            }
            1 => {
                conn.execute_batch(MIGRATE_1_TO_2)?;
                conn.execute_batch(MIGRATE_2_TO_3)?;
                Ok(())
            }
            2 => {
                conn.execute_batch(MIGRATE_2_TO_3)?;
                Ok(())
            }
            SCHEMA_VERSION => Ok(()),
            other => Err(CacheError::UnknownSchema(other)),
        }
    }

    pub fn total_bytes(&self) -> u64 {
        self.bytes
    }

    /// Crypto-erase: closes the connection (SQLCipher wipes its in-memory key),
    /// drops the caller's key, runs `delete_key` to remove the persistent key, and
    /// only then unlinks the files. If deleting the key fails, the files are kept
    /// and the error is returned, so the caller can retry rather than believe the
    /// data is erased.
    pub fn erase<E>(
        self,
        dir: &Path,
        key: CacheKey,
        delete_key: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), EraseError<E>> {
        self.conn.close().map_err(|(_, e)| EraseError::Close(e))?;
        drop(key);
        delete_key().map_err(EraseError::DeleteKey)?;
        super::remove_files(dir).map_err(EraseError::Files)
    }

    /// Tests: runs `probe` after the connection closes, before the census
    /// entry is released.
    #[cfg(test)]
    pub(crate) fn set_drop_probe(&mut self, probe: DropProbe) {
        self.probe = Some(probe);
    }

    #[cfg(test)]
    pub(super) fn set_budget(&mut self, budget: u64) {
        self.budget = budget;
    }

    #[cfg(test)]
    pub(super) fn conn(&self) -> &Connection {
        &self.conn
    }
}

/// What [`Cache::bind`] found (plan section 16, account binding).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Binding {
    /// The cache belongs to the given account.
    Same,
    /// The cache was empty and unowned; it now belongs to the given account.
    Claimed,
    /// Empty and unowned, and no account was given: nothing to protect.
    Blank,
    /// Owned by another account, or holding rows of an unknown owner. The
    /// caller must not serve it: crypto-erase and recreate.
    Foreign,
}

// Account binding.
impl Cache {
    /// Binds the cache to `owner` (the verified self user id), or checks an
    /// unbound cache when `owner` is `None`. Rows are served only after
    /// `Same` or `Claimed`.
    pub fn bind(&mut self, owner: Option<i64>) -> Result<Binding, CacheError> {
        let stored: Option<i64> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'owner'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if let (Some(stored), Some(owner)) = (stored, owner)
            && stored == owner
        {
            return Ok(Binding::Same);
        }
        if stored.is_some() || !self.is_empty()? {
            return Ok(Binding::Foreign);
        }
        let Some(owner) = owner else {
            return Ok(Binding::Blank);
        };
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('owner', ?1)",
            params![owner],
        )?;
        Ok(Binding::Claimed)
    }

    fn is_empty(&self) -> Result<bool, CacheError> {
        let any: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM chats) OR EXISTS (SELECT 1 FROM messages)",
            [],
            |r| r.get(0),
        )?;
        Ok(!any)
    }
}

#[derive(Debug)]
pub enum EraseError<E> {
    Close(rusqlite::Error),
    DeleteKey(E),
    Files(std::io::Error),
}

// Chat list.
impl Cache {
    /// Replaces the cached chat list (server order). Invalid or oversized chats are
    /// skipped. Bounded by `MAX_DIALOGS`.
    pub fn put_chats(&mut self, chats: &[ChatSummary]) -> Result<(), CacheError> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM chats", [])?;
        let mut ord = 0i64;
        for chat in chats.iter().filter(|c| c.is_valid()).take(MAX_DIALOGS) {
            let Some(body) = encode(chat)? else { continue };
            tx.execute(
                "INSERT OR REPLACE INTO chats (kind, id, ord, body) VALUES (?1, ?2, ?3, ?4)",
                params![kind_code(chat.id.kind), chat.id.id, ord, body],
            )?;
            ord += 1;
        }
        tx.commit()?;
        Ok(())
    }

    /// Cached chat list in stored order. Invalid rows are deleted.
    pub fn chats(&mut self) -> Result<Vec<ChatSummary>, CacheError> {
        let sql = format!(
            "SELECT kind, id, {} FROM chats ORDER BY ord LIMIT ?1",
            bounded_body(2)
        );
        let limit = i64::try_from(MAX_DIALOGS).unwrap_or(i64::MAX);
        let mut out = Vec::new();
        let mut bad: Vec<(i64, i64)> = Vec::new();
        {
            let mut stmt = self.conn.prepare(&sql)?;
            let mut rows = stmt.query(params![limit, row_limit()])?;
            while let Some(row) = rows.next()? {
                let (kind, id): (i64, i64) = (row.get(0)?, row.get(1)?);
                match decode_chat(kind, id, row.get(2)?) {
                    Some(chat) => out.push(chat),
                    None => bad.push((kind, id)),
                }
            }
        }
        for (kind, id) in bad {
            self.conn.execute(
                "DELETE FROM chats WHERE kind = ?1 AND id = ?2",
                params![kind, id],
            )?;
        }
        Ok(out)
    }

    /// Marks `chat` most recently used for eviction purposes.
    pub fn touch(&mut self, chat: ChatId) -> Result<(), CacheError> {
        let tx = self.conn.transaction()?;
        let clock: i64 = tx.query_row(
            "UPDATE meta SET value = value + 1 WHERE key = 'clock' RETURNING value",
            [],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO cached_chats (kind, chat, last_access) VALUES (?1, ?2, ?3)
             ON CONFLICT (kind, chat) DO UPDATE SET last_access = ?3",
            params![kind_code(chat.kind), chat.id, clock],
        )?;
        tx.commit()?;
        Ok(())
    }
}

// Messages.
impl Cache {
    /// Runs `f` in a transaction with the current byte total and commits; the
    /// total `f` returns is assigned only after the commit succeeds.
    fn write<T>(
        &mut self,
        f: impl FnOnce(&Transaction<'_>, u64) -> Result<(T, u64), CacheError>,
    ) -> Result<T, CacheError> {
        let tx = self.conn.transaction()?;
        let (out, bytes) = f(&tx, self.bytes)?;
        tx.commit()?;
        self.bytes = bytes;
        Ok(out)
    }

    /// Inserts or replaces messages of one thread, then enforces the bounds.
    /// Messages of other chats, invalid or oversized messages are skipped.
    /// The topic is not stored yet: rows are per chat (schema version 3).
    pub fn put_messages(&mut self, thread: ThreadKey, msgs: &[Message]) -> Result<(), CacheError> {
        let chat = thread.chat;
        self.write(|tx, mut bytes| {
            register_chat(tx, chat)?;
            for m in msgs.iter().filter(|m| m.thread.chat == chat) {
                bytes = upsert(tx, chat, m, bytes)?;
            }
            Ok(((), trim_chat(tx, chat, bytes)?))
        })?;
        self.enforce_budget()
    }

    /// Makes the cache agree with a server page covering ids `lo..=hi`: cached
    /// messages in that range that the server did not return are deleted.
    /// Returns the deleted ids.
    pub fn reconcile_range(
        &mut self,
        thread: ThreadKey,
        lo: MsgId,
        hi: MsgId,
        server: &[Message],
    ) -> Result<Vec<MsgId>, CacheError> {
        let chat = thread.chat;
        let page: Vec<&Message> = server
            .iter()
            .filter(|m| m.thread.chat == chat && (lo..=hi).contains(&m.id))
            .collect();
        let present: HashSet<MsgId> = page.iter().map(|m| m.id).collect();
        let gone = self.write(|tx, bytes| {
            register_chat(tx, chat)?;
            let gone: Vec<MsgId> = ids_in_range(tx, chat, lo, hi)?
                .into_iter()
                .filter(|id| !present.contains(id))
                .collect();
            let mut bytes = delete_ids(tx, chat, &gone, bytes)?;
            for m in page {
                bytes = upsert(tx, chat, m, bytes)?;
            }
            Ok((gone, trim_chat(tx, chat, bytes)?))
        })?;
        self.enforce_budget()?;
        Ok(gone)
    }

    pub fn delete_messages(&mut self, chat: ChatId, ids: &[MsgId]) -> Result<(), CacheError> {
        self.write(|tx, bytes| Ok(((), delete_ids(tx, chat, ids, bytes)?)))
    }

    /// Deletion without a chat (private chats and basic groups, whose ids are
    /// account-unique). Returns what was deleted, grouped by chat.
    pub fn delete_unscoped(
        &mut self,
        ids: &[MsgId],
    ) -> Result<Vec<(ChatId, Vec<MsgId>)>, CacheError> {
        self.write(|tx, mut bytes| {
            let mut deleted = Vec::new();
            for &id in ids {
                let owner: Option<(i64, i64)> = tx
                    .query_row(
                        "SELECT kind, chat FROM messages WHERE kind IN (0, 1) AND id = ?1",
                        [id.0],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let Some(chat) = owner.and_then(|(kind, chat)| chat_id(kind, chat)) else {
                    continue;
                };
                bytes = delete_ids(tx, chat, &[id], bytes)?;
                deleted.push((chat, id));
            }
            Ok((group_by_chat(deleted), bytes))
        })
    }

    /// Newest `limit` messages, ascending.
    pub fn tail(&mut self, thread: ThreadKey, limit: u32) -> Result<Vec<Message>, CacheError> {
        let mut v = self.page(
            thread,
            "id <= ?3 ORDER BY id DESC",
            i64::from(i32::MAX),
            limit,
        )?;
        v.reverse();
        Ok(v)
    }

    /// Up to `limit` messages older than `before`, ascending.
    pub fn before(
        &mut self,
        thread: ThreadKey,
        before: MsgId,
        limit: u32,
    ) -> Result<Vec<Message>, CacheError> {
        let mut v = self.page(
            thread,
            "id < ?3 ORDER BY id DESC",
            i64::from(before.0),
            limit,
        )?;
        v.reverse();
        Ok(v)
    }

    /// Up to `limit` messages newer than `after`, ascending.
    pub fn after(
        &mut self,
        thread: ThreadKey,
        after: MsgId,
        limit: u32,
    ) -> Result<Vec<Message>, CacheError> {
        self.page(thread, "id > ?3 ORDER BY id ASC", i64::from(after.0), limit)
    }

    /// A page of `thread`'s chat: the topic is not stored yet (schema
    /// version 3), so rows are selected per chat.
    fn page(
        &mut self,
        thread: ThreadKey,
        clause: &str,
        bound: i64,
        limit: u32,
    ) -> Result<Vec<Message>, CacheError> {
        let chat = thread.chat;
        let sql = format!(
            "SELECT id, {} FROM messages WHERE kind = ?1 AND chat = ?2 AND {clause} LIMIT ?4",
            bounded_body(5)
        );
        let mut out = Vec::new();
        let mut bad = Vec::new();
        {
            let mut stmt = self.conn.prepare_cached(&sql)?;
            let args = params![kind_code(chat.kind), chat.id, bound, limit, row_limit()];
            let mut rows = stmt.query(args)?;
            while let Some(row) = rows.next()? {
                let id: i32 = row.get(0)?;
                match decode_message(chat, id, row.get(1)?) {
                    Some(m) => out.push(m),
                    None => bad.push(MsgId(id)),
                }
            }
        }
        if !bad.is_empty() {
            self.delete_messages(chat, &bad)?;
        }
        Ok(out)
    }

    /// Evicts the oldest messages of the least recently used chats until under
    /// budget. Each step is an indexed lookup plus one bounded delete.
    fn enforce_budget(&mut self) -> Result<(), CacheError> {
        for _ in 0..MAX_EVICT_STEPS {
            if self.bytes <= self.budget {
                return Ok(());
            }
            if !self.write(evict_step)? {
                return Err(CacheError::OverBudget);
            }
        }
        if self.bytes <= self.budget {
            Ok(())
        } else {
            Err(CacheError::OverBudget)
        }
    }
}

/// Deletes up to `EVICT_BATCH` of the least recently used chat's oldest
/// messages, forgetting the chat once it is empty. Returns whether there was a
/// chat to evict from.
fn evict_step(tx: &Transaction<'_>, bytes: u64) -> Result<(bool, u64), CacheError> {
    let victim: Option<(i64, i64)> = tx
        .query_row(
            "SELECT kind, chat FROM cached_chats ORDER BY last_access LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((kind, chat)) = victim else {
        return Ok((false, bytes));
    };
    let (rows, freed) = delete_oldest(tx, kind, chat, EVICT_BATCH)?;
    if rows == 0 {
        tx.execute(
            "DELETE FROM cached_chats WHERE kind = ?1 AND chat = ?2",
            params![kind, chat],
        )?;
    }
    Ok((true, bytes.saturating_sub(freed)))
}

/// Ensures `chat` has an LRU row (never opened = evicted first).
fn register_chat(tx: &Transaction<'_>, chat: ChatId) -> Result<(), CacheError> {
    tx.execute(
        "INSERT OR IGNORE INTO cached_chats (kind, chat, last_access) VALUES (?1, ?2, 0)",
        params![kind_code(chat.kind), chat.id],
    )?;
    Ok(())
}

/// Inserts or replaces one message; returns the updated byte total.
fn upsert(tx: &Transaction<'_>, chat: ChatId, m: &Message, bytes: u64) -> Result<u64, CacheError> {
    if !m.is_valid() {
        return Ok(bytes);
    }
    let Some(body) = encode(m)? else {
        return Ok(bytes); // never store what we would refuse to load
    };
    let kind = kind_code(chat.kind);
    let old: Option<i64> = tx
        .query_row(
            "SELECT size FROM messages WHERE kind = ?1 AND chat = ?2 AND id = ?3",
            params![kind, chat.id, m.id.0],
            |r| r.get(0),
        )
        .optional()?;
    let size = i64::try_from(body.len()).unwrap_or(i64::MAX);
    tx.execute(
        "INSERT OR REPLACE INTO messages (kind, chat, id, size, body) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![kind, chat.id, m.id.0, size, body],
    )?;
    Ok(bytes
        .saturating_sub(to_u64(old.unwrap_or(0)))
        .saturating_add(to_u64(size)))
}

/// Deletes `ids` of `chat`; returns the updated byte total.
fn delete_ids(
    tx: &Transaction<'_>,
    chat: ChatId,
    ids: &[MsgId],
    bytes: u64,
) -> Result<u64, CacheError> {
    let mut stmt = tx.prepare_cached(
        "DELETE FROM messages WHERE kind = ?1 AND chat = ?2 AND id = ?3 RETURNING size",
    )?;
    let kind = kind_code(chat.kind);
    let mut bytes = bytes;
    for id in ids {
        let size: Option<i64> = stmt
            .query_row(params![kind, chat.id, id.0], |r| r.get(0))
            .optional()?;
        bytes = bytes.saturating_sub(to_u64(size.unwrap_or(0)));
    }
    Ok(bytes)
}

/// Deletes the `limit` oldest messages of a chat, given by its raw SQL key so
/// eviction also works on rows whose kind no longer decodes. Returns the number
/// of rows deleted and the bytes they freed.
fn delete_oldest(
    tx: &Transaction<'_>,
    kind: i64,
    chat: i64,
    limit: i64,
) -> Result<(usize, u64), CacheError> {
    let mut stmt = tx.prepare_cached(
        "DELETE FROM messages WHERE kind = ?1 AND chat = ?2 AND id IN
         (SELECT id FROM messages WHERE kind = ?1 AND chat = ?2 ORDER BY id LIMIT ?3)
         RETURNING size",
    )?;
    let mut rows = stmt.query(params![kind, chat, limit])?;
    let (mut count, mut freed) = (0usize, 0u64);
    while let Some(row) = rows.next()? {
        count += 1;
        freed = freed.saturating_add(to_u64(row.get(0)?));
    }
    Ok((count, freed))
}

fn ids_in_range(
    tx: &Transaction<'_>,
    chat: ChatId,
    lo: MsgId,
    hi: MsgId,
) -> Result<Vec<MsgId>, CacheError> {
    let mut stmt = tx.prepare_cached(
        "SELECT id FROM messages WHERE kind = ?1 AND chat = ?2 AND id BETWEEN ?3 AND ?4",
    )?;
    let ids = stmt
        .query_map(params![kind_code(chat.kind), chat.id, lo.0, hi.0], |r| {
            Ok(MsgId(r.get(0)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

/// Drops the oldest messages of `chat` beyond `MAX_CACHED_PER_CHAT`; returns the
/// updated byte total.
fn trim_chat(tx: &Transaction<'_>, chat: ChatId, bytes: u64) -> Result<u64, CacheError> {
    let kind = kind_code(chat.kind);
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM messages WHERE kind = ?1 AND chat = ?2",
        params![kind, chat.id],
        |r| r.get(0),
    )?;
    let excess = count - i64::from(MAX_CACHED_PER_CHAT);
    if excess <= 0 {
        return Ok(bytes);
    }
    let (_, freed) = delete_oldest(tx, kind, chat.id, excess)?;
    Ok(bytes.saturating_sub(freed))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `kind IN (0, 1)` SQL (index and unscoped deletion) must select exactly
    /// the kinds whose message ids are account-unique.
    #[test]
    fn account_unique_kinds_match_sql_codes() {
        for kind in [PeerKind::User, PeerKind::Chat, PeerKind::Channel] {
            let code = kind_code(kind);
            assert_eq!(kind_from_code(code), Some(kind));
            assert_eq!(
                kind.has_account_unique_ids(),
                [0, 1].contains(&code),
                "{kind:?}"
            );
        }
    }
}
