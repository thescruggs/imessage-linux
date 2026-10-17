//! SQLite message store (`state_dir/messages.db`).
//!
//! * One connection, WAL journal, `foreign_keys = ON`, `synchronous = NORMAL`.
//! * Schema = the embedded `migrations/NNNN_*.sql` files, applied in order
//!   inside a transaction each and recorded in `schema_migrations`. A
//!   database written by a *newer* imessaged (higher version) is refused.
//! * `messages.db`, `-wal` and `-shm` are mode 0600 (the state directory is
//!   0700 anyway).
//! * Attachment download locators contain the per-file decryption key, so
//!   they are stored sealed with the state key ([`crate::state::seal_bytes`],
//!   same AES-256-GCM format as `state.enc`).
//!
//! **Concurrency model.** [`Store`] is synchronous: a `Mutex` around one
//! `rusqlite::Connection`. Async code goes through [`StoreHandle::call`],
//! which runs the closure on tokio's blocking pool (`spawn_blocking`), so
//! SQLite I/O never blocks a runtime worker. Calls are serialized by the
//! mutex; every multi-statement operation runs in one transaction.
//!
//! Typing indicators and connection state are *not* stored (in memory in the
//! daemon).

use std::collections::VecDeque;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use imessage_common::model::{
    Attachment, AttachmentId, Chat, ChatBackground, ChatId, DeliveryState, Handle, Message, MessageId, MessagePart, Reaction, ReactionKind,
};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::engine::{BackgroundRef, EngineEvent, MAX_BACKGROUND_BYTES};
use crate::keyring::StateKey;

pub const DB_FILE: &str = "messages.db";

/// (version, SQL). Append only; never edit a released migration.
const MIGRATIONS: &[(i64, &str)] =
    &[
        (1, include_str!("migrations/0001_init.sql")),
        (2, include_str!("migrations/0002_chat_backgrounds.sql")),
        (3, include_str!("migrations/0003_background_image.sql")),
        (4, include_str!("migrations/0004_profiles_icons.sql")),
    ];

/// Default / maximum page sizes for list calls.
pub const DEFAULT_PAGE: u32 = 50;
pub const MAX_PAGE: u32 = 500;
/// Delivery updates that arrive before their message row (a send's
/// `MessageSent` can beat the D-Bus handler that inserts the row) are kept
/// here and applied on insert.
const EARLY_DELIVERY_CAP: usize = 256;
/// Same for edits / unsends whose original has not arrived yet (APNs does
/// not guarantee order): held in memory, applied when the original is
/// inserted, so it shows up already edited / unsent.
const EARLY_MOD_CAP: usize = 256;

/// What [`Store::chat_background_locator`] returns. `locator` holds the
/// decryption key, so it is redacted from `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct BackgroundDownload {
    pub background_id: String,
    pub locator: String,
    /// As announced by the sender.
    pub size: u64,
    pub local_path: Option<String>,
}

impl fmt::Debug for BackgroundDownload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackgroundDownload")
            .field("background_id", &self.background_id)
            .field("size", &self.size)
            .field("local_path", &self.local_path)
            .finish_non_exhaustive()
    }
}

/// An edit or unsend held until its message row exists.
#[derive(Debug, Clone)]
enum EarlyMod {
    Edit { part: u64, new_parts: Vec<MessagePart>, sender: Option<Handle>, from_me: bool, at: u64 },
    Unsend { part: u64, sender: Option<Handle>, from_me: bool, at: u64 },
}

/// Bounded in-memory holds for updates that beat their message row.
#[derive(Debug, Default)]
struct Early {
    delivery: VecDeque<(MessageId, DeliveryState, Option<String>)>,
    mods: VecDeque<(MessageId, EarlyMod)>,
}

impl Early {
    fn hold_mod(&mut self, id: &MessageId, m: EarlyMod) {
        if self.mods.len() >= EARLY_MOD_CAP {
            self.mods.pop_front();
        }
        self.mods.push_back((id.clone(), m));
    }

    fn take_mods(&mut self, id: &MessageId) -> Vec<EarlyMod> {
        let mut hit = vec![];
        self.mods.retain(|(mid, m)| {
            if mid == id {
                hit.push(m.clone());
                false
            } else {
                true
            }
        });
        hit
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("database schema version {found} is newer than this imessaged supports ({supported})")]
    TooNew { found: i64, supported: i64 },
    #[error("corrupt row: {0}")]
    Corrupt(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("store is closed")]
    Closed,
    #[error("store task failed: {0}")]
    Task(String),
}

pub type StoreResult<T> = Result<T, StoreError>;

/// Opaque pagination cursor: `"<sent_at>-<seq>"` of the oldest message on
/// the previous page. Ordering is `(sent_at, seq)`, a total order, so pages
/// are stable while new messages arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub sent_at: i64,
    pub seq: i64,
}

impl Cursor {
    pub fn parse(s: &str) -> StoreResult<Cursor> {
        let bad = || StoreError::InvalidInput(format!("invalid cursor {s:?}"));
        let (a, b) = s.split_once('-').ok_or_else(bad)?;
        Ok(Cursor { sent_at: a.parse().map_err(|_| bad())?, seq: b.parse().map_err(|_| bad())? })
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.sent_at, self.seq)
    }
}

/// One page of a chat, oldest first. `next_cursor`: pass as `before` to get
/// the page before this one; `None` = this is the oldest page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessagePage {
    pub messages: Vec<Message>,
    pub next_cursor: Option<String>,
}

/// What an operation changed (for D-Bus signals).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum StoreUpdate {
    /// A message row that did not exist before (incoming or outgoing).
    NewMessage(Message),
    /// Delivery state, reaction, edit or unsend on an existing row.
    MessageUpdated(Message),
    /// Chat metadata, participants or unread count changed.
    ChatUpdated(Chat),
    /// Local files that belonged to removed attachments (the caller deletes them).
    FilesRemoved(Vec<String>),
}

fn delivery_str(d: DeliveryState) -> &'static str {
    match d {
        DeliveryState::Pending => "pending",
        DeliveryState::Sent => "sent",
        DeliveryState::Delivered => "delivered",
        DeliveryState::Read => "read",
        DeliveryState::Failed => "failed",
    }
}

fn delivery_from(s: &str) -> DeliveryState {
    match s {
        "sent" => DeliveryState::Sent,
        "delivered" => DeliveryState::Delivered,
        "read" => DeliveryState::Read,
        "failed" => DeliveryState::Failed,
        _ => DeliveryState::Pending,
    }
}

/// Whether delivery may move from `old` to `new`: forward only
/// (pending < sent < delivered < read); `failed` only replaces pending/sent.
fn delivery_advances(old: DeliveryState, new: DeliveryState) -> bool {
    fn rank(d: DeliveryState) -> u8 {
        match d {
            DeliveryState::Pending => 0,
            DeliveryState::Sent => 1,
            DeliveryState::Delivered => 2,
            DeliveryState::Read => 3,
            DeliveryState::Failed => 0,
        }
    }
    match (old, new) {
        (o, n) if o == n => false,
        (DeliveryState::Pending | DeliveryState::Sent, DeliveryState::Failed) => true,
        (_, DeliveryState::Failed) => false,
        (DeliveryState::Failed, _) => true, // a later success report wins over an error
        (o, n) => rank(n) > rank(o),
    }
}

/// Plain text of the text/mention parts (same rule as `engine::convert`).
pub(crate) fn text_of(parts: &[MessagePart]) -> Option<String> {
    let mut t = String::new();
    for p in parts {
        match p {
            MessagePart::Text { text } | MessagePart::Mention { text, .. } => t.push_str(text),
            _ => {}
        }
    }
    if t.trim().is_empty() { None } else { Some(t) }
}

fn json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("model types serialize")
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Escape `%`, `_` and `\` for `LIKE ... ESCAPE '\'`.
fn like_pattern(q: &str) -> String {
    let mut s = String::with_capacity(q.len() + 2);
    s.push('%');
    for c in q.chars() {
        if matches!(c, '%' | '_' | '\\') {
            s.push('\\');
        }
        s.push(c);
    }
    s.push('%');
    s
}

fn clamp_limit(limit: u32) -> u32 {
    if limit == 0 { DEFAULT_PAGE } else { limit.min(MAX_PAGE) }
}

/// Create `path` 0600 if missing, and tighten it to 0600 if it exists.
fn private_file(path: &Path) -> StoreResult<()> {
    let io = |source| StoreError::Io { path: path.to_path_buf(), source };
    if !path.exists() {
        OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(path).map_err(io)?;
    }
    let mode = fs::metadata(path).map_err(io)?.permissions().mode();
    if mode & 0o777 != 0o600 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(io)?;
    }
    Ok(())
}

struct Inner {
    conn: Option<Connection>,
    early: Early,
}

pub struct Store {
    inner: Mutex<Inner>,
    key: StateKey,
    path: PathBuf,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store").field("path", &self.path).finish_non_exhaustive()
    }
}

impl Store {
    /// Open (creating / migrating) `dir/messages.db`.
    pub fn open(dir: &Path, key: StateKey) -> StoreResult<Store> {
        crate::state::ensure_private_dir(dir).map_err(|source| StoreError::Io { path: dir.to_path_buf(), source })?;
        let path = dir.join(DB_FILE);
        private_file(&path)?;
        let mut conn = Connection::open(&path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            log::warn!("{}: journal_mode is {mode}, not WAL", path.display());
        }
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        migrate(&mut conn)?;
        let store = Store { inner: Mutex::new(Inner { conn: Some(conn), early: Early::default() }), key, path };
        store.tighten_sidecars()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `-wal` / `-shm` inherit the db file's mode in SQLite's unix VFS;
    /// enforce 0600 anyway.
    fn tighten_sidecars(&self) -> StoreResult<()> {
        for suffix in ["-wal", "-shm"] {
            let mut p = self.path.clone().into_os_string();
            p.push(suffix);
            let p = PathBuf::from(p);
            if p.exists() {
                private_file(&p)?;
            }
        }
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Run `f` in one transaction.
    fn tx<R>(&self, f: impl FnOnce(&Transaction<'_>, &mut Early) -> StoreResult<R>) -> StoreResult<R> {
        let mut g = self.lock();
        let Inner { conn, early } = &mut *g;
        let conn = conn.as_mut().ok_or(StoreError::Closed)?;
        let tx = conn.transaction()?;
        let r = f(&tx, early)?;
        tx.commit()?;
        Ok(r)
    }

    pub fn schema_version(&self) -> StoreResult<i64> {
        let g = self.lock();
        let conn = g.conn.as_ref().ok_or(StoreError::Closed)?;
        Ok(conn.query_row("SELECT COALESCE(MAX(version), 0) FROM schema_migrations", [], |r| r.get(0))?)
    }

    // ------------------------------------------------------------------
    // chats
    // ------------------------------------------------------------------

    /// Insert or update a chat: `display_name` when `Some`, participants when
    /// non-empty, `last_message_at` only forwards. Unread count is derived.
    pub fn upsert_chat(&self, chat: &Chat) -> StoreResult<Chat> {
        self.tx(|tx, _| {
            upsert_chat_tx(tx, chat, None)?;
            get_chat_tx(tx, &chat.id)?.ok_or_else(|| StoreError::NotFound(chat.id.to_string()))
        })
    }

    pub fn get_chat(&self, id: &ChatId) -> StoreResult<Option<Chat>> {
        self.tx(|tx, _| get_chat_tx(tx, id))
    }

    /// Newest activity first.
    pub fn list_chats(&self, limit: u32, offset: u32) -> StoreResult<Vec<Chat>> {
        self.tx(|tx, _| {
            let ids: Vec<String> = {
                let mut st = tx.prepare_cached("SELECT id FROM chats ORDER BY COALESCE(last_message_at, 0) DESC, id LIMIT ?1 OFFSET ?2")?;
                st.query_map(params![clamp_limit(limit), offset], |r| r.get(0))?.collect::<Result<_, _>>()?
            };
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(c) = get_chat_tx(tx, &ChatId(id))? {
                    out.push(c);
                }
            }
            Ok(out)
        })
    }

    /// All group chats (for re-seeding the engine after a restart).
    pub fn group_chats(&self) -> StoreResult<Vec<Chat>> {
        self.tx(|tx, _| {
            let ids: Vec<String> = {
                let mut st = tx.prepare_cached("SELECT id FROM chats WHERE is_group = 1")?;
                st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
            };
            let mut out = vec![];
            for id in ids {
                if let Some(c) = get_chat_tx(tx, &ChatId(id))? {
                    out.push(c);
                }
            }
            Ok(out)
        })
    }

    pub fn rename_chat(&self, id: &ChatId, name: Option<&str>, at: u64) -> StoreResult<Option<Chat>> {
        self.tx(|tx, _| rename_chat_tx(tx, id, name, at))
    }

    pub fn set_participants(&self, id: &ChatId, participants: &[Handle], at: u64) -> StoreResult<Option<Chat>> {
        self.tx(|tx, _| set_participants_at_tx(tx, id, participants, at))
    }

    /// `(chat id, unread count)` for every chat with unread messages.
    pub fn unread_counts(&self) -> StoreResult<Vec<(ChatId, u32)>> {
        self.tx(|tx, _| {
            let mut st = tx.prepare_cached("SELECT id, unread_count FROM chats WHERE unread_count > 0 ORDER BY id")?;
            let r = st.query_map([], |r| Ok((ChatId(r.get(0)?), r.get(1)?)))?.collect::<Result<_, _>>()?;
            Ok(r)
        })
    }

    pub fn total_unread(&self) -> StoreResult<u32> {
        self.tx(|tx, _| Ok(tx.query_row("SELECT COALESCE(SUM(unread_count), 0) FROM chats", [], |r| r.get(0))?))
    }

    /// Mark incoming messages read up to and including `up_to` (everything
    /// if `None`). Returns the updated chat.
    pub fn mark_read(&self, chat: &ChatId, up_to: Option<&MessageId>) -> StoreResult<Chat> {
        self.tx(|tx, _| {
            mark_read_tx(tx, chat, up_to)?;
            get_chat_tx(tx, chat)?.ok_or_else(|| StoreError::NotFound(chat.to_string()))
        })
    }

    // ------------------------------------------------------------------
    // messages
    // ------------------------------------------------------------------

    /// Insert a message (and its attachments) or merge a redelivery into the
    /// existing row. Returns the stored message and whether it is new.
    pub fn insert_message(&self, msg: &Message, attachments: &[Attachment]) -> StoreResult<(Message, bool)> {
        self.tx(|tx, early| insert_message_tx(tx, early, &self.key, msg, attachments))
    }

    pub fn get_message(&self, id: &MessageId) -> StoreResult<Option<Message>> {
        self.tx(|tx, _| load_message(tx, id))
    }

    /// Returns the message if it exists and changed. Unknown ids are
    /// remembered (bounded) and applied when the row is inserted.
    pub fn update_delivery(&self, id: &MessageId, state: DeliveryState, error: Option<&str>) -> StoreResult<Option<Message>> {
        self.tx(|tx, early| update_delivery_tx(tx, early, id, state, error))
    }

    pub fn apply_reaction(&self, message_id: &MessageId, reaction: &Reaction, at: u64) -> StoreResult<Option<Message>> {
        self.tx(|tx, _| apply_reaction_tx(tx, message_id, reaction, at))
    }

    /// `sender: None` = a local operation (the caller checked ownership).
    /// Otherwise see [`sender_may_modify`]. An edit for an unknown id is held
    /// (bounded) and applied when the message is inserted.
    pub fn apply_edit(&self, message_id: &MessageId, part: u64, new_parts: &[MessagePart], sender: Option<&Handle>, from_me: bool, at: u64) -> StoreResult<Option<Message>> {
        self.tx(|tx, early| apply_edit_tx(tx, early, message_id, part, new_parts, sender, from_me, at))
    }

    /// Returns the updated message and the local files of removed
    /// attachments (the caller deletes them). Same rules as [`Store::apply_edit`].
    pub fn apply_unsend(&self, message_id: &MessageId, part: u64, sender: Option<&Handle>, from_me: bool, at: u64) -> StoreResult<Option<(Message, Vec<String>)>> {
        self.tx(|tx, early| apply_unsend_tx(tx, early, message_id, part, sender, from_me, at))
    }

    /// [`Store::apply_unsend`] plus the updated chat, in one transaction, as
    /// signal-ready updates (empty if nothing changed).
    pub fn unsend_updates(&self, message_id: &MessageId, part: u64, sender: Option<&Handle>, from_me: bool, at: u64) -> StoreResult<Vec<StoreUpdate>> {
        self.tx(|tx, early| {
            let Some((m, files)) = apply_unsend_tx(tx, early, message_id, part, sender, from_me, at)? else { return Ok(vec![]) };
            let chat = get_chat_tx(tx, &m.chat_id)?;
            let mut out = vec![StoreUpdate::MessageUpdated(m)];
            out.extend(chat.map(StoreUpdate::ChatUpdated));
            if !files.is_empty() {
                out.push(StoreUpdate::FilesRemoved(files));
            }
            Ok(out)
        })
    }

    /// Newest page first in time, returned oldest-first; see [`MessagePage`].
    pub fn list_messages(&self, chat: &ChatId, before: Option<&str>, limit: u32) -> StoreResult<MessagePage> {
        let before = before.filter(|s| !s.is_empty()).map(Cursor::parse).transpose()?;
        let limit = clamp_limit(limit);
        self.tx(|tx, _| {
            let rows: Vec<(String, i64, i64)> = {
                let mut st = tx.prepare_cached(
                    "SELECT id, sent_at, seq FROM messages WHERE chat_id = ?1
                       AND (?2 IS NULL OR sent_at < ?2 OR (sent_at = ?2 AND seq < ?3))
                     ORDER BY sent_at DESC, seq DESC LIMIT ?4",
                )?;
                st.query_map(params![chat.0, before.map(|c| c.sent_at), before.map(|c| c.seq), limit + 1], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?
                .collect::<Result<_, _>>()?
            };
            let more = rows.len() > limit as usize;
            let rows = &rows[..rows.len().min(limit as usize)];
            let next_cursor = if more { rows.last().map(|(_, sent_at, seq)| Cursor { sent_at: *sent_at, seq: *seq }.to_string()) } else { None };
            let mut messages = Vec::with_capacity(rows.len());
            for (id, ..) in rows.iter().rev() {
                if let Some(m) = load_message(tx, &MessageId(id.clone()))? {
                    messages.push(m);
                }
            }
            Ok(MessagePage { messages, next_cursor })
        })
    }

    /// Case-insensitive (ASCII) substring search over message text and
    /// subjects, newest first. Unsent messages are excluded.
    pub fn search(&self, query: &str, limit: u32) -> StoreResult<Vec<Message>> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(vec![]);
        }
        let pat = like_pattern(q);
        self.tx(|tx, _| {
            let ids: Vec<String> = {
                let mut st = tx.prepare_cached(
                    "SELECT id FROM messages WHERE unsent = 0
                       AND (text LIKE ?1 ESCAPE '\\' OR subject LIKE ?1 ESCAPE '\\')
                     ORDER BY sent_at DESC, seq DESC LIMIT ?2",
                )?;
                st.query_map(params![pat, clamp_limit(limit)], |r| r.get(0))?.collect::<Result<_, _>>()?
            };
            let mut out = vec![];
            for id in ids {
                if let Some(m) = load_message(tx, &MessageId(id))? {
                    out.push(m);
                }
            }
            Ok(out)
        })
    }

    // ------------------------------------------------------------------
    // attachments
    // ------------------------------------------------------------------

    /// Insert (or fill in the locator / local path of) an attachment. The
    /// message row must exist.
    pub fn upsert_attachment(&self, a: &Attachment) -> StoreResult<()> {
        self.tx(|tx, _| upsert_attachment_tx(tx, &self.key, a))
    }

    /// The attachment with its locator decrypted into `mmcs` (empty if none).
    /// `Attachment`'s `Debug` redacts the locator.
    pub fn get_attachment(&self, id: &AttachmentId) -> StoreResult<Option<Attachment>> {
        self.tx(|tx, _| {
            let row = tx
                .query_row(
                    "SELECT id, message_id, mime, filename, size, local_path, locator FROM attachments WHERE id = ?1",
                    params![id.0],
                    |r| {
                        Ok((
                            Attachment {
                                id: AttachmentId(r.get(0)?),
                                message_id: MessageId(r.get(1)?),
                                mime: r.get(2)?,
                                filename: r.get(3)?,
                                size: r.get::<_, i64>(4)? as u64,
                                local_path: r.get(5)?,
                                mmcs: String::new(),
                            },
                            r.get::<_, Option<Vec<u8>>>(6)?,
                        ))
                    },
                )
                .optional()?;
            let Some((mut a, sealed)) = row else { return Ok(None) };
            if let Some(sealed) = sealed {
                // A locator that does not authenticate was sealed with a
                // different state key (state.enc moved aside / key provider
                // switched): the file key is gone, so the attachment can no
                // longer be downloaded. Report it as such, not as a corrupt
                // database. See docs/DAEMON.md (recovery).
                let plain = crate::state::open_bytes(&self.key, &sealed).map_err(|e| {
                    log::warn!("attachment {}: cannot open its sealed locator ({e}); was the state key changed?", a.id);
                    StoreError::NotFound("attachment locator unreadable (state key changed)".into())
                })?;
                a.mmcs = String::from_utf8(plain).map_err(|_| StoreError::Corrupt(format!("attachment {} locator: not UTF-8", a.id)))?;
            }
            Ok(Some(a))
        })
    }

    /// Attachments of a message, locators omitted.
    pub fn list_attachments(&self, message_id: &MessageId) -> StoreResult<Vec<Attachment>> {
        self.tx(|tx, _| {
            let mut st = tx.prepare_cached("SELECT id, mime, filename, size, local_path FROM attachments WHERE message_id = ?1 ORDER BY id")?;
            let r = st
                .query_map(params![message_id.0], |r| {
                    Ok(Attachment {
                        id: AttachmentId(r.get(0)?),
                        message_id: message_id.clone(),
                        mime: r.get(1)?,
                        filename: r.get(2)?,
                        size: r.get::<_, i64>(3)? as u64,
                        local_path: r.get(4)?,
                        mmcs: String::new(),
                    })
                })?
                .collect::<Result<_, _>>()?;
            Ok(r)
        })
    }

    pub fn set_attachment_path(&self, id: &AttachmentId, local_path: Option<&str>) -> StoreResult<()> {
        self.tx(|tx, _| {
            let n = tx.execute("UPDATE attachments SET local_path = ?2 WHERE id = ?1", params![id.0, local_path])?;
            if n == 0 { Err(StoreError::NotFound(id.to_string())) } else { Ok(()) }
        })
    }

    /// Delete an attachment row; returns its local file (caller deletes it).
    pub fn delete_attachment(&self, id: &AttachmentId) -> StoreResult<Option<String>> {
        self.tx(|tx, _| {
            let p: Option<Option<String>> =
                tx.query_row("SELECT local_path FROM attachments WHERE id = ?1", params![id.0], |r| r.get(0)).optional()?;
            tx.execute("DELETE FROM attachments WHERE id = ?1", params![id.0])?;
            Ok(p.flatten())
        })
    }

    // ------------------------------------------------------------------
    // transcript backgrounds
    // ------------------------------------------------------------------

    /// Set (`Some`) or remove (`None`) a chat's background, unless a newer
    /// change (by `at`) is already stored. Returns the updated chat and the
    /// replaced background's local file, if any.
    /// A `Set` announcing more than [`MAX_BACKGROUND_BYTES`] is ignored
    /// (logged): it would never be downloaded.
    pub fn set_chat_background(&self, id: &ChatId, bg: Option<&BackgroundRef>, by: Option<&Handle>, at: u64) -> StoreResult<Vec<StoreUpdate>> {
        if let Some(b) = bg.filter(|b| b.size > MAX_BACKGROUND_BYTES) {
            log::warn!("chat {id}: ignoring background {} of {} bytes (limit {MAX_BACKGROUND_BYTES})", b.background_id, b.size);
            return Ok(vec![]);
        }
        self.tx(|tx, _| {
            ensure_chat(tx, id)?;
            let at_i = to_i64(at);
            let (cur_at, cur_id, cur_files): (Option<i64>, Option<String>, Vec<String>) = tx
                .query_row("SELECT set_at, background_id, local_path, image_path FROM chat_backgrounds WHERE chat_id = ?1", params![id.0], |r| {
                    Ok((r.get(0)?, r.get(1)?, [r.get::<_, Option<String>>(2)?, r.get(3)?].into_iter().flatten().collect()))
                })
                .optional()?
                .unwrap_or((None, None, vec![]));
            let removed_at: Option<i64> =
                tx.query_row("SELECT background_removed_at FROM chats WHERE id = ?1", params![id.0], |r| r.get(0))?;
            if cur_at.is_some_and(|c| c > at_i) || removed_at.is_some_and(|r| r > at_i) {
                return Ok(vec![]);
            }
            match bg {
                Some(b) if cur_id.as_deref() == Some(b.background_id.as_str()) => return Ok(vec![]),
                Some(b) => {
                    let sealed = if b.locator.is_empty() { None } else { Some(crate::state::seal_bytes(&self.key, b.locator.as_bytes())) };
                    tx.execute(
                        "INSERT OR REPLACE INTO chat_backgrounds
                             (chat_id, background_id, payload_version, seq, size, locator, local_path, set_by, set_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8)",
                        params![id.0, b.background_id, b.payload_version, to_i64(b.seq), to_i64(b.size), sealed, by.map(|h| h.uri.as_str()), at_i],
                    )?;
                }
                None => {
                    tx.execute("UPDATE chats SET background_removed_at = ?2 WHERE id = ?1", params![id.0, at_i])?;
                    if tx.execute("DELETE FROM chat_backgrounds WHERE chat_id = ?1", params![id.0])? == 0 {
                        return Ok(vec![]);
                    }
                }
            }
            let mut out: Vec<StoreUpdate> = get_chat_tx(tx, id)?.map(StoreUpdate::ChatUpdated).into_iter().collect();
            if !cur_files.is_empty() {
                out.push(StoreUpdate::FilesRemoved(cur_files));
            }
            Ok(out)
        })
    }

    /// Install a background that did not arrive over iMessage (imported
    /// from a Mac): no locator, `set_at` = `at` (the import time), so a
    /// redelivered older change does not undo it and any newer one wins.
    /// Replaces whatever is there; returns the update (and the replaced
    /// file).
    pub fn import_chat_background(
        &self,
        id: &ChatId,
        background_id: &str,
        payload_version: u32,
        payload: &str,
        image: Option<&str>,
        at: u64,
    ) -> StoreResult<Vec<StoreUpdate>> {
        self.tx(|tx, _| {
            ensure_chat(tx, id)?;
            let old: Vec<String> = tx
                .query_row("SELECT local_path, image_path FROM chat_backgrounds WHERE chat_id = ?1", params![id.0], |r| {
                    Ok([r.get::<_, Option<String>>(0)?, r.get(1)?])
                })
                .optional()?
                .into_iter()
                .flatten()
                .flatten()
                .filter(|p| p != payload && Some(p.as_str()) != image)
                .collect();
            tx.execute(
                "INSERT OR REPLACE INTO chat_backgrounds
                     (chat_id, background_id, payload_version, seq, size, locator, local_path, image_path, set_by, set_at)
                     VALUES (?1, ?2, ?3, 0, 0, NULL, ?4, ?5, NULL, ?6)",
                params![id.0, background_id, payload_version, payload, image, to_i64(at)],
            )?;
            let mut out: Vec<StoreUpdate> = get_chat_tx(tx, id)?.map(StoreUpdate::ChatUpdated).into_iter().collect();
            if !old.is_empty() {
                out.push(StoreUpdate::FilesRemoved(old));
            }
            Ok(out)
        })
    }

    /// The current background's id, (opened) download locator, announced
    /// size and downloaded payload path; `None` if there is no background
    /// or it has no locator (imported).
    pub fn chat_background_locator(&self, id: &ChatId) -> StoreResult<Option<BackgroundDownload>> {
        self.tx(|tx, _| {
            // background_id, sealed locator, size, local_path
            type Row = (String, Option<Vec<u8>>, i64, Option<String>);
            let row: Option<Row> = tx
                .query_row("SELECT background_id, locator, size, local_path FROM chat_backgrounds WHERE chat_id = ?1", params![id.0], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })
                .optional()?;
            let Some((background_id, Some(sealed), size, local_path)) = row else { return Ok(None) };
            match crate::state::open_bytes(&self.key, &sealed) {
                Ok(plain) => Ok(Some(BackgroundDownload {
                    background_id,
                    locator: String::from_utf8_lossy(&plain).into_owned(),
                    size: size.max(0) as u64,
                    local_path,
                })),
                Err(e) => {
                    log::warn!("chat {id}: cannot open its sealed background locator ({e}); was the state key changed?");
                    Ok(None)
                }
            }
        })
    }

    /// Record where background `background_id` of `id` was downloaded to
    /// and the image extracted from it. Returns the updated chat, or `None`
    /// if the chat's background changed meanwhile.
    pub fn set_background_files(&self, id: &ChatId, background_id: &str, payload: Option<&str>, image: Option<&str>) -> StoreResult<Option<Chat>> {
        self.tx(|tx, _| {
            let n = tx.execute(
                "UPDATE chat_backgrounds SET local_path = ?3, image_path = ?4 WHERE chat_id = ?1 AND background_id = ?2",
                params![id.0, background_id, payload, image],
            )?;
            if n == 0 { Ok(None) } else { get_chat_tx(tx, id) }
        })
    }

    // ------------------------------------------------------------------
    // maintenance
    // ------------------------------------------------------------------

    /// Delete a chat with all its messages, attachments and reactions.
    /// Returns local attachment files to delete.
    pub fn purge_chat(&self, id: &ChatId) -> StoreResult<Vec<String>> {
        self.tx(|tx, _| {
            let mut paths = attachment_paths(tx, "SELECT a.local_path FROM attachments a JOIN messages m ON m.id = a.message_id WHERE m.chat_id = ?1", id.0.clone())?;
            paths.extend(attachment_paths(tx, "SELECT local_path FROM chat_backgrounds WHERE chat_id = ?1", id.0.clone())?);
            paths.extend(attachment_paths(tx, "SELECT image_path FROM chat_backgrounds WHERE chat_id = ?1", id.0.clone())?);
            paths.extend(attachment_paths(tx, "SELECT icon_path FROM chats WHERE id = ?1", id.0.clone())?);
            tx.execute("DELETE FROM reactions WHERE message_id IN (SELECT id FROM messages WHERE chat_id = ?1)", params![id.0])?;
            tx.execute("DELETE FROM chats WHERE id = ?1", params![id.0])?;
            Ok(paths)
        })
    }

    /// Delete messages older than `before` (ms). Returns (messages deleted,
    /// local files to delete).
    pub fn purge_before(&self, before: u64) -> StoreResult<(usize, Vec<String>)> {
        let before = to_i64(before);
        self.tx(|tx, _| {
            let paths = attachment_paths(tx, "SELECT a.local_path FROM attachments a JOIN messages m ON m.id = a.message_id WHERE m.sent_at < ?1", before)?;
            tx.execute("DELETE FROM reactions WHERE message_id IN (SELECT id FROM messages WHERE sent_at < ?1)", params![before])?;
            let n = tx.execute("DELETE FROM messages WHERE sent_at < ?1", params![before])?;
            let chats: Vec<String> = {
                let mut st = tx.prepare("SELECT id FROM chats")?;
                st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
            };
            for c in chats {
                refresh_unread(tx, &ChatId(c))?;
            }
            Ok((n, paths))
        })
    }

    /// `VACUUM` + WAL checkpoint (truncate).
    pub fn vacuum(&self) -> StoreResult<()> {
        let g = self.lock();
        let conn = g.conn.as_ref().ok_or(StoreError::Closed)?;
        conn.execute_batch("VACUUM")?;
        checkpoint(conn)
    }

    /// Fold the WAL back into the database file.
    pub fn checkpoint(&self) -> StoreResult<()> {
        let g = self.lock();
        checkpoint(g.conn.as_ref().ok_or(StoreError::Closed)?)
    }

    /// Checkpoint and close. Later calls fail with [`StoreError::Closed`].
    pub fn close(&self) -> StoreResult<()> {
        let mut g = self.lock();
        if let Some(conn) = g.conn.take() {
            checkpoint(&conn)?;
            conn.close().map_err(|(_, e)| StoreError::Sqlite(e))?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // engine events
    // ------------------------------------------------------------------

    /// Persist one engine event. Typing / connection / registration / 2FA /
    /// error events are not stored and return nothing.
    pub fn apply_event(&self, ev: &EngineEvent) -> StoreResult<Vec<StoreUpdate>> {
        let mut out = vec![];
        match ev {
            EngineEvent::MessageReceived { message, chat, attachments } => {
                let (m, chat) = self.tx(|tx, early| {
                    upsert_chat_tx(tx, chat, Some(message.sent_at))?;
                    let (m, new) = insert_message_tx(tx, early, &self.key, message, attachments)?;
                    Ok(((m, new), get_chat_tx(tx, &message.chat_id)?))
                })?;
                let (m, new) = m;
                out.push(if new { StoreUpdate::NewMessage(m) } else { StoreUpdate::MessageUpdated(m) });
                out.extend(chat.map(StoreUpdate::ChatUpdated));
            }
            EngineEvent::MessageSent { id, delivery, error, .. } => {
                out.extend(self.update_delivery(id, *delivery, error.as_deref())?.map(StoreUpdate::MessageUpdated));
            }
            EngineEvent::Delivered(r) => {
                out.extend(self.update_delivery(&r.message_id, DeliveryState::Delivered, None)?.map(StoreUpdate::MessageUpdated));
            }
            EngineEvent::Read(r) => {
                // * a peer read our message -> its delivery becomes Read;
                // * we read someone else's message on another of our
                //   devices -> local read marker.
                // Anything else (a peer "reading" their own message, or our
                // own device reading our own message) is ignored: a peer
                // must not be able to clear our unread state.
                let updates = self.tx(|tx, early| {
                    let own: Option<(bool, String)> = tx
                        .query_row("SELECT is_from_me, chat_id FROM messages WHERE id = ?1", params![r.message_id.0], |row| Ok((row.get(0)?, row.get(1)?)))
                        .optional()?;
                    match (own, r.by_me) {
                        (Some((true, _)), false) | (None, false) => Ok(update_delivery_tx(tx, early, &r.message_id, DeliveryState::Read, None)?
                            .map(StoreUpdate::MessageUpdated)
                            .into_iter()
                            .collect::<Vec<_>>()),
                        (Some((false, chat)), true) => {
                            let chat = ChatId(chat);
                            mark_read_tx(tx, &chat, Some(&r.message_id))?;
                            Ok(get_chat_tx(tx, &chat)?.map(StoreUpdate::ChatUpdated).into_iter().collect())
                        }
                        (Some((false, _)), false) => {
                            log::warn!("ignoring a read receipt from a peer for message {} (not ours)", r.message_id);
                            Ok(vec![])
                        }
                        (Some((true, _)), true) | (None, true) => Ok(vec![]),
                    }
                })?;
                out.extend(updates);
            }
            EngineEvent::Reaction { message_id, reaction, at, .. } => {
                out.extend(self.apply_reaction(message_id, reaction, *at)?.map(StoreUpdate::MessageUpdated));
            }
            EngineEvent::Edited { message_id, part, new_parts, sender, from_me, at, .. } => {
                out.extend(self.apply_edit(message_id, *part, new_parts, Some(sender), *from_me, *at)?.map(StoreUpdate::MessageUpdated));
            }
            EngineEvent::Unsent { message_id, part, sender, from_me, at, .. } => {
                out.extend(self.unsend_updates(message_id, *part, Some(sender), *from_me, *at)?);
            }
            EngineEvent::ChatRenamed { chat_id, name, at, .. } => {
                out.extend(self.rename_chat(chat_id, name.as_deref(), *at)?.map(StoreUpdate::ChatUpdated));
            }
            EngineEvent::ParticipantsChanged { chat_id, participants, at, .. } => {
                out.extend(self.set_participants(chat_id, participants, *at)?.map(StoreUpdate::ChatUpdated));
            }
            EngineEvent::ChatBackground { chat_id, background, by, at } => {
                out.extend(self.set_chat_background(chat_id, background.as_ref(), Some(by), *at)?);
            }
            EngineEvent::ChatIcon { chat_id, icon, at, .. } => {
                out.extend(self.set_chat_icon(chat_id, icon.as_ref(), *at)?);
            }
            // The hub records shares itself (Hub::on_profile_shared): the
            // outcome decides about a fetch, which no StoreUpdate expresses.
            EngineEvent::ProfileShared { .. }
            | EngineEvent::Typing(_)
            | EngineEvent::ConnectionChanged(_)
            | EngineEvent::RegistrationChanged(_)
            | EngineEvent::TwoFactorRequired
            | EngineEvent::Error(_) => {}
        }
        Ok(out)
    }
}

// ----------------------------------------------------------------------
// migrations
// ----------------------------------------------------------------------

pub fn latest_schema_version() -> i64 {
    MIGRATIONS.last().map(|m| m.0).unwrap_or(0)
}

fn migrate(conn: &mut Connection) -> StoreResult<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL)")?;
    let current: i64 = conn.query_row("SELECT COALESCE(MAX(version), 0) FROM schema_migrations", [], |r| r.get(0))?;
    let latest = latest_schema_version();
    if current > latest {
        return Err(StoreError::TooNew { found: current, supported: latest });
    }
    for (version, sql) in MIGRATIONS.iter().filter(|(v, _)| *v > current) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute("INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)", params![version, now_ms()])?;
        tx.commit()?;
        log::info!("message store migrated to schema version {version}");
    }
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn checkpoint(conn: &Connection) -> StoreResult<()> {
    // Returns (busy, log frames, checkpointed frames).
    let _: (i64, i64, i64) = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(())
}

// ----------------------------------------------------------------------
// transaction helpers
// ----------------------------------------------------------------------

fn upsert_handle(tx: &Transaction<'_>, h: &Handle) -> StoreResult<()> {
    tx.prepare_cached("INSERT INTO handles (uri, display) VALUES (?1, ?2) ON CONFLICT(uri) DO UPDATE SET display = COALESCE(excluded.display, handles.display)")?
        .execute(params![h.uri, h.display])?;
    Ok(())
}

fn ensure_chat(tx: &Transaction<'_>, id: &ChatId) -> StoreResult<()> {
    tx.prepare_cached("INSERT OR IGNORE INTO chats (id, is_group) VALUES (?1, ?2)")?.execute(params![id.0, id.is_group()])?;
    Ok(())
}

fn replace_participants(tx: &Transaction<'_>, id: &ChatId, participants: &[Handle]) -> StoreResult<()> {
    tx.execute("DELETE FROM chat_participants WHERE chat_id = ?1", params![id.0])?;
    let mut seen = std::collections::HashSet::new();
    for (i, h) in participants.iter().enumerate() {
        if !seen.insert(h.uri.clone()) {
            continue;
        }
        upsert_handle(tx, h)?;
        tx.prepare_cached("INSERT INTO chat_participants (chat_id, handle, position) VALUES (?1, ?2, ?3)")?
            .execute(params![id.0, h.uri, i as i64])?;
    }
    Ok(())
}

/// `at`: timestamp of the message carrying this chat snapshot; `None` =
/// authoritative (explicit upsert). An older snapshot does not override a
/// newer rename / membership change, and an undated one (`Some(0)`) never
/// replaces a non-empty participant list.
fn upsert_chat_tx(tx: &Transaction<'_>, chat: &Chat, at: Option<u64>) -> StoreResult<()> {
    ensure_chat(tx, &chat.id)?;
    let at = at.map(to_i64);
    tx.execute("UPDATE chats SET is_group = ?2 WHERE id = ?1", params![chat.id.0, chat.is_group])?;
    if let Some(name) = &chat.display_name {
        tx.execute(
            "UPDATE chats SET display_name = ?2 WHERE id = ?1 AND (?3 IS NULL OR name_at IS NULL OR name_at <= ?3)",
            params![chat.id.0, name, at],
        )?;
    }
    if !chat.participants.is_empty() {
        let newer: bool = tx.query_row(
            "SELECT (?2 IS NULL OR participants_at IS NULL OR participants_at <= ?2)
                AND (?2 IS NULL OR ?2 > 0 OR NOT EXISTS (SELECT 1 FROM chat_participants WHERE chat_id = ?1))
             FROM chats WHERE id = ?1",
            params![chat.id.0, at],
            |r| r.get(0),
        )?;
        if newer {
            replace_participants(tx, &chat.id, &chat.participants)?;
        }
    }
    if let Some(t) = chat.last_message_at {
        tx.execute(
            "UPDATE chats SET last_message_at = MAX(COALESCE(last_message_at, 0), ?2) WHERE id = ?1",
            params![chat.id.0, to_i64(t)],
        )?;
    }
    Ok(())
}

fn rename_chat_tx(tx: &Transaction<'_>, id: &ChatId, name: Option<&str>, at: u64) -> StoreResult<Option<Chat>> {
    ensure_chat(tx, id)?;
    let n = tx.execute(
        "UPDATE chats SET display_name = ?2, name_at = ?3 WHERE id = ?1 AND (name_at IS NULL OR name_at <= ?3)",
        params![id.0, name, to_i64(at)],
    )?;
    if n == 0 { Ok(None) } else { get_chat_tx(tx, id) }
}

fn set_participants_at_tx(tx: &Transaction<'_>, id: &ChatId, participants: &[Handle], at: u64) -> StoreResult<Option<Chat>> {
    ensure_chat(tx, id)?;
    let n = tx.execute(
        "UPDATE chats SET participants_at = ?2 WHERE id = ?1 AND (participants_at IS NULL OR participants_at <= ?2)",
        params![id.0, to_i64(at)],
    )?;
    if n == 0 {
        return Ok(None);
    }
    replace_participants(tx, id, participants)?;
    get_chat_tx(tx, id)
}

fn get_chat_tx(tx: &Transaction<'_>, id: &ChatId) -> StoreResult<Option<Chat>> {
    let row = tx
        .prepare_cached("SELECT display_name, is_group, last_message_at, unread_count FROM chats WHERE id = ?1")?
        .query_row(params![id.0], |r| {
            Ok((r.get::<_, Option<String>>(0)?, r.get::<_, bool>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, i64>(3)?))
        })
        .optional()?;
    let Some((display_name, is_group, last, unread)) = row else { return Ok(None) };
    let participants = {
        let mut st = tx.prepare_cached(
            "SELECT p.handle, h.display FROM chat_participants p LEFT JOIN handles h ON h.uri = p.handle WHERE p.chat_id = ?1 ORDER BY p.position",
        )?;
        st.query_map(params![id.0], |r| Ok(Handle { uri: r.get(0)?, display: r.get(1)?, profile: None }))?.collect::<Result<Vec<_>, _>>()?
    };
    let mut participants = participants;
    for h in &mut participants {
        h.profile = profiles::handle_profile_tx(tx, &h.uri)?;
    }
    Ok(Some(Chat {
        id: id.clone(),
        participants,
        display_name,
        is_group,
        last_message_at: last.map(|t| t as u64),
        unread_count: unread.clamp(0, u32::MAX as i64) as u32,
        background: chat_background_tx(tx, id)?,
        icon_path: profiles::chat_icon_path_tx(tx, id)?,
    }))
}

fn chat_background_tx(tx: &Transaction<'_>, id: &ChatId) -> StoreResult<Option<ChatBackground>> {
    Ok(tx
        .prepare_cached("SELECT background_id, payload_version, image_path, local_path, set_by, set_at FROM chat_backgrounds WHERE chat_id = ?1")?
        .query_row(params![id.0], |r| {
            Ok(ChatBackground {
                id: r.get(0)?,
                version: r.get::<_, i64>(1)?.clamp(0, u32::MAX as i64) as u32,
                path: r.get(2)?,
                payload: r.get(3)?,
                set_by: r.get(4)?,
                set_at: r.get::<_, i64>(5)?.max(0) as u64,
            })
        })
        .optional()?)
}

fn refresh_unread(tx: &Transaction<'_>, chat: &ChatId) -> StoreResult<()> {
    tx.prepare_cached(
        "UPDATE chats SET unread_count = (SELECT COUNT(*) FROM messages
             WHERE chat_id = ?1 AND is_from_me = 0 AND is_read = 0 AND unsent = 0) WHERE id = ?1",
    )?
    .execute(params![chat.0])?;
    Ok(())
}

fn mark_read_tx(tx: &Transaction<'_>, chat: &ChatId, up_to: Option<&MessageId>) -> StoreResult<()> {
    match up_to {
        Some(m) => {
            let pos: Option<(i64, i64)> = tx
                .query_row("SELECT sent_at, seq FROM messages WHERE id = ?1 AND chat_id = ?2", params![m.0, chat.0], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            let (sent_at, seq) = pos.ok_or_else(|| StoreError::NotFound(format!("message {m} in {chat}")))?;
            tx.execute(
                "UPDATE messages SET is_read = 1 WHERE chat_id = ?1 AND is_read = 0
                   AND (sent_at < ?2 OR (sent_at = ?2 AND seq <= ?3))",
                params![chat.0, sent_at, seq],
            )?;
        }
        None => {
            if get_chat_tx(tx, chat)?.is_none() {
                return Err(StoreError::NotFound(chat.to_string()));
            }
            tx.execute("UPDATE messages SET is_read = 1 WHERE chat_id = ?1 AND is_read = 0", params![chat.0])?;
        }
    }
    refresh_unread(tx, chat)
}

fn row_to_message(r: &Row<'_>) -> rusqlite::Result<(Message, String)> {
    let parts: String = r.get("parts")?;
    let delivery: String = r.get("delivery")?;
    Ok((
        Message {
            id: MessageId(r.get("id")?),
            chat_id: ChatId(r.get("chat_id")?),
            sender: Handle { uri: r.get("sender")?, display: None, profile: None },
            sent_at: r.get::<_, i64>("sent_at")? as u64,
            text: r.get("text")?,
            parts: vec![],
            subject: r.get("subject")?,
            reply_to: r.get::<_, Option<String>>("reply_to")?.map(MessageId),
            is_from_me: r.get("is_from_me")?,
            delivery: delivery_from(&delivery),
            edited: r.get("edited")?,
            unsent: r.get("unsent")?,
            reactions: vec![],
            effect: r.get("effect")?,
            verified: r.get("verified")?,
        },
        parts,
    ))
}

fn load_message(tx: &Transaction<'_>, id: &MessageId) -> StoreResult<Option<Message>> {
    let row = tx.prepare_cached("SELECT * FROM messages WHERE id = ?1")?.query_row(params![id.0], row_to_message).optional()?;
    let Some((mut m, parts)) = row else { return Ok(None) };
    m.parts = serde_json::from_str(&parts).map_err(|e| StoreError::Corrupt(format!("message {id} parts: {e}")))?;
    let mut st = tx.prepare_cached("SELECT kind, sender FROM reactions WHERE message_id = ?1 AND added = 1 ORDER BY at, sender")?;
    let reactions: Vec<(String, String)> = st.query_map(params![id.0], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
    for (kind, sender) in reactions {
        let kind: ReactionKind = serde_json::from_str(&kind).map_err(|e| StoreError::Corrupt(format!("reaction on {id}: {e}")))?;
        m.reactions.push(Reaction { kind, sender: Handle { uri: sender, display: None, profile: None }, added: true });
    }
    Ok(Some(m))
}

fn insert_message_tx(
    tx: &Transaction<'_>,
    early: &mut Early,
    key: &StateKey,
    msg: &Message,
    attachments: &[Attachment],
) -> StoreResult<(Message, bool)> {
    ensure_chat(tx, &msg.chat_id)?;
    if !msg.sender.uri.is_empty() {
        upsert_handle(tx, &msg.sender)?;
    }
    let existing: Option<(String, bool, bool)> = tx
        .query_row("SELECT delivery, edited, unsent FROM messages WHERE id = ?1", params![msg.id.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    let new = existing.is_none();
    let mut unsent_row = existing.as_ref().map(|e| e.2).unwrap_or(msg.unsent);
    match existing {
        None => {
            tx.prepare_cached(
                "INSERT INTO messages (id, chat_id, sender, sent_at, text, parts, subject, reply_to, is_from_me, delivery,
                                       edited, unsent, verified, effect, is_read)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            )?
            .execute(params![
                msg.id.0,
                msg.chat_id.0,
                msg.sender.uri,
                to_i64(msg.sent_at),
                msg.text,
                json(&msg.parts),
                msg.subject,
                msg.reply_to.as_ref().map(|r| r.0.clone()),
                msg.is_from_me,
                delivery_str(msg.delivery),
                msg.edited,
                msg.unsent,
                msg.verified,
                msg.effect,
                msg.is_from_me, // our own messages are read
            ])?;
        }
        Some((delivery, edited, unsent)) => {
            // Redelivery / echo: never resurrect unsent content, never undo
            // an edit, never move delivery backwards. `verified` only
            // downgrades.
            if !unsent && !edited {
                tx.execute(
                    "UPDATE messages SET text = ?2, parts = ?3, subject = ?4, reply_to = COALESCE(?5, reply_to), effect = COALESCE(?6, effect)
                     WHERE id = ?1",
                    params![msg.id.0, msg.text, json(&msg.parts), msg.subject, msg.reply_to.as_ref().map(|r| r.0.clone()), msg.effect],
                )?;
            }
            tx.execute("UPDATE messages SET verified = verified AND ?2 WHERE id = ?1", params![msg.id.0, msg.verified])?;
            if delivery_advances(delivery_from(&delivery), msg.delivery) {
                tx.execute("UPDATE messages SET delivery = ?2 WHERE id = ?1", params![msg.id.0, delivery_str(msg.delivery)])?;
            }
        }
    }
    // Edits / unsends that beat the row: edits in timestamp order, then the
    // unsend (final). The row is new, so nothing is on disk to delete.
    if new {
        let mut mods = early.take_mods(&msg.id);
        mods.sort_by_key(|m| match m {
            EarlyMod::Edit { at, .. } => (0, *at),
            EarlyMod::Unsend { at, .. } => (1, *at),
        });
        for m in mods {
            match m {
                EarlyMod::Edit { part, new_parts, sender, from_me, at } => {
                    apply_edit_tx(tx, early, &msg.id, part, &new_parts, sender.as_ref(), from_me, at)?;
                }
                EarlyMod::Unsend { part, sender, from_me, at } => {
                    if apply_unsend_tx(tx, early, &msg.id, part, sender.as_ref(), from_me, at)?.is_some() {
                        unsent_row = true;
                    }
                }
            }
        }
    }
    // Never (re)create attachment rows - and their sealed locators - for an
    // unsent message (a redelivered original must not bring them back).
    if !unsent_row {
        for a in attachments {
            upsert_attachment_tx(tx, key, a)?;
        }
    }
    // Delivery reports that beat the row.
    let pending: Vec<_> = {
        let mut keep = VecDeque::new();
        let mut hit = vec![];
        for e in early.delivery.drain(..) {
            if e.0 == msg.id { hit.push(e) } else { keep.push_back(e) }
        }
        early.delivery = keep;
        hit
    };
    for (_, state, error) in pending {
        update_delivery_tx(tx, early, &msg.id, state, error.as_deref())?;
    }
    tx.execute(
        "UPDATE chats SET last_message_at = MAX(COALESCE(last_message_at, 0), ?2) WHERE id = ?1",
        params![msg.chat_id.0, to_i64(msg.sent_at)],
    )?;
    refresh_unread(tx, &msg.chat_id)?;
    let m = load_message(tx, &msg.id)?.ok_or_else(|| StoreError::NotFound(msg.id.to_string()))?;
    Ok((m, new))
}

fn upsert_attachment_tx(tx: &Transaction<'_>, key: &StateKey, a: &Attachment) -> StoreResult<()> {
    let sealed = if a.mmcs.is_empty() { None } else { Some(crate::state::seal_bytes(key, a.mmcs.as_bytes())) };
    tx.prepare_cached(
        "INSERT INTO attachments (id, message_id, mime, filename, size, local_path, locator) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET locator = COALESCE(attachments.locator, excluded.locator),
                                       local_path = COALESCE(attachments.local_path, excluded.local_path)",
    )?
    .execute(params![a.id.0, a.message_id.0, a.mime, a.filename, to_i64(a.size), a.local_path, sealed])?;
    Ok(())
}

fn update_delivery_tx(
    tx: &Transaction<'_>,
    early: &mut Early,
    id: &MessageId,
    state: DeliveryState,
    error: Option<&str>,
) -> StoreResult<Option<Message>> {
    let cur: Option<String> = tx.query_row("SELECT delivery FROM messages WHERE id = ?1", params![id.0], |r| r.get(0)).optional()?;
    let Some(cur) = cur else {
        if early.delivery.len() >= EARLY_DELIVERY_CAP {
            early.delivery.pop_front();
        }
        early.delivery.push_back((id.clone(), state, error.map(str::to_string)));
        return Ok(None);
    };
    if !delivery_advances(delivery_from(&cur), state) {
        return Ok(None);
    }
    let error = if state == DeliveryState::Failed { error } else { None };
    tx.execute("UPDATE messages SET delivery = ?2, error = ?3 WHERE id = ?1", params![id.0, delivery_str(state), error])?;
    load_message(tx, id)
}

fn apply_reaction_tx(tx: &Transaction<'_>, message_id: &MessageId, reaction: &Reaction, at: u64) -> StoreResult<Option<Message>> {
    let at = to_i64(at);
    let kind = json(&reaction.kind);
    let existing: Option<(String, bool, i64)> = tx
        .query_row(
            "SELECT kind, added, at FROM reactions WHERE message_id = ?1 AND sender = ?2",
            params![message_id.0, reaction.sender.uri],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let apply = match &existing {
        None => true,
        Some((_, _, old_at)) if *old_at > at => false,
        // The same reaction again (redelivery, a re-run import): no change.
        Some((old_kind, old_added, old_at)) if *old_at == at && *old_kind == kind && *old_added == reaction.added => false,
        // Removing a tapback the sender does not currently have: ignore.
        Some((old_kind, old_added, _)) if !reaction.added => *old_kind == kind && *old_added,
        Some(_) => true,
    };
    if apply {
        tx.execute(
            "INSERT INTO reactions (message_id, sender, kind, added, at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(message_id, sender) DO UPDATE SET kind = excluded.kind, added = excluded.added, at = excluded.at",
            params![message_id.0, reaction.sender.uri, kind, reaction.added, at],
        )?;
        load_message(tx, message_id)
    } else {
        Ok(None)
    }
}

/// Who may edit / unsend `m`:
/// * `sender: None` - a local operation; the caller checked ownership;
/// * our own message - only one of our own devices (`from_me`), from any of
///   our handles;
/// * someone else's message - only its original sender, and never "us".
fn sender_may_modify(m: &Message, sender: Option<&Handle>, from_me: bool) -> bool {
    let Some(s) = sender else { return true };
    let ok = m.is_from_me == from_me && (from_me || s.uri == m.sender.uri);
    if !ok {
        // Message id only: the stranger's handle is not ours to log.
        log::warn!("ignoring an edit/unsend of message {} by someone other than its sender", m.id);
    }
    ok
}

#[allow(clippy::too_many_arguments)]
fn apply_edit_tx(
    tx: &Transaction<'_>,
    early: &mut Early,
    id: &MessageId,
    part: u64,
    new_parts: &[MessagePart],
    sender: Option<&Handle>,
    from_me: bool,
    at: u64,
) -> StoreResult<Option<Message>> {
    let Some(m) = load_message(tx, id)? else {
        early.hold_mod(id, EarlyMod::Edit { part, new_parts: new_parts.to_vec(), sender: sender.cloned(), from_me, at });
        return Ok(None);
    };
    if m.unsent || !sender_may_modify(&m, sender, from_me) {
        return Ok(None);
    }
    let edited_at: Option<i64> = tx.query_row("SELECT edited_at FROM messages WHERE id = ?1", params![id.0], |r| r.get(0))?;
    if edited_at.is_some_and(|e| e > to_i64(at)) {
        return Ok(None); // an older edit arriving late
    }
    let mut parts = m.parts.clone();
    let idx = part as usize;
    if idx < parts.len() && parts.len() > 1 {
        parts.splice(idx..=idx, new_parts.iter().cloned());
    } else {
        // Single-part message (the usual case) or an index we cannot map.
        let atts: Vec<_> = parts.into_iter().filter(|p| matches!(p, MessagePart::Attachment { .. })).collect();
        parts = new_parts.to_vec();
        parts.extend(atts);
    }
    tx.execute(
        "UPDATE messages SET parts = ?2, text = ?3, edited = 1, edited_at = ?4 WHERE id = ?1",
        params![id.0, json(&parts), text_of(&parts), to_i64(at)],
    )?;
    load_message(tx, id)
}

/// Unsend removes the whole message (text, parts, attachments, reactions).
/// iMessage can unsend a single part of a multi-part message, but our
/// `parts` vector is not indexed like rustpush's part numbers, so we treat
/// every unsend as whole-message (the common case: one part per bubble).
fn apply_unsend_tx(
    tx: &Transaction<'_>,
    early: &mut Early,
    id: &MessageId,
    part: u64,
    sender: Option<&Handle>,
    from_me: bool,
    at: u64,
) -> StoreResult<Option<(Message, Vec<String>)>> {
    let Some(m) = load_message(tx, id)? else {
        early.hold_mod(id, EarlyMod::Unsend { part, sender: sender.cloned(), from_me, at });
        return Ok(None);
    };
    let _ = (part, at); // see above; unsend is final, no ordering needed
    if m.unsent || !sender_may_modify(&m, sender, from_me) {
        return Ok(None);
    }
    let files = attachment_paths(tx, "SELECT local_path FROM attachments WHERE message_id = ?1", id.0.clone())?;
    tx.execute("DELETE FROM attachments WHERE message_id = ?1", params![id.0])?;
    tx.execute("DELETE FROM reactions WHERE message_id = ?1", params![id.0])?;
    tx.execute("UPDATE messages SET parts = '[]', text = NULL, subject = NULL, unsent = 1 WHERE id = ?1", params![id.0])?;
    refresh_unread(tx, &m.chat_id)?;
    Ok(load_message(tx, id)?.map(|m| (m, files)))
}

fn attachment_paths<P: rusqlite::ToSql>(tx: &Transaction<'_>, sql: &str, p: P) -> StoreResult<Vec<String>> {
    let mut st = tx.prepare(sql)?;
    let v: Vec<Option<String>> = st.query_map(params![p], |r| r.get(0))?.collect::<Result<_, _>>()?;
    Ok(v.into_iter().flatten().collect())
}

// ----------------------------------------------------------------------
// async handle
// ----------------------------------------------------------------------

/// Cheap-to-clone async front end: every call runs on the blocking pool.
#[derive(Clone, Debug)]
pub struct StoreHandle(Arc<Store>);

impl StoreHandle {
    pub fn new(store: Store) -> StoreHandle {
        StoreHandle(Arc::new(store))
    }

    pub fn store(&self) -> &Store {
        &self.0
    }

    pub async fn call<R, F>(&self, f: F) -> StoreResult<R>
    where
        R: Send + 'static,
        F: FnOnce(&Store) -> StoreResult<R> + Send + 'static,
    {
        let s = self.0.clone();
        tokio::task::spawn_blocking(move || f(&s)).await.map_err(|e| StoreError::Task(e.to_string()))?
    }
}

mod housekeeping;
mod import;
mod profiles;
pub use profiles::{IconDownload, ShareOutcome};
pub use housekeeping::{PurgeSummary, PurgedChat};
pub use import::ImportOutcome;

#[cfg(test)]
mod tests;
