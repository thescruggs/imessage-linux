//! Merging the one-to-one chats of one person (contact links, see
//! `crate::contacts`): their messages, background and pin move to the
//! chat with the person's canonical address, and the other chat goes.

use imessage_common::model::{ChatId, Handle};
use rusqlite::{OptionalExtension, Transaction, params};

use super::{Store, StoreResult, StoreUpdate, attachment_paths, ensure_chat, get_chat_tx, pinned_ids, refresh_unread, renumber_pins, replace_participants};

impl Store {
    /// Move every `(from, into)` one-to-one chat into `into` (created with
    /// `into`'s address as the participant if it does not exist yet):
    /// messages, unread state, the newer of the two backgrounds, the pin
    /// (`into`'s if both are pinned) and the newer activity time. Group
    /// ids, `from == into` and unknown `from` chats are skipped. Returns
    /// the `ChatUpdated`s of the chats merged into, a `FilesRemoved` for a
    /// background that lost, and the chats removed.
    pub fn merge_chats(&self, pairs: &[(ChatId, ChatId)]) -> StoreResult<(Vec<StoreUpdate>, Vec<ChatId>)> {
        self.tx(|tx, _| {
            let mut into_ids: Vec<ChatId> = vec![];
            let mut removed = vec![];
            let mut files = vec![];
            for (from, into) in pairs {
                if from == into || from.is_group() || into.is_group() || !chat_exists(tx, from)? {
                    continue;
                }
                files.extend(merge_one(tx, from, into)?);
                removed.push(from.clone());
                if !into_ids.contains(into) {
                    into_ids.push(into.clone());
                }
            }
            let mut out = vec![];
            for id in &into_ids {
                out.extend(get_chat_tx(tx, id)?.map(StoreUpdate::ChatUpdated));
            }
            if !files.is_empty() {
                out.push(StoreUpdate::FilesRemoved(files));
            }
            Ok((out, removed))
        })
    }

    /// The address the newest incoming message of `chat` (sent at or
    /// before `before`, if given) came from.
    pub fn latest_incoming_sender(&self, chat: &ChatId, before: Option<u64>) -> StoreResult<Option<String>> {
        self.tx(|tx, _| {
            Ok(tx
                .prepare_cached(
                    "SELECT sender FROM messages WHERE chat_id = ?1 AND is_from_me = 0 AND (?2 IS NULL OR sent_at <= ?2)
                     ORDER BY sent_at DESC, seq DESC LIMIT 1",
                )?
                .query_row(params![chat.0, before.map(super::to_i64)], |r| r.get(0))
                .optional()?)
        })
    }
}

fn chat_exists(tx: &Transaction<'_>, id: &ChatId) -> StoreResult<bool> {
    Ok(tx.query_row("SELECT EXISTS(SELECT 1 FROM chats WHERE id = ?1)", params![id.0], |r| r.get(0))?)
}

/// Returns the files of a background that was dropped.
fn merge_one(tx: &Transaction<'_>, from: &ChatId, into: &ChatId) -> StoreResult<Vec<String>> {
    if !chat_exists(tx, into)? {
        ensure_chat(tx, into)?;
        replace_participants(tx, into, &[Handle::new(&into.0)])?;
    }
    tx.execute(
        "UPDATE chats SET last_message_at = MAX(COALESCE(last_message_at, 0), COALESCE((SELECT last_message_at FROM chats WHERE id = ?2), 0))
         WHERE id = ?1",
        params![into.0, from.0],
    )?;
    // The pin: `into`'s own if it has one, else `from`'s place.
    let pins = pinned_ids(tx)?;
    if pins.contains(from) && !pins.contains(into) {
        tx.execute(
            "UPDATE chats SET (pinned_at, pin_order) = (SELECT pinned_at, pin_order FROM chats WHERE id = ?2) WHERE id = ?1",
            params![into.0, from.0],
        )?;
    }
    tx.execute("UPDATE messages SET chat_id = ?1 WHERE chat_id = ?2", params![into.0, from.0])?;
    // Backgrounds: the newer one stays.
    let set_at = |id: &ChatId| -> StoreResult<Option<i64>> {
        Ok(tx.query_row("SELECT set_at FROM chat_backgrounds WHERE chat_id = ?1", params![id.0], |r| r.get(0)).optional()?)
    };
    let mut files = vec![];
    match (set_at(from)?, set_at(into)?) {
        (Some(f), Some(i)) if f > i => {
            files.extend(background_files(tx, into)?);
            tx.execute("DELETE FROM chat_backgrounds WHERE chat_id = ?1", params![into.0])?;
            tx.execute("UPDATE chat_backgrounds SET chat_id = ?1 WHERE chat_id = ?2", params![into.0, from.0])?;
        }
        (Some(_), Some(_)) => files.extend(background_files(tx, from)?),
        (Some(_), None) => {
            tx.execute("UPDATE chat_backgrounds SET chat_id = ?1 WHERE chat_id = ?2", params![into.0, from.0])?;
        }
        (None, _) => {}
    }
    tx.execute("DELETE FROM chats WHERE id = ?1", params![from.0])?;
    renumber_pins(tx, &pinned_ids(tx)?)?;
    refresh_unread(tx, into)?;
    Ok(files)
}

fn background_files(tx: &Transaction<'_>, id: &ChatId) -> StoreResult<Vec<String>> {
    let mut v = attachment_paths(tx, "SELECT local_path FROM chat_backgrounds WHERE chat_id = ?1", id.0.clone())?;
    v.extend(attachment_paths(tx, "SELECT image_path FROM chat_backgrounds WHERE chat_id = ?1", id.0.clone())?);
    Ok(v)
}
