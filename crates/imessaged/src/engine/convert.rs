//! rustpush -> model conversion (and the few model -> rustpush helpers the
//! send path needs). Pure functions; no I/O, no network.

use imessage_common::model::{
    Attachment, AttachmentId, Chat, ChatId, DeliveryReceipt, DeliveryState, Handle, Message, MessageId, MessagePart,
    Reaction, ReactionKind, ReadReceipt, TypingEvent,
};
use rustpush::{
    Attachment as RpAttachment, AttachmentType, ConversationData, IndexedMessagePart, MMCSFile, Message as RpMessage, MessageInst,
    MessagePart as RpMessagePart, MessageParts, NormalMessage, ReactMessageType, Reaction as RpReaction,
    SetTranscriptBackgroundMessage, ShareProfileMessage, SharedPoster,
};

use super::{BackgroundRef, EngineEvent, IconRef, PosterTags, ShareRef};

fn is_mine(handle: &str, my_handles: &[String]) -> bool {
    let n = Handle::normalize(handle);
    my_handles.iter().any(|h| Handle::normalize(h) == n)
}

/// Participants other than us, normalized, de-duplicated, order preserved.
pub fn others(participants: &[String], my_handles: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in participants {
        let n = Handle::normalize(p);
        if !is_mine(&n, my_handles) && !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

/// The handle a note-to-self conversation is with: our own handle in the
/// participants (normally the only one), else the sender if it is ours.
/// `None` if the conversation has someone else in it, or if the sender is
/// someone else (a stranger listing only us is a 1:1 with that stranger,
/// never our note to self).
pub fn self_chat_handle(conv: &ConversationData, sender: Option<&str>, my_handles: &[String]) -> Option<String> {
    if !others(&conv.participants, my_handles).is_empty() {
        return None;
    }
    if sender.is_some_and(|s| !s.is_empty() && !is_mine(s, my_handles)) {
        return None;
    }
    conv.participants.iter().map(String::as_str).chain(sender).map(Handle::normalize).find(|h| !h.is_empty() && is_mine(h, my_handles))
}

/// See [`ChatId`] for the scheme. A group is a conversation with more than
/// one party besides us (`my_handles` = every handle of ours, including
/// `other_handles`); everything else is the 1:1 chat with the one other
/// party, however many of our own handles it lists and whatever its
/// `sender_guid` (rustpush's `is_group()` counts participants, so the
/// peer plus two of our addresses would look like a group, and Apple
/// reuses one group id across unrelated 1:1 chats). A note to self (only
/// our own handles in the conversation) is the 1:1 chat with our own
/// handle, the same id the send path uses; `sender` covers a conversation
/// that lists nobody.
pub fn chat_id_for(conv: &ConversationData, sender: Option<&str>, my_handles: &[String]) -> ChatId {
    let others = others(&conv.participants, my_handles);
    if others.len() > 1 {
        match &conv.sender_guid {
            Some(g) if !g.is_empty() => ChatId::group(g),
            _ => ChatId::group_by_participants(&conv.participants),
        }
    } else if let Some(o) = others.first() {
        ChatId::direct(o)
    } else {
        // Note to self; a conversation listing nobody (or only us, sent by
        // someone else) is the chat with the sender.
        let h = self_chat_handle(conv, sender, my_handles).or_else(|| sender.map(Handle::normalize));
        ChatId::direct(&h.unwrap_or_default())
    }
}

/// The group a membership change applies to, or `None` when it is not
/// about a group. A change sent to a two-party conversation (Bob adding
/// Carol to what was our 1:1) is about the group the union of the old and
/// new members forms, never the 1:1 chat with the one old party, whose
/// participant list must not change.
pub fn membership_chat_id(conv: &ConversationData, new_participants: &[String], sender: Option<&str>, my_handles: &[String]) -> Option<ChatId> {
    let id = chat_id_for(conv, sender, my_handles);
    if id.is_group() {
        return Some(id);
    }
    let mut union = conv.clone();
    for p in new_participants {
        if !union.participants.contains(p) {
            union.participants.push(p.clone());
        }
    }
    Some(chat_id_for(&union, sender, my_handles)).filter(ChatId::is_group)
}

/// `participants` excludes us, except in a note to self, where it is our
/// own handle (so it matches the chat the send path stores).
pub fn chat_for(conv: &ConversationData, sender: Option<&str>, my_handles: &[String], last_message_at: Option<u64>) -> Chat {
    let id = chat_id_for(conv, sender, my_handles);
    let mut participants: Vec<Handle> = others(&conv.participants, my_handles).iter().map(|p| Handle::new(p)).collect();
    if participants.is_empty() && !id.is_group() && !id.0.is_empty() {
        participants.push(Handle::new(&id.0));
    }
    Chat {
        is_group: id.is_group(),
        id,
        participants,
        display_name: conv.cv_name.clone().filter(|n| !n.is_empty()),
        last_message_at,
        unread_count: 0,
        background: None,
        icon_path: None,
    }
}

pub fn reaction_kind(r: &RpReaction) -> Option<ReactionKind> {
    Some(match r {
        RpReaction::Heart => ReactionKind::Love,
        RpReaction::Like => ReactionKind::Like,
        RpReaction::Dislike => ReactionKind::Dislike,
        RpReaction::Laugh => ReactionKind::Laugh,
        RpReaction::Emphasize => ReactionKind::Emphasize,
        RpReaction::Question => ReactionKind::Question,
        RpReaction::Emoji(e) => ReactionKind::Emoji(e.clone()),
        RpReaction::Sticker { .. } => return None,
    })
}

pub fn reaction_to_rustpush(k: &ReactionKind) -> RpReaction {
    match k {
        ReactionKind::Love => RpReaction::Heart,
        ReactionKind::Like => RpReaction::Like,
        ReactionKind::Dislike => RpReaction::Dislike,
        ReactionKind::Laugh => RpReaction::Laugh,
        ReactionKind::Emphasize => RpReaction::Emphasize,
        ReactionKind::Question => RpReaction::Question,
        ReactionKind::Emoji(e) => RpReaction::Emoji(e.clone()),
    }
}

/// Opaque download locator for an attachment: a JSON object
/// `{"v":1,"plist":"<XML plist of rustpush's Attachment>"}` (MMCS
/// url/owner/signature/key, or inline bytes). rustpush's binary fields only
/// round-trip through plist, hence the embedded plist.
pub fn attachment_locator(a: &RpAttachment) -> String {
    let mut xml = Vec::new();
    if plist::to_writer_xml(&mut xml, a).is_err() {
        return String::new();
    }
    serde_json::json!({ "v": 1, "plist": String::from_utf8_lossy(&xml) }).to_string()
}

#[derive(Debug, thiserror::Error)]
pub enum LocatorError {
    #[error("locator json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("locator plist: {0}")]
    Plist(#[from] plist::Error),
    #[error("unsupported locator version")]
    Version,
}

pub fn attachment_from_locator(s: &str) -> Result<RpAttachment, LocatorError> {
    #[derive(serde::Deserialize)]
    struct L {
        v: u32,
        plist: String,
    }
    let l: L = serde_json::from_str(s)?;
    if l.v != 1 {
        return Err(LocatorError::Version);
    }
    Ok(plist::from_bytes(l.plist.as_bytes())?)
}

/// Convert message parts. Returns (model parts, attachments, text).
pub fn convert_parts(message_id: &MessageId, parts: &MessageParts) -> (Vec<MessagePart>, Vec<Attachment>, Option<String>) {
    let mut out = Vec::new();
    let mut atts = Vec::new();
    let mut text = String::new();
    for (i, IndexedMessagePart { part, idx, .. }) in parts.0.iter().enumerate() {
        match part {
            RpMessagePart::Text(t, _) => {
                text.push_str(t);
                out.push(MessagePart::Text { text: t.clone() });
            }
            RpMessagePart::Mention(uri, t) => {
                text.push_str(t);
                out.push(MessagePart::Mention { handle: Handle::normalize(uri), text: t.clone() });
            }
            RpMessagePart::Attachment(a) => {
                let id = AttachmentId::for_part(message_id, idx.unwrap_or(i));
                let size = a.get_size() as u64;
                out.push(MessagePart::Attachment {
                    attachment_id: id.clone(),
                    mime: a.mime.clone(),
                    filename: a.name.clone(),
                    size,
                });
                atts.push(Attachment {
                    id,
                    message_id: message_id.clone(),
                    mime: a.mime.clone(),
                    filename: a.name.clone(),
                    size,
                    local_path: None,
                    mmcs: attachment_locator(a),
                });
            }
            RpMessagePart::Object(o) => out.push(MessagePart::Unsupported { description: format!("object {o}") }),
        }
    }
    let text = if text.trim().is_empty() { None } else { Some(text) };
    (out, atts, text)
}

fn normal_to_message(inst: &MessageInst, n: &NormalMessage, chat_id: ChatId, my_handles: &[String]) -> (Message, Vec<Attachment>) {
    let id = MessageId::new(&inst.id);
    let (mut parts, atts, text) = convert_parts(&id, &n.parts);
    if n.app.is_some() && parts.is_empty() {
        parts.push(MessagePart::Unsupported { description: "app message".into() });
    }
    let sender = inst.sender.as_deref().unwrap_or("");
    (
        Message {
            id,
            chat_id,
            sender: Handle::new(sender),
            sent_at: inst.sent_timestamp,
            text,
            parts,
            subject: n.subject.clone(),
            reply_to: n.reply_guid.as_ref().map(MessageId::new),
            is_from_me: is_mine(sender, my_handles),
            delivery: if is_mine(sender, my_handles) { DeliveryState::Sent } else { DeliveryState::Delivered },
            edited: false,
            unsent: false,
            reactions: vec![],
            effect: n.effect.clone(),
            verified: !inst.verification_failed,
        },
        atts,
    )
}

/// What to do with an incoming rustpush message.
#[derive(Debug, Default)]
pub struct Converted {
    pub events: Vec<EngineEvent>,
}

/// Convert one received [`MessageInst`] into engine events. Messages with no
/// user-visible meaning (profile sharing, cache invalidation, …) produce none.
pub fn convert_incoming(inst: &MessageInst, my_handles: &[String]) -> Converted {
    let sender_raw = inst.sender.as_deref().unwrap_or("");
    let sender = Handle::new(sender_raw);
    let chat_id = inst.conversation.as_ref().map(|c| chat_id_for(c, inst.sender.as_deref(), my_handles));
    let id = MessageId::new(&inst.id);
    let at = inst.sent_timestamp;
    let mut events = Vec::new();

    // A contact's name and photo share rides along on messages and
    // reactions; ours (from our other devices) is not interesting here.
    let profile_from = (!sender_raw.is_empty() && !is_mine(sender_raw, my_handles)).then(|| sender.clone());
    let shared = |p: Option<&ShareProfileMessage>| -> Option<EngineEvent> {
        let handle = profile_from.clone()?;
        Some(EngineEvent::ProfileShared { handle, share: p.and_then(share_ref), at })
    };

    match &inst.message {
        RpMessage::Message(n) => {
            if let Some(conv) = &inst.conversation {
                let chat = chat_for(conv, inst.sender.as_deref(), my_handles, Some(inst.sent_timestamp));
                let (message, attachments) = normal_to_message(inst, n, chat.id.clone(), my_handles);
                events.push(EngineEvent::MessageReceived { message, chat, attachments });
            }
            if let Some(p) = &n.embedded_profile {
                events.extend(shared(Some(p)));
            }
        }
        RpMessage::ShareProfile(p) => events.extend(shared(Some(p))),
        // `profile: None` = the sender turned sharing off.
        RpMessage::UpdateProfile(u) => events.extend(shared(u.profile.as_ref())),
        RpMessage::IconChange(i) => {
            if let Some(chat_id) = chat_id.filter(ChatId::is_group) {
                match &i.file {
                    None => events.push(EngineEvent::ChatIcon { chat_id, icon: None, by: sender, at }),
                    Some(f) => match icon_ref(&id, f) {
                        Some(icon) => events.push(EngineEvent::ChatIcon { chat_id, icon: Some(icon), by: sender, at }),
                        None => log::debug!("ignoring unusable group photo reference in {}", inst.id),
                    },
                }
            }
        }
        RpMessage::React(r) => {
            if let Some(p) = &r.embedded_profile {
                events.extend(shared(Some(p)));
            }
            if let (Some(chat_id), ReactMessageType::React { reaction, enable }) = (chat_id, &r.reaction)
                && let Some(kind) = reaction_kind(reaction)
            {
                events.push(EngineEvent::Reaction {
                    chat_id,
                    message_id: MessageId::new(&r.to_uuid),
                    reaction: Reaction { kind, sender, added: *enable },
                    at,
                });
            }
        }
        RpMessage::Delivered => events.push(EngineEvent::Delivered(DeliveryReceipt {
            chat_id,
            message_id: id,
            recipient: sender,
            delivered_at: inst.sent_timestamp,
        })),
        RpMessage::Read => events.push(EngineEvent::Read(ReadReceipt {
            chat_id,
            message_id: id,
            reader: sender,
            read_at: inst.sent_timestamp,
            by_me: is_mine(sender_raw, my_handles),
        })),
        RpMessage::Typing(typing, _app) => {
            if let Some(chat_id) = chat_id
                && !is_mine(sender_raw, my_handles)
            {
                events.push(EngineEvent::Typing(TypingEvent { chat_id, sender, typing: *typing }));
            }
        }
        RpMessage::Unsend(u) => {
            if let Some(chat_id) = chat_id {
                let from_me = is_mine(sender_raw, my_handles);
                events.push(EngineEvent::Unsent { chat_id, message_id: MessageId::new(&u.tuuid), part: u.edit_part, sender, from_me, at });
            }
        }
        RpMessage::Edit(e) => {
            if let Some(chat_id) = chat_id {
                let target = MessageId::new(&e.tuuid);
                let (new_parts, _atts, new_text) = convert_parts(&target, &e.new_parts);
                let from_me = is_mine(sender_raw, my_handles);
                events.push(EngineEvent::Edited { chat_id, message_id: target, part: e.edit_part, new_parts, new_text, sender, from_me, at });
            }
        }
        RpMessage::RenameMessage(r) => {
            if let Some(chat_id) = chat_id {
                let name = Some(r.new_name.clone()).filter(|n| !n.is_empty());
                events.push(EngineEvent::ChatRenamed { chat_id, name, by: sender, at });
            }
        }
        RpMessage::ChangeParticipants(c) => {
            if let Some(chat_id) =
                inst.conversation.as_ref().and_then(|conv| membership_chat_id(conv, &c.new_participants, inst.sender.as_deref(), my_handles))
            {
                let participants = others(&c.new_participants, my_handles).iter().map(|p| Handle::new(p)).collect();
                events.push(EngineEvent::ParticipantsChanged { chat_id, participants, by: sender, at });
            }
        }
        RpMessage::Error(e) => events.push(EngineEvent::MessageSent {
            id: MessageId::new(&e.for_uuid),
            chat_id,
            delivery: DeliveryState::Failed,
            error: Some(format!("{} ({})", e.status_str, e.status)),
            at,
        }),
        RpMessage::SetTranscriptBackground(b) => {
            log_background(inst, b);
            let background = match b {
                SetTranscriptBackgroundMessage::Remove { .. } => Ok(None),
                SetTranscriptBackgroundMessage::Set { .. } => background_ref(b).map(Some).ok_or(()),
            };
            match (chat_id, background) {
                (Some(chat_id), Ok(background)) => events.push(EngineEvent::ChatBackground { chat_id, background, by: sender, at }),
                // A malformed Set is dropped, never mistaken for a removal.
                (_, Err(())) => log::debug!("ignoring malformed background set in {}", inst.id),
                (None, _) => {}
            }
        }
        // Everything else (our own devices' profile-sharing settings and
        // recovery requests, SMS forwarding, recycle bin, cache
        // invalidation, ...) has no model equivalent yet.
        other => log::debug!("ignoring inbound message type c={} ({})", other.get_c(), inst.id),
    }
    Converted { events }
}

/// A usable share reference, or `None` for an empty one (no record or no
/// key: the sender stopped sharing).
pub fn share_ref(p: &ShareProfileMessage) -> Option<ShareRef> {
    if p.cloud_kit_record_key.trim().is_empty() || p.cloud_kit_decryption_record_key.is_empty() {
        return None;
    }
    Some(ShareRef {
        record_key: p.cloud_kit_record_key.clone(),
        decryption_key: p.cloud_kit_decryption_record_key.clone(),
        poster: p.poster.as_ref().map(|t| PosterTags {
            low_res_wallpaper_tag: t.low_res_wallpaper_tag.clone(),
            wallpaper_tag: t.wallpaper_tag.clone(),
            message_tag: t.message_tag.clone(),
        }),
    })
}

/// [`ShareRef`] back to rustpush, for the fetch. The poster tags are left
/// out on purpose: with them rustpush would also download the (large)
/// contact poster, which we do not show.
pub fn share_message(r: &ShareRef) -> ShareProfileMessage {
    ShareProfileMessage { cloud_kit_decryption_record_key: r.decryption_key.clone(), cloud_kit_record_key: r.record_key.clone(), poster: None }
}

/// The poster tags as rustpush has them (only kept for completeness).
pub fn poster_tags(t: &PosterTags) -> SharedPoster {
    SharedPoster { low_res_wallpaper_tag: t.low_res_wallpaper_tag.clone(), wallpaper_tag: t.wallpaper_tag.clone(), message_tag: t.message_tag.clone() }
}

/// The download reference for a group photo, wrapped as an attachment like
/// backgrounds; `None` if the MMCS reference cannot work (no object, not an
/// https URL, no key).
pub fn icon_ref(message_id: &MessageId, f: &MMCSFile) -> Option<IconRef> {
    let url_ok = url::Url::parse(&f.url).is_ok_and(|u| u.scheme() == "https" && u.host_str().is_some_and(|h| !h.is_empty()));
    if f.object.trim().is_empty() || !url_ok || f.key.is_empty() {
        return None;
    }
    let att = RpAttachment {
        a_type: AttachmentType::MMCS(f.clone()),
        part: 0,
        uti_type: String::new(),
        mime: String::new(),
        name: "GroupPhotoImage".into(),
        iris: false,
    };
    Some(IconRef { icon_id: message_id.0.clone(), size: f.size as u64, locator: attachment_locator(&att) })
}

/// The download reference for a background `Set`; `None` for `Remove`.
/// The MMCS file is wrapped in a rustpush attachment so the ordinary
/// attachment locator / download path handles it.
/// A `Set` whose key, signature, object id or URL would not make a usable
/// MMCS reference is `None` too (rustpush's `to_mmcs` would panic on it).
pub fn background_ref(b: &SetTranscriptBackgroundMessage) -> Option<BackgroundRef> {
    let SetTranscriptBackgroundMessage::Set { bid, payload_version, background_id, file_size, object_id, url, signature, key, .. } = b else {
        return None;
    };
    if let Err(why) = check_background_set(object_id, url, signature, key) {
        log::debug!("background {background_id}: unusable reference ({why})");
        return None;
    }
    let mmcs = b.to_mmcs()?;
    let att = RpAttachment {
        a_type: AttachmentType::MMCS(mmcs),
        part: 0,
        uti_type: String::new(),
        mime: String::new(),
        name: background_id.clone(),
        iris: false,
    };
    Some(BackgroundRef {
        background_id: background_id.clone(),
        payload_version: *payload_version,
        seq: *bid,
        size: *file_size as u64,
        locator: attachment_locator(&att),
    })
}

/// What `SetTranscriptBackgroundMessage::to_mmcs` needs to not panic and
/// the download to have a chance: base64 key (a leading flag byte + the
/// file key) and signature, an object id, an https URL with a host.
fn check_background_set(object_id: &str, url: &str, signature: &str, key: &str) -> Result<(), &'static str> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let key = b64.decode(key).map_err(|_| "key is not base64")?;
    if key.len() < 2 {
        return Err("key too short");
    }
    b64.decode(signature).map_err(|_| "signature is not base64")?;
    if object_id.trim().is_empty() {
        return Err("no object id");
    }
    match url::Url::parse(url) {
        Ok(u) if u.scheme() == "https" && u.host_str().is_some_and(|h| !h.is_empty()) => Ok(()),
        _ => Err("not an https url"),
    }
}

/// Host part of a URL (no path, query or credentials) for logging.
fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    host.rsplit('@').next().unwrap_or("")
}

/// Debug-log the non-secret fields of a background change (never the key
/// or signature).
fn log_background(inst: &MessageInst, b: &SetTranscriptBackgroundMessage) {
    let conv = inst.conversation.as_ref().map(|c| (c.participants.len(), c.sender_guid.clone()));
    match b {
        SetTranscriptBackgroundMessage::Remove { aid, bid, chat_id, remove } => {
            log::debug!("background remove: msg {} aid={aid} bid={bid} cid={chat_id:?} remove={remove} conversation={conv:?}", inst.id)
        }
        SetTranscriptBackgroundMessage::Set { aid, bid, chat_id, object_id, payload_version, background_id, url, file_size, .. } => log::debug!(
            "background set: msg {} aid={aid} bid={bid} cid={chat_id:?} object_id={object_id} payload_version={payload_version} \
             background_id={background_id} file_size={file_size} url_host={} conversation={conv:?}",
            inst.id,
            url_host(url)
        ),
    }
}

/// Whether we owe the sender a "Delivered" receipt for this message, per
/// rustpush's own rule (`MessageInst::send_delivered`, set from the wire) and
/// never for our own messages.
pub fn wants_delivered_receipt(inst: &MessageInst, my_handles: &[String]) -> bool {
    inst.send_delivered && inst.sender.as_deref().is_some_and(|s| !is_mine(s, my_handles))
}

/// Our handles as stored in IDS registrations, keeping only email handles
/// (Mac-class registration cannot carry phone numbers; see RESEARCH.md).
pub fn email_handles(handles: &[String]) -> Vec<String> {
    handles.iter().filter(|h| !h.starts_with("tel:")).cloned().collect()
}

/// Best-effort UTI for a MIME type (iMessage wants both).
pub fn uti_for_mime(mime: &str) -> &'static str {
    match mime.to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => "public.jpeg",
        "image/png" => "public.png",
        "image/gif" => "com.compuserve.gif",
        "image/heic" => "public.heic",
        "image/heif" => "public.heif",
        "image/webp" => "org.webmproject.webp",
        "video/mp4" => "public.mpeg-4",
        "video/quicktime" => "com.apple.quicktime-movie",
        "audio/mp4" | "audio/x-m4a" | "audio/m4a" => "com.apple.m4a-audio",
        "audio/mpeg" => "public.mp3",
        "audio/x-caf" => "com.apple.coreaudio-format",
        "text/plain" => "public.plain-text",
        "application/pdf" => "com.adobe.pdf",
        "application/zip" => "public.zip-archive",
        "text/vcard" | "text/x-vcard" => "public.vcard",
        _ => "public.data",
    }
}

/// Inline attachments are complete in the message; MMCS ones need a download.
pub fn is_inline(a: &RpAttachment) -> bool {
    matches!(a.a_type, AttachmentType::Inline(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustpush::{
        ChangeParticipantMessage, EditMessage, ErrorMessage, MMCSFile, MessageType, ReactMessage, RenameMessage,
        TextFormat, UnsendMessage,
    };

    const ME: &str = "mailto:me@example.com";
    const BOB: &str = "mailto:bob@example.com";
    const CAROL: &str = "mailto:carol@example.com";

    fn mine() -> Vec<String> {
        vec![ME.to_string()]
    }

    fn conv(p: &[&str], guid: Option<&str>, name: Option<&str>) -> ConversationData {
        ConversationData {
            participants: p.iter().map(|s| s.to_string()).collect(),
            cv_name: name.map(String::from),
            sender_guid: guid.map(String::from),
            after_guid: None,
        }
    }

    fn inst(sender: &str, conv: Option<ConversationData>, message: RpMessage) -> MessageInst {
        MessageInst {
            id: "1b2c3d4e-0000-4000-8000-000000000001".into(),
            sender: Some(sender.to_string()),
            conversation: conv,
            message,
            sent_timestamp: 1_700_000_000_123,
            target: None,
            send_delivered: true,
            verification_failed: false,
            certified_context: None,
        }
    }

    fn text_part(t: &str) -> IndexedMessagePart {
        IndexedMessagePart { part: RpMessagePart::Text(t.into(), TextFormat::default()), idx: None, ext: None }
    }

    fn chat_id_for_t(c: &ConversationData, mine: &[String]) -> ChatId {
        chat_id_for(c, None, mine)
    }

    fn chat_for_t(c: &ConversationData, mine: &[String], at: Option<u64>) -> Chat {
        chat_for(c, None, mine, at)
    }

    /// Note to self: a message from one of our devices whose only
    /// participant is our own handle lands in the chat with that handle
    /// (the id the send path uses), never in a chat with an empty id.
    #[test]
    fn note_to_self() {
        let me2 = "mailto:me2@example.com";
        let mine = vec![ME.to_string(), me2.to_string()];
        let n = || RpMessage::Message(NormalMessage::new("note".into(), MessageType::IMessage));
        for (participants, sender, want) in [
            (vec![me2], me2, me2),
            (vec!["mailto:ME2@Example.com"], me2, me2),
            (vec![], me2, me2),
            (vec![me2, me2], ME, me2),
        ] {
            let c = conv(&participants, Some("g-1"), None);
            let i = inst(sender, Some(c.clone()), n());
            let ev = convert_incoming(&i, &mine).events;
            let EngineEvent::MessageReceived { message, chat, .. } = &ev[0] else { panic!("{ev:?}") };
            assert_eq!(chat.id.0, want, "{participants:?} from {sender}");
            assert_eq!(message.chat_id, chat.id);
            assert!(!chat.is_group);
            assert_eq!(chat.participants, vec![Handle::new(want)]);
            assert!(message.is_from_me);
            assert_eq!(self_chat_handle(&c, Some(sender), &mine).as_deref(), Some(want));
        }
        // someone else in it: not a self chat
        assert_eq!(self_chat_handle(&conv(&[BOB, ME], None, None), Some(ME), &mine), None);
        // a peer is never "us": a peer's conversation listing nobody is the chat with the peer
        assert_eq!(self_chat_handle(&conv(&[], None, None), Some(BOB), &mine), None);
        assert_eq!(chat_id_for(&conv(&[], None, None), Some(BOB), &mine).0, BOB);
        // a stranger listing only us is the stranger's 1:1, not our note to self
        let c = conv(&[me2], Some("g-1"), None);
        assert_eq!(self_chat_handle(&c, Some(BOB), &mine), None);
        let i = inst(BOB, Some(c), n());
        let ev = convert_incoming(&i, &mine).events;
        let EngineEvent::MessageReceived { message, chat, .. } = &ev[0] else { panic!("{ev:?}") };
        assert_eq!(chat.id.0, BOB);
        assert_eq!(chat.participants, vec![Handle::new(BOB)]);
        assert!(!message.is_from_me);
    }

    #[test]
    fn chat_ids() {
        assert_eq!(chat_id_for_t(&conv(&[BOB, ME], Some("g1"), None), &mine()).0, BOB);
        assert_eq!(chat_id_for_t(&conv(&["mailto:Bob@Example.com"], None, None), &mine()).0, BOB);
        assert_eq!(chat_id_for_t(&conv(&[BOB, CAROL, ME], Some("abc-1"), None), &mine()).0, "group:ABC-1");
        assert_eq!(
            chat_id_for_t(&conv(&[CAROL, BOB, ME], None, None), &mine()).0,
            "group-p:mailto:bob@example.com,mailto:carol@example.com,mailto:me@example.com"
        );
        assert_eq!(chat_id_for_t(&conv(&[ME], None, None), &mine()).0, ME);
        // the peer plus two of our handles, with a gid: still the 1:1
        // (the Mac sending from one address with the other listed)
        let me2 = "mailto:me2@example.com";
        let both = vec![ME.to_string(), me2.to_string()];
        assert_eq!(chat_id_for_t(&conv(&[BOB, ME, me2], Some("B5E489CA-1"), None), &both), ChatId::direct(BOB));
        let c = chat_for_t(&conv(&[BOB, ME, me2], Some("B5E489CA-1"), None), &both, Some(5));
        assert!(!c.is_group);
        assert_eq!(c.participants, vec![Handle::new(BOB)]);
        // two peers and us, with a gid: a group
        assert_eq!(chat_id_for_t(&conv(&[BOB, CAROL, ME], Some("g-2"), None), &both), ChatId::group("g-2"));
        // receipts / typing / reactions of such a 1:1 land in the 1:1
        let t = convert_incoming(&inst(BOB, Some(conv(&[BOB, ME, me2], Some("G-3"), None)), RpMessage::Typing(true, None)), &both).events;
        assert_eq!(t, vec![EngineEvent::Typing(TypingEvent { chat_id: ChatId::direct(BOB), sender: Handle::new(BOB), typing: true })]);
        let r = convert_incoming(&inst(ME, Some(conv(&[BOB, ME, me2], Some("G-3"), None)), RpMessage::RenameMessage(RenameMessage { new_name: "x".into() })), &both).events;
        assert!(matches!(&r[..], [EngineEvent::ChatRenamed { chat_id, .. }] if *chat_id == ChatId::direct(BOB)));
        let c = chat_for_t(&conv(&[BOB, CAROL, ME], Some("g"), Some("Friends")), &mine(), Some(5));
        assert!(c.is_group);
        assert_eq!(c.participants, vec![Handle::new(BOB), Handle::new(CAROL)]);
        assert_eq!(c.display_name.as_deref(), Some("Friends"));
        assert_eq!(c.last_message_at, Some(5));
    }

    #[test]
    fn text_message_with_reply_and_attachment() {
        let mut n = NormalMessage::new("hello ".into(), MessageType::IMessage);
        n.parts.0.push(IndexedMessagePart {
            part: RpMessagePart::Mention(CAROL.into(), "Carol".into()),
            idx: None,
            ext: None,
        });
        n.parts.0.push(IndexedMessagePart {
            part: RpMessagePart::Attachment(RpAttachment {
                a_type: AttachmentType::MMCS(MMCSFile {
                    signature: vec![1, 2],
                    object: "obj".into(),
                    url: "https://mmcs.example/x".into(),
                    key: vec![9; 32],
                    size: 1234,
                }),
                part: 2,
                uti_type: "public.jpeg".into(),
                mime: "image/jpeg".into(),
                name: "IMG_1.jpg".into(),
                iris: false,
            }),
            idx: Some(2),
            ext: None,
        });
        n.reply_guid = Some("aaaaaaaa-0000-4000-8000-000000000000".into());
        n.subject = Some("subj".into());
        let i = inst(BOB, Some(conv(&[BOB, ME], None, None)), RpMessage::Message(n));
        let ev = convert_incoming(&i, &mine()).events;
        assert_eq!(ev.len(), 1);
        let EngineEvent::MessageReceived { message, chat, attachments } = &ev[0] else { panic!("{ev:?}") };
        assert_eq!(message.id.0, "1B2C3D4E-0000-4000-8000-000000000001");
        assert_eq!(message.chat_id.0, BOB);
        assert_eq!(chat.id, message.chat_id);
        assert_eq!(message.sender, Handle::new(BOB));
        assert!(!message.is_from_me);
        assert_eq!(message.sent_at, 1_700_000_000_123);
        assert_eq!(message.text.as_deref(), Some("hello Carol"));
        assert_eq!(message.subject.as_deref(), Some("subj"));
        assert_eq!(message.reply_to.as_ref().unwrap().0, "AAAAAAAA-0000-4000-8000-000000000000");
        assert_eq!(message.parts.len(), 3);
        assert!(matches!(&message.parts[1], MessagePart::Mention { handle, text } if handle == CAROL && text == "Carol"));
        let MessagePart::Attachment { attachment_id, mime, filename, size } = &message.parts[2] else { panic!() };
        assert_eq!(attachment_id.0, "1B2C3D4E-0000-4000-8000-000000000001:2");
        assert_eq!((mime.as_str(), filename.as_str(), *size), ("image/jpeg", "IMG_1.jpg", 1234));
        assert_eq!(attachments.len(), 1);
        let back = attachment_from_locator(&attachments[0].mmcs).unwrap();
        assert!(!is_inline(&back));
        let AttachmentType::MMCS(m) = back.a_type else { panic!() };
        assert_eq!((m.url.as_str(), m.size, m.key.len()), ("https://mmcs.example/x", 1234, 32));
        assert!(wants_delivered_receipt(&i, &mine()));
    }

    #[test]
    fn from_my_other_device() {
        let i = inst(ME, Some(conv(&[BOB, ME], None, None)), RpMessage::Message(NormalMessage::new("yo".into(), MessageType::IMessage)));
        let ev = convert_incoming(&i, &mine()).events;
        let EngineEvent::MessageReceived { message, .. } = &ev[0] else { panic!() };
        assert!(message.is_from_me);
        assert_eq!(message.chat_id.0, BOB);
        assert_eq!(message.delivery, DeliveryState::Sent);
        assert!(message.verified);
        assert!(!wants_delivered_receipt(&i, &mine()));
    }

    /// other_handles (here the iPhone's number, not registered on this
    /// device) are in the handle set: the phone's message to Bob is ours
    /// and in Bob's 1:1, not a group with Bob and the phone.
    #[test]
    fn from_my_phone_via_other_handles() {
        const PHONE: &str = "tel:+15551234567";
        let all = vec![ME.to_string(), PHONE.to_string()];
        let i = inst(PHONE, Some(conv(&[BOB, PHONE], Some("g-9"), None)), RpMessage::Message(NormalMessage::new("yo".into(), MessageType::IMessage)));
        let ev = convert_incoming(&i, &all).events;
        let EngineEvent::MessageReceived { message, chat, .. } = &ev[0] else { panic!("{ev:?}") };
        assert!(message.is_from_me);
        assert_eq!(message.chat_id.0, BOB);
        assert!(!chat.is_group);
        assert_eq!(chat.participants, vec![Handle::new(BOB)]);
        assert!(!wants_delivered_receipt(&i, &all));
        // its typing echo is dropped
        assert!(convert_incoming(&inst(PHONE, Some(conv(&[BOB, PHONE], None, None)), RpMessage::Typing(true, None)), &all).events.is_empty());
        // without the setting it looked like a group from someone else
        let ev = convert_incoming(&i, &mine()).events;
        let EngineEvent::MessageReceived { message, chat, .. } = &ev[0] else { panic!("{ev:?}") };
        assert!(!message.is_from_me);
        assert!(chat.is_group);
    }

    /// rustpush returns messages whose sender key could not be verified
    /// with `verification_failed = true`; that must reach the model.
    #[test]
    fn unverified_sender_is_flagged() {
        let mut i = inst(BOB, Some(conv(&[BOB, ME], None, None)), RpMessage::Message(NormalMessage::new("hi".into(), MessageType::IMessage)));
        let EngineEvent::MessageReceived { message, .. } = &convert_incoming(&i, &mine()).events[0] else { panic!() };
        assert!(message.verified);
        i.verification_failed = true;
        let EngineEvent::MessageReceived { message, .. } = &convert_incoming(&i, &mine()).events[0] else { panic!() };
        assert!(!message.verified);
        assert_eq!(message.sender, Handle::new(BOB));
        // serialized for D-Bus / JSON so 2b can store it
        assert!(serde_json::to_string(message).unwrap().contains("\"verified\":false"));
    }

    #[test]
    fn reactions() {
        let r = ReactMessage {
            to_uuid: "cccccccc-0000-4000-8000-000000000000".into(),
            to_part: Some(0),
            reaction: ReactMessageType::React { reaction: RpReaction::Emoji("🔥".into()), enable: false },
            to_text: "x".into(),
            embedded_profile: None,
        };
        let ev = convert_incoming(&inst(BOB, Some(conv(&[BOB, ME], None, None)), RpMessage::React(r)), &mine()).events;
        assert_eq!(
            ev,
            vec![EngineEvent::Reaction {
                chat_id: ChatId::direct(BOB),
                message_id: MessageId::new("cccccccc-0000-4000-8000-000000000000"),
                reaction: Reaction { kind: ReactionKind::Emoji("🔥".into()), sender: Handle::new(BOB), added: false },
                at: 1_700_000_000_123,
            }]
        );
        for k in [ReactionKind::Love, ReactionKind::Like, ReactionKind::Dislike, ReactionKind::Laugh, ReactionKind::Emphasize, ReactionKind::Question, ReactionKind::Emoji("a".into())] {
            assert_eq!(reaction_kind(&reaction_to_rustpush(&k)), Some(k));
        }
        assert_eq!(reaction_kind(&RpReaction::Sticker { spec: None, body: MessageParts(vec![]) }), None);
    }

    #[test]
    fn receipts_and_typing() {
        let d = convert_incoming(&inst(BOB, None, RpMessage::Delivered), &mine()).events;
        assert!(matches!(&d[0], EngineEvent::Delivered(r) if r.chat_id.is_none() && r.recipient == Handle::new(BOB) && r.message_id.0.starts_with("1B2C")));
        let r = convert_incoming(&inst(BOB, None, RpMessage::Read), &mine()).events;
        assert!(matches!(&r[0], EngineEvent::Read(r) if r.reader == Handle::new(BOB) && r.read_at == 1_700_000_000_123 && !r.by_me));
        // a read receipt from one of our own handles = we read it elsewhere
        let r = convert_incoming(&inst(ME, None, RpMessage::Read), &mine()).events;
        assert!(matches!(&r[0], EngineEvent::Read(r) if r.by_me));
        let t = convert_incoming(&inst(BOB, Some(conv(&[BOB, ME], None, None)), RpMessage::Typing(true, None)), &mine()).events;
        assert_eq!(t, vec![EngineEvent::Typing(TypingEvent { chat_id: ChatId::direct(BOB), sender: Handle::new(BOB), typing: true })]);
        // our own typing echoes are dropped
        assert!(convert_incoming(&inst(ME, Some(conv(&[BOB, ME], None, None)), RpMessage::Typing(false, None)), &mine()).events.is_empty());
    }

    #[test]
    fn edit_unsend_rename_participants_error() {
        let g = conv(&[BOB, CAROL, ME], Some("G-1"), None);
        let gid = ChatId::group("G-1");
        let e = convert_incoming(
            &inst(BOB, Some(g.clone()), RpMessage::Edit(EditMessage { tuuid: "dddd-1".into(), edit_part: 0, new_parts: MessageParts(vec![text_part("fixed")]) })),
            &mine(),
        )
        .events;
        assert!(matches!(&e[0], EngineEvent::Edited { chat_id, message_id, new_text: Some(t), from_me: false, at: 1_700_000_000_123, .. } if *chat_id == gid && message_id.0 == "DDDD-1" && t == "fixed"));
        let e = convert_incoming(
            &inst(ME, Some(g.clone()), RpMessage::Edit(EditMessage { tuuid: "dddd-2".into(), edit_part: 0, new_parts: MessageParts(vec![text_part("mine")]) })),
            &mine(),
        )
        .events;
        assert!(matches!(&e[0], EngineEvent::Edited { from_me: true, .. }), "edit from another of our devices");

        let u = convert_incoming(&inst(BOB, Some(g.clone()), RpMessage::Unsend(UnsendMessage { tuuid: "eeee-1".into(), edit_part: 1 })), &mine()).events;
        assert_eq!(u, vec![EngineEvent::Unsent { chat_id: gid.clone(), message_id: MessageId::new("eeee-1"), part: 1, sender: Handle::new(BOB), from_me: false, at: 1_700_000_000_123 }]);
        let u = convert_incoming(&inst(ME, Some(g.clone()), RpMessage::Unsend(UnsendMessage { tuuid: "eeee-2".into(), edit_part: 0 })), &mine()).events;
        assert!(matches!(&u[0], EngineEvent::Unsent { from_me: true, .. }), "unsend from another of our devices");

        let r = convert_incoming(&inst(BOB, Some(g.clone()), RpMessage::RenameMessage(RenameMessage { new_name: "Crew".into() })), &mine()).events;
        assert_eq!(r, vec![EngineEvent::ChatRenamed { chat_id: gid.clone(), name: Some("Crew".into()), by: Handle::new(BOB), at: 1_700_000_000_123 }]);

        let p = convert_incoming(
            &inst(BOB, Some(g.clone()), RpMessage::ChangeParticipants(ChangeParticipantMessage { new_participants: vec![BOB.into(), ME.into(), "mailto:dan@example.com".into()], group_version: 2 })),
            &mine(),
        )
        .events;
        assert_eq!(p, vec![EngineEvent::ParticipantsChanged { chat_id: gid.clone(), participants: vec![Handle::new(BOB), Handle::new("dan@example.com")], by: Handle::new(BOB), at: 1_700_000_000_123 }]);

        // a membership change sent to a two-party conversation is about the
        // group the old and new members form, never the 1:1 with Bob
        let dan = "mailto:dan@example.com";
        let add = |c: ConversationData, new: Vec<&str>| {
            let m = RpMessage::ChangeParticipants(ChangeParticipantMessage { new_participants: new.into_iter().map(String::from).collect(), group_version: 3 });
            convert_incoming(&inst(BOB, Some(c), m), &mine()).events
        };
        let p = add(conv(&[BOB, ME], Some("G-2"), None), vec![BOB, ME, dan]);
        assert_eq!(p, vec![EngineEvent::ParticipantsChanged { chat_id: ChatId::group("G-2"), participants: vec![Handle::new(BOB), Handle::new("dan@example.com")], by: Handle::new(BOB), at: 1_700_000_000_123 }]);
        // still two parties (or nobody else at all): not a group, no event
        assert!(add(conv(&[BOB, ME], Some("G-2"), None), vec![BOB, ME]).is_empty());
        assert!(add(conv(&[BOB, ME], None, None), vec![ME]).is_empty());
        // a group shrinking to one other party keeps the group id
        let p = add(g.clone(), vec![BOB, ME]);
        assert!(matches!(&p[0], EngineEvent::ParticipantsChanged { chat_id, participants, .. } if *chat_id == gid && *participants == vec![Handle::new(BOB)]));
        assert_eq!(membership_chat_id(&conv(&[BOB, ME], Some("G-2"), None), &[CAROL.into()], Some(BOB), &mine()), Some(ChatId::group("G-2")));
        assert_eq!(membership_chat_id(&conv(&[BOB, ME], Some("G-2"), None), &[BOB.into()], Some(BOB), &mine()), None);

        let er = convert_incoming(&inst(BOB, None, RpMessage::Error(ErrorMessage { for_uuid: "ffff-1".into(), status: 4, status_str: "nope".into() })), &mine()).events;
        assert!(matches!(&er[0], EngineEvent::MessageSent { id, delivery: DeliveryState::Failed, error: Some(e), .. } if id.0 == "FFFF-1" && e.contains("nope")));

        assert!(convert_incoming(&inst(BOB, None, RpMessage::PeerCacheInvalidate), &mine()).events.is_empty());
    }

    #[test]
    fn helpers() {
        assert_eq!(email_handles(&["tel:+1555".into(), ME.into()]), vec![ME.to_string()]);
        assert_eq!(uti_for_mime("IMAGE/PNG"), "public.png");
        assert_eq!(uti_for_mime("application/x-whatever"), "public.data");
        assert_eq!(others(&[BOB.into(), "mailto:BOB@example.com".into(), ME.into()], &mine()), vec![BOB.to_string()]);
    }

    #[test]
    fn transcript_background_set_and_remove() {
        let g = conv(&[BOB, CAROL, ME], Some("G-1"), None);
        let gid = ChatId::group("G-1");
        // key: one leading byte (dropped by rustpush) + the 32-byte file key.
        let key = "A".repeat(44); // base64 of 33 zero bytes
        let set = SetTranscriptBackgroundMessage::Set {
            aid: 1,
            bid: 7,
            chat_id: Some("any;+;chat1".into()),
            object_id: "OBJ".into(),
            payload_version: 1,
            background_id: "BG-1".into(),
            url: "https://user:pw@p42-content.icloud.com/x/y?z".into(),
            signature: "AQEB".repeat(7),
            key,
            file_size: 1234,
        };
        let ev = convert_incoming(&inst(BOB, Some(g.clone()), RpMessage::SetTranscriptBackground(set)), &mine()).events;
        let [EngineEvent::ChatBackground { chat_id, background: Some(b), by, at }] = &ev[..] else { panic!("{ev:?}") };
        assert_eq!((chat_id, by, *at), (&gid, &Handle::new(BOB), 1_700_000_000_123));
        assert_eq!((b.background_id.as_str(), b.payload_version, b.seq, b.size), ("BG-1", 1, 7, 1234));
        let att = attachment_from_locator(&b.locator).unwrap();
        assert!(matches!(&att.a_type, AttachmentType::MMCS(m) if m.object == "OBJ" && m.key.len() == 32));
        assert!(!format!("{b:?}").contains(&b.locator), "locator is redacted from Debug");
        assert_eq!(url_host("https://user:pw@p42-content.icloud.com/x/y?z"), "p42-content.icloud.com");

        // S1: malformed sets are dropped (no panic, and not a removal).
        let good_key = "A".repeat(44);
        let good_sig = "AQEB".repeat(7);
        let url = "https://p42-content.icloud.com/x";
        let malformed = |key: &str, signature: &str, object_id: &str, url: &str| SetTranscriptBackgroundMessage::Set {
            aid: 1,
            bid: 9,
            chat_id: None,
            object_id: object_id.into(),
            payload_version: 1,
            background_id: "BG-BAD".into(),
            url: url.into(),
            signature: signature.into(),
            key: key.into(),
            file_size: 10,
        };
        for (what, m) in [
            ("empty key", malformed("", &good_sig, "OBJ", url)),
            ("one-byte key", malformed("AA==", &good_sig, "OBJ", url)),
            ("bad base64 key", malformed("not base64!", &good_sig, "OBJ", url)),
            ("bad base64 signature", malformed(&good_key, "%%%", "OBJ", url)),
            ("empty object id", malformed(&good_key, &good_sig, "", url)),
            ("plain http", malformed(&good_key, &good_sig, "OBJ", "http://p42-content.icloud.com/x")),
            ("not a url", malformed(&good_key, &good_sig, "OBJ", "not a url")),
        ] {
            assert!(background_ref(&m).is_none(), "{what}");
            let ev = convert_incoming(&inst(BOB, Some(g.clone()), RpMessage::SetTranscriptBackground(m)), &mine()).events;
            assert!(ev.is_empty(), "{what}: {ev:?}");
        }
        assert!(background_ref(&malformed(&good_key, &good_sig, "OBJ", url)).is_some());

        let rm = SetTranscriptBackgroundMessage::Remove { aid: 1, bid: 8, chat_id: None, remove: true };
        let ev = convert_incoming(&inst(BOB, Some(g), RpMessage::SetTranscriptBackground(rm)), &mine()).events;
        assert!(matches!(&ev[..], [EngineEvent::ChatBackground { background: None, .. }]));
    }

    fn share(key: &str) -> ShareProfileMessage {
        ShareProfileMessage { cloud_kit_decryption_record_key: vec![7; 16], cloud_kit_record_key: key.into(), poster: None }
    }

    #[test]
    fn shared_profiles() {
        // embedded in a message: the message, then the share
        let mut n = NormalMessage::new("hi".into(), MessageType::IMessage);
        n.embedded_profile = Some(share("REC1"));
        let ev = convert_incoming(&inst(BOB, Some(conv(&[BOB, ME], None, None)), RpMessage::Message(n.clone())), &mine()).events;
        assert_eq!(ev.len(), 2);
        let EngineEvent::ProfileShared { handle, share: Some(r), at } = &ev[1] else { panic!("{ev:?}") };
        assert_eq!((handle, r.record_key.as_str(), r.decryption_key.len(), *at), (&Handle::new(BOB), "REC1", 16, 1_700_000_000_123));
        assert!(!format!("{ev:?}").contains("REC1"), "record key not in Debug");
        // ours (another of our devices): no share event
        let ev = convert_incoming(&inst(ME, Some(conv(&[BOB, ME], None, None)), RpMessage::Message(n)), &mine()).events;
        assert_eq!(ev.len(), 1);
        // embedded in a reaction
        let r = ReactMessage {
            to_uuid: "cccccccc-0000-4000-8000-000000000000".into(),
            to_part: Some(0),
            reaction: ReactMessageType::React { reaction: RpReaction::Heart, enable: true },
            to_text: String::new(),
            embedded_profile: Some(share("REC2")),
        };
        let ev = convert_incoming(&inst(BOB, Some(conv(&[BOB, ME], None, None)), RpMessage::React(r)), &mine()).events;
        assert!(matches!(&ev[..], [EngineEvent::ProfileShared { share: Some(s), .. }, EngineEvent::Reaction { .. }] if s.record_key == "REC2"));
        // standalone share, update, and a stop
        let ev = convert_incoming(&inst(BOB, None, RpMessage::ShareProfile(share("REC3"))), &mine()).events;
        assert!(matches!(&ev[..], [EngineEvent::ProfileShared { share: Some(s), .. }] if s.record_key == "REC3"));
        let up = |p| RpMessage::UpdateProfile(rustpush::UpdateProfileMessage { profile: p, share_contacts: false });
        let ev = convert_incoming(&inst(BOB, None, up(Some(share("REC4")))), &mine()).events;
        assert!(matches!(&ev[..], [EngineEvent::ProfileShared { share: Some(s), .. }] if s.record_key == "REC4"));
        let ev = convert_incoming(&inst(BOB, None, up(None)), &mine()).events;
        assert!(matches!(&ev[..], [EngineEvent::ProfileShared { share: None, .. }]));
        // an empty share is a stop too
        let ev = convert_incoming(&inst(BOB, None, RpMessage::ShareProfile(share(""))), &mine()).events;
        assert!(matches!(&ev[..], [EngineEvent::ProfileShared { share: None, .. }]));
        // round trip back to rustpush, without the poster
        let mut p = share("REC5");
        p.poster = Some(SharedPoster { low_res_wallpaper_tag: vec![1], wallpaper_tag: vec![2], message_tag: vec![3] });
        let r = share_ref(&p).unwrap();
        assert_eq!(r.poster.as_ref().map(|t| t.message_tag.clone()), Some(vec![3]));
        let back = share_message(&r);
        assert_eq!((back.cloud_kit_record_key.as_str(), back.poster.is_none()), ("REC5", true));
    }

    #[test]
    fn group_icons() {
        let g = conv(&[BOB, CAROL, ME], Some("g-icon"), None);
        let file = MMCSFile { signature: vec![1], object: "obj".into(), url: "https://mmcs.example/i".into(), key: vec![9; 33], size: 4321 };
        let set = |f: Option<MMCSFile>| RpMessage::IconChange(rustpush::IconChangeMessage { file: f, group_version: 1 });
        let ev = convert_incoming(&inst(BOB, Some(g.clone()), set(Some(file.clone()))), &mine()).events;
        let [EngineEvent::ChatIcon { chat_id, icon: Some(i), by, .. }] = &ev[..] else { panic!("{ev:?}") };
        assert_eq!((chat_id, by), (&ChatId::group("g-icon"), &Handle::new(BOB)));
        assert_eq!((i.icon_id.as_str(), i.size), ("1B2C3D4E-0000-4000-8000-000000000001", 4321));
        assert!(!format!("{i:?}").contains("plist"), "locator redacted");
        let AttachmentType::MMCS(m) = attachment_from_locator(&i.locator).unwrap().a_type else { panic!() };
        assert_eq!(m.object, "obj");
        let ev = convert_incoming(&inst(BOB, Some(g.clone()), set(None)), &mine()).events;
        assert!(matches!(&ev[..], [EngineEvent::ChatIcon { icon: None, .. }]));
        // unusable reference, or not a group: nothing
        let bad = MMCSFile { url: "http://x/".into(), ..file.clone() };
        assert!(convert_incoming(&inst(BOB, Some(g), set(Some(bad))), &mine()).events.is_empty());
        assert!(convert_incoming(&inst(BOB, Some(conv(&[BOB, ME], None, None)), set(Some(file))), &mine()).events.is_empty());
    }
}
