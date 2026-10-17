//! Store housekeeping: empty "ghost" group rows and chats whose id is not a
//! valid chat id (see docs/DAEMON.md, "recovering from a bad import").

use imessage_common::model::{ChatId, Handle};
use rusqlite::{Transaction, params};
use serde::{Deserialize, Serialize};

use super::{Store, StoreResult, attachment_paths};

/// One chat [`Store::purge_invalid_chats`] removed (or would remove).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgedChat {
    pub id: ChatId,
    pub messages: u64,
    pub reactions: u64,
    pub attachments: u64,
}

/// `PurgeInvalidChats` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeSummary {
    /// Nothing was deleted; the counts say what would be.
    pub dry_run: bool,
    pub chats: Vec<PurgedChat>,
    pub messages: u64,
    pub reactions: u64,
    pub attachments: u64,
    /// Local attachment / background files belonging to those chats
    /// (deleted unless `dry_run`).
    pub files: u64,
    /// Handle rows with an invalid URI no chat refers to any more.
    pub handles: u64,
    /// The files to delete (not serialized; the caller removes them).
    #[serde(skip)]
    pub paths: Vec<String>,
}

impl Store {
    /// Delete group chats that hold nothing: no participants, no messages,
    /// no background and no name. Live traffic used to create these for
    /// 1:1 conversations carrying a group id. Returns how many went.
    pub fn delete_empty_groups(&self) -> StoreResult<usize> {
        self.tx(|tx, _| Ok(delete_empty_groups_tx(tx)?))
    }

    /// Chats whose id is not a valid `mailto:` / `tel:` / `group:` /
    /// `group-p:` id ([`ChatId::is_valid`]), with their messages, reactions
    /// and attachment rows; plus handle rows with an invalid URI that are
    /// no longer referenced. `dry_run`: only count. The summary lists the
    /// local files to remove in `paths`.
    pub fn purge_invalid_chats(&self, dry_run: bool) -> StoreResult<PurgeSummary> {
        self.tx(|tx, _| {
            let ids: Vec<String> = {
                let mut st = tx.prepare("SELECT id FROM chats ORDER BY id")?;
                st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
            };
            let mut out = PurgeSummary { dry_run, ..Default::default() };
            for id in ids.into_iter().map(ChatId).filter(|c| !c.is_valid()) {
                let count = |sql: &str| -> rusqlite::Result<u64> { tx.query_row(sql, params![id.0], |r| r.get::<_, i64>(0)).map(|n| n.max(0) as u64) };
                let c = PurgedChat {
                    messages: count("SELECT COUNT(*) FROM messages WHERE chat_id = ?1")?,
                    reactions: count("SELECT COUNT(*) FROM reactions WHERE message_id IN (SELECT id FROM messages WHERE chat_id = ?1)")?,
                    attachments: count("SELECT COUNT(*) FROM attachments a JOIN messages m ON m.id = a.message_id WHERE m.chat_id = ?1")?,
                    id: id.clone(),
                };
                let mut paths = attachment_paths(tx, "SELECT a.local_path FROM attachments a JOIN messages m ON m.id = a.message_id WHERE m.chat_id = ?1", id.0.clone())?;
                paths.extend(attachment_paths(tx, "SELECT local_path FROM chat_backgrounds WHERE chat_id = ?1", id.0.clone())?);
                paths.extend(attachment_paths(tx, "SELECT image_path FROM chat_backgrounds WHERE chat_id = ?1", id.0.clone())?);
                if !dry_run {
                    tx.execute("DELETE FROM reactions WHERE message_id IN (SELECT id FROM messages WHERE chat_id = ?1)", params![id.0])?;
                    // messages, attachments, participants, background: ON DELETE CASCADE
                    tx.execute("DELETE FROM messages WHERE chat_id = ?1", params![id.0])?;
                    tx.execute("DELETE FROM chats WHERE id = ?1", params![id.0])?;
                }
                out.messages += c.messages;
                out.reactions += c.reactions;
                out.attachments += c.attachments;
                out.files += paths.len() as u64;
                out.paths.extend(paths);
                out.chats.push(c);
            }
            // Handles: unreferenced after the deletions above (in a dry
            // run, unreferenced by any chat that stays).
            let handles: Vec<String> = {
                let mut st = tx.prepare("SELECT uri FROM handles")?;
                st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
            };
            let doomed: std::collections::HashSet<&str> = out.chats.iter().map(|c| c.id.0.as_str()).collect();
            for uri in handles.iter().filter(|u| !Handle::is_valid_uri(u)) {
                let users: Vec<String> = {
                    let mut st = tx.prepare_cached("SELECT chat_id FROM chat_participants WHERE handle = ?1")?;
                    st.query_map(params![uri], |r| r.get(0))?.collect::<Result<_, _>>()?
                };
                if users.iter().all(|c| doomed.contains(c.as_str())) {
                    if !dry_run {
                        tx.execute("DELETE FROM handles WHERE uri = ?1", params![uri])?;
                    }
                    out.handles += 1;
                }
            }
            Ok(out)
        })
    }
}

/// [`Store::delete_empty_groups`] inside a caller's transaction.
pub(super) fn delete_empty_groups_tx(tx: &Transaction<'_>) -> rusqlite::Result<usize> {
    tx.execute(
        "DELETE FROM chats WHERE is_group = 1
           AND (display_name IS NULL OR TRIM(display_name) = '')
           AND NOT EXISTS (SELECT 1 FROM chat_participants p WHERE p.chat_id = chats.id)
           AND NOT EXISTS (SELECT 1 FROM messages m WHERE m.chat_id = chats.id)
           AND NOT EXISTS (SELECT 1 FROM chat_backgrounds b WHERE b.chat_id = chats.id)",
        [],
    )
}
