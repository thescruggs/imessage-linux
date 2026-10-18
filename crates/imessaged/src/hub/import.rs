//! `ImportMessages`: one-time history import (see docs/DAEMON.md).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use imessage_common::import::{ImportBatch, ImportLink, ImportResult};
use imessage_common::model::{Attachment, AttachmentId, ChatId, Handle, Message, MessageId, MessagePart, Reaction};

use super::{DaemonSignal, Hub, OpError, OpResult, safe_component};
use crate::store::PurgeSummary;

/// Largest link-preview archive (base64) we accept per message.
pub const MAX_IMPORT_LINK_PAYLOAD: usize = 1 << 20;

/// Upper bound per call; callers split larger imports into batches.
pub const MAX_IMPORT_MESSAGES: usize = 2000;
pub const MAX_IMPORT_REACTIONS: usize = 10_000;

fn invalid(s: String) -> OpError {
    OpError::InvalidInput(s)
}

/// Structural checks; nothing is stored if any fails.
fn validate(batch: &ImportBatch) -> OpResult<()> {
    if batch.messages.len() > MAX_IMPORT_MESSAGES || batch.reactions.len() > MAX_IMPORT_REACTIONS {
        return Err(invalid(format!("batch too large (at most {MAX_IMPORT_MESSAGES} messages and {MAX_IMPORT_REACTIONS} reactions per call)")));
    }
    // Every chat id and handle must be what live iMessage would store
    // (no raw Mac handle ids like `46339(smsft)`).
    let bad_chat = |id: &ChatId| (!id.is_valid()).then(|| invalid(format!("invalid chat id {:?}", id.0)));
    let bad_handle = |h: &Handle| (!Handle::is_valid_uri(&h.uri)).then(|| invalid(format!("invalid handle {:?}", h.uri)));
    for c in &batch.chats {
        if let Some(e) = bad_chat(&c.id).or_else(|| c.participants.iter().find_map(bad_handle)) {
            return Err(e);
        }
    }
    for im in &batch.messages {
        if let Some(e) = bad_chat(&im.message.chat_id).or_else(|| bad_handle(&im.message.sender)) {
            return Err(e);
        }
    }
    if let Some(e) = batch.reactions.iter().find_map(|r| bad_handle(&r.reaction.sender)) {
        return Err(e);
    }
    let mut seen = HashSet::new();
    for im in &batch.messages {
        let m = &im.message;
        if m.id.0.trim().is_empty() || m.chat_id.0.trim().is_empty() {
            return Err(invalid("message with an empty id or chat id".into()));
        }
        if !seen.insert(m.id.clone()) {
            return Err(invalid(format!("message {} appears twice", m.id)));
        }
        let part_ids: HashSet<_> =
            m.parts.iter().filter_map(|p| if let MessagePart::Attachment { attachment_id, .. } = p { Some(attachment_id) } else { None }).collect();
        for a in &im.attachments {
            if !part_ids.contains(&a.id) {
                return Err(invalid(format!("attachment {} is not a part of message {}", a.id, m.id)));
            }
            if let Some(p) = &a.source_path
                && !Path::new(p).is_absolute()
            {
                return Err(invalid(format!("attachment {}: source_path {p:?} is not absolute", a.id)));
            }
        }
        if let Some(l) = &im.link {
            if l.payload.len() > MAX_IMPORT_LINK_PAYLOAD {
                return Err(invalid(format!("message {}: link preview payload too large", m.id)));
            }
            if let Some(p) = l.images.iter().flatten().find(|p| !Path::new(p).is_absolute()) {
                return Err(invalid(format!("message {}: link image {p:?} is not absolute", m.id)));
            }
        }
    }
    Ok(())
}

impl Hub {
    /// Insert history from another device (see [`crate::store::Store::import`]).
    /// New messages' attachment files are copied to
    /// `state_dir/attachments/<id>/<filename>` first (a file that cannot be
    /// copied is stored without one and logged). No `MessageReceived`:
    /// `MessageUpdated` for messages that existed before the call and got
    /// a reaction, then one `ChatUpdated` per touched chat. Group chats are taught to
    /// the engine so sending to them works.
    pub async fn import_messages(self: &Arc<Self>, batch: ImportBatch) -> OpResult<ImportResult> {
        validate(&batch)?;
        let store = self.store()?.clone();
        let ids: Vec<MessageId> = batch.messages.iter().map(|m| m.message.id.clone()).collect();
        let existing = store.call(move |s| s.existing_messages(&ids)).await?;

        let mut rows: Vec<(Message, Vec<Attachment>)> = Vec::with_capacity(batch.messages.len());
        let mut copied: Vec<(MessageId, PathBuf)> = vec![];
        let mut without_file = 0u32;
        for im in batch.messages {
            let mut m = im.message;
            if existing.contains(&m.id) {
                rows.push((m, vec![]));
                continue;
            }
            let mut atts = Vec::with_capacity(im.attachments.len());
            if let Some(link) = &im.link {
                let link_atts = self.import_link(&mut m, link).await;
                copied.extend(link_atts.iter().filter_map(|a| a.local_path.as_ref()).map(|p| (m.id.clone(), PathBuf::from(p))));
                atts.extend(link_atts);
            }
            for a in im.attachments {
                let filename = safe_component(&a.filename, "attachment");
                let local_path = match &a.source_path {
                    // Only a regular file, not a symlink (to anything the
                    // daemon can read) or a FIFO.
                    Some(src) if !tokio::fs::symlink_metadata(src).await.is_ok_and(|m| m.is_file()) => {
                        log::warn!("import: attachment {}: source is not a regular file; storing it without one", a.id);
                        None
                    }
                    Some(src) => match self.keep_copy(&a.id, &filename, Path::new(src)).await {
                        Ok(p) => {
                            copied.push((m.id.clone(), p.clone()));
                            Some(p.to_string_lossy().into_owned())
                        }
                        Err(e) => {
                            log::warn!("import: attachment {}: cannot copy the file ({e}); storing it without one", a.id);
                            None
                        }
                    },
                    None => None,
                };
                if local_path.is_none() {
                    without_file += 1;
                }
                atts.push(Attachment { id: a.id, message_id: m.id.clone(), mime: a.mime, filename, size: a.size, local_path, mmcs: String::new() });
            }
            rows.push((m, atts));
        }
        let reactions: Vec<(MessageId, Reaction, u64)> = batch.reactions.into_iter().map(|r| (r.message_id, r.reaction, r.at)).collect();
        let chats = batch.chats;
        let outcome = match store.call(move |s| s.import(&chats, &rows, &reactions)).await {
            Ok(o) => o,
            Err(e) => {
                self.remove_files(copied.into_iter().map(|(_, p)| p.to_string_lossy().into_owned()).collect()).await;
                return Err(e.into());
            }
        };
        // A live message that landed between the existence check and the
        // transaction was skipped: its copies are nobody's.
        let skipped: HashSet<&MessageId> = outcome.skipped.iter().collect();
        let orphans: Vec<String> = copied.iter().filter(|(m, _)| skipped.contains(m)).map(|(_, p)| p.to_string_lossy().into_owned()).collect();
        if !orphans.is_empty() {
            self.remove_files(orphans).await;
        }
        let mut result = outcome.result;
        result.attachments_without_file = without_file.min(result.attachments);
        result.attachments -= result.attachments_without_file;
        for c in &outcome.chats {
            if c.is_group {
                self.engine.upsert_chat(c).await;
            }
        }
        for m in outcome.updated {
            self.emit(DaemonSignal::MessageUpdated { chat_id: m.chat_id.clone(), message: m }).await;
        }
        for c in outcome.chats {
            self.emit(DaemonSignal::ChatUpdated(c)).await;
        }
        if outcome.removed_empty_groups > 0 {
            log::info!("import: removed {} empty group chat(s) (no participants, messages, name or background)", outcome.removed_empty_groups);
        }
        log::info!(
            "import: {} message(s) inserted, {} already present, {} chat(s) created, {} attachment file(s), {} reaction(s) ({} orphaned)",
            result.inserted,
            result.skipped_existing,
            result.chats_created,
            result.attachments,
            result.reactions,
            result.reactions_orphaned
        );
        Ok(result)
    }
}

/// The bytes of an imported link-preview file: a regular file (not a
/// symlink) of at most [`convert::MAX_LINK_IMAGE_BYTES`].
async fn read_link_image(path: &str) -> Option<Vec<u8>> {
    let meta = tokio::fs::symlink_metadata(path).await.ok()?;
    if !meta.is_file() || meta.len() > crate::engine::convert::MAX_LINK_IMAGE_BYTES as u64 {
        return None;
    }
    tokio::fs::read(path).await.ok()
}

impl Hub {
    /// Turn a Mac link preview into a `link` part on `m` (after its text;
    /// the URL becomes the text of a message without one) and return its
    /// image / icon rows, files written. An archive that cannot be read
    /// leaves the message as it is (an empty one becomes "app message").
    async fn import_link(&self, m: &mut Message, link: &ImportLink) -> Vec<Attachment> {
        use base64::Engine as _;
        use crate::engine::convert;
        let parsed = base64::engine::general_purpose::STANDARD.decode(link.payload.trim()).ok().and_then(|b| convert::rich_link_from_archive(&b));
        let Some(parsed) = parsed.filter(|_| !m.unsent) else {
            if !m.unsent {
                log::warn!("import: link preview of {} could not be read", m.id);
            }
            if m.parts.is_empty() && !m.unsent {
                m.parts.push(MessagePart::Unsupported { description: "app message".into() });
            }
            return vec![];
        };
        let url = parsed.link_url().unwrap_or_default();
        let mut atts = vec![];
        let mut take = async |sub: &Option<(u64, String)>, id: AttachmentId, kind: &str| -> Option<AttachmentId> {
            let (idx, mime) = sub.as_ref()?;
            let path = link.images.get(usize::try_from(*idx).ok()?)?.as_deref()?;
            let data = read_link_image(path).await?;
            let mime = if mime.is_empty() { crate::linkpreview::image_info(&data).map(|(m, _, _)| m.to_string()).unwrap_or_default() } else { mime.clone() };
            let a = convert::link_attachment(id, &m.id, kind, &mime, &data)?;
            let id = a.id.clone();
            atts.push(a);
            Some(id)
        };
        let image_attachment_id = take(&parsed.image, AttachmentId::link_image(&m.id), "link-image").await;
        let icon_attachment_id = take(&parsed.icon, AttachmentId::link_icon(&m.id), "link-icon").await;
        m.parts.retain(|p| !matches!(p, MessagePart::Unsupported { .. } | MessagePart::Link { .. }));
        if m.text.as_deref().is_none_or(|t| t.trim().is_empty()) {
            m.parts.retain(|p| !matches!(p, MessagePart::Text { .. }));
            m.parts.insert(0, MessagePart::Text { text: url.clone() });
            m.text = Some(url.clone());
        }
        m.parts.push(MessagePart::Link { url, title: parsed.title, summary: parsed.summary, site_name: None, image_attachment_id, icon_attachment_id });
        self.keep_link_files(atts).await
    }
}

impl Hub {
    /// Housekeeping: drop group chats with no members, messages, name or
    /// background (see [`crate::store::Store::delete_empty_groups`]).
    /// Logged; no signals (nobody can have them open with content).
    pub async fn delete_empty_groups(&self) {
        let Ok(store) = self.store() else { return };
        match store.call(|s| s.delete_empty_groups()).await {
            Ok(0) => {}
            Ok(n) => log::info!("removed {n} empty group chat(s) (no participants, messages, name or background)"),
            Err(e) => log::warn!("cannot remove empty group chats: {e}"),
        }
    }

    /// `PurgeInvalidChats`: remove chats whose id is not a valid chat id
    /// with everything in them, and their local files (see
    /// [`crate::store::Store::purge_invalid_chats`]). `dry_run` only counts.
    /// No signals; logged.
    pub async fn purge_invalid_chats(&self, dry_run: bool) -> OpResult<PurgeSummary> {
        let store = self.store()?.clone();
        let mut summary = store.call(move |s| s.purge_invalid_chats(dry_run)).await?;
        let paths = std::mem::take(&mut summary.paths);
        if !dry_run && !summary.chats.is_empty() {
            self.remove_files(paths).await;
        }
        log::info!(
            "purge invalid chats{}: {} chat(s), {} message(s), {} reaction(s), {} attachment(s), {} file(s), {} handle(s)",
            if dry_run { " (dry run)" } else { "" },
            summary.chats.len(),
            summary.messages,
            summary.reactions,
            summary.attachments,
            summary.files,
            summary.handles
        );
        Ok(summary)
    }
}
