//! Human-readable output. Pure functions over model types (unit-tested);
//! colours are applied through [`Style`], which is a no-op unless the stream
//! it is used for (stdout or stderr) is a terminal and `NO_COLOR` is unset.

use std::fmt::Write as _;

use chrono::{DateTime, Datelike, Local, TimeZone};
use imessage_common::model::{
    Chat, ConnectionState, DeliveryState, Handle, HandleProfile, Message, MessagePart, ProfilesState, ProfilesStatus, Reaction, ReactionKind, Service, SmsForwarding, SmsState,
};

use crate::client::{Reachability, Status};

// ---------------------------------------------------------------------------
// Colours
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub color: bool,
}

impl Style {
    pub const PLAIN: Style = Style { color: false };

    /// Colour for stdout: iff stdout is a TTY and `NO_COLOR` is unset or empty.
    pub fn detect() -> Style {
        use std::io::IsTerminal;
        Style { color: !no_color() && std::io::stdout().is_terminal() }
    }

    /// Colour for stderr (hints): iff stderr is a TTY and `NO_COLOR` is unset
    /// or empty. Decided separately so `imsg … 2>log` never writes escapes.
    pub fn detect_stderr() -> Style {
        use std::io::IsTerminal;
        Style { color: !no_color() && std::io::stderr().is_terminal() }
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.color && !s.is_empty() { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }
    pub fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    pub fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }
    pub fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
    pub fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }
    pub fn blue(&self, s: &str) -> String {
        self.paint("34", s)
    }
    pub fn cyan(&self, s: &str) -> String {
        self.paint("36", s)
    }
}

fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// Render `ms` (Unix epoch milliseconds) relative to `now`, in `now`'s zone:
/// today `14:03`, last 6 days `Tue 14:03`, this year `Mar 05 14:03`,
/// otherwise `2023-03-05 14:03`.
pub fn render_time<Tz: TimeZone>(ms: u64, now: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let Some(t) = DateTime::from_timestamp_millis(ms as i64) else { return format!("@{ms}") };
    let t = t.with_timezone(&now.timezone());
    let (td, nd) = (t.date_naive(), now.date_naive());
    if td == nd {
        t.format("%H:%M").to_string()
    } else if td < nd && (nd - td).num_days() < 7 {
        t.format("%a %H:%M").to_string()
    } else if td.year() == nd.year() {
        t.format("%b %d %H:%M").to_string()
    } else {
        t.format("%Y-%m-%d %H:%M").to_string()
    }
}

/// Full local timestamp for seconds since the epoch (registration times).
pub fn abs_time_secs(secs: u64) -> String {
    match DateTime::from_timestamp(secs as i64, 0) {
        Some(t) => t.with_timezone(&Local).format("%Y-%m-%d %H:%M %:z").to_string(),
        None => format!("@{secs}"),
    }
}

// ---------------------------------------------------------------------------
// Handles, chats
// ---------------------------------------------------------------------------

/// Short form of a handle: the display name if any, else the name the
/// person shared with us, else the bare address.
pub fn short_handle(h: &Handle) -> String {
    match &h.display {
        Some(d) if !d.trim().is_empty() => d.clone(),
        _ => h.profile.as_ref().and_then(HandleProfile::display_name).unwrap_or_else(|| h.address().to_string()),
    }
}

/// `uri (Shared Name)` for a participant list.
fn participant(h: &Handle) -> String {
    match h.profile.as_ref().and_then(HandleProfile::display_name) {
        Some(n) => format!("{} ({n})", h.uri),
        None => h.uri.clone(),
    }
}

/// `imsg profile`: what a contact shared (`None` = nothing).
pub fn profile_details(handle: &str, p: Option<&HandleProfile>, st: Style) -> String {
    let Some(p) = p else { return format!("{handle}: no shared name or photo\n") };
    let rows: Vec<(&str, String)> = vec![
        ("handle", handle.to_string()),
        ("name", p.name.clone().unwrap_or_else(|| "-".into())),
        ("first", p.first.clone().unwrap_or_else(|| "-".into())),
        ("last", p.last.clone().unwrap_or_else(|| "-".into())),
        ("photo", p.photo.clone().unwrap_or_else(|| "none".into())),
        ("updated", if p.updated_at > 0 { abs_time_secs(p.updated_at / 1000) } else { "-".into() }),
    ];
    rows.iter().map(|(k, v)| format!("{}  {v}\n", st.bold(&pad(k, 7)))).collect()
}

/// The `profiles` status line (`ready`, `needs_login (…)`, …).
pub fn profiles_text(p: &ProfilesStatus) -> String {
    let name = match p.state {
        ProfilesState::Ready => "ready",
        ProfilesState::Pending => "not tried yet",
        ProfilesState::NeedsLogin => "needs `imsg login`",
        ProfilesState::Unavailable => "unavailable",
    };
    match &p.detail {
        Some(d) => format!("{name} ({d})"),
        None => name.to_string(),
    }
}

/// Short sender: `me` for our own messages, plus `(unverified)` when the
/// sender's key could not be verified.
pub fn sender(m: &Message) -> String {
    let mut s = if m.is_from_me { "me".to_string() } else { short_handle(&m.sender) };
    if !m.verified {
        s.push_str(" (unverified)");
    }
    s
}

/// Chat title: its name, else its participants.
pub fn chat_name(c: &Chat) -> String {
    match &c.display_name {
        Some(n) if !n.trim().is_empty() => n.clone(),
        _ if c.participants.is_empty() => c.id.0.clone(),
        _ => c.participants.iter().map(short_handle).collect::<Vec<_>>().join(", "),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn pad(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width { s.to_string() } else { format!("{s}{}", " ".repeat(width - n)) }
}

/// Chat table: `ID  NAME  LAST  UNREAD`.
pub fn chats_table(chats: &[Chat], st: Style, now: &DateTime<impl TimeZone<Offset: std::fmt::Display>>) -> String {
    if chats.is_empty() {
        return "no conversations\n".to_string();
    }
    let rows: Vec<[String; 4]> = chats
        .iter()
        .map(|c| {
            [
                c.id.0.clone(),
                truncate(&chat_name(c), 40),
                c.last_message_at.map(|t| render_time(t, now)).unwrap_or_else(|| "-".into()),
                if c.unread_count > 0 { c.unread_count.to_string() } else { String::new() },
            ]
        })
        .collect();
    let headers = ["ID", "NAME", "LAST", "UNREAD"];
    let mut w = headers.map(|h| h.len());
    for r in &rows {
        for (i, cell) in r.iter().enumerate() {
            w[i] = w[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let head: Vec<String> = headers.iter().enumerate().map(|(i, h)| pad(h, w[i])).collect();
    let _ = writeln!(out, "{}", st.bold(head.join("  ").trim_end()));
    for r in rows {
        let unread = if r[3].is_empty() { String::new() } else { st.blue(&r[3]) };
        let line = format!("{}  {}  {}  {}", st.dim(&pad(&r[0], w[0])), pad(&r[1], w[1]), pad(&r[2], w[2]), unread);
        let _ = writeln!(out, "{}", line.trim_end());
    }
    out
}

/// `imsg chat`: one chat's details, `key  value` lines.
pub fn chat_details(c: &Chat, st: Style, now: &DateTime<impl TimeZone<Offset: std::fmt::Display>>) -> String {
    let mut rows: Vec<(&str, String)> = vec![
        ("id", c.id.0.clone()),
        ("name", chat_name(c)),
        ("group", if c.is_group { "yes".into() } else { "no".into() }),
        ("participants", c.participants.iter().map(participant).collect::<Vec<_>>().join(", ")),
        ("last", c.last_message_at.map(|t| render_time(t, now)).unwrap_or_else(|| "-".into())),
        ("unread", c.unread_count.to_string()),
    ];
    match &c.background {
        None => rows.push(("background", "none".into())),
        Some(b) => {
            let by = b.set_by.as_ref().map(|h| format!(" by {}", short_handle(&Handle::new(h)))).unwrap_or_default();
            rows.push(("background", format!("{} (v{}){by}", b.id, b.version)));
            rows.push(("  file", b.path.clone().unwrap_or_else(|| "(not downloaded yet)".into())));
        }
    }
    if let Some(p) = &c.icon_path {
        rows.push(("group photo", p.clone()));
    }
    for h in &c.participants {
        if let Some(photo) = h.profile.as_ref().and_then(|p| p.photo.as_ref()) {
            rows.push(("photo", format!("{}: {photo}", h.address())));
        }
    }
    let mut out = String::new();
    for (k, v) in rows {
        let _ = writeln!(out, "{}  {v}", st.bold(&pad(k, 12)));
    }
    out
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Emoji for a reaction kind.
pub fn reaction_symbol(k: &ReactionKind) -> String {
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

/// `❤️2 👍1`: current reactions grouped by kind, in first-seen order.
pub fn reaction_summary(rs: &[Reaction]) -> String {
    let mut groups: Vec<(String, usize)> = Vec::new();
    for r in rs.iter().filter(|r| r.added) {
        let sym = reaction_symbol(&r.kind);
        match groups.iter_mut().find(|(s, _)| *s == sym) {
            Some(g) => g.1 += 1,
            None => groups.push((sym, 1)),
        }
    }
    groups.into_iter().map(|(s, n)| format!("{s}{n}")).collect::<Vec<_>>().join(" ")
}

/// Human file size.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

fn delivery_marker(d: DeliveryState) -> &'static str {
    match d {
        DeliveryState::Pending => "pending",
        DeliveryState::Sent => "sent",
        DeliveryState::Delivered => "delivered",
        DeliveryState::Read => "read",
        DeliveryState::Failed => "FAILED",
    }
}

/// One message:
///
/// ```text
/// 14:03 bob@example.com: look at this  ❤️1  [edited]
///       📎 IMG_0001.jpeg (179.1 KB, image/jpeg)  id 0F1E…:1
///       id 0F1E…  ·  reply to 7A2B…
/// ```
///
/// `show_chat` prefixes the chat id (search / watch output).
pub fn message(m: &Message, st: Style, now: &DateTime<impl TimeZone<Offset: std::fmt::Display>>, show_chat: bool) -> String {
    let mut out = String::new();
    let t = render_time(m.sent_at, now);
    let who = sender(m);
    let who = if m.is_from_me {
        st.green(&who)
    } else if !m.verified {
        st.red(&who)
    } else {
        st.cyan(&who)
    };
    let indent = " ".repeat(t.chars().count() + 1);
    let mut head = format!("{} ", st.dim(&t));
    if show_chat {
        head.push_str(&st.dim(&format!("[{}] ", m.chat_id)));
    }
    head.push_str(&who);
    head.push(':');
    if m.unsent {
        head.push(' ');
        head.push_str(&st.dim("(unsent)"));
    } else if let Some(text) = &m.text {
        let mut lines = text.lines();
        if let Some(first) = lines.next() {
            head.push(' ');
            head.push_str(first);
        }
        for l in lines {
            head.push('\n');
            head.push_str(&indent);
            head.push_str(l);
        }
    }
    if let Some(s) = &m.subject {
        head.push_str(&st.dim(&format!("  (subject: {s})")));
    }
    let reactions = reaction_summary(&m.reactions);
    if !reactions.is_empty() {
        head.push_str("  ");
        head.push_str(&reactions);
    }
    if m.edited {
        head.push_str(&st.dim("  [edited]"));
    }
    if m.service == Service::Sms {
        head.push_str(&st.green("  [SMS]"));
    }
    if m.is_from_me {
        let d = delivery_marker(m.delivery);
        head.push_str("  ");
        head.push_str(&if m.delivery == DeliveryState::Failed { st.red(&format!("[{d}]")) } else { st.dim(&format!("[{d}]")) });
    }
    let _ = writeln!(out, "{head}");
    for p in &m.parts {
        match p {
            MessagePart::Attachment { attachment_id, mime, filename, size: sz } => {
                let _ = writeln!(out, "{indent}📎 {filename} ({}, {mime})  {}", size(*sz), st.dim(&format!("id {attachment_id}")));
            }
            MessagePart::Unsupported { description } => {
                let _ = writeln!(out, "{indent}{}", st.dim(&format!("[unsupported: {description}]")));
            }
            MessagePart::Text { .. } | MessagePart::Mention { .. } => {}
        }
    }
    let mut meta = format!("id {}", m.id);
    if let Some(r) = &m.reply_to {
        meta.push_str(&format!("  ·  reply to {r}"));
    }
    if let Some(e) = &m.effect {
        meta.push_str(&format!("  ·  effect {e}"));
    }
    let _ = writeln!(out, "{indent}{}", st.dim(&meta));
    out
}

// ---------------------------------------------------------------------------
// Status, lookup
// ---------------------------------------------------------------------------

pub fn connection_name(c: &ConnectionState) -> &'static str {
    match c {
        ConnectionState::Disconnected => "disconnected",
        ConnectionState::Connecting => "connecting",
        ConnectionState::Connected => "connected",
        ConnectionState::NeedsLogin => "needs_login",
        ConnectionState::NeedsTwoFactor => "needs_two_factor",
        ConnectionState::Registering => "registering",
        ConnectionState::Error(_) => "error",
    }
}

pub fn connection_colored(c: &ConnectionState, st: Style) -> String {
    let name = connection_name(c);
    match c {
        ConnectionState::Connected => st.green(name),
        ConnectionState::Error(d) => format!("{} ({d})", st.red(name)),
        _ => st.yellow(name),
    }
}

pub fn status_table(s: &Status, st: Style) -> String {
    let mut rows: Vec<(&str, String)> = vec![
        ("daemon", s.version.clone()),
        ("connection", connection_colored(&s.connection, st)),
        ("registered", if s.registered { st.green("yes") } else { st.yellow("no") }),
    ];
    let handles: Vec<String> = s.registration.handles.iter().map(short_handle).collect();
    rows.push(("handles", if handles.is_empty() { "-".into() } else { handles.join(", ") }));
    if let Some(h) = &s.registration.sender {
        rows.push(("sends as", short_handle(h)));
    }
    if !s.registration.other_handles.is_empty() {
        let other: Vec<String> = s.registration.other_handles.iter().map(|u| short_handle(&Handle::new(u))).collect();
        rows.push(("also mine", other.join(", ")));
    }
    if let Some(t) = s.registration.registered_at {
        rows.push(("registered at", abs_time_secs(t)));
    }
    if let Some(t) = s.registration.expires_at {
        rows.push(("renews at", abs_time_secs(t)));
    }
    rows.push(("unread", s.unread.map(|u| u.to_string()).unwrap_or_else(|| "? (store unavailable)".into())));
    if let Some(p) = &s.profiles {
        let t = profiles_text(p);
        rows.push(("profiles", if p.state == ProfilesState::Ready { t } else { st.yellow(&t) }));
    }
    if let Some(sms) = &s.sms {
        rows.push(("sms", sms_state_colored(sms, st)));
    }
    if matches!(s.connection, ConnectionState::NeedsLogin) {
        rows.push(("hint", "run `imsg login`".into()));
    }
    let w = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    rows.iter().map(|(k, v)| format!("{}  {v}\n", st.bold(&pad(k, w)))).collect()
}

/// `active (via +1555…)`, `requested`, `error (why)`, `off`.
pub fn sms_state_colored(s: &SmsForwarding, st: Style) -> String {
    let name = s.state.as_str();
    let via = s.number.as_ref().map(|n| format!(" (via {})", short_handle(&Handle::new(n)))).unwrap_or_default();
    match s.state {
        SmsState::Active => format!("{}{via}", st.green(name)),
        SmsState::Requested => format!("{}{via}", st.yellow(name)),
        SmsState::Error => format!("{} ({})", st.red(name), s.detail.as_deref().unwrap_or("?")),
        SmsState::Off => name.to_string(),
    }
}

/// `imsg sms status`.
pub fn sms_table(s: &SmsForwarding, st: Style) -> String {
    let mut rows: Vec<(&str, String)> = vec![("state", sms_state_colored(s, st))];
    rows.push(("number", s.number.as_ref().map(|n| short_handle(&Handle::new(n))).unwrap_or_else(|| "- (set sms_relay_number or other_handles)".into())));
    if let Some(t) = s.since {
        rows.push(("since", abs_time_secs(t / 1000)));
    }
    let w = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    rows.iter().map(|(k, v)| format!("{}  {v}\n", st.bold(&pad(k, w)))).collect()
}

pub fn lookup_table(rs: &[Reachability], st: Style) -> String {
    rs.iter()
        .map(|r| {
            let h = Handle { uri: r.handle.clone(), display: None, profile: None };
            let mark = if r.reachable { st.green("iMessage") } else { st.yellow("not on iMessage") };
            format!("{}  {mark}\n", h.address())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use imessage_common::model::{AttachmentId, ChatId, MessageId, RegistrationState};

    fn now() -> DateTime<Utc> {
        // Wed 2026-10-14 12:00:00 UTC
        Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
    }

    fn ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> u64 {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis() as u64
    }

    #[test]
    fn time_rendering() {
        let n = now();
        assert_eq!(render_time(ms(2026, 10, 14, 9, 5), &n), "09:05");
        assert_eq!(render_time(ms(2026, 10, 13, 23, 59), &n), "Tue 23:59");
        assert_eq!(render_time(ms(2026, 10, 8, 8, 0), &n), "Thu 08:00");
        assert_eq!(render_time(ms(2026, 10, 7, 8, 0), &n), "Oct 07 08:00");
        assert_eq!(render_time(ms(2026, 3, 5, 14, 3), &n), "Mar 05 14:03");
        assert_eq!(render_time(ms(2023, 3, 5, 14, 3), &n), "2023-03-05 14:03");
        // future (clock skew) in the same year -> month form
        assert_eq!(render_time(ms(2026, 10, 20, 1, 0), &n), "Oct 20 01:00");
        // zone of `now` is used
        let tz = chrono::FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(render_time(ms(2026, 10, 14, 9, 5), &n.with_timezone(&tz)), "11:05");
    }

    fn bob() -> Handle {
        Handle::new("bob@example.com")
    }

    fn msg() -> Message {
        Message {
            id: MessageId::new("AAAA"),
            chat_id: ChatId::direct("bob@example.com"),
            sender: bob(),
            sent_at: ms(2026, 10, 14, 9, 5),
            text: Some("hello\nworld".into()),
            parts: vec![
                MessagePart::Text { text: "hello\nworld".into() },
                MessagePart::Attachment { attachment_id: AttachmentId("AAAA:1".into()), mime: "image/png".into(), filename: "a.png".into(), size: 2048 },
            ],
            subject: None,
            reply_to: Some(MessageId::new("BBBB")),
            is_from_me: false,
            delivery: DeliveryState::Delivered,
            edited: true,
            unsent: false,
            reactions: vec![
                Reaction { kind: ReactionKind::Love, sender: Handle::new("me@x.com"), added: true },
                Reaction { kind: ReactionKind::Emoji("🔥".into()), sender: Handle::new("c@x.com"), added: true },
                Reaction { kind: ReactionKind::Love, sender: Handle::new("d@x.com"), added: true },
                Reaction { kind: ReactionKind::Like, sender: Handle::new("e@x.com"), added: false },
            ],
            effect: None,
            verified: true,
            service: Default::default(),
        }
    }

    #[test]
    fn reactions_summarized() {
        assert_eq!(reaction_summary(&msg().reactions), "❤️2 🔥1");
        assert_eq!(reaction_summary(&[]), "");
        assert_eq!(reaction_symbol(&ReactionKind::Question), "❓");
    }

    #[test]
    fn senders() {
        let mut m = msg();
        assert_eq!(sender(&m), "bob@example.com");
        m.verified = false;
        assert_eq!(sender(&m), "bob@example.com (unverified)");
        m.is_from_me = true;
        m.verified = true;
        assert_eq!(sender(&m), "me");
        let h = Handle { uri: "tel:+15551234567".into(), display: Some("Bob".into()), profile: None };
        assert_eq!(short_handle(&h), "Bob");
        assert_eq!(short_handle(&Handle::new("+1 555 123 4567")), "+15551234567");
    }

    #[test]
    fn message_rendering() {
        let s = message(&msg(), Style::PLAIN, &now(), false);
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[0], "09:05 bob@example.com: hello");
        assert_eq!(lines[1], "      world  ❤️2 🔥1  [edited]");
        assert_eq!(lines[2], "      📎 a.png (2.0 KB, image/png)  id AAAA:1");
        assert_eq!(lines[3], "      id AAAA  ·  reply to BBBB");
        let mut m = msg();
        m.unsent = true;
        m.text = None;
        m.parts.clear();
        m.reactions.clear();
        m.edited = false;
        m.reply_to = None;
        m.is_from_me = true;
        m.delivery = DeliveryState::Read;
        let s = message(&m, Style::PLAIN, &now(), true);
        assert_eq!(s, "09:05 [mailto:bob@example.com] me: (unsent)  [read]\n      id AAAA\n");
        // no escape codes without colour, some with
        assert!(!s.contains('\x1b'));
        assert!(message(&m, Style { color: true }, &now(), false).contains('\x1b'));
    }

    #[test]
    fn chat_table_and_names() {
        let c = Chat {
            id: ChatId::group("G1"),
            participants: vec![bob(), Handle::new("+15551234567")],
            display_name: None,
            is_group: true,
            last_message_at: Some(ms(2026, 10, 14, 9, 5)),
            unread_count: 2,
            background: None,
            icon_path: None,
        };
        assert_eq!(chat_name(&c), "bob@example.com, +15551234567");
        let named = Chat { display_name: Some("Team".into()), unread_count: 0, last_message_at: None, ..c.clone() };
        assert_eq!(chat_name(&named), "Team");
        let t = chats_table(&[c, named], Style::PLAIN, &now());
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "ID        NAME                           LAST   UNREAD");
        assert_eq!(lines[1], "group:G1  bob@example.com, +15551234567  09:05  2");
        assert_eq!(lines[2], "group:G1  Team                           -");
        assert_eq!(chats_table(&[], Style::PLAIN, &now()), "no conversations\n");
        assert_eq!(truncate("abcdef", 4), "abc…");
    }

    #[test]
    fn shared_profiles_in_details() {
        let mut bob = bob();
        bob.profile = Some(HandleProfile { name: Some("Bobby".into()), photo: Some("/s/p/b.jpg".into()), updated_at: 1_700_000_000_000, ..Default::default() });
        let c = Chat {
            id: ChatId::group("G2"),
            participants: vec![bob.clone(), Handle::new("+15551234567")],
            display_name: None,
            is_group: true,
            last_message_at: None,
            unread_count: 0,
            background: None,
            icon_path: Some("/s/i/g.jpg".into()),
        };
        assert_eq!(chat_name(&c), "Bobby, +15551234567");
        let t = chat_details(&c, Style::PLAIN, &now());
        assert!(t.contains("participants  mailto:bob@example.com (Bobby), tel:+15551234567"), "{t}");
        assert!(t.contains("group photo   /s/i/g.jpg"), "{t}");
        assert!(t.contains("photo         bob@example.com: /s/p/b.jpg"), "{t}");
        let t = profile_details(&bob.uri, bob.profile.as_ref(), Style::PLAIN);
        assert!(t.contains("name     Bobby") && t.contains("first    -") && t.contains("photo    /s/p/b.jpg"), "{t}");
        assert_eq!(profile_details("tel:+1555", None, Style::PLAIN), "tel:+1555: no shared name or photo\n");
    }

    #[test]
    fn sizes() {
        assert_eq!(size(10), "10 B");
        assert_eq!(size(183422), "179.1 KB");
        assert_eq!(size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn status_rendering() {
        let s = Status {
            version: "0.1.0".into(),
            connection: ConnectionState::NeedsLogin,
            registered: false,
            registration: Default::default(),
            unread: Some(0),
            profiles: Some(ProfilesStatus { state: ProfilesState::NeedsLogin, detail: Some("Apple wants a fresh sign-in for iCloud".into()) }),
            sms: Some(SmsForwarding { state: SmsState::Active, number: Some("tel:+15555550100".into()), since: None, detail: None }),
        };
        let t = status_table(&s, Style::PLAIN);
        assert!(t.contains("profiles    needs `imsg login` (Apple wants a fresh sign-in for iCloud)"), "{t}");
        assert!(t.contains("connection  needs_login"), "{t}");
        assert!(t.contains("sms         active (via +15555550100)"), "{t}");
        let e = SmsForwarding { state: SmsState::Error, detail: Some("no iPhone".into()), ..Default::default() };
        assert_eq!(sms_state_colored(&e, Style::PLAIN), "error (no iPhone)");
        assert!(sms_table(&SmsForwarding::default(), Style::PLAIN).contains("state   off"));
        assert!(t.contains("hint        run `imsg login`"), "{t}");
        assert!(!t.contains("sends as"), "{t}");
        assert!(!t.contains("also mine"), "{t}");
        let s = Status {
            connection: ConnectionState::Connected,
            registered: true,
            registration: RegistrationState {
                handles: vec![Handle::new("mailto:a@icloud.com"), Handle::new("mailto:me@example.org")],
                registered_at: Some(1_700_000_000),
                expires_at: None,
                sender: Some(Handle::new("mailto:me@example.org")),
                other_handles: vec!["tel:+15551234567".into()],
            },
            ..s
        };
        let t = status_table(&s, Style::PLAIN);
        assert!(t.contains("handles        a@icloud.com, me@example.org"), "{t}");
        assert!(t.contains("sends as       me@example.org"), "{t}");
        assert!(t.contains("also mine      +15551234567"), "{t}");
        let e = ConnectionState::Error("boom".into());
        assert_eq!(connection_colored(&e, Style::PLAIN), "error (boom)");
    }
}
