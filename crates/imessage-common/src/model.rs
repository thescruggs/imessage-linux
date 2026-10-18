//! Messaging data model shared by `imessaged`, the `imsg` CLI and the KDE app.
//!
//! This is the single source of truth for what crosses the daemon boundary
//! (D-Bus payloads, SQLite rows, JSON output). It is plain data: no rustpush
//! types, no I/O. The daemon converts rustpush structures into these in
//! `imessaged::engine::convert`.
//!
//! Conventions:
//! * Timestamps are **milliseconds since the Unix epoch** (`u64`), the unit
//!   rustpush uses for `MessageInst::sent_timestamp`.
//! * Message ids are Apple's message GUIDs: upper-case hyphenated UUID strings
//!   (`"0F1E…-…"`), exactly as rustpush reports them.
//! * Handles are IDS URIs: `mailto:user@example.com` or `tel:+15551234567`.

use std::fmt;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Handles
// ---------------------------------------------------------------------------

/// An iMessage address (IDS URI).
///
/// `uri` is always normalized (see [`Handle::normalize`]): `mailto:` +
/// lower-cased email, or `tel:` + the number with spaces/dashes/parentheses
/// removed. `display` is an optional human name (contacts integration is a
/// later phase; the daemon fills it with the bare address for now).
/// `profile` is the name and photo the person shared with us (Apple's
/// "Share Name and Photo"); the daemon fills it only for chat participants.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Handle {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<HandleProfile>,
}

/// A contact's self-shared name and photo (Messages "Share Name and
/// Photo"), as the daemon fetched it from iCloud. Any field may be missing:
/// people share a name without a photo, and a photo arrives after the name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub struct HandleProfile {
    /// The display name the person chose (Apple's `dn`).
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub first: Option<String>,
    #[serde(default)]
    pub last: Option<String>,
    /// Absolute path of the downloaded photo (JPEG, PNG or HEIC).
    #[serde(default)]
    pub photo: Option<String>,
    /// ms since epoch of the fetch that produced this.
    #[serde(default)]
    pub updated_at: u64,
}

impl HandleProfile {
    /// The name to show: the display name, else "first last".
    pub fn display_name(&self) -> Option<String> {
        let dn = self.name.as_deref().map(str::trim).filter(|n| !n.is_empty());
        if let Some(n) = dn {
            return Some(n.to_string());
        }
        let full = [self.first.as_deref(), self.last.as_deref()].into_iter().flatten().map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
        (!full.is_empty()).then_some(full)
    }

    /// Nothing to show (no name, no photo).
    pub fn is_empty(&self) -> bool {
        self.display_name().is_none() && self.photo.is_none()
    }
}

impl Handle {
    /// Normalize a user-entered or wire address into IDS URI form, or `None`
    /// if `raw` is neither an email address nor a phone number.
    ///
    /// * `Foo@Example.COM` / `mailto:Foo@Example.com` -> `mailto:foo@example.com`
    /// * `+1 (555) 123-4567` / `tel:+15551234567`     -> `tel:+15551234567`
    /// * `""`, `"bob"`, `"tel:"`, `"@x"`, `"a1"`      -> `None`
    ///
    /// Phone numbers may only contain digits, `+`, spaces, `-`, `.`, `(`, `)`
    /// and must contain at least one digit.
    pub fn try_normalize(raw: &str) -> Option<String> {
        let raw = raw.trim();
        let lower = raw.to_ascii_lowercase();
        let (body, forced_email) = if let Some(rest) = lower.strip_prefix("mailto:") {
            (rest.trim().to_string(), true)
        } else if lower.starts_with("tel:") {
            (raw[4..].trim().to_string(), false)
        } else {
            (raw.to_string(), false)
        };
        if forced_email || body.contains('@') {
            let email = body.to_ascii_lowercase();
            let (local, domain) = email.split_once('@')?;
            if local.is_empty() || domain.is_empty() || domain.contains('@') || email.chars().any(char::is_whitespace) {
                return None;
            }
            return Some(format!("mailto:{email}"));
        }
        if !body.chars().all(|c| c.is_ascii_digit() || matches!(c, '+' | ' ' | '-' | '.' | '(' | ')')) {
            return None;
        }
        let digits: String = body.chars().filter(|c| c.is_ascii_digit() || *c == '+').collect();
        if !digits.chars().any(|c| c.is_ascii_digit()) {
            return None;
        }
        Some(format!("tel:{digits}"))
    }

    /// Infallible variant for data that is already an address (rustpush wire
    /// data, stored rows). Un-normalizable input is returned trimmed and
    /// lower-cased instead of being turned into a bogus `tel:` URI. Validate
    /// user input with [`Handle::try_normalize`] / [`Handle::parse`] instead.
    pub fn normalize(raw: &str) -> String {
        Handle::try_normalize(raw).unwrap_or_else(|| raw.trim().to_ascii_lowercase())
    }

    /// Whether `uri` is already a normalized IDS URI: `mailto:` + a
    /// lower-cased address, or `tel:` + an optional `+` and digits only.
    /// Stored / imported handles must pass this; anything else (raw Mac
    /// handle ids like `46339(smsft)`, `urn:biz:…`, contact names) is not
    /// an address iMessage can use.
    pub fn is_valid_uri(uri: &str) -> bool {
        if Handle::try_normalize(uri).as_deref() != Some(uri) {
            return false;
        }
        match uri.strip_prefix("tel:") {
            Some(n) => {
                let d = n.strip_prefix('+').unwrap_or(n);
                !d.is_empty() && d.chars().all(|c| c.is_ascii_digit())
            }
            None => !uri.contains(['(', ')', '<', '>', ',', ';']),
        }
    }

    /// Validated constructor for user input.
    pub fn parse(raw: &str) -> Option<Handle> {
        Handle::try_normalize(raw).map(|uri| Handle { uri, display: None, profile: None })
    }

    pub fn new(raw: &str) -> Handle {
        Handle { uri: Handle::normalize(raw), display: None, profile: None }
    }

    pub fn is_email(&self) -> bool {
        self.uri.starts_with("mailto:")
    }

    pub fn is_phone(&self) -> bool {
        self.uri.starts_with("tel:")
    }

    /// The address without its `mailto:` / `tel:` scheme.
    pub fn address(&self) -> &str {
        self.uri
            .strip_prefix("mailto:")
            .or_else(|| self.uri.strip_prefix("tel:"))
            .unwrap_or(&self.uri)
    }
}

impl fmt::Display for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.uri)
    }
}

// ---------------------------------------------------------------------------
// Ids
// ---------------------------------------------------------------------------

/// Stable conversation identifier.
///
/// rustpush identifies a conversation by its `ConversationData`
/// (`participants`, `cv_name`, `sender_guid`). Apple's own key for a group is
/// the group GUID (`gid`, rustpush `sender_guid`), which survives renames and
/// membership changes; one-to-one chats are keyed by the other party. So:
///
/// * one-to-one: the other participant's normalized URI, e.g.
///   `mailto:bob@example.com`;
/// * group with a GUID: `group:<GUID upper-case>`;
/// * group without a GUID (rare; old clients): `group-p:` + sorted participant
///   URIs joined by `,`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChatId(pub String);

impl ChatId {
    pub const GROUP_PREFIX: &'static str = "group:";
    pub const GROUP_PARTICIPANTS_PREFIX: &'static str = "group-p:";

    pub fn direct(other: &str) -> ChatId {
        ChatId(Handle::normalize(other))
    }

    pub fn group(guid: &str) -> ChatId {
        ChatId(format!("{}{}", Self::GROUP_PREFIX, guid.to_ascii_uppercase()))
    }

    pub fn group_by_participants(participants: &[String]) -> ChatId {
        let mut p: Vec<String> = participants.iter().map(|p| Handle::normalize(p)).collect();
        p.sort();
        p.dedup();
        ChatId(format!("{}{}", Self::GROUP_PARTICIPANTS_PREFIX, p.join(",")))
    }

    pub fn is_group(&self) -> bool {
        self.0.starts_with(Self::GROUP_PREFIX) || self.0.starts_with(Self::GROUP_PARTICIPANTS_PREFIX)
    }

    /// Whether this is a well-formed id: a valid IDS URI
    /// ([`Handle::is_valid_uri`]) for a one-to-one chat, `group:` + a
    /// non-empty upper-case GUID without whitespace or commas, or
    /// `group-p:` + valid URIs joined by `,`.
    pub fn is_valid(&self) -> bool {
        if let Some(g) = self.group_guid() {
            return !g.is_empty() && g == g.to_ascii_uppercase() && !g.chars().any(|c| c.is_whitespace() || c.is_control() || c == ',');
        }
        if let Some(p) = self.0.strip_prefix(Self::GROUP_PARTICIPANTS_PREFIX) {
            return !p.is_empty() && p.split(',').all(Handle::is_valid_uri);
        }
        Handle::is_valid_uri(&self.0)
    }

    /// The group GUID, for `group:` ids.
    pub fn group_guid(&self) -> Option<&str> {
        self.0.strip_prefix(Self::GROUP_PREFIX)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ChatId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Apple message GUID (upper-case UUID string).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageId(pub String);

impl MessageId {
    pub fn new(s: impl Into<String>) -> MessageId {
        MessageId(s.into().to_ascii_uppercase())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Attachment id: `<message id>:<part index>` (rustpush attachments carry no
/// id of their own; the part index is stable within a message).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttachmentId(pub String);

impl AttachmentId {
    pub fn for_part(message: &MessageId, part: usize) -> AttachmentId {
        AttachmentId(format!("{}:{}", message.0, part))
    }
}

impl fmt::Display for AttachmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Chats and messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chat {
    pub id: ChatId,
    /// Everyone in the chat **except** the local user.
    pub participants: Vec<Handle>,
    pub display_name: Option<String>,
    pub is_group: bool,
    /// ms since epoch of the newest message, if any.
    pub last_message_at: Option<u64>,
    pub unread_count: u32,
    /// The chat's transcript background (Messages "Backgrounds", shared by
    /// everyone in the chat), if one is set.
    #[serde(default)]
    pub background: Option<ChatBackground>,
    /// Absolute path of the group's photo (Messages "Change Name and
    /// Photo" on a group), once downloaded; `None` for none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_path: Option<String>,
    /// Pinned to the top of the chat list (local to this computer, like
    /// Messages' pins; `imsg sync-mac` copies the Mac's).
    #[serde(default)]
    pub pinned: bool,
    /// Position among the pinned chats, ascending (0 = first); `None` when
    /// not pinned.
    #[serde(default)]
    pub pin_order: Option<u32>,
}

/// A chat's transcript background as the daemon has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatBackground {
    /// Apple's background id; a new one for every change.
    pub id: String,
    /// Absolute path of the image to paint behind the transcript (HEIC,
    /// PNG or JPEG, extracted from the payload); `None` until downloaded,
    /// or when the payload holds no picture (e.g. a colour poster).
    #[serde(default)]
    pub path: Option<String>,
    /// Absolute path of the payload as Apple sent it (a PosterKit
    /// archive); `None` until downloaded.
    #[serde(default)]
    pub payload: Option<String>,
    /// Who set it, set_by/set_at: `None`/0 for one imported locally.
    /// Apple's payload format version.
    #[serde(default)]
    pub version: u32,
    /// Who set it (one of our handles when we did, from any device).
    #[serde(default)]
    pub set_by: Option<String>,
    /// ms since epoch.
    #[serde(default)]
    pub set_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    /// Queued locally, not yet accepted by Apple.
    #[default]
    Pending,
    /// Accepted by Apple's servers.
    Sent,
    /// Recipient device acknowledged.
    Delivered,
    /// Recipient read it (only if they have read receipts on).
    Read,
    Failed,
}

/// How a message travelled: iMessage, or as a text message (SMS / MMS /
/// RCS) relayed by the user's iPhone ("Text Message Forwarding").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    #[default]
    Imessage,
    Sms,
}

impl Service {
    pub fn as_str(self) -> &'static str {
        match self {
            Service::Imessage => "imessage",
            Service::Sms => "sms",
        }
    }

    /// `"imessage"` / `"sms"` (also `"text"`, `"rcs"`, `"mms"`), any case.
    pub fn parse(s: &str) -> Option<Service> {
        match s.trim().to_ascii_lowercase().as_str() {
            "imessage" => Some(Service::Imessage),
            "sms" | "mms" | "rcs" | "text" => Some(Service::Sms),
            _ => None,
        }
    }
}

impl fmt::Display for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessagePart {
    Text { text: String },
    Attachment {
        attachment_id: AttachmentId,
        mime: String,
        filename: String,
        size: u64,
    },
    /// An @-mention: `handle` is the mentioned URI, `text` the visible text.
    Mention { handle: String, text: String },
    /// Something we can't render (app balloon object, etc.).
    Unsupported { description: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReactionKind {
    Love,
    Like,
    Dislike,
    Laugh,
    Emphasize,
    Question,
    Emoji(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    pub kind: ReactionKind,
    pub sender: Handle,
    /// `true` = reaction added, `false` = removed.
    pub added: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub chat_id: ChatId,
    pub sender: Handle,
    /// ms since epoch.
    pub sent_at: u64,
    /// Plain text of all text/mention parts concatenated (attachments
    /// omitted); `None` if the message has no text.
    pub text: Option<String>,
    pub parts: Vec<MessagePart>,
    pub subject: Option<String>,
    /// For inline replies: the message being replied to.
    pub reply_to: Option<MessageId>,
    /// Sent by this account (from this or another of the user's devices).
    pub is_from_me: bool,
    pub delivery: DeliveryState,
    pub edited: bool,
    pub unsent: bool,
    pub reactions: Vec<Reaction>,
    /// Screen/bubble effect id (e.g. `com.apple.messages.effect.CKConfettiEffect`).
    pub effect: Option<String>,
    /// `false` if rustpush could not verify the sender's signing key
    /// (`MessageInst::verification_failed`): the message decrypted, but its
    /// claimed sender is unauthenticated. Stores must keep this flag and UIs
    /// must flag the sender as unverified. Always `true` for our own sends.
    #[serde(default = "default_true")]
    pub verified: bool,
    /// iMessage, or a text message relayed by the user's iPhone (green
    /// bubble). Missing in older JSON = iMessage.
    #[serde(default)]
    pub service: Service,
}

fn default_true() -> bool {
    true
}

/// Attachment metadata. Content is downloaded separately (Phase 2b) using the
/// opaque `mmcs` locator.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: AttachmentId,
    pub message_id: MessageId,
    pub mime: String,
    pub filename: String,
    pub size: u64,
    /// Where the downloaded file lives, once fetched.
    pub local_path: Option<String>,
    /// Opaque JSON produced by the daemon from rustpush's `Attachment`
    /// (includes the MMCS URL and the per-file decryption key, or the inline
    /// bytes). Only the daemon interprets it. Treat as secret.
    pub mmcs: String,
}

impl fmt::Debug for Attachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attachment")
            .field("id", &self.id)
            .field("message_id", &self.message_id)
            .field("mime", &self.mime)
            .field("filename", &self.filename)
            .field("size", &self.size)
            .field("local_path", &self.local_path)
            .field("mmcs", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypingEvent {
    pub chat_id: ChatId,
    pub sender: Handle,
    pub typing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadReceipt {
    pub chat_id: Option<ChatId>,
    pub message_id: MessageId,
    pub reader: Handle,
    /// ms since epoch.
    pub read_at: u64,
    /// The reader is one of our own handles (we read someone else's message
    /// on another device). `false` = a peer read one of our messages.
    #[serde(default)]
    pub by_me: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryReceipt {
    pub chat_id: Option<ChatId>,
    pub message_id: MessageId,
    pub recipient: Handle,
    /// ms since epoch.
    pub delivered_at: u64,
}

// ---------------------------------------------------------------------------
// Account / connection state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RegistrationState {
    /// Registered iMessage handles: the account's email addresses and, when
    /// an iPhone on the account has linked it, its phone number.
    pub handles: Vec<Handle>,
    /// Seconds since epoch of the last successful IDS registration.
    pub registered_at: Option<u64>,
    /// Seconds since epoch when rustpush will re-register (heartbeat or
    /// 5 min before certificate expiry).
    pub expires_at: Option<u64>,
    /// The handle new messages are sent from: the configured
    /// `sender_handle` if it is registered, else the first handle.
    #[serde(default)]
    pub sender: Option<Handle>,
    /// The daemon's `other_handles` setting (IDS URIs): addresses of ours
    /// that this device is not registered for (e.g. the iPhone's number).
    /// Messages from them count as our own; nothing is sent from them.
    #[serde(default)]
    pub other_handles: Vec<String>,
}

impl RegistrationState {
    pub fn is_registered(&self) -> bool {
        !self.handles.is_empty() && self.registered_at.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
pub enum ConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    /// No usable account (no hwconfig, never logged in, or Apple demands a
    /// fresh login, e.g. IDS error 6005).
    NeedsLogin,
    NeedsTwoFactor,
    Registering,
    Error(String),
}

/// Whether the daemon can fetch contacts' shared names and photos (it
/// needs iCloud / CloudKit access, which comes with the Apple ID login).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProfilesState {
    /// CloudKit access works.
    Ready,
    /// Not tried yet (or a try is running).
    #[default]
    Pending,
    /// The saved session cannot get CloudKit access without the user
    /// (Apple wants a fresh login / 2FA): run `imsg login` once.
    NeedsLogin,
    /// Not possible right now (no saved credentials, network or anisette
    /// trouble); `detail` says why. Retried later where that makes sense.
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProfilesStatus {
    pub state: ProfilesState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Text Message Forwarding (SMS / MMS / RCS relayed by the user's
/// iPhone) as the daemon knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SmsState {
    /// Not requested (or turned off).
    #[default]
    Off,
    /// We asked the iPhone; waiting for the user to turn this device on
    /// under Settings > Messages > Text Message Forwarding.
    Requested,
    /// The iPhone confirmed, or relayed a text message to us.
    Active,
    /// The request could not be sent; `detail` says why.
    Error,
}

impl SmsState {
    pub fn as_str(self) -> &'static str {
        match self {
            SmsState::Off => "off",
            SmsState::Requested => "requested",
            SmsState::Active => "active",
            SmsState::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SmsForwarding {
    #[serde(default)]
    pub state: SmsState,
    /// The iPhone's number that relays text messages (`tel:+…`), if known.
    #[serde(default)]
    pub number: Option<String>,
    /// ms since epoch of the last state change.
    #[serde(default)]
    pub since: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl SmsForwarding {
    pub fn is_active(&self) -> bool {
        self.state == SmsState::Active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_handles() {
        assert_eq!(Handle::normalize("Foo@Example.COM"), "mailto:foo@example.com");
        assert_eq!(Handle::normalize("mailto:Foo@Example.com"), "mailto:foo@example.com");
        assert_eq!(Handle::normalize("MAILTO:a@b.c"), "mailto:a@b.c");
        assert_eq!(Handle::normalize("+1 (555) 123-4567"), "tel:+15551234567");
        assert_eq!(Handle::normalize("tel:+15551234567"), "tel:+15551234567");
        assert_eq!(Handle::normalize("tel: 555.123"), "tel:555123");
        for bad in ["", "  ", "bob", "tel:", "tel:abc", "+", "@x.com", "a@", "a1", "mailto:", "mailto:bob"] {
            assert_eq!(Handle::try_normalize(bad), None, "{bad:?}");
            assert!(Handle::parse(bad).is_none());
            // never invents a tel: URI out of garbage
            assert!(bad.starts_with("tel:") || !Handle::normalize(bad).starts_with("tel:"), "{bad:?}");
        }
        assert_eq!(Handle::normalize("Bob"), "bob");
        let h = Handle::new("x@y.z");
        assert!(h.is_email() && !h.is_phone());
        assert_eq!(h.address(), "x@y.z");
    }

    #[test]
    fn chat_ids() {
        assert_eq!(ChatId::direct("Bob@x.com").0, "mailto:bob@x.com");
        let g = ChatId::group("abc-def");
        assert_eq!(g.0, "group:ABC-DEF");
        assert!(g.is_group());
        assert_eq!(g.group_guid(), Some("ABC-DEF"));
        let p = ChatId::group_by_participants(&["mailto:b@x".into(), "a@x".into(), "mailto:b@x".into()]);
        assert_eq!(p.0, "group-p:mailto:a@x,mailto:b@x");
        assert!(p.is_group());
        assert!(!ChatId::direct("a@b").is_group());
    }

    #[test]
    fn valid_ids() {
        for ok in ["mailto:bob@x.com", "tel:+12055551234", "tel:46339", "group:ABC-1", "group-p:mailto:a@x,tel:+1555"] {
            assert!(ChatId(ok.into()).is_valid(), "{ok}");
        }
        for bad in [
            "",
            "46339(smsft)",
            "tel:46339(smsft)",
            "+12125550199(smsfp)",
            "tel:+1 555",
            "tel:1+2",
            "mailto:Bob@x.com",
            "mailto:google@rbm.goog(smsft)",
            "bob@x.com",
            "mom",
            "urn:biz:88d22123",
            "group:",
            "group:abc",
            "group:A B",
            "group-p:",
            "group-p:mailto:a@x,bob",
        ] {
            assert!(!ChatId(bad.into()).is_valid(), "{bad:?}");
        }
        assert!(Handle::is_valid_uri(&Handle::new("+1 (205) 555-1234").uri));
        assert!(!Handle::is_valid_uri("tel:"));
    }

    #[test]
    fn serde_shapes() {
        let s = serde_json::to_string(&ConnectionState::Error("x".into())).unwrap();
        assert_eq!(s, r#"{"state":"error","detail":"x"}"#);
        let p = MessagePart::Text { text: "hi".into() };
        assert_eq!(serde_json::to_string(&p).unwrap(), r#"{"type":"text","text":"hi"}"#);
        let k = serde_json::to_string(&ReactionKind::Emoji("🔥".into())).unwrap();
        assert_eq!(serde_json::from_str::<ReactionKind>(&k).unwrap(), ReactionKind::Emoji("🔥".into()));
        assert_eq!(serde_json::to_string(&MessageId::new("ab")).unwrap(), r#""AB""#);
        // no profile: the handle JSON is unchanged
        assert_eq!(serde_json::to_string(&Handle::new("a@b.c")).unwrap(), r#"{"uri":"mailto:a@b.c"}"#);
        assert_eq!(serde_json::to_string(&Service::Sms).unwrap(), r#""sms""#);
        assert_eq!(serde_json::from_str::<Service>(r#""imessage""#).unwrap(), Service::Imessage);
        assert_eq!(Service::parse("SMS"), Some(Service::Sms));
        assert_eq!(Service::parse("rcs"), Some(Service::Sms));
        assert_eq!(Service::parse("bogus"), None);
        let f = SmsForwarding { state: SmsState::Requested, number: Some("tel:+15555550100".into()), since: Some(5), detail: None };
        assert_eq!(serde_json::to_string(&f).unwrap(), r#"{"state":"requested","number":"tel:+15555550100","since":5}"#);
        assert_eq!(serde_json::from_str::<SmsForwarding>("{}").unwrap(), SmsForwarding::default());
        let s = ProfilesStatus { state: ProfilesState::NeedsLogin, detail: None };
        assert_eq!(serde_json::to_string(&s).unwrap(), r#"{"state":"needs_login"}"#);
    }

    #[test]
    fn profile_display_name() {
        let mut p = HandleProfile { first: Some("Ann".into()), last: Some("Lee".into()), ..Default::default() };
        assert_eq!(p.display_name().as_deref(), Some("Ann Lee"));
        p.name = Some("  Annie ".into());
        assert_eq!(p.display_name().as_deref(), Some("Annie"));
        assert!(!p.is_empty());
        assert!(HandleProfile { name: Some(" ".into()), ..Default::default() }.is_empty());
    }

    #[test]
    fn attachment_debug_redacts_locator() {
        let a = Attachment {
            id: AttachmentId("M:0".into()),
            message_id: MessageId("M".into()),
            mime: "image/png".into(),
            filename: "a.png".into(),
            size: 1,
            local_path: None,
            mmcs: "SECRETKEY".into(),
        };
        assert!(!format!("{a:?}").contains("SECRETKEY"));
    }
}
