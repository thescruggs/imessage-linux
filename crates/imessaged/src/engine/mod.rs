//! The protocol-engine boundary.
//!
//! Everything above this line (D-Bus server, SQLite store, CLI, KDE app) talks
//! to an [`Engine`] and consumes [`EngineEvent`]s; nothing above it touches
//! rustpush types. [`rustpush_engine::RustpushEngine`] is the real
//! implementation, [`mock::MockEngine`] the test double (always compiled, so
//! integration tests in `crates/imessaged/tests/` can use it).

use std::path::Path;

use async_trait::async_trait;
use imessage_common::model::{
    Chat, ChatId, ConnectionState, DeliveryReceipt, DeliveryState, Handle, Message, MessageId, MessagePart,
    ReactionKind, ReadReceipt, RegistrationState, TypingEvent,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

pub mod anisette;
pub mod convert;
pub mod mock;
pub mod rustpush_engine;
pub mod supervise;

pub use mock::MockEngine;
pub use rustpush_engine::RustpushEngine;

/// Capacity of the event broadcast channel. Slow consumers see
/// `RecvError::Lagged` and should resync from the store.
pub const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Largest attachment [`Engine::download_attachment`] fetches for a message
/// part. The size in a message is the sender's claim; the cap applies to
/// the bytes actually received.
pub const MAX_ATTACHMENT_BYTES: u64 = 1 << 30;
/// Largest transcript background we store or download (Apple's poster
/// payloads are a few MB).
pub const MAX_BACKGROUND_BYTES: u64 = 32 << 20;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("not logged in")]
    NotLoggedIn,
    #[error("no Mac hardware identity imported (run `imsg hwconfig import <file>`)")]
    NoHardwareConfig,
    #[error("no two-factor login is pending")]
    NoPendingTwoFactor,
    #[error("Apple rejected the two-factor code")]
    BadTwoFactorCode,
    /// Apple rejected the credentials or demands something we cannot do
    /// (extra step, hardware key, ...). Not worth retrying without the user.
    #[error("login failed: {0}")]
    Login(String),
    /// Network / Apple-side failure that may succeed on retry.
    #[error("temporary failure: {0}")]
    Transient(String),
    /// The anisette source (normally `ssh <mac> imessage-anisette`) failed
    /// or is unreachable. Transient: the saved credentials are still good.
    /// The text starts with "anisette ..." and is already redacted.
    #[error("{0}")]
    Anisette(String),
    #[error("invalid handle {0:?} (expected an email address or phone number)")]
    InvalidHandle(String),
    #[error("unknown chat {0}")]
    ChatNotFound(ChatId),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("state storage: {0}")]
    State(String),
    /// `state.enc` exists but does not authenticate with the key we were
    /// given (wrong key, or the file was tampered with / truncated).
    #[error("cannot decrypt state: {0}")]
    StateKeyMismatch(String),
    #[error("{0}")]
    Protocol(String),
}

pub type EngineResult<T> = Result<T, EngineError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginOutcome {
    /// Logged in and registered; the engine is (or is becoming) connected.
    Ok,
    /// Apple sent a code to the user's trusted devices / phone; call
    /// [`Engine::submit_two_factor`]. Also returned by `submit_two_factor`
    /// itself when Apple accepted the code but asks for another one.
    TwoFactorRequired,
}

/// What a send-type call learned synchronously. For
/// `ChatTarget::Participants` naming a new group the engine mints the group
/// GUID, so the caller only learns the chat id from here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentMessage {
    pub id: MessageId,
    pub chat_id: ChatId,
    /// ms since epoch (rustpush's `sent_timestamp` for the send).
    pub sent_at: u64,
    /// The handle the engine actually sent from (one of ours).
    pub sender: Handle,
}

/// Where to send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatTarget {
    /// New or existing conversation with these handles (emails or phone
    /// numbers, any format; normalized by the engine). One handle = 1:1 chat.
    Participants(Vec<String>),
    /// A conversation the engine already knows (seen in this session or
    /// seeded via [`Engine::upsert_chat`]).
    Chat(ChatId),
}

/// A transcript background as announced on the wire. `locator` is the
/// opaque download locator (same format as an attachment's `mmcs`; it
/// contains the decryption key, so it is redacted from `Debug`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundRef {
    /// Apple's background id (`trabaid`).
    pub background_id: String,
    /// Payload format version (`trabapv`).
    pub payload_version: u32,
    /// The sender's background sequence number (`tbid`).
    pub seq: u64,
    pub size: u64,
    pub locator: String,
}

impl std::fmt::Debug for BackgroundRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundRef")
            .field("background_id", &self.background_id)
            .field("payload_version", &self.payload_version)
            .field("seq", &self.seq)
            .field("size", &self.size)
            .field("locator", &"<redacted>")
            .finish()
    }
}

/// Everything the engine reports asynchronously.
///
/// `at` fields are ms since epoch from the message that caused the event
/// (`MessageInst::sent_timestamp`), so a store can order edits/unsends/
/// reactions that arrive out of order or are redelivered later.
// MessageReceived dominates the size; events are rare enough that boxing
// would only cost ergonomics for consumers.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EngineEvent {
    /// A new message (from someone else, or from another of our devices).
    /// Check `message.verified` before trusting `message.sender`.
    MessageReceived { message: Message, chat: Chat, attachments: Vec<imessage_common::model::Attachment> },
    /// Outcome of a send we initiated (or a later failure report from Apple).
    MessageSent { id: MessageId, chat_id: Option<ChatId>, delivery: DeliveryState, error: Option<String>, at: u64 },
    Delivered(DeliveryReceipt),
    Read(ReadReceipt),
    Typing(TypingEvent),
    Reaction { chat_id: ChatId, message_id: MessageId, reaction: imessage_common::model::Reaction, at: u64 },
    /// `from_me`: the edit came from one of our own handles (another of our
    /// devices). Only then may it touch one of our own messages.
    Edited {
        chat_id: ChatId,
        message_id: MessageId,
        part: u64,
        new_parts: Vec<MessagePart>,
        new_text: Option<String>,
        sender: Handle,
        #[serde(default)]
        from_me: bool,
        at: u64,
    },
    /// `from_me`: as for [`EngineEvent::Edited`].
    Unsent {
        chat_id: ChatId,
        message_id: MessageId,
        part: u64,
        sender: Handle,
        #[serde(default)]
        from_me: bool,
        at: u64,
    },
    ChatRenamed { chat_id: ChatId, name: Option<String>, by: Handle, at: u64 },
    ParticipantsChanged { chat_id: ChatId, participants: Vec<Handle>, by: Handle, at: u64 },
    /// Someone (or another of our devices) set the chat's transcript
    /// background; `None` = removed.
    ChatBackground { chat_id: ChatId, background: Option<BackgroundRef>, by: Handle, at: u64 },
    ConnectionChanged(ConnectionState),
    RegistrationChanged(RegistrationState),
    TwoFactorRequired,
    Error(String),
}

#[async_trait]
pub trait Engine: Send + Sync {
    /// GSA (Apple ID) login. On success continues straight into IDS
    /// registration. Never logs the password.
    async fn login(&self, apple_id: &str, password: &str) -> EngineResult<LoginOutcome>;
    /// Complete a login that returned [`LoginOutcome::TwoFactorRequired`].
    /// `Ok(TwoFactorRequired)` = code accepted but Apple wants another one;
    /// `Err(BadTwoFactorCode)` = wrong code, the session is kept for a retry.
    async fn submit_two_factor(&self, code: &str) -> EngineResult<LoginOutcome>;
    /// Forget the account locally: stop the client, drop the IDS users,
    /// identity and saved Apple ID credentials, cancel a pending 2FA login
    /// and any silent re-login, and go to `NeedsLogin`. Does not deregister
    /// from Apple (the registration expires on its own). The message store
    /// is not touched.
    async fn logout(&self) -> EngineResult<()>;
    /// Register (first time) or force a re-registration now.
    async fn register(&self) -> EngineResult<RegistrationState>;
    async fn registration_state(&self) -> RegistrationState;
    async fn connection_state(&self) -> ConnectionState;
    /// Our registered handles (email only).
    async fn handles(&self) -> Vec<Handle>;

    async fn send_text(&self, chat: &ChatTarget, text: &str, reply_to: Option<&MessageId>) -> EngineResult<SentMessage>;
    async fn send_attachment(&self, chat: &ChatTarget, path: &Path, mime: &str) -> EngineResult<SentMessage>;
    async fn react(&self, chat: &ChatTarget, message_id: &MessageId, kind: ReactionKind, remove: bool) -> EngineResult<SentMessage>;
    async fn edit(&self, chat: &ChatTarget, message_id: &MessageId, new_text: &str) -> EngineResult<SentMessage>;
    async fn unsend(&self, chat: &ChatTarget, message_id: &MessageId) -> EngineResult<SentMessage>;
    async fn set_typing(&self, chat: &ChatTarget, typing: bool) -> EngineResult<()>;
    /// Send a read receipt for `up_to` (iMessage acknowledges one message
    /// GUID; clients mark everything older read locally).
    async fn mark_read(&self, chat: &ChatTarget, up_to: &MessageId) -> EngineResult<()>;
    /// IDS lookup: which of `handles` can receive iMessage. Returns
    /// `(normalized handle, reachable)` in input order. Any un-normalizable
    /// input fails the whole call with [`EngineError::InvalidHandle`].
    async fn lookup_handles(&self, handles: Vec<String>) -> EngineResult<Vec<(String, bool)>>;

    /// Fetch an attachment's content (MMCS download, or the inline bytes)
    /// using the opaque `model::Attachment::mmcs` locator, and write it to
    /// `dest` atomically (temp file in the same directory + rename, mode
    /// 0600). `dest`'s directory must exist. Fails with
    /// [`EngineError::InvalidInput`] once more than `max_bytes` arrive (or
    /// are announced), never buffering more than that.
    async fn download_attachment(&self, locator: &str, dest: &Path, max_bytes: u64) -> EngineResult<()>;

    /// Teach the engine about a chat (e.g. loaded from the store at startup)
    /// so `ChatTarget::Chat` can be resolved after a restart.
    async fn upsert_chat(&self, chat: &Chat);

    fn events(&self) -> broadcast::Receiver<EngineEvent>;

    /// Persist state and stop background tasks.
    async fn shutdown(&self);
}
