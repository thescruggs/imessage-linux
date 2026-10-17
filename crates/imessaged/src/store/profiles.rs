//! Contacts' shared names and photos (`handle_profiles`) and group photos
//! (`chats.icon_*`), schema version 4.
//!
//! The CloudKit decryption key of a share (and the poster tags) are sealed
//! with the state key like attachment locators; the group photo's MMCS
//! locator too.

use std::fmt;

use imessage_common::model::{Chat, ChatId, Handle, HandleProfile};
use rusqlite::{OptionalExtension, Transaction, params};

use super::{Store, StoreResult, StoreUpdate, ensure_chat, get_chat_tx, to_i64};
use crate::engine::{IconRef, MAX_ICON_BYTES, PosterTags, ShareRef};

/// What [`Store::record_profile_share`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareOutcome {
    /// Older than what is stored, or nothing changed and nothing is missing.
    Nothing,
    /// Same record as before, never fetched successfully (worth a fetch
    /// unless one failed just now).
    Unfetched,
    /// A new or different record: fetch it.
    Changed,
    /// The contact stopped sharing: name and photo were cleared (`files`
    /// = the old photo, for the caller to delete). `had_profile` = there
    /// was something to clear (worth a signal).
    Cleared { files: Vec<String>, had_profile: bool },
}

/// What [`Store::chat_icon_locator`] returns; `locator` holds the file key.
#[derive(Clone, PartialEq, Eq)]
pub struct IconDownload {
    pub icon_id: String,
    pub locator: String,
    pub size: u64,
    pub local_path: Option<String>,
}

impl fmt::Debug for IconDownload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IconDownload")
            .field("icon_id", &self.icon_id)
            .field("size", &self.size)
            .field("local_path", &self.local_path)
            .field("locator", &"<redacted>")
            .finish()
    }
}

fn poster_plist(p: &PosterTags) -> Vec<u8> {
    let dict = plist::Value::Dictionary(plist::Dictionary::from_iter([
        ("lrwt", plist::Value::Data(p.low_res_wallpaper_tag.clone())),
        ("wt", plist::Value::Data(p.wallpaper_tag.clone())),
        ("mt", plist::Value::Data(p.message_tag.clone())),
    ]));
    let mut out = Vec::new();
    let _ = plist::to_writer_binary(&mut out, &dict);
    out
}

fn poster_from_plist(b: &[u8]) -> Option<PosterTags> {
    let v = plist::Value::from_reader(std::io::Cursor::new(b)).ok()?;
    let d = v.as_dictionary()?;
    let get = |k: &str| d.get(k).and_then(|v| v.as_data()).map(<[u8]>::to_vec);
    Some(PosterTags { low_res_wallpaper_tag: get("lrwt")?, wallpaper_tag: get("wt")?, message_tag: get("mt")? })
}

/// A participant's profile for the Chat JSON (only if there is something to
/// show).
pub(super) fn handle_profile_tx(tx: &Transaction<'_>, handle: &str) -> StoreResult<Option<HandleProfile>> {
    let p = tx
        .prepare_cached("SELECT name, first, last, photo_path, fetched_at FROM handle_profiles WHERE handle = ?1")?
        .query_row(params![handle], |r| {
            Ok(HandleProfile {
                name: r.get(0)?,
                first: r.get(1)?,
                last: r.get(2)?,
                photo: r.get(3)?,
                updated_at: r.get::<_, Option<i64>>(4)?.unwrap_or(0).max(0) as u64,
            })
        })
        .optional()?;
    Ok(p.filter(|p| !p.is_empty()))
}

pub(super) fn chat_icon_path_tx(tx: &Transaction<'_>, id: &ChatId) -> StoreResult<Option<String>> {
    Ok(tx.prepare_cached("SELECT icon_path FROM chats WHERE id = ?1")?.query_row(params![id.0], |r| r.get(0)).optional()?.flatten())
}

impl Store {
    // ------------------------------------------------------------------
    // shared profiles
    // ------------------------------------------------------------------

    /// Remember that `handle` shared `share` (or stopped, `None`) in a
    /// message dated `at`. A share older than the stored one is ignored, so
    /// a late redelivery cannot bring back an old record or undo a stop.
    pub fn record_profile_share(&self, handle: &Handle, share: Option<&ShareRef>, at: u64) -> StoreResult<ShareOutcome> {
        let at_i = to_i64(at);
        self.tx(|tx, _| {
            type Row = (Option<String>, i64, Option<String>, Option<String>, Option<String>, Option<String>);
            let row: Option<Row> = tx
                .query_row(
                    "SELECT record_key, shared_at, fetched_key, name, photo_path, COALESCE(first, last) FROM handle_profiles WHERE handle = ?1",
                    params![handle.uri],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
                )
                .optional()?;
            if let Some((_, shared_at, ..)) = &row
                && *shared_at > at_i
            {
                return Ok(ShareOutcome::Nothing);
            }
            match share {
                Some(s) => {
                    if let Some((Some(key), _, fetched, ..)) = &row
                        && *key == s.record_key
                    {
                        tx.execute(
                            "UPDATE handle_profiles SET shared_at = MAX(shared_at, ?2), last_seen_at = MAX(last_seen_at, ?2) WHERE handle = ?1",
                            params![handle.uri, at_i],
                        )?;
                        return Ok(if fetched.as_deref() == Some(key.as_str()) { ShareOutcome::Nothing } else { ShareOutcome::Unfetched });
                    }
                    let sealed = crate::state::seal_bytes(&self.key, &s.decryption_key);
                    let poster = s.poster.as_ref().map(|p| crate::state::seal_bytes(&self.key, &poster_plist(p)));
                    // The old name / photo stay until the new record is in.
                    tx.execute(
                        "INSERT INTO handle_profiles (handle, record_key, decryption_key, poster, shared_at, last_seen_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                         ON CONFLICT(handle) DO UPDATE SET record_key = excluded.record_key,
                             decryption_key = excluded.decryption_key, poster = excluded.poster,
                             shared_at = excluded.shared_at, last_seen_at = excluded.last_seen_at",
                        params![handle.uri, s.record_key, sealed, poster, at_i],
                    )?;
                    Ok(ShareOutcome::Changed)
                }
                None => {
                    let (files, had_profile) = match &row {
                        Some((key, _, _, name, photo, other)) => {
                            (photo.iter().cloned().collect(), key.is_some() || name.is_some() || photo.is_some() || other.is_some())
                        }
                        None => (vec![], false),
                    };
                    tx.execute(
                        "INSERT INTO handle_profiles (handle, shared_at, last_seen_at) VALUES (?1, ?2, ?2)
                         ON CONFLICT(handle) DO UPDATE SET record_key = NULL, decryption_key = NULL, poster = NULL,
                             name = NULL, first = NULL, last = NULL, photo_path = NULL, fetched_at = NULL,
                             fetched_key = NULL, shared_at = excluded.shared_at, last_seen_at = excluded.last_seen_at",
                        params![handle.uri, at_i],
                    )?;
                    Ok(ShareOutcome::Cleared { files, had_profile })
                }
            }
        })
    }

    /// The current share of `handle` with its key opened; `None` if none (or
    /// its key no longer opens: the state key changed).
    pub fn profile_share(&self, handle: &str) -> StoreResult<Option<ShareRef>> {
        self.tx(|tx, _| {
            type Row = (Option<String>, Option<Vec<u8>>, Option<Vec<u8>>);
            let row: Option<Row> = tx
                .query_row("SELECT record_key, decryption_key, poster FROM handle_profiles WHERE handle = ?1", params![handle], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .optional()?;
            let Some((Some(record_key), Some(sealed), poster)) = row else { return Ok(None) };
            let decryption_key = match crate::state::open_bytes(&self.key, &sealed) {
                Ok(k) => k,
                Err(e) => {
                    log::warn!("{handle}: cannot open the sealed profile key ({e}); was the state key changed?");
                    return Ok(None);
                }
            };
            let poster = poster.and_then(|p| crate::state::open_bytes(&self.key, &p).ok()).and_then(|p| poster_from_plist(&p));
            Ok(Some(ShareRef { record_key, decryption_key, poster }))
        })
    }

    /// Store a successful fetch of `record_key`, unless the share changed
    /// meanwhile (then `None`, and the caller deletes `photo`). Returns the
    /// new profile and the replaced photo file (if it is a different one).
    pub fn set_profile_fetched(
        &self,
        handle: &str,
        record_key: &str,
        name: (&str, &str, &str),
        photo: Option<&str>,
        at: u64,
    ) -> StoreResult<Option<(Option<HandleProfile>, Vec<String>)>> {
        let blank = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
        self.tx(|tx, _| {
            let old: Option<Option<String>> = tx
                .query_row("SELECT photo_path FROM handle_profiles WHERE handle = ?1 AND record_key = ?2", params![handle, record_key], |r| r.get(0))
                .optional()?;
            let Some(old_photo) = old else { return Ok(None) };
            tx.execute(
                "UPDATE handle_profiles SET name = ?3, first = ?4, last = ?5, photo_path = ?6, fetched_at = ?7, fetched_key = ?2
                 WHERE handle = ?1 AND record_key = ?2",
                params![handle, record_key, blank(name.0), blank(name.1), blank(name.2), photo, to_i64(at)],
            )?;
            let replaced = old_photo.into_iter().filter(|p| Some(p.as_str()) != photo).collect();
            Ok(Some((handle_profile_tx(tx, handle)?, replaced)))
        })
    }

    pub fn handle_profile(&self, handle: &str) -> StoreResult<Option<HandleProfile>> {
        self.tx(|tx, _| handle_profile_tx(tx, handle))
    }

    /// Those of `handles` with a current share that was never fetched, or
    /// last fetched before `older_than` (ms).
    pub fn stale_profiles(&self, handles: &[String], older_than: u64) -> StoreResult<Vec<String>> {
        self.tx(|tx, _| {
            let mut out = vec![];
            let mut st = tx.prepare_cached(
                "SELECT 1 FROM handle_profiles WHERE handle = ?1 AND record_key IS NOT NULL
                   AND (fetched_key IS NULL OR fetched_key <> record_key OR fetched_at IS NULL OR fetched_at < ?2)",
            )?;
            for h in handles {
                if st.exists(params![h, to_i64(older_than)])? {
                    out.push(h.clone());
                }
            }
            Ok(out)
        })
    }

    /// Every chat `handle` takes part in (to re-announce after its profile
    /// changed).
    pub fn chats_with_participant(&self, handle: &str) -> StoreResult<Vec<Chat>> {
        self.tx(|tx, _| {
            let ids: Vec<String> = {
                let mut st = tx.prepare_cached("SELECT chat_id FROM chat_participants WHERE handle = ?1")?;
                st.query_map(params![handle], |r| r.get(0))?.collect::<Result<_, _>>()?
            };
            let mut out = vec![];
            for id in ids {
                out.extend(get_chat_tx(tx, &ChatId(id))?);
            }
            Ok(out)
        })
    }

    // ------------------------------------------------------------------
    // group photos
    // ------------------------------------------------------------------

    /// Set (`Some`) or remove (`None`) a group's photo unless a newer change
    /// is stored. A redelivery of the same change is a no-op; one announcing
    /// more than [`MAX_ICON_BYTES`] is ignored.
    pub fn set_chat_icon(&self, id: &ChatId, icon: Option<&IconRef>, at: u64) -> StoreResult<Vec<StoreUpdate>> {
        if let Some(i) = icon.filter(|i| i.size > MAX_ICON_BYTES) {
            log::warn!("chat {id}: ignoring group photo {} of {} bytes (limit {MAX_ICON_BYTES})", i.icon_id, i.size);
            return Ok(vec![]);
        }
        let at_i = to_i64(at);
        self.tx(|tx, _| {
            ensure_chat(tx, id)?;
            let (cur_id, cur_path, cur_at): (Option<String>, Option<String>, Option<i64>) =
                tx.query_row("SELECT icon_id, icon_path, icon_at FROM chats WHERE id = ?1", params![id.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            if cur_at.is_some_and(|c| c > at_i) {
                return Ok(vec![]);
            }
            match icon {
                Some(i) if cur_id.as_deref() == Some(i.icon_id.as_str()) => return Ok(vec![]),
                Some(i) => {
                    let sealed = crate::state::seal_bytes(&self.key, i.locator.as_bytes());
                    tx.execute(
                        "UPDATE chats SET icon_id = ?2, icon_locator = ?3, icon_size = ?4, icon_path = NULL, icon_at = ?5 WHERE id = ?1",
                        params![id.0, i.icon_id, sealed, to_i64(i.size), at_i],
                    )?;
                }
                None => {
                    tx.execute(
                        "UPDATE chats SET icon_id = NULL, icon_locator = NULL, icon_size = NULL, icon_path = NULL, icon_at = ?2 WHERE id = ?1",
                        params![id.0, at_i],
                    )?;
                    if cur_id.is_none() && cur_path.is_none() {
                        return Ok(vec![]);
                    }
                }
            }
            let mut out: Vec<StoreUpdate> = get_chat_tx(tx, id)?.map(StoreUpdate::ChatUpdated).into_iter().collect();
            if let Some(p) = cur_path {
                out.push(StoreUpdate::FilesRemoved(vec![p]));
            }
            Ok(out)
        })
    }

    /// The current group photo's id, opened locator, size and file.
    pub fn chat_icon_locator(&self, id: &ChatId) -> StoreResult<Option<IconDownload>> {
        self.tx(|tx, _| {
            type Row = (Option<String>, Option<Vec<u8>>, Option<i64>, Option<String>);
            let row: Option<Row> = tx
                .query_row("SELECT icon_id, icon_locator, icon_size, icon_path FROM chats WHERE id = ?1", params![id.0], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })
                .optional()?;
            let Some((Some(icon_id), Some(sealed), size, local_path)) = row else { return Ok(None) };
            match crate::state::open_bytes(&self.key, &sealed) {
                Ok(plain) => Ok(Some(IconDownload {
                    icon_id,
                    locator: String::from_utf8_lossy(&plain).into_owned(),
                    size: size.unwrap_or(0).max(0) as u64,
                    local_path,
                })),
                Err(e) => {
                    log::warn!("chat {id}: cannot open its sealed group photo locator ({e}); was the state key changed?");
                    Ok(None)
                }
            }
        })
    }

    /// Record the downloaded file of group photo `icon_id`; `None` if the
    /// photo changed meanwhile.
    pub fn set_icon_file(&self, id: &ChatId, icon_id: &str, path: &str) -> StoreResult<Option<Chat>> {
        self.tx(|tx, _| {
            let n = tx.execute("UPDATE chats SET icon_path = ?3 WHERE id = ?1 AND icon_id = ?2", params![id.0, icon_id, path])?;
            if n == 0 { Ok(None) } else { get_chat_tx(tx, id) }
        })
    }
}
