//! Payloads of the daemon's one-time history import (`ImportMessages`, see
//! docs/DBUS.md), used by `imsg import-mac`.
//!
//! A batch carries ready-made model rows: [`Chat`]s, [`Message`]s with the
//! attachment files to copy, and reactions to apply. The daemon inserts
//! messages that it does not have yet (by message id) and leaves existing
//! ones alone, so importing the same batch twice is harmless.

use serde::{Deserialize, Serialize};

use crate::model::{AttachmentId, Chat, Message, MessageId, Reaction};

/// `ImportMessages` argument.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ImportBatch {
    /// Chats the messages belong to (created, or participants / name
    /// updated unless the daemon saw a newer change).
    #[serde(default)]
    pub chats: Vec<Chat>,
    #[serde(default)]
    pub messages: Vec<ImportMessage>,
    /// Tapbacks, applied in order after the messages; one whose target
    /// message the store does not have is skipped (counted as orphaned).
    #[serde(default)]
    pub reactions: Vec<ImportReaction>,
}

/// One message plus the attachments its `Attachment` parts refer to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportMessage {
    pub message: Message,
    #[serde(default)]
    pub attachments: Vec<ImportAttachment>,
    /// A link preview as the Mac stores it; the daemon turns it into a
    /// `link` part (and its image / icon attachments).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<ImportLink>,
}

/// A Mac link preview (`chat.db` `message.payload_data` of a
/// `com.apple.messages.URLBalloonProvider` balloon).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportLink {
    /// The `RichLink` keyed archive, base64 (standard alphabet).
    pub payload: String,
    /// The message's hidden link-preview files in `chat.db` order (the
    /// archive's image / icon indexes point into this list): absolute
    /// paths the daemon can read, `None` where the file was not available.
    #[serde(default)]
    pub images: Vec<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportAttachment {
    /// `<message id>:<part>`, as in the message's `Attachment` part.
    pub id: AttachmentId,
    pub mime: String,
    pub filename: String,
    pub size: u64,
    /// Absolute path of a file the daemon can read; it is copied into the
    /// daemon's attachments directory. `None`: metadata only (the file was
    /// not available), the attachment cannot be opened.
    #[serde(default)]
    pub source_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReaction {
    /// The message reacted to.
    pub message_id: MessageId,
    pub reaction: Reaction,
    /// ms since epoch; a newer stored reaction by the same sender wins.
    pub at: u64,
}

/// `ImportMessages` result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportResult {
    /// New message rows.
    pub inserted: u32,
    /// Messages the store already had (left unchanged).
    pub skipped_existing: u32,
    pub chats_created: u32,
    /// Attachment rows stored with a local copy of the file.
    pub attachments: u32,
    /// Attachment rows stored without a file (none given, or copying failed).
    #[serde(default)]
    pub attachments_without_file: u32,
    /// Reactions whose target message exists (applied unless a newer one
    /// by the same sender was already stored).
    #[serde(default)]
    pub reactions: u32,
    /// Reactions skipped because their target message is not in the store.
    #[serde(default)]
    pub reactions_orphaned: u32,
}

impl ImportResult {
    pub fn add(&mut self, o: &ImportResult) {
        self.inserted += o.inserted;
        self.skipped_existing += o.skipped_existing;
        self.chats_created += o.chats_created;
        self.attachments += o.attachments;
        self.attachments_without_file += o.attachments_without_file;
        self.reactions += o.reactions;
        self.reactions_orphaned += o.reactions_orphaned;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_batch_and_result_shapes() {
        let b: ImportBatch = serde_json::from_str("{}").unwrap();
        assert_eq!(b, ImportBatch::default());
        let r: ImportResult = serde_json::from_str(r#"{"inserted":1,"skipped_existing":2,"chats_created":3,"attachments":4}"#).unwrap();
        assert_eq!((r.inserted, r.skipped_existing, r.chats_created, r.attachments, r.reactions), (1, 2, 3, 4, 0));
        let mut t = ImportResult::default();
        t.add(&r);
        t.add(&r);
        assert_eq!(t.inserted, 2);
    }
}
