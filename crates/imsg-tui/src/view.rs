//! Pure presentation helpers: names, chat titles, previews, times and
//! sizes. No I/O, so everything here is unit-testable.

use chrono::{DateTime, Datelike, Local, TimeZone};
use imessage_common::model::{AttachmentId, Chat, DeliveryState, Handle, HandleProfile, Message, MessagePart, ReactionKind};

use crate::contacts::Contacts;

/// The name to show for a handle: the address book, else the name the
/// person shared with us, else the bare address.
pub fn handle_name(contacts: &Contacts, h: &Handle) -> String {
    if let Some(n) = contacts.name_for(&h.uri) {
        return n.to_string();
    }
    if let Some(d) = h.display.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        return d.to_string();
    }
    h.profile.as_ref().and_then(HandleProfile::display_name).unwrap_or_else(|| h.address().to_string())
}

pub fn first_name(s: &str) -> String {
    s.split_whitespace().next().unwrap_or(s).to_string()
}

/// Chat title: its name, else its participants' names.
pub fn chat_title(contacts: &Contacts, c: &Chat) -> String {
    if let Some(n) = c.display_name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        return n.to_string();
    }
    if c.participants.is_empty() {
        return c.id.0.clone();
    }
    let names: Vec<String> = c.participants.iter().map(|h| handle_name(contacts, h)).collect();
    if c.is_group && names.len() > 1 {
        let (last, rest) = names.split_last().unwrap();
        format!("{} & {}", rest.iter().map(|n| first_name(n)).collect::<Vec<_>>().join(", "), first_name(last))
    } else {
        names.join(", ")
    }
}

/// The attachments of a message (link preview images excluded), as
/// `(id, mime, filename, size)`.
pub fn attachments(m: &Message) -> Vec<(AttachmentId, String, String, u64)> {
    m.parts
        .iter()
        .filter_map(|p| match p {
            MessagePart::Attachment { attachment_id, mime, filename, size } => Some((attachment_id.clone(), mime.clone(), filename.clone(), *size)),
            _ => None,
        })
        .collect()
}

/// The link preview of a message, if any: `(url, title, site_name)`.
pub fn link(m: &Message) -> Option<(&str, Option<&str>, Option<&str>)> {
    m.parts.iter().find_map(|p| match p {
        MessagePart::Link { url, title, site_name, .. } => Some((url.as_str(), title.as_deref(), site_name.as_deref())),
        _ => None,
    })
}

/// A message whose text is just the previewed link: show the card alone.
pub fn is_only_link(m: &Message) -> bool {
    let Some((url, ..)) = link(m) else { return false };
    let t = m.text.as_deref().unwrap_or("").trim();
    if t.is_empty() {
        return true;
    }
    let original = m.parts.iter().find_map(|p| match p {
        MessagePart::Link { original_url, .. } => original_url.as_deref(),
        _ => None,
    });
    !t.contains(char::is_whitespace) && (t == url || Some(t) == original)
}

/// What the chat list shows for a chat's newest message (kmessages' rules).
pub fn snippet(m: &Message) -> String {
    if m.unsent {
        return "This message was unsent".into();
    }
    let text = m.text.as_deref().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some((url, title, _)) = link(m)
        && (is_only_link(m) || text.is_empty()) {
            return title.map(str::trim).filter(|t| !t.is_empty()).map(|t| t.split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_else(|| url.trim().to_string());
        }
    if !text.is_empty() {
        return text;
    }
    let atts = attachments(m);
    match atts.len() {
        0 => {}
        1 => return if atts[0].1.starts_with("image/") { "Image".into() } else { format!("Attachment: {}", atts[0].2) },
        n => return format!("{n} attachments"),
    }
    if let Some(d) = m.parts.iter().find_map(|p| match p {
        MessagePart::Unsupported { description } => Some(description.as_str()),
        _ => None,
    }) {
        return format!("Unsupported: {d}");
    }
    String::new()
}

pub fn reaction_emoji(k: &ReactionKind) -> String {
    match k {
        ReactionKind::Love => "❤️".into(),
        ReactionKind::Like => "👍".into(),
        ReactionKind::Dislike => "👎".into(),
        ReactionKind::Laugh => "😂".into(),
        ReactionKind::Emphasize => "‼️".into(),
        ReactionKind::Question => "❓".into(),
        ReactionKind::Emoji(e) => e.clone(),
    }
}

/// `com.apple.messages.effect.CKConfettiEffect` -> `confetti`.
pub fn effect_name(id: &str) -> String {
    let last = id.rsplit('.').next().unwrap_or(id);
    let last = last.strip_prefix("CK").unwrap_or(last);
    let last = last.strip_suffix("Effect").unwrap_or(last);
    let mut out = String::new();
    for (i, ch) in last.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            out.push(' ');
        }
        out.push(ch.to_ascii_lowercase());
    }
    if out.is_empty() { id.to_string() } else { out }
}

pub fn delivery_label(d: DeliveryState) -> &'static str {
    match d {
        DeliveryState::Pending => "Sending…",
        DeliveryState::Sent => "Sent",
        DeliveryState::Delivered => "Delivered",
        DeliveryState::Read => "Read",
        DeliveryState::Failed => "Not delivered",
    }
}

pub fn human_size(b: u64) -> String {
    let b = b as f64;
    if b > 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b > 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else if b > 1e3 {
        format!("{:.0} KB", b / 1e3)
    } else {
        format!("{b} B")
    }
}

pub fn att_icon(mime: &str) -> &'static str {
    if mime.starts_with("image/") {
        "🖼"
    } else if mime.starts_with("video/") {
        "🎬"
    } else if mime.starts_with("audio/") {
        "🎵"
    } else if mime.contains("pdf") {
        "📄"
    } else if mime.contains("vcard") {
        "👤"
    } else {
        "📎"
    }
}

fn local(ms: u64) -> Option<DateTime<Local>> {
    if ms == 0 {
        return None;
    }
    Local.timestamp_millis_opt(ms as i64).single()
}

/// `3:04 PM` today, `Tue` this week, `Mar 4` this year, else `3/4/24`.
pub fn short_time(ms: u64) -> String {
    let Some(d) = local(ms) else { return String::new() };
    let now = Local::now();
    if d.date_naive() == now.date_naive() {
        d.format("%-I:%M %p").to_string()
    } else if (now - d).num_days() < 7 {
        d.format("%a").to_string()
    } else if d.year() == now.year() {
        d.format("%b %-d").to_string()
    } else {
        d.format("%-m/%-d/%y").to_string()
    }
}

pub fn short_clock(ms: u64) -> String {
    local(ms).map(|d| d.format("%-I:%M %p").to_string()).unwrap_or_default()
}

pub fn full_time(ms: u64) -> String {
    let Some(d) = local(ms) else { return String::new() };
    let now = Local::now();
    if d.date_naive() == now.date_naive() {
        format!("Today {}", d.format("%-I:%M %p"))
    } else if (now - d).num_days() < 7 {
        d.format("%A %-I:%M %p").to_string()
    } else {
        d.format("%b %-d, %Y %-I:%M %p").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contacts::Person;
    use imessage_common::model::{ChatId, MessageId, Service};

    pub fn message(id: &str, chat: &str, from_me: bool, text: Option<&str>) -> Message {
        Message {
            id: MessageId::new(id),
            chat_id: ChatId(chat.into()),
            sender: Handle::new(if from_me { "me@example.com" } else { "bob@example.com" }),
            sent_at: 1_700_000_000_000,
            text: text.map(str::to_string),
            parts: text.map(|t| vec![MessagePart::Text { text: t.into() }]).unwrap_or_default(),
            subject: None,
            reply_to: None,
            is_from_me: from_me,
            delivery: DeliveryState::Delivered,
            edited: false,
            unsent: false,
            reactions: vec![],
            effect: None,
            verified: true,
            service: Service::Imessage,
        }
    }

    fn contacts() -> Contacts {
        Contacts::from_people(vec![
            Person { name: "Alice Anderson".into(), phones: vec!["+1 205 555 0100".into()], emails: vec![] },
            Person { name: "Bob Smith".into(), phones: vec![], emails: vec!["bob@example.com".into()] },
        ])
    }

    #[test]
    fn names_prefer_address_book_then_profile_then_address() {
        let c = contacts();
        assert_eq!(handle_name(&c, &Handle::new("bob@example.com")), "Bob Smith");
        let mut shared = Handle::new("carol@example.com");
        shared.profile = Some(HandleProfile { name: Some("Caz".into()), ..Default::default() });
        assert_eq!(handle_name(&c, &shared), "Caz");
        assert_eq!(handle_name(&c, &Handle::new("+1 555 000 1111")), "+15550001111");
    }

    #[test]
    fn chat_titles() {
        let c = contacts();
        let mut chat = Chat {
            id: ChatId::group("G1"),
            participants: vec![Handle::new("+12055550100"), Handle::new("bob@example.com"), Handle::new("dan@x.y")],
            display_name: None,
            is_group: true,
            last_message_at: None,
            unread_count: 0,
            background: None,
            icon_path: None,
            pinned: false,
            pin_order: None,
        };
        assert_eq!(chat_title(&c, &chat), "Alice, Bob & dan@x.y");
        chat.display_name = Some(" Team ".into());
        assert_eq!(chat_title(&c, &chat), "Team");
        let one = Chat { id: ChatId::direct("bob@example.com"), participants: vec![Handle::new("bob@example.com")], is_group: false, display_name: None, ..chat };
        assert_eq!(chat_title(&c, &one), "Bob Smith");
    }

    #[test]
    fn snippets() {
        assert_eq!(snippet(&message("1", "c", false, Some("  hi\n there "))), "hi there");
        let mut m = message("2", "c", false, None);
        m.parts = vec![MessagePart::Attachment { attachment_id: AttachmentId("2:1".into()), mime: "image/jpeg".into(), filename: "a.jpg".into(), size: 1 }];
        assert_eq!(snippet(&m), "Image");
        m.parts.push(MessagePart::Attachment { attachment_id: AttachmentId("2:2".into()), mime: "application/pdf".into(), filename: "b.pdf".into(), size: 1 });
        assert_eq!(snippet(&m), "2 attachments");
        m.parts.truncate(1);
        m.parts[0] = MessagePart::Attachment { attachment_id: AttachmentId("2:1".into()), mime: "application/pdf".into(), filename: "b.pdf".into(), size: 1 };
        assert_eq!(snippet(&m), "Attachment: b.pdf");
        let mut u = message("3", "c", false, Some("x"));
        u.unsent = true;
        assert_eq!(snippet(&u), "This message was unsent");
        let mut l = message("4", "c", false, Some("https://example.com/a"));
        l.parts.push(MessagePart::Link { url: "https://example.com/a".into(), original_url: None, title: Some("A story".into()), summary: None, site_name: None, image_attachment_id: None, icon_attachment_id: None });
        assert!(is_only_link(&l));
        assert_eq!(snippet(&l), "A story");
        l.text = Some("look https://example.com/a".into());
        assert!(!is_only_link(&l));
        assert_eq!(snippet(&l), "look https://example.com/a");
        let mut un = message("5", "c", false, None);
        un.parts = vec![MessagePart::Unsupported { description: "app message".into() }];
        assert_eq!(snippet(&un), "Unsupported: app message");
    }

    #[test]
    fn effects_and_sizes() {
        assert_eq!(effect_name("com.apple.messages.effect.CKConfettiEffect"), "confetti");
        assert_eq!(effect_name("com.apple.MobileSMS.expressivesend.impact"), "impact");
        assert_eq!(effect_name("com.apple.messages.effect.CKHappyBirthdayEffect"), "happy birthday");
        assert_eq!(human_size(900), "900 B");
        assert_eq!(human_size(183_422), "183 KB");
        assert_eq!(human_size(2_500_000), "2.5 MB");
        assert_eq!(reaction_emoji(&ReactionKind::Emoji("🔥".into())), "🔥");
        assert_eq!(reaction_emoji(&ReactionKind::Like), "👍");
    }
}
