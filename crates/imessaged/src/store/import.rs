//! One-time history import (`ImportMessages`): insert-only, idempotent by
//! message id, imported rows count as read.

use std::collections::{BTreeSet, HashSet};

use imessage_common::import::ImportResult;
use imessage_common::model::{Attachment, Chat, ChatId, Message, MessageId, Reaction};
use rusqlite::{OptionalExtension, params};

use super::housekeeping::delete_empty_groups_tx;
use super::{Store, StoreResult, apply_reaction_tx, get_chat_tx, insert_message_tx, load_message, refresh_unread, to_i64, upsert_chat_tx};

/// What [`Store::import`] did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportOutcome {
    pub result: ImportResult,
    /// Messages skipped because the store already had them.
    pub skipped: Vec<MessageId>,
    /// Every chat the import touched, as stored afterwards.
    pub chats: Vec<Chat>,
    /// Messages that existed before this call (live ones, or imported by
    /// an earlier batch) whose reactions the import changed, as stored
    /// afterwards.
    pub updated: Vec<Message>,
    /// Empty group rows removed in the same transaction (see
    /// [`Store::delete_empty_groups`]); never listed in `chats`.
    pub removed_empty_groups: usize,
}

impl Store {
    /// Which of `ids` the store already has.
    pub fn existing_messages(&self, ids: &[MessageId]) -> StoreResult<HashSet<MessageId>> {
        self.tx(|tx, _| {
            let mut st = tx.prepare_cached("SELECT 1 FROM messages WHERE id = ?1")?;
            let mut out = HashSet::new();
            for id in ids {
                if st.query_row(params![id.0], |_| Ok(())).optional()?.is_some() {
                    out.insert(id.clone());
                }
            }
            Ok(out)
        })
    }

    /// Import in one transaction:
    /// * `chats` are created or updated like a message's chat snapshot
    ///   dated `last_message_at` (a newer live rename or membership change
    ///   wins; participants are left alone when the stored chat has newer
    ///   messages, or the snapshot is undated and the chat has members);
    /// * a message whose id exists is skipped and left untouched; new ones
    ///   are inserted with their attachments and marked read;
    /// * `reactions` are applied in order when their target exists, else
    ///   counted as orphaned;
    /// * empty group rows (no members, messages, name or background, e.g.
    ///   a Mac group of only our own handles) are deleted before the
    ///   outcome is read, so `chats` never lists a row that is gone.
    ///
    /// `result.attachments` counts every attachment row inserted (the
    /// caller splits it by whether a file was copied).
    pub fn import(&self, chats: &[Chat], messages: &[(Message, Vec<Attachment>)], reactions: &[(MessageId, Reaction, u64)]) -> StoreResult<ImportOutcome> {
        self.tx(|tx, early| {
            let mut out = ImportOutcome::default();
            let mut touched: BTreeSet<ChatId> = BTreeSet::new();
            let mut inserted: HashSet<MessageId> = HashSet::new();
            let mut updated: Vec<MessageId> = vec![];
            let mut created: BTreeSet<ChatId> = BTreeSet::new();
            for c in chats {
                let last: Option<Option<i64>> =
                    tx.query_row("SELECT last_message_at FROM chats WHERE id = ?1", params![c.id.0], |r| r.get(0)).optional()?;
                let exists = last.is_some();
                let at = c.last_message_at.unwrap_or(0);
                // A store that has seen newer messages in this chat (live)
                // knows its members better than the import's snapshot.
                let stale = last.flatten().is_some_and(|l| l > to_i64(at));
                let snapshot;
                let c = if stale {
                    snapshot = Chat { participants: vec![], display_name: None, ..c.clone() };
                    &snapshot
                } else {
                    c
                };
                upsert_chat_tx(tx, c, Some(at))?;
                if !exists && created.insert(c.id.clone()) {
                    out.result.chats_created += 1;
                }
                touched.insert(c.id.clone());
            }
            for (m, atts) in messages {
                let exists = tx.query_row("SELECT 1 FROM messages WHERE id = ?1", params![m.id.0], |_| Ok(())).optional()?.is_some();
                if exists {
                    out.result.skipped_existing += 1;
                    out.skipped.push(m.id.clone());
                    continue;
                }
                let chat_existed = tx.query_row("SELECT 1 FROM chats WHERE id = ?1", params![m.chat_id.0], |_| Ok(())).optional()?.is_some();
                insert_message_tx(tx, early, &self.key, m, atts)?;
                tx.execute("UPDATE messages SET is_read = 1 WHERE id = ?1", params![m.id.0])?;
                if !chat_existed {
                    out.result.chats_created += 1;
                }
                out.result.inserted += 1;
                inserted.insert(m.id.clone());
                out.result.attachments += atts.len() as u32;
                touched.insert(m.chat_id.clone());
            }
            for (target, reaction, at) in reactions {
                let chat: Option<String> = tx.query_row("SELECT chat_id FROM messages WHERE id = ?1", params![target.0], |r| r.get(0)).optional()?;
                let Some(chat) = chat else {
                    out.result.reactions_orphaned += 1;
                    continue;
                };
                if apply_reaction_tx(tx, target, reaction, *at)?.is_some() && !inserted.contains(target) && !updated.contains(target) {
                    updated.push(target.clone());
                }
                out.result.reactions += 1;
                touched.insert(ChatId(chat));
            }
            out.removed_empty_groups = delete_empty_groups_tx(tx)?;
            for id in &touched {
                refresh_unread(tx, id)?;
                match get_chat_tx(tx, id)? {
                    Some(c) => out.chats.push(c),
                    // an empty group this import created and just removed
                    None if created.contains(id) => out.result.chats_created -= 1,
                    None => {}
                }
            }
            for id in &updated {
                out.updated.extend(load_message(tx, id)?);
            }
            Ok(out)
        })
    }
}
