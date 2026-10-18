//! `imsg import-mac`: one-time import of recent history from a Mac's
//! Messages database (see docs/CLI.md).
//!
//! 1. `tools/mac-messages-export.py` (embedded) runs on the Mac over one ssh
//!    connection and streams a tar: `export.jsonl` + attachment files.
//! 2. The tar is unpacked into `~/.cache/imsg/import/<run>/`, which the
//!    sandboxed daemon can read.
//! 3. Records are converted to the daemon's model ([`convert`], pure) and
//!    sent in batches to `ImportMessages`; the daemon copies the files.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};

use imessage_common::import::{ImportAttachment, ImportBatch, ImportLink, ImportMessage, ImportReaction, ImportResult};
use imessage_common::model::{AttachmentId, Chat, ChatId, DeliveryState, Handle, Message, MessageId, MessagePart, Reaction, ReactionKind, Service};
use serde::{Deserialize, Serialize};

use crate::Ctx;
use crate::client::{CliError, Client, exit};

/// The Mac-side exporter, shipped inside the binary.
const EXPORT_SCRIPT: &str = include_str!("../../../tools/mac-messages-export.py");
/// Messages per `ImportMessages` call.
pub const BATCH_MESSAGES: usize = 200;
/// Reactions per call (reactions go after all messages).
pub const BATCH_REACTIONS: usize = 1000;
/// 2001-01-01T00:00:00Z in Unix ms.
const APPLE_EPOCH_MS: i64 = 978_307_200_000;

#[derive(Debug, Clone, PartialEq)]
pub struct ImportArgs {
    pub host: String,
    pub days: u32,
    pub since: Option<String>,
    pub max_attachment_mb: u64,
    /// Abort if the export (the tar stream) grows beyond this.
    pub max_total_mb: u64,
    pub imessage_only: bool,
    pub dry_run: bool,
}

// ---------------------------------------------------------------------------
// export.jsonl records (see tools/mac-messages-export.py)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MetaRec {
    #[serde(default)]
    pub text_decode_failures: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChatRec {
    pub rowid: i64,
    #[serde(default)]
    pub chat_identifier: String,
    /// 43 = group, 45 = one-to-one.
    #[serde(default)]
    pub style: i64,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub group_id: Option<String>,
    #[serde(default)]
    pub participants: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AttachmentRec {
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub transfer_name: Option<String>,
    #[serde(default)]
    pub total_bytes: u64,
    #[serde(default)]
    pub hidden: bool,
    /// Path inside the tar, if the file was included.
    #[serde(default)]
    pub path: Option<String>,
    /// Why not: `not_downloaded`, `too_large`, `hidden`, `not_requested`.
    #[serde(default)]
    pub missing: Option<String>,
    /// A link preview's image or icon (hidden in Messages; the preview
    /// archive indexes these, in order).
    #[serde(default)]
    pub link_file: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MessageRec {
    pub guid: String,
    #[serde(default)]
    pub chats: Vec<i64>,
    #[serde(default)]
    pub handle: Option<String>,
    #[serde(default)]
    pub is_from_me: bool,
    #[serde(default)]
    pub date: i64,
    #[serde(default)]
    pub date_delivered: i64,
    #[serde(default)]
    pub date_read: i64,
    #[serde(default)]
    pub date_edited: i64,
    #[serde(default)]
    pub date_retracted: i64,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default)]
    pub item_type: i64,
    #[serde(default)]
    pub associated_guid: Option<String>,
    #[serde(default)]
    pub associated_type: i64,
    #[serde(default)]
    pub associated_emoji: Option<String>,
    #[serde(default)]
    pub thread_originator_guid: Option<String>,
    #[serde(default)]
    pub balloon: Option<String>,
    /// A link preview's `RichLink` keyed archive (base64), for
    /// `balloon` = [`URL_BALLOON`].
    #[serde(default)]
    pub payload_data: Option<String>,
    #[serde(default)]
    pub effect: Option<String>,
    #[serde(default)]
    pub is_delivered: bool,
    #[serde(default)]
    pub error: i64,
    #[serde(default)]
    pub destination_caller_id: Option<String>,
    #[serde(default)]
    pub attachments: Vec<AttachmentRec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    Meta(MetaRec),
    Chat(ChatRec),
    Message(Box<MessageRec>),
}

// ---------------------------------------------------------------------------
// conversion (pure)
// ---------------------------------------------------------------------------

/// chat.db date (ns since 2001, or seconds in very old rows) -> Unix ms.
/// `None` for 0 / unset.
pub fn apple_ms(d: i64) -> Option<u64> {
    if d <= 0 {
        return None;
    }
    let ms = if d > 100_000_000_000 { d / 1_000_000 } else { d * 1000 };
    u64::try_from(APPLE_EPOCH_MS + ms).ok()
}

/// `p:0/<GUID>` / `bp:<GUID>` / `<GUID>` -> the target message id.
pub fn reaction_target(associated: &str) -> Option<MessageId> {
    let g = match associated.split_once('/') {
        Some((_, g)) => g,
        None => associated.strip_prefix("bp:").unwrap_or(associated),
    };
    let g = g.trim();
    (!g.is_empty()).then(|| MessageId::new(g))
}

/// `associated_message_type` -> (kind, added). 2000-2005 classic tapbacks,
/// 2006 emoji; 3000-3006 the removals.
pub fn reaction_kind(t: i64, emoji: Option<&str>) -> Option<(ReactionKind, bool)> {
    let added = match t {
        2000..=2006 => true,
        3000..=3006 => false,
        _ => return None,
    };
    let kind = match t % 1000 {
        0 => ReactionKind::Love,
        1 => ReactionKind::Like,
        2 => ReactionKind::Dislike,
        3 => ReactionKind::Laugh,
        4 => ReactionKind::Emphasize,
        5 => ReactionKind::Question,
        _ => ReactionKind::Emoji(emoji.filter(|e| !e.is_empty())?.to_string()),
    };
    Some((kind, added))
}

fn guess_mime(name: &str) -> String {
    let m = crate::resolve::guess_mime(Path::new(name));
    if m.is_empty() { "application/octet-stream".into() } else { m.into() }
}

#[derive(Debug, Clone, Default)]
pub struct ConvertOptions {
    /// Our own handles (registered + other_handles), any form.
    pub my_handles: Vec<String>,
    /// Sender for our messages without a `destination_caller_id`.
    pub default_sender: Option<String>,
    /// Drop SMS / RCS (relayed by the iPhone) rows.
    pub imessage_only: bool,
}

/// What the export contains and what the conversion skipped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Stats {
    pub chats: usize,
    pub messages: usize,
    pub reactions: usize,
    pub attachments_with_file: usize,
    /// Attachments whose file is not on the Mac (never downloaded there).
    pub attachments_not_on_mac: usize,
    pub attachments_too_large: usize,
    /// Text recovered from `attributedBody` failed (text left empty).
    pub text_decode_failures: u64,
    pub skipped_system: usize,
    pub skipped_no_chat: usize,
    pub skipped_sms: usize,
    pub skipped_other: usize,
    /// Rows in chats, or from senders, that have no valid iMessage address
    /// (contact names, `urn:biz:` business chats, ...).
    pub skipped_invalid: usize,
    /// Messages with a link preview (the daemon decodes it into a card).
    pub link_previews: usize,
}

/// Messages' link-preview balloon.
pub const URL_BALLOON: &str = "com.apple.messages.URLBalloonProvider";

#[derive(Debug, Clone, Default)]
pub struct Converted {
    pub chats: BTreeMap<ChatId, Chat>,
    /// Oldest first.
    pub messages: Vec<ImportMessage>,
    /// Oldest first.
    pub reactions: Vec<ImportReaction>,
    pub stats: Stats,
}

pub(crate) struct Me {
    /// Every handle of ours, normalized: `configured` plus the addresses
    /// the Mac received or sent on.
    all: BTreeSet<String>,
    /// The handles the daemon knows (registered + `other_handles`), in order.
    configured: Vec<String>,
}

impl Me {
    pub(crate) fn new(configured: &[String], seen: impl IntoIterator<Item = String>) -> Self {
        let configured: Vec<String> = configured.iter().map(|h| Handle::normalize(h)).collect();
        let all = configured.iter().cloned().chain(seen).collect();
        Me { all, configured }
    }

    /// Our handle `h` stands for, as we know it (`2055551234` matches a
    /// configured `tel:+12055551234`); `None` if it is not ours.
    fn canonical(&self, h: &str) -> Option<String> {
        let n = Handle::normalize(strip_tags(h));
        if self.all.contains(&n) {
            return Some(n);
        }
        if !n.starts_with("tel:") {
            return None;
        }
        self.configured.iter().chain(&self.all).find(|m| m.starts_with("tel:") && same_number(m, &n)).cloned()
    }

    fn has(&self, h: &str) -> bool {
        self.canonical(h).is_some()
    }
}

/// `tel:+12055551234` == `tel:2055551234` (missing country code).
fn same_number(a: &str, b: &str) -> bool {
    let d = |s: &str| s.chars().filter(char::is_ascii_digit).collect::<String>();
    let (a, b) = (d(a), d(b));
    a.len() >= 7 && b.len() >= 7 && (a.ends_with(&b) || b.ends_with(&a)) && a.len().abs_diff(b.len()) <= 3
}

/// A Mac handle id without the tags Messages appends to filtered SMS / RCS
/// senders: `46339(smsft)`, `+12055551234(smsfp)`, `(filtered)`,
/// `(smsft_rm)`, `(smsft_fi)`, `(smsft_or)`, repeated (`(smsft)(smsft)`),
/// also on RCS business addresses (`google@rbm.goog(smsft)`). Only a
/// trailing parenthesised word with a letter in it is a tag; a phone
/// number's `(205)` is not.
pub fn strip_tags(raw: &str) -> &str {
    let mut t = raw.trim();
    while let Some(rest) = t.strip_suffix(')')
        && let Some(open) = rest.rfind('(')
        && open > 0
        && rest[open + 1..].chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && rest[open + 1..].chars().any(|c| c.is_ascii_alphabetic())
    {
        t = rest[..open].trim_end();
    }
    t
}

/// A peer's handle as live iMessage would name it. Mac SMS rows often
/// carry a bare national number (`2055551234`, `12055551234`) where the
/// live chat has `tel:+12055551234`: a 10-digit number with a NANP area
/// code (first digit 2-9), or 11 digits starting with `1` + one, gets the
/// `+1` country code (the same assumption as [`same_number`]). Anything
/// else (emails, `+`-prefixed numbers, short codes, other lengths) is
/// normalized as is (a short code `46339` is `tel:46339`). Tags like
/// `(smsft)` are stripped first ([`strip_tags`]). The result may still not
/// be a valid address (a contact name, `urn:biz:…`); callers check.
pub fn peer_handle(raw: &str) -> Handle {
    let t = strip_tags(raw);
    let digits: String = t.chars().filter(char::is_ascii_digit).collect();
    let formatting_only = t.chars().all(|c| c.is_ascii_digit() || matches!(c, ' ' | '-' | '(' | ')' | '.'));
    let nanp = |d: &str| d.as_bytes().first().is_some_and(|b| (b'2'..=b'9').contains(b));
    if formatting_only && !t.is_empty() {
        if digits.len() == 10 && nanp(&digits) {
            return Handle::new(&format!("+1{digits}"));
        }
        if digits.len() == 11 && digits.starts_with('1') && nanp(&digits[1..]) {
            return Handle::new(&format!("+{digits}"));
        }
    }
    Handle::new(t)
}

/// The chat as live iMessage would key it (see `convert::chat_id_for` in
/// the daemon): a group only with more than one other participant (a
/// two-person Mac "group" is the 1:1 chat, as live), keyed by its group
/// GUID or else by every participant *including* `our` handle; a 1:1 by the
/// other party; only our own handles listed = note to self, also for a Mac
/// "group" (style 43) row, as live iMessage keys such traffic between our
/// own devices (the Mac reuses a group id across such chats). A Mac group
/// listing nobody (everyone else left) stays a group (there is no live
/// equivalent).
///
/// The note-to-self handle is, in order: the address the Mac last used in
/// the chat (`dest`, a `destination_caller_id`) if the chat lists it, the
/// first listed handle the daemon knows (registered or `other_handles`),
/// the first listed; always in the form the daemon knows it.
pub(crate) fn chat_for(c: &ChatRec, me: &Me, dest: Option<&str>, our: Option<&str>) -> Chat {
    let mut others: Vec<Handle> = vec![];
    for h in c.participants.iter().filter(|p| !me.has(p)).map(|p| peer_handle(p)) {
        if !others.contains(&h) {
            others.push(h);
        }
    }
    let group = others.len() > 1 || (c.participants.is_empty() && c.style == 43);
    let (id, participants) = if group {
        let id = match c.group_id.as_deref().map(str::trim).filter(|g| !g.is_empty()) {
            Some(g) => ChatId::group(g),
            None => ChatId::group_by_participants(&others.iter().map(|h| h.uri.clone()).chain(our.map(String::from)).collect::<Vec<_>>()),
        };
        (id, others)
    } else if let Some(o) = others.first() {
        (ChatId::direct(&o.uri), vec![o.clone()])
    } else {
        // Only our own handles: note to self, the chat with our handle. A
        // chat listing nobody is the chat with its identifier (an SMS short
        // code's `57513(smsft)`), ours or not.
        let listed = || c.participants.iter().filter_map(|p| me.canonical(p));
        let h = if c.participants.is_empty() {
            me.canonical(&c.chat_identifier).map(|h| Handle::new(&h)).unwrap_or_else(|| peer_handle(&c.chat_identifier))
        } else {
            let dest = dest.and_then(|d| me.canonical(d)).filter(|d| listed().any(|h| h == *d));
            let h = dest.or_else(|| listed().find(|h| me.configured.contains(h))).or_else(|| listed().next());
            Handle::new(&h.unwrap_or_default())
        };
        (ChatId::direct(&h.uri), vec![h])
    };
    Chat {
        is_group: id.is_group(),
        id,
        participants,
        display_name: c.display_name.clone().filter(|n| !n.trim().is_empty()),
        last_message_at: None,
        unread_count: 0,
        background: None,
        icon_path: None,
        pinned: false,
        pin_order: None,
    }
}

/// Text segments around U+FFFC placeholders interleaved with the
/// attachments when they line up; otherwise attachments first, then text.
fn build_parts(text: &str, atts: Vec<(MessagePart, ImportAttachment)>, id: &MessageId) -> (Vec<MessagePart>, Vec<ImportAttachment>) {
    let mut parts = vec![];
    let mut out_atts = vec![];
    let push_text = |parts: &mut Vec<MessagePart>, t: &str| {
        if !t.trim().is_empty() {
            parts.push(MessagePart::Text { text: t.to_string() });
        }
    };
    let mut push_att = |parts: &mut Vec<MessagePart>, (part, mut a): (MessagePart, ImportAttachment)| {
        let aid = AttachmentId::for_part(id, parts.len());
        let MessagePart::Attachment { mime, filename, size, .. } = part else { return };
        a.id = aid.clone();
        parts.push(MessagePart::Attachment { attachment_id: aid, mime, filename, size });
        out_atts.push(a);
    };
    let segments: Vec<&str> = text.split('\u{FFFC}').collect();
    if !atts.is_empty() && segments.len() == atts.len() + 1 {
        let mut atts = atts.into_iter();
        for (i, seg) in segments.iter().enumerate() {
            push_text(&mut parts, seg);
            if i < segments.len() - 1
                && let Some(a) = atts.next()
            {
                push_att(&mut parts, a);
            }
        }
    } else {
        for a in atts {
            push_att(&mut parts, a);
        }
        push_text(&mut parts, &text.replace('\u{FFFC}', ""));
    }
    (parts, out_atts)
}

/// The staged file an export record's `path` names, if it is a safe one:
/// relative, plain components only, under `attachments/`, every directory
/// on the way a real directory and the file itself a regular file (no
/// symlinks, FIFOs, ...). The export comes from another machine.
pub fn staged_file(stage: &Path, p: &str) -> Option<PathBuf> {
    staged_file_under(stage, "attachments", p)
}

/// [`staged_file`] for files under the stage's `top` directory.
pub fn staged_file_under(stage: &Path, top: &str, p: &str) -> Option<PathBuf> {
    use std::path::Component;
    let comps: Vec<Component> = Path::new(p).components().collect();
    if comps.len() < 2 || comps[0] != Component::Normal(top.as_ref()) || !comps.iter().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    let mut cur = stage.to_path_buf();
    for (i, c) in comps.iter().enumerate() {
        cur.push(c);
        let m = cur.symlink_metadata().ok()?;
        let ok = if i + 1 == comps.len() { m.is_file() } else { m.is_dir() };
        if !ok {
            return None;
        }
    }
    Some(cur)
}

pub fn convert(records: Vec<Record>, stage: &Path, opts: &ConvertOptions) -> Converted {
    let mut out = Converted::default();
    let mut meta = MetaRec::default();
    let mut chat_recs: BTreeMap<i64, ChatRec> = BTreeMap::new();
    let mut msgs = vec![];
    for r in records {
        match r {
            Record::Meta(m) => meta = m,
            Record::Chat(c) => {
                chat_recs.insert(c.rowid, c);
            }
            Record::Message(m) => msgs.push(*m),
        }
    }
    msgs.sort_by_key(|m| m.date);
    out.stats.text_decode_failures = meta.text_decode_failures;
    // Our handles: the configured ones plus every address the Mac received
    // or sent on (`destination_caller_id` is always one of ours).
    let me = Me::new(
        &opts.my_handles,
        msgs.iter().filter_map(|m| m.destination_caller_id.as_deref()).filter(|d| !d.trim().is_empty()).map(|d| Handle::normalize(strip_tags(d))),
    );
    let default_sender = opts.default_sender.clone().map(|s| Handle::normalize(&s));
    // Our handle in each chat: the newest address the Mac used there.
    let mut our_in: BTreeMap<i64, String> = BTreeMap::new();
    for m in &msgs {
        if let Some(d) = m.destination_caller_id.as_deref().filter(|d| !d.trim().is_empty()) {
            for c in &m.chats {
                our_in.insert(*c, Handle::normalize(strip_tags(d)));
            }
        }
    }
    let chats: BTreeMap<i64, Chat> =
        chat_recs.iter().map(|(k, c)| (*k, {
            let dest = our_in.get(k).map(String::as_str);
            chat_for(c, &me, dest, dest.or(default_sender.as_deref()))
        })).collect();

    for m in msgs {
        let Some(chat) = m.chats.iter().find_map(|c| chats.get(c)) else {
            out.stats.skipped_no_chat += 1;
            continue;
        };
        // The daemon refuses a batch with any invalid id or handle.
        if !chat.id.is_valid() || !chat.participants.iter().all(|h| Handle::is_valid_uri(&h.uri)) {
            out.stats.skipped_invalid += 1;
            continue;
        }
        let sms = m.service.as_deref().is_some_and(|s| s != "iMessage");
        if opts.imessage_only && sms {
            out.stats.skipped_sms += 1;
            continue;
        }
        let at = apple_ms(m.date).unwrap_or(0);
        let sender = if m.is_from_me {
            m.destination_caller_id.as_deref().filter(|d| !d.trim().is_empty()).map(|d| Handle::new(strip_tags(d))).or_else(|| default_sender.as_deref().map(Handle::new))
        } else {
            m.handle.as_deref().filter(|h| !h.trim().is_empty()).map(peer_handle).or_else(|| (!chat.is_group).then(|| Handle::new(&chat.id.0)))
        }
        .unwrap_or_else(|| Handle { uri: String::new(), display: None, profile: None });
        if !Handle::is_valid_uri(&sender.uri) {
            out.stats.skipped_invalid += 1;
            continue;
        }

        if m.associated_type != 0 {
            match (reaction_kind(m.associated_type, m.associated_emoji.as_deref()), m.associated_guid.as_deref().and_then(reaction_target)) {
                (Some((kind, added)), Some(target)) => {
                    out.reactions.push(ImportReaction { message_id: target, reaction: Reaction { kind, sender, added }, at });
                    out.stats.reactions += 1;
                }
                _ => out.stats.skipped_other += 1,
            }
            continue;
        }
        if m.item_type != 0 {
            out.stats.skipped_system += 1;
            continue;
        }

        let id = MessageId::new(&m.guid);
        let unsent = m.date_retracted > 0;
        let mut atts = vec![];
        for a in m.attachments.iter().filter(|a| !a.hidden && a.missing.as_deref() != Some("hidden")) {
            let filename = a.transfer_name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "attachment".into());
            let mime = a.mime.clone().filter(|m| !m.is_empty()).unwrap_or_else(|| guess_mime(&filename));
            let source = a.path.as_deref().and_then(|p| staged_file(stage, p));
            match (&source, a.missing.as_deref()) {
                (Some(_), _) => out.stats.attachments_with_file += 1,
                (None, Some("too_large")) => out.stats.attachments_too_large += 1,
                (None, _) => out.stats.attachments_not_on_mac += 1,
            }
            let placeholder = AttachmentId(String::new());
            atts.push((
                MessagePart::Attachment { attachment_id: placeholder.clone(), mime: mime.clone(), filename: filename.clone(), size: a.total_bytes },
                ImportAttachment {
                    id: placeholder,
                    mime,
                    filename,
                    size: a.total_bytes,
                    source_path: source.map(|p| p.to_string_lossy().into_owned()),
                },
            ));
        }
        let (mut parts, attachments) =
            if unsent { (vec![], vec![]) } else { build_parts(m.text.as_deref().unwrap_or(""), atts, &id) };
        // A link preview: the daemon decodes the archive (rustpush's keyed
        // archive reader lives there) into a `link` part with its images.
        let link = match (m.balloon.as_deref(), m.payload_data.as_deref()) {
            (Some(URL_BALLOON), Some(payload)) if !unsent && !payload.trim().is_empty() => {
                out.stats.link_previews += 1;
                Some(ImportLink {
                    payload: payload.to_string(),
                    images: m
                        .attachments
                        .iter()
                        .filter(|a| a.link_file)
                        .map(|a| a.path.as_deref().and_then(|p| staged_file(stage, p)).map(|p| p.to_string_lossy().into_owned()))
                        .collect(),
                })
            }
            _ => None,
        };
        if parts.is_empty() && !unsent && link.is_none() {
            if m.balloon.is_some() {
                parts.push(MessagePart::Unsupported { description: "app message".into() });
            } else {
                out.stats.skipped_other += 1;
                continue;
            }
        }
        let text = {
            let t: String = parts.iter().filter_map(|p| if let MessagePart::Text { text } = p { Some(text.as_str()) } else { None }).collect();
            (!t.trim().is_empty()).then_some(t)
        };
        let delivery = if !m.is_from_me {
            DeliveryState::Delivered
        } else if m.error != 0 {
            DeliveryState::Failed
        } else if m.date_read > 0 {
            DeliveryState::Read
        } else if m.date_delivered > 0 || m.is_delivered {
            DeliveryState::Delivered
        } else {
            DeliveryState::Sent
        };
        let message = Message {
            id,
            chat_id: chat.id.clone(),
            sender,
            sent_at: at,
            text,
            parts,
            subject: m.subject.clone().filter(|s| !s.trim().is_empty()),
            reply_to: m.thread_originator_guid.as_deref().filter(|g| !g.is_empty()).map(MessageId::new),
            is_from_me: m.is_from_me,
            delivery,
            edited: m.date_edited > 0 && !unsent,
            unsent,
            reactions: vec![],
            effect: m.effect.clone().filter(|e| !e.is_empty()),
            verified: true,
            service: if sms { Service::Sms } else { Service::Imessage },
        };
        // Several Mac chats can map to one id (SMS + iMessage with the same
        // number; group rows sharing a group GUID after membership
        // changes): the newest message's chat wins, like live snapshots.
        let c = out.chats.entry(chat.id.clone()).or_insert_with(|| chat.clone());
        c.participants = chat.participants.clone();
        // Dates the snapshot, so the daemon can tell it from newer live state.
        c.last_message_at = Some(c.last_message_at.unwrap_or(0).max(at));
        if chat.display_name.is_some() {
            c.display_name = chat.display_name.clone();
        }
        out.messages.push(ImportMessage { message, attachments, link });
    }
    out.messages.sort_by_key(|m| m.message.sent_at);
    out.reactions.sort_by_key(|r| r.at);
    out.stats.chats = out.chats.len();
    out.stats.messages = out.messages.len();
    out
}

/// Messages in batches of [`BATCH_MESSAGES`], each with the chats it
/// references; then the reactions in batches of [`BATCH_REACTIONS`].
pub fn batches(c: &Converted) -> Vec<ImportBatch> {
    let mut out = vec![];
    for chunk in c.messages.chunks(BATCH_MESSAGES) {
        let ids: BTreeSet<&ChatId> = chunk.iter().map(|m| &m.message.chat_id).collect();
        out.push(ImportBatch { chats: ids.into_iter().filter_map(|id| c.chats.get(id).cloned()).collect(), messages: chunk.to_vec(), reactions: vec![] });
    }
    for chunk in c.reactions.chunks(BATCH_REACTIONS) {
        out.push(ImportBatch { chats: vec![], messages: vec![], reactions: chunk.to_vec() });
    }
    out
}

// ---------------------------------------------------------------------------
// running it
// ---------------------------------------------------------------------------

/// Unix seconds to export from: `--since` (YYYY-MM-DD local midnight, or
/// RFC 3339) or now minus `--days`.
pub fn since_unix(args: &ImportArgs, now: chrono::DateTime<chrono::Local>) -> Result<i64, CliError> {
    use chrono::TimeZone;
    match args.since.as_deref().map(str::trim) {
        Some(s) => {
            if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                let t = d.and_hms_opt(0, 0, 0).expect("midnight");
                return chrono::Local.from_local_datetime(&t).earliest().map(|t| t.timestamp()).ok_or_else(|| CliError::usage(format!("--since {s}: no such local time")));
            }
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|t| t.timestamp())
                .map_err(|_| CliError::usage(format!("--since {s:?}: expected YYYY-MM-DD or an RFC 3339 time")))
        }
        None => Ok(now.timestamp() - i64::from(args.days) * 86_400),
    }
}

/// `ssh [-F ~/.ssh/config] -o BatchMode=yes -o LogLevel=ERROR <host> python3 - ...`
pub fn ssh_argv(host: &str, since: i64, max_mb: u64, home: Option<&Path>) -> Vec<String> {
    ssh_python_argv(host, home, &["--since".into(), since.to_string(), "--max-attachment-mb".into(), max_mb.to_string()])
}

/// `ssh [-F ~/.ssh/config] -o BatchMode=yes -o LogLevel=ERROR <host> python3 - <script args>`
pub fn ssh_python_argv(host: &str, home: Option<&Path>, script_args: &[String]) -> Vec<String> {
    let mut v = vec!["ssh".to_string()];
    if let Some(cfg) = home.map(|h| h.join(".ssh/config")).filter(|p| p.is_file()) {
        v.push("-F".into());
        v.push(cfg.to_string_lossy().into_owned());
    }
    for o in ["BatchMode=yes", "LogLevel=ERROR"] {
        v.push("-o".into());
        v.push(o.into());
    }
    v.extend([host.to_string(), "python3".into(), "-".into()]);
    v.extend(script_args.iter().cloned());
    v
}

pub(crate) fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join(".cache"))
}

pub(crate) fn private_dir(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(p)
}

/// Run the exporter on the Mac and unpack its tar into `stage`.
fn fetch(args: &ImportArgs, since: i64, stage: &Path) -> anyhow::Result<()> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let argv = ssh_argv(&args.host, since, args.max_attachment_mb, home.as_deref());
    fetch_tar(&argv, EXPORT_SCRIPT.to_string(), &args.host, args.max_total_mb, stage)
}

/// Run `argv` (an ssh command running `python3 -`), feed it `script` on
/// stdin and unpack the tar it prints into `stage` (`tar -x
/// --no-same-owner --no-same-permissions`), aborting past `max_total_mb`.
pub(crate) fn fetch_tar(argv: &[String], script: String, host: &str, max_total_mb: u64, stage: &Path) -> anyhow::Result<()> {
    let mut ssh = Process::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot run ssh: {e}"))?;
    let mut stdin = ssh.stdin.take().expect("piped");
    let writer = std::thread::spawn(move || stdin.write_all(script.as_bytes()));
    let tar = Process::new("tar")
        .args(["-x", "-f", "-", "--no-same-owner", "--no-same-permissions", "-C"])
        .arg(stage)
        .stdin(Stdio::piped())
        .spawn();
    let mut tar = match tar {
        Ok(t) => t,
        Err(e) => {
            let _ = ssh.kill();
            let _ = ssh.wait();
            anyhow::bail!("cannot run tar: {e}");
        }
    };
    // Through us, so the total is bounded.
    let max = max_total_mb.saturating_mul(1 << 20);
    let mut from = ssh.stdout.take().expect("piped");
    let copied = {
        let mut to = tar.stdin.take().expect("piped");
        copy_capped(&mut from, &mut to, max)
        // `to` dropped: tar sees EOF
    };
    let over = matches!(&copied, Err(e) if e.kind() == std::io::ErrorKind::FileTooLarge);
    if copied.is_err() {
        let _ = ssh.kill();
    }
    drop(from);
    let tar = tar.wait();
    let ssh_status = ssh.wait()?;
    let _ = writer.join();
    if over {
        anyhow::bail!("the export is larger than --max-total-mb {max_total_mb} MB; nothing changed");
    }
    let tar = tar.map_err(|e| anyhow::anyhow!("cannot run tar: {e}"))?;
    if !ssh_status.success() {
        anyhow::bail!("the export on {host} failed ({ssh_status}); see the messages above");
    }
    if !tar.success() {
        anyhow::bail!("unpacking the export failed ({tar})");
    }
    Ok(())
}

/// Copy `from` to `to` until EOF; `ErrorKind::FileTooLarge` once more than
/// `max` bytes came in (nothing past the limit is written). Returns the
/// byte count.
pub fn copy_capped(from: &mut impl std::io::Read, to: &mut impl Write, max: u64) -> std::io::Result<u64> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        total += n as u64;
        if total > max {
            return Err(std::io::Error::new(std::io::ErrorKind::FileTooLarge, format!("more than {max} bytes")));
        }
        to.write_all(&buf[..n])?;
    }
}

fn read_records(stage: &Path) -> anyhow::Result<Vec<Record>> {
    let path = stage.join("export.jsonl");
    let f = std::fs::File::open(&path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    let mut out = vec![];
    for (i, line) in std::io::BufReader::new(f).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(&line).map_err(|e| anyhow::anyhow!("export.jsonl line {}: {e}", i + 1))?);
    }
    Ok(out)
}

#[derive(Debug, Serialize)]
struct Summary<'a> {
    host: &'a str,
    since: i64,
    dry_run: bool,
    export: &'a Stats,
    #[serde(skip_serializing_if = "Option::is_none")]
    imported: Option<ImportResult>,
    /// Dry run: the chats the import would write, with message counts.
    #[serde(skip_serializing_if = "Option::is_none")]
    chats: Option<BTreeMap<&'a str, usize>>,
}

/// Messages per chat id.
fn chat_counts(c: &Converted) -> BTreeMap<&str, usize> {
    let mut out: BTreeMap<&str, usize> = c.chats.keys().map(|k| (k.as_str(), 0)).collect();
    for m in &c.messages {
        *out.entry(m.message.chat_id.as_str()).or_default() += 1;
    }
    out
}

fn print_stats(s: &Stats) {
    println!("chats:        {}", s.chats);
    println!("messages:     {}", s.messages);
    println!("reactions:    {}", s.reactions);
    println!(
        "attachments:  {} with file, {} not on the Mac, {} over the size limit",
        s.attachments_with_file, s.attachments_not_on_mac, s.attachments_too_large
    );
    println!(
        "skipped:      {} system, {} without chat, {} SMS/RCS, {} without a valid address, {} other",
        s.skipped_system, s.skipped_no_chat, s.skipped_sms, s.skipped_invalid, s.skipped_other
    );
    println!("link previews: {}", s.link_previews);
    println!("text decoding failures: {}", s.text_decode_failures);
}

pub async fn run(args: ImportArgs, ctx: &Ctx) -> anyhow::Result<u8> {
    if args.host.trim().is_empty() || args.host.starts_with('-') {
        return Err(CliError::usage(format!("invalid host {:?}", args.host)).into());
    }
    let since = since_unix(&args, chrono::Local::now())?;
    // Connect first: no point copying the history if the daemon is down.
    let client = if args.dry_run { None } else { Some(Client::connect(&ctx.bus).await?) };
    let mut opts = ConvertOptions { imessage_only: args.imessage_only, ..Default::default() };
    if let Some(c) = &client {
        let st = c.status().await?.value;
        opts.my_handles = st.registration.handles.iter().map(|h| h.uri.clone()).chain(st.registration.other_handles.iter().cloned()).collect();
        opts.default_sender = st.registration.sender.map(|h| h.uri).or_else(|| opts.my_handles.first().cloned());
    }

    let run_id = format!("{}-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"), std::process::id());
    let stage = cache_dir().join("imsg/import").join(run_id);
    private_dir(&stage).map_err(|e| anyhow::anyhow!("{}: {e}", stage.display()))?;
    if !ctx.quiet && !ctx.json {
        eprintln!("exporting from {} (since {}) into {} ...", args.host, chrono::DateTime::from_timestamp(since, 0).map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default(), stage.display());
    }
    let fetched = tokio::task::block_in_place(|| fetch(&args, since, &stage).and_then(|_| read_records(&stage)));
    let records = match fetched {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(e);
        }
    };
    let converted = convert(records, &stage, &opts);

    let Some(client) = client else {
        let _ = std::fs::remove_dir_all(&stage);
        if ctx.json {
            let chats = Some(chat_counts(&converted));
            println!("{}", serde_json::to_string(&Summary { host: &args.host, since, dry_run: true, export: &converted.stats, imported: None, chats })?);
        } else {
            println!("dry run: nothing imported");
            print_stats(&converted.stats);
            if !ctx.quiet {
                println!("chats it would write:");
                for (id, n) in chat_counts(&converted) {
                    println!("  {id}  {n} message(s)");
                }
            }
        }
        return Ok(exit::OK);
    };

    let batches = batches(&converted);
    let mut total = ImportResult::default();
    for (i, b) in batches.iter().enumerate() {
        if !ctx.quiet && !ctx.json {
            eprint!("\rimporting batch {}/{} ...", i + 1, batches.len());
        }
        let json = serde_json::to_string(b)?;
        match client.import_messages(&json).await {
            Ok(r) => total.add(&r),
            Err(e) => {
                if !ctx.quiet && !ctx.json {
                    eprintln!();
                }
                return Err(anyhow::anyhow!("{e} (batch {}/{}; files kept in {}; running the import again is safe)", i + 1, batches.len(), stage.display()));
            }
        }
    }
    if !ctx.quiet && !ctx.json && !batches.is_empty() {
        eprintln!();
    }
    let _ = std::fs::remove_dir_all(&stage);
    if ctx.json {
        println!("{}", serde_json::to_string(&Summary { host: &args.host, since, dry_run: false, export: &converted.stats, imported: Some(total), chats: None })?);
    } else if !ctx.quiet {
        print_stats(&converted.stats);
        println!(
            "imported:     {} new message(s), {} already present, {} new chat(s), {} attachment file(s) ({} without file), {} reaction(s) ({} for messages not in the store)",
            total.inserted,
            total.skipped_existing,
            total.chats_created,
            total.attachments,
            total.attachments_without_file,
            total.reactions,
            total.reactions_orphaned
        );
    }
    Ok(exit::OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat_rec(rowid: i64, style: i64, ident: &str, group_id: Option<&str>, participants: &[&str]) -> Record {
        Record::Chat(ChatRec {
            rowid,
            chat_identifier: ident.into(),
            style,
            display_name: None,
            group_id: group_id.map(String::from),
            participants: participants.iter().map(|s| s.to_string()).collect(),
        })
    }

    /// 2026-05-09, in seconds after 2001.
    const BASE: i64 = 800_000_000;

    fn msg_rec(guid: &str, chat: i64, handle: Option<&str>, from_me: bool, secs_after_2001: i64, text: &str) -> MessageRec {
        MessageRec {
            guid: guid.into(),
            chats: vec![chat],
            handle: handle.map(String::from),
            is_from_me: from_me,
            date: (BASE + secs_after_2001) * 1_000_000_000,
            text: Some(text.into()),
            service: Some("iMessage".into()),
            destination_caller_id: Some("+12055550100".into()),
            ..Default::default()
        }
    }

    #[test]
    fn dates_and_reaction_parsing() {
        assert_eq!(apple_ms(0), None);
        assert_eq!(apple_ms(1_000_000_000_000), Some(978_308_200_000)); // 1000 s in ns
        assert_eq!(apple_ms(1), Some(978_307_201_000)); // 1 s, old seconds format
        assert_eq!(apple_ms(813_081_607_531_116_544), Some(1_791_388_807_531));
        assert_eq!(reaction_target("p:0/ABC-def").unwrap().0, "ABC-DEF");
        assert_eq!(reaction_target("bp:XYZ").unwrap().0, "XYZ");
        assert_eq!(reaction_target("p:1/"), None);
        assert_eq!(reaction_kind(2000, None), Some((ReactionKind::Love, true)));
        assert_eq!(reaction_kind(3003, None), Some((ReactionKind::Laugh, false)));
        assert_eq!(reaction_kind(2006, Some("🔥")), Some((ReactionKind::Emoji("🔥".into()), true)));
        assert_eq!(reaction_kind(2006, None), None);
        assert_eq!(reaction_kind(3, None), None);
        assert_eq!(reaction_kind(1000, None), None);
    }

    #[test]
    fn chats_messages_and_reactions_convert() {
        let stage = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(stage.path().join("attachments/A1")).unwrap();
        std::fs::write(stage.path().join("attachments/A1/IMG_1.HEIC"), b"x").unwrap();
        let mut photo = msg_rec("m2", 1, None, true, 20, "\u{FFFC}nice");
        photo.attachments = vec![
            AttachmentRec { mime: None, transfer_name: Some("IMG_1.HEIC".into()), total_bytes: 1, path: Some("attachments/A1/IMG_1.HEIC".into()), ..Default::default() },
            AttachmentRec { hidden: true, missing: Some("hidden".into()), ..Default::default() },
        ];
        photo.date_read = 5;
        let mut big = msg_rec("m5", 2, Some("bob@Example.com"), false, 25, "");
        big.text = Some("\u{FFFC}".into());
        big.attachments = vec![AttachmentRec { mime: Some("video/quicktime".into()), transfer_name: Some("v.mov".into()), missing: Some("too_large".into()), total_bytes: 9 << 20, ..Default::default() }];
        let mut tapback = msg_rec("r1", 2, Some("bob@example.com"), false, 30, "Loved “x”");
        tapback.associated_type = 2000;
        tapback.associated_guid = Some("p:0/m3".into());
        let mut mine = msg_rec("r2", 2, None, true, 31, "");
        mine.associated_type = 3001;
        mine.associated_guid = Some("p:0/m3".into());
        let mut rename = msg_rec("s1", 3, None, true, 40, "");
        rename.item_type = 2;
        let mut sms = msg_rec("m6", 1, Some("+15551234567"), false, 41, "via sms");
        sms.service = Some("SMS".into());
        let records = vec![
            Record::Meta(MetaRec { text_decode_failures: 2 }),
            chat_rec(1, 45, "+15551234567", Some("ignored-for-1to1"), &["+15551234567"]),
            chat_rec(2, 43, "chat123", Some("abcd-0001"), &["bob@example.com", "+12055550100", "carol@example.com"]),
            chat_rec(3, 45, "me@icloud.example", None, &["me@icloud.example"]),
            chat_rec(4, 43, "chat999", Some("ABCD-0001"), &["bob@example.com", "dave@example.com"]),
            Record::Message(Box::new(msg_rec("m1", 1, Some("+15551234567"), false, 10, "hello"))),
            Record::Message(Box::new(photo)),
            Record::Message(Box::new(msg_rec("m3", 2, Some("bob@example.com"), false, 21, "group hi"))),
            Record::Message(Box::new(msg_rec("m0", 4, Some("dave@example.com"), false, 1, "older membership"))),
            Record::Message(Box::new(msg_rec("m4", 3, None, true, 22, "note to self"))),
            Record::Message(Box::new(big)),
            Record::Message(Box::new(tapback)),
            Record::Message(Box::new(mine)),
            Record::Message(Box::new(rename)),
            Record::Message(Box::new(sms.clone())),
            Record::Message(Box::new(MessageRec { chats: vec![], ..msg_rec("orphan", 0, None, false, 50, "x") })),
        ];
        let opts = ConvertOptions { my_handles: vec!["mailto:me@icloud.example".into()], default_sender: None, imessage_only: false };
        let c = convert(records.clone(), stage.path(), &opts);

        let ids: Vec<&str> = c.chats.keys().map(|k| k.0.as_str()).collect();
        assert_eq!(ids, vec!["group:ABCD-0001", "mailto:me@icloud.example", "tel:+15551234567"]);
        let g = &c.chats[&ChatId::group("ABCD-0001")];
        assert!(g.is_group);
        assert_eq!(g.participants, vec![Handle::new("bob@example.com"), Handle::new("carol@example.com")], "our phone number is not a participant");
        assert_eq!(c.chats[&ChatId::direct("me@icloud.example")].participants, vec![Handle::new("me@icloud.example")]);
        // S6: each chat snapshot is dated by its newest imported message
        assert_eq!(g.last_message_at, apple_ms((BASE + 25) * 1_000_000_000));
        assert_eq!(c.chats[&ChatId::direct("+15551234567")].last_message_at, apple_ms((BASE + 41) * 1_000_000_000));

        let by_id = |id: &str| c.messages.iter().find(|m| m.message.id.0 == id.to_ascii_uppercase()).cloned();
        let m1 = by_id("m1").unwrap().message;
        assert_eq!((m1.chat_id.0.as_str(), m1.sender.uri.as_str(), m1.is_from_me, m1.delivery), ("tel:+15551234567", "tel:+15551234567", false, DeliveryState::Delivered));
        assert_eq!(m1.sent_at, apple_ms((BASE + 10) * 1_000_000_000).unwrap());
        assert_eq!(m1.sent_at, 1_778_307_210_000);
        let p = by_id("m2").unwrap();
        assert_eq!(p.message.sender.uri, "tel:+12055550100");
        assert_eq!(p.message.delivery, DeliveryState::Read);
        assert_eq!(p.message.text.as_deref(), Some("nice"));
        assert_eq!(p.attachments.len(), 1, "hidden link-preview payload dropped");
        assert_eq!(p.attachments[0].id.0, "M2:0");
        assert_eq!(p.attachments[0].mime, "image/heic");
        assert!(p.attachments[0].source_path.as_deref().unwrap().ends_with("attachments/A1/IMG_1.HEIC"));
        assert!(matches!(&p.message.parts[..], [MessagePart::Attachment { attachment_id, .. }, MessagePart::Text { text }] if attachment_id.0 == "M2:0" && text == "nice"));
        let b = by_id("m5").unwrap();
        assert_eq!(b.message.text, None);
        assert_eq!(b.attachments[0].source_path, None);
        assert_eq!(b.message.sender.uri, "mailto:bob@example.com");
        assert_eq!(by_id("m4").unwrap().message.chat_id.0, "mailto:me@icloud.example");
        assert!(by_id("s1").is_none() && by_id("orphan").is_none());
        assert_eq!(by_id("m6").expect("SMS kept by default").message.service, Service::Sms);
        assert_eq!(by_id("m5").unwrap().message.service, Service::Imessage);

        assert_eq!(c.reactions.len(), 2);
        assert_eq!(c.reactions[0].message_id.0, "M3");
        assert_eq!(c.reactions[0].reaction, Reaction { kind: ReactionKind::Love, sender: Handle::new("bob@example.com"), added: true });
        assert_eq!(c.reactions[1].reaction, Reaction { kind: ReactionKind::Like, sender: Handle::new("tel:+12055550100"), added: false });

        assert_eq!(
            c.stats,
            Stats {
                chats: 3,
                messages: 7,
                reactions: 2,
                attachments_with_file: 1,
                attachments_not_on_mac: 0,
                attachments_too_large: 1,
                text_decode_failures: 2,
                skipped_system: 1,
                skipped_no_chat: 1,
                skipped_sms: 0,
                skipped_other: 0,
                skipped_invalid: 0,
                link_previews: 0,
            }
        );
        // messages oldest first
        assert!(c.messages.windows(2).all(|w| w[0].message.sent_at <= w[1].message.sent_at));

        let c2 = convert(records, stage.path(), &ConvertOptions { imessage_only: true, ..opts });
        assert_eq!(c2.stats.skipped_sms, 1);
    }

    /// A URL balloon: the archive and its preview files go to the daemon
    /// (which decodes them); no "app message" placeholder, no attachment
    /// parts for the hidden files.
    #[test]
    fn link_previews_are_passed_on() {
        let stage = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(stage.path().join("attachments/L1")).unwrap();
        std::fs::write(stage.path().join("attachments/L1/x.pluginPayloadAttachment"), b"img").unwrap();
        let mut only = msg_rec("l1", 1, Some("bob@example.com"), false, 10, "");
        only.text = None;
        only.balloon = Some(URL_BALLOON.into());
        only.payload_data = Some("YnBsaXN0".into());
        only.attachments = vec![
            AttachmentRec { hidden: true, link_file: true, path: Some("attachments/L1/x.pluginPayloadAttachment".into()), ..Default::default() },
            AttachmentRec { hidden: true, link_file: true, missing: Some("not_downloaded".into()), ..Default::default() },
            AttachmentRec { hidden: true, link_file: true, path: Some("../escape".into()), ..Default::default() },
        ];
        let mut texted = msg_rec("l2", 1, Some("bob@example.com"), false, 11, "see https://example.com");
        texted.balloon = Some(URL_BALLOON.into());
        texted.payload_data = Some("YnBsaXN0".into());
        let mut no_payload = msg_rec("l3", 1, Some("bob@example.com"), false, 12, "");
        no_payload.text = None;
        no_payload.balloon = Some(URL_BALLOON.into());
        let records = vec![
            chat_rec(1, 45, "bob@example.com", None, &["bob@example.com"]),
            Record::Message(Box::new(only)),
            Record::Message(Box::new(texted)),
            Record::Message(Box::new(no_payload)),
        ];
        let c = convert(records, stage.path(), &ConvertOptions::default());
        assert_eq!(c.stats.link_previews, 2);
        let l1 = &c.messages[0];
        assert!(l1.message.parts.is_empty() && l1.attachments.is_empty(), "{:?}", l1.message.parts);
        let link = l1.link.as_ref().unwrap();
        assert_eq!(link.payload, "YnBsaXN0");
        assert_eq!(link.images.len(), 3);
        assert!(link.images[0].as_deref().unwrap().ends_with("attachments/L1/x.pluginPayloadAttachment"));
        assert_eq!((link.images[1].as_deref(), link.images[2].as_deref()), (None, None));
        let l2 = &c.messages[1];
        assert_eq!(l2.message.text.as_deref(), Some("see https://example.com"));
        assert!(l2.link.as_ref().unwrap().images.is_empty());
        // no archive: as before
        assert_eq!(c.messages[2].message.parts, vec![MessagePart::Unsupported { description: "app message".into() }]);
        assert!(c.messages[2].link.is_none());
        // the batch JSON carries it
        let j = serde_json::to_value(&batches(&c)[0]).unwrap();
        assert_eq!(j["messages"][0]["link"]["payload"], "YnBsaXN0");
        assert!(j["messages"][2].get("link").is_none());
    }

    #[test]
    fn batching_carries_the_chats() {
        let recs: Vec<Record> = std::iter::once(chat_rec(1, 45, "bob@example.com", None, &["bob@example.com"]))
            .chain(std::iter::once(chat_rec(2, 45, "carol@example.com", None, &["carol@example.com"])))
            .chain((0..450).map(|i| Record::Message(Box::new(msg_rec(&format!("m{i}"), if i < 300 { 1 } else { 2 }, Some("x@y.z"), false, i, "t")))))
            .collect();
        let c = convert(recs, Path::new("/nonexistent"), &ConvertOptions::default());
        let b = batches(&c);
        assert_eq!(b.iter().map(|b| b.messages.len()).collect::<Vec<_>>(), vec![200, 200, 50]);
        assert_eq!(b[0].chats.len(), 1);
        assert_eq!(b[1].chats.len(), 2);
        assert_eq!(b[2].chats[0].id.0, "mailto:carol@example.com");
    }

    #[test]
    fn since_and_ssh_args() {
        let now = chrono::Local::now();
        let a = ImportArgs { host: "mac.local".into(), days: 10, since: None, max_attachment_mb: 50, max_total_mb: 2048, imessage_only: false, dry_run: true };
        assert_eq!(since_unix(&a, now).unwrap(), now.timestamp() - 864_000);
        let a2 = ImportArgs { since: Some("2026-10-01T00:00:00Z".into()), ..a.clone() };
        assert_eq!(since_unix(&a2, now).unwrap(), 1_790_812_800);
        assert!(since_unix(&ImportArgs { since: Some("2026-10-01".into()), ..a.clone() }, now).is_ok());
        assert!(since_unix(&ImportArgs { since: Some("last week".into()), ..a.clone() }, now).is_err());
        let v = ssh_argv("mac.local", 123, 50, None);
        assert_eq!(v.join(" "), "ssh -o BatchMode=yes -o LogLevel=ERROR mac.local python3 - --since 123 --max-attachment-mb 50");
        assert!(EXPORT_SCRIPT.contains("def decode_attributed_body"));
    }

    #[test]
    fn same_number_variants() {
        assert!(same_number("tel:+12055550100", "tel:2055550100"));
        assert!(!same_number("tel:+12055550100", "tel:+12055550101"));
        assert!(!same_number("tel:22395", "tel:+122395"));
    }

    /// S4: `path` comes from the Mac; only regular files under
    /// `attachments/` inside the stage are used.
    #[test]
    fn staged_file_validation() {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"s").unwrap();
        let stage = tempfile::tempdir().unwrap();
        let st = stage.path();
        std::fs::create_dir_all(st.join("attachments/A1")).unwrap();
        std::fs::write(st.join("attachments/A1/ok.png"), b"x").unwrap();
        std::fs::write(st.join("export.jsonl"), b"").unwrap();
        symlink(outside.path().join("secret"), st.join("attachments/A1/link")).unwrap();
        symlink(outside.path(), st.join("attachments/D")).unwrap();
        std::fs::create_dir(st.join("attachments/A1/dir")).unwrap();
        let fifo = std::ffi::CString::new(st.join("attachments/A1/fifo").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

        assert_eq!(staged_file(st, "attachments/A1/ok.png"), Some(st.join("attachments/A1/ok.png")));
        assert_eq!(staged_file(st, "attachments/./A1/ok.png"), Some(st.join("attachments/A1/ok.png")));
        for bad in [
            "",
            "export.jsonl",
            "attachments",
            "/etc/passwd",
            "attachments/../export.jsonl",
            "attachments/A1/../../export.jsonl",
            "../attachments/A1/ok.png",
            "attachments/A1/link",
            "attachments/D/secret",
            "attachments/A1/dir",
            "attachments/A1/fifo",
            "attachments/A1/missing",
        ] {
            assert_eq!(staged_file(st, bad), None, "{bad}");
        }
        // convert() drops such paths (the attachment is kept without a file).
        let mut m = msg_rec("M-1", 1, Some("+12055550000"), false, 10, "\u{FFFC}");
        m.attachments = vec![AttachmentRec { path: Some("attachments/D/secret".into()), total_bytes: 1, ..Default::default() }];
        let c = convert(vec![chat_rec(1, 45, "+12055550000", None, &["+12055550000"]), Record::Message(Box::new(m))], st, &ConvertOptions::default());
        assert_eq!(c.messages[0].attachments[0].source_path, None);
        assert_eq!((c.stats.attachments_with_file, c.stats.attachments_not_on_mac), (0, 1));
    }

    #[test]
    fn copy_is_capped() {
        let mut out = Vec::new();
        assert_eq!(copy_capped(&mut &b"0123456789"[..], &mut out, 10).unwrap(), 10);
        assert_eq!(out, b"0123456789");
        let mut out = Vec::new();
        let e = copy_capped(&mut &b"0123456789"[..], &mut out, 9).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::FileTooLarge);
        assert!(out.len() <= 9);
    }

    /// S5: chat ids match what live iMessage produces.
    #[test]
    fn chat_ids_match_live() {
        let opts = ConvertOptions { my_handles: vec!["mailto:me@icloud.com".into()], default_sender: Some("mailto:me@icloud.com".into()), imessage_only: false };
        let mut m3 = msg_rec("g1", 3, Some("bob@example.com"), false, 3, "no guid group");
        m3.destination_caller_id = Some("me@icloud.com".into());
        let records = vec![
            // (b) a Mac "group" with one other person is the 1:1 chat
            chat_rec(1, 43, "chat1", Some("AAAA-1"), &["bob@example.com"]),
            Record::Message(Box::new(msg_rec("d1", 1, Some("bob@example.com"), false, 1, "hi"))),
            // (c) bare national numbers get +1
            chat_rec(2, 45, "2055551234", None, &["2055551234"]),
            Record::Message(Box::new(msg_rec("s1", 2, Some("2055551234"), false, 2, "sms"))),
            // (a) no group GUID: participants including our handle
            chat_rec(3, 43, "chat3", None, &["bob@example.com", "carol@example.com"]),
            Record::Message(Box::new(m3)),
            chat_rec(4, 45, "12055550000", None, &["1 (205) 555-0000"]),
            Record::Message(Box::new(msg_rec("s2", 4, Some("12055550000"), false, 4, "sms"))),
        ];
        let c = convert(records, Path::new("/nonexistent"), &opts);
        let id = |guid: &str| c.messages.iter().find(|m| m.message.id.0 == guid.to_ascii_uppercase()).unwrap().message.chat_id.0.clone();
        assert_eq!(id("d1"), "mailto:bob@example.com");
        assert!(!c.chats[&ChatId::direct("bob@example.com")].is_group);
        assert_eq!(id("s1"), "tel:+12055551234");
        assert_eq!(c.messages.iter().find(|m| m.message.id.0 == "S1").unwrap().message.sender.uri, "tel:+12055551234");
        assert_eq!(id("s2"), "tel:+12055550000");
        // the same id the daemon computes for a live message without a GUID
        let live = ChatId::group_by_participants(&["mailto:bob@example.com".into(), "mailto:carol@example.com".into(), "mailto:me@icloud.com".into()]);
        assert_eq!(id("g1"), live.0);

        assert_eq!(peer_handle("2055551234").uri, "tel:+12055551234");
        assert_eq!(peer_handle("(205) 555-1234").uri, "tel:+12055551234");
        assert_eq!(peer_handle("12055551234").uri, "tel:+12055551234");
        assert_eq!(peer_handle("+442071234567").uri, "tel:+442071234567");
        assert_eq!(peer_handle("1055551234").uri, Handle::new("1055551234").uri, "not a NANP area code");
        assert_eq!(peer_handle("07700900123").uri, Handle::new("07700900123").uri, "11 digits not starting with 1");
        assert_eq!(peer_handle("22395").uri, Handle::new("22395").uri, "short code");
        assert_eq!(peer_handle("Bob@Example.com").uri, "mailto:bob@example.com");
    }

    /// Mac SMS handles carry filter tags: stripped, short codes are
    /// `tel:<digits>`, and rows with no valid address are skipped (the
    /// daemon refuses invalid ids).
    #[test]
    fn sms_tags_and_short_codes() {
        for (raw, want) in [
            ("46339(smsft)", "tel:46339"),
            ("24516(smsfp)", "tel:24516"),
            ("57513(smsft_or)", "tel:57513"),
            ("787473(smsft_rm)", "tel:787473"),
            ("+12125550199(smsfp)", "tel:+12125550199"),
            ("+12542247929(smsft)(smsft)", "tel:+12542247929"),
            ("+12053108710(filtered)", "tel:+12053108710"),
            ("2055551234(smsft_fi)", "tel:+12055551234"),
            ("google@rbm.goog(smsft)(smsft)", "mailto:google@rbm.goog"),
            ("(205) 555-1234", "tel:+12055551234"),
        ] {
            assert_eq!(peer_handle(raw).uri, want, "{raw}");
            assert!(Handle::is_valid_uri(&peer_handle(raw).uri), "{raw}");
        }
        assert_eq!(strip_tags("(smsft)"), "(smsft)", "nothing left: not a tag");
        assert_eq!(strip_tags("+1555 (205)"), "+1555 (205)", "digits only: not a tag");

        let opts = ConvertOptions { my_handles: vec!["mailto:me@icloud.com".into()], default_sender: Some("mailto:me@icloud.com".into()), imessage_only: false };
        let sms = |guid: &str, chat: i64, handle: Option<&str>, from_me: bool, at: i64| {
            let mut m = msg_rec(guid, chat, handle, from_me, at, "code 1234");
            m.service = Some("SMS".into());
            Record::Message(Box::new(m))
        };
        let records = vec![
            chat_rec(1, 45, "46339(smsft)", None, &["46339(smsft)"]),
            sms("a1", 1, Some("46339(smsft)"), false, 1),
            // a second Mac chat for the same short code with another tag
            chat_rec(2, 45, "46339(smsft_rm)", None, &["46339(smsft_rm)"]),
            sms("a2", 2, Some("46339(smsft_rm)"), false, 2),
            // nobody listed: the chat with its identifier, not a note to self
            chat_rec(3, 45, "+18005550142(smsft)", None, &[]),
            sms("a3", 3, None, false, 3),
            sms("a4", 3, None, true, 4),
            // no valid address: skipped, not sent to the daemon
            chat_rec(4, 45, "mom", None, &["mom"]),
            sms("a5", 4, Some("mom"), false, 5),
            chat_rec(5, 45, "urn:biz:88d22123", None, &["urn:biz:88d22123"]),
            Record::Message(Box::new(msg_rec("a6", 5, Some("urn:biz:88d22123"), false, 6, "biz"))),
        ];
        let c = convert(records, Path::new("/nonexistent"), &opts);
        let ids: Vec<&str> = c.chats.keys().map(ChatId::as_str).collect();
        assert_eq!(ids, vec!["tel:+18005550142", "tel:46339"]);

        let get = |guid: &str| c.messages.iter().find(|m| m.message.id.0 == guid.to_ascii_uppercase()).unwrap().message.clone();
        assert_eq!((get("a1").chat_id.0.as_str(), get("a1").sender.uri.as_str()), ("tel:46339", "tel:46339"));
        assert_eq!(get("a2").chat_id.0, "tel:46339");
        assert_eq!((get("a3").chat_id.0.as_str(), get("a3").sender.uri.as_str()), ("tel:+18005550142", "tel:+18005550142"));
        assert!(get("a4").is_from_me);
        assert_eq!(c.chats[&ChatId::direct("tel:+18005550142")].participants, vec![Handle::new("tel:+18005550142")]);
        assert_eq!(c.stats.skipped_invalid, 2);
        for b in batches(&c) {
            assert!(b.chats.iter().all(|c| c.id.is_valid() && c.participants.iter().all(|h| Handle::is_valid_uri(&h.uri))));
            assert!(b.messages.iter().all(|m| m.message.chat_id.is_valid() && Handle::is_valid_uri(&m.message.sender.uri)));
        }

        // a Mac "group" row of only our own handles (Mac <-> Linux tests):
        // the note-to-self chat, as live, not group:<gid>; a group everyone
        // else left (nobody listed) stays a group
        let own_opts = ConvertOptions {
            my_handles: vec!["mailto:me@icloud.com".into(), "mailto:me@example.org".into(), "tel:+12055550100".into()],
            default_sender: Some("mailto:me@example.org".into()),
            imessage_only: false,
        };
        let own_records = vec![
            chat_rec(1, 43, "47f5563e", Some("8DE60804-1"), &["+12055550100", "me@icloud.com"]),
            Record::Message(Box::new(msg_rec("o1", 1, None, true, 1, "test"))),
            chat_rec(2, 43, "ac20d461", Some("B5E489CA-1"), &["me@example.org", "+12055550100"]),
            Record::Message(Box::new(msg_rec("o2", 2, Some("me@example.org"), false, 2, "test"))),
            chat_rec(3, 43, "left", Some("LEFT-1"), &[]),
            Record::Message(Box::new(msg_rec("o3", 3, None, true, 3, "anyone?"))),
        ];
        let own = convert(own_records, Path::new("/nonexistent"), &own_opts);
        let ids: Vec<&str> = own.chats.keys().map(ChatId::as_str).collect();
        // both rows list the number the Mac sent from: that is the handle
        assert_eq!(ids, vec!["group:LEFT-1", "tel:+12055550100"]);
        assert!(!own.chats[&ChatId::direct("tel:+12055550100")].is_group);

        // the note-to-self handle: the address the Mac used there, else the
        // first listed handle the daemon knows, in the daemon's form
        let pick = |participants: &[&str], dest: Option<&str>| {
            let mut m = msg_rec("p1", 1, None, true, 1, "x");
            m.destination_caller_id = dest.map(String::from);
            let recs = vec![chat_rec(1, 43, "pick", Some("PICK-1"), participants), Record::Message(Box::new(m))];
            convert(recs, Path::new("/nonexistent"), &own_opts).chats.into_keys().map(|c| c.0).collect::<Vec<_>>()
        };
        assert_eq!(pick(&["+12055550100", "me@icloud.com"], Some("me@icloud.com")), vec!["mailto:me@icloud.com"]);
        assert_eq!(pick(&["me@icloud.com"], Some("+12055550100")), vec!["mailto:me@icloud.com"], "a destination not listed");
        assert_eq!(pick(&["(205) 555-0100", "me@icloud.com"], None), vec!["tel:+12055550100"]);
        assert_eq!(pick(&["2055550100"], None), vec!["tel:+12055550100"]);
        // a handle only seen as a destination elsewhere comes after a known one
        let mut seen = msg_rec("p2", 2, None, true, 2, "y");
        seen.destination_caller_id = Some("old@example.net".into());
        let recs = vec![
            chat_rec(1, 43, "pick", Some("PICK-1"), &["old@example.net", "me@example.org"]),
            Record::Message(Box::new(MessageRec { destination_caller_id: None, ..msg_rec("p1", 1, None, true, 1, "x") })),
            chat_rec(2, 45, "old", None, &["bob@example.com"]),
            Record::Message(Box::new(seen)),
        ];
        let ids: Vec<String> = convert(recs, Path::new("/nonexistent"), &own_opts).chats.into_keys().map(|c| c.0).collect();
        assert_eq!(ids, vec!["mailto:bob@example.com", "mailto:me@example.org"]);
    }

    /// S7: the exporter's own Python tests (tools/test_mac_messages_export.py),
    /// skipped when python3 is not installed.
    #[test]
    fn exporter_python_tests() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("mac-messages-export.py");
        let test = dir.path().join("test_mac_messages_export.py");
        std::fs::write(&script, EXPORT_SCRIPT).unwrap();
        std::fs::write(&test, include_str!("../../../tools/test_mac_messages_export.py")).unwrap();
        let out = match Process::new("python3").args(["-I", "-B"]).arg(&test).arg(&script).output() {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return eprintln!("python3 not found; skipping the exporter tests"),
            Err(e) => panic!("cannot run python3: {e}"),
        };
        assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    }
}
