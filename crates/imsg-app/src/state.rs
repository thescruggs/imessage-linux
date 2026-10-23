//! The app's data, without any UI: chats (ordering, names, previews), the
//! open chat's transcript (paging, live merges, grouping into bubbles),
//! typing indicators. Everything here is unit-tested; `ui.rs` turns
//! the views into Slint models.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, Local, TimeZone};
use imessage_common::api::{MessagePage, Status};
use imessage_common::contacts::Contacts;
use imessage_common::model::{Chat, ConnectionState, DeliveryState, Handle, HandleProfile, Message, MessagePart, ReactionKind, Service};

/// How long a typing indicator lasts without a refresh (the daemon
/// clears it after 60 s too).
pub const TYPING_TTL: Duration = Duration::from_secs(60);
/// Messages per page.
pub const PAGE: u32 = 60;
/// A new time header after this much silence.
const HEADER_GAP_MS: u64 = 30 * 60 * 1000;
/// Bubbles of one sender within this time form one run.
const RUN_GAP_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    pub text: String,
    pub at: u64,
    pub message_id: String,
    pub from_me: bool,
    pub service: Service,
}

#[derive(Debug, Default)]
pub struct OpenChat {
    pub id: String,
    /// Oldest first.
    pub messages: Vec<Message>,
    /// `Some` = older pages exist.
    pub next_cursor: Option<String>,
    /// The newest page arrived.
    pub loaded: bool,
    pub loading: bool,
    pub loading_older: bool,
    /// Live messages that arrived while the first page was in flight.
    pending_live: Vec<Message>,
}

/// What a page or live message did to the open transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Not the open chat (or nothing changed).
    None,
    /// The transcript was replaced (first page / resync).
    Reset,
    /// Older messages went in at the top.
    Prepended,
    /// A new message at the bottom.
    Appended,
    /// A message changed in place.
    Updated,
}

#[derive(Default)]
pub struct State {
    /// Pinned first (by `pin_order`), then newest activity.
    pub chats: Vec<Chat>,
    pub contacts: Contacts,
    pub contacts_etag: Option<String>,
    /// Our own handles (`registration.handles` + `other_handles`).
    pub me: HashSet<String>,
    pub filter: String,
    pub previews: HashMap<String, Preview>,
    pub preview_pending: HashSet<String>,
    pub open: Option<OpenChat>,
    /// chat id -> (handle, when).
    pub typing: HashMap<String, (String, Instant)>,
    pub status: Option<Status>,
}

// ----- names -----

/// The name to show for a handle: the address book, else the name the
/// person shared, else the bare address.
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

pub fn chat_title(contacts: &Contacts, c: &Chat) -> String {
    if let Some(n) = c.display_name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        return n.to_string();
    }
    if c.participants.is_empty() {
        return Handle::new(c.id.as_str()).address().to_string();
    }
    let names: Vec<String> = c.participants.iter().map(|h| handle_name(contacts, h)).collect();
    if c.is_group && names.len() > 1 {
        let (last, rest) = names.split_last().unwrap();
        format!("{} & {}", rest.iter().map(|n| first_name(n)).collect::<Vec<_>>().join(", "), first_name(last))
    } else {
        names.join(", ")
    }
}

/// Up to two letters for the avatar circle; `#` for numbers.
pub fn initials(title: &str) -> String {
    let t = title.trim();
    if t.starts_with(|c: char| c == '+' || c.is_ascii_digit()) {
        return "#".into();
    }
    let letters: String = t
        .split(|c: char| c.is_whitespace() || c == ',' || c == '&')
        .filter_map(|w| w.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    if letters.is_empty() { "?".into() } else { letters }
}

/// Number of avatar tints (theme.slint `avatar-ink` / `avatar-fill`).
pub const TINTS: u32 = 8;

/// A stable avatar tint for a chat or handle (FNV-1a of the lowercased
/// key): the same person gets the same colour in the list and in groups,
/// on every start.
pub fn tint_index(key: &str) -> i32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in key.trim().to_lowercase().bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    (h % TINTS) as i32
}

// ----- message helpers -----

/// The attachments of a message (link preview images excluded), as
/// `(id, mime, filename, size)`.
pub fn attachments(m: &Message) -> Vec<(String, String, String, u64)> {
    m.parts
        .iter()
        .filter_map(|p| match p {
            MessagePart::Attachment { attachment_id, mime, filename, size } => Some((attachment_id.0.clone(), mime.clone(), filename.clone(), *size)),
            _ => None,
        })
        .collect()
}

/// A link part: `(url, title, site_name, original_url)`.
type LinkRef<'a> = (&'a str, Option<&'a str>, Option<&'a str>, Option<&'a str>);

fn link(m: &Message) -> Option<LinkRef<'_>> {
    m.parts.iter().find_map(|p| match p {
        MessagePart::Link { url, title, site_name, original_url, .. } => Some((url.as_str(), title.as_deref(), site_name.as_deref(), original_url.as_deref())),
        _ => None,
    })
}

/// A message whose text is just the previewed link: show the card alone.
pub fn is_only_link(m: &Message) -> bool {
    let Some((url, _, _, original)) = link(m) else { return false };
    let t = m.text.as_deref().unwrap_or("").trim();
    t.is_empty() || (!t.contains(char::is_whitespace) && (t == url || Some(t) == original))
}

/// Message text without U+FFFC (the "object replacement" placeholder
/// Apple puts where an attachment sits) and outer whitespace.
pub fn clean_text(t: &str) -> String {
    t.replace('\u{fffc}', "").trim().to_string()
}

/// The chat list's one-line summary of a message.
pub fn snippet(m: &Message) -> String {
    if m.unsent {
        return "This message was unsent".into();
    }
    let text = clean_text(m.text.as_deref().unwrap_or("")).split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some((url, title, ..)) = link(m)
        && (is_only_link(m) || text.is_empty())
    {
        return title.map(|t| t.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|t| !t.is_empty()).unwrap_or_else(|| url.trim().to_string());
    }
    if !text.is_empty() {
        return text;
    }
    let atts = attachments(m);
    match atts.len() {
        0 => {}
        1 => return if atts[0].1.starts_with("image/") { "Photo".into() } else { format!("Attachment: {}", atts[0].2) },
        n => return format!("{n} attachments"),
    }
    m.parts
        .iter()
        .find_map(|p| match p {
            MessagePart::Unsupported { description } => Some(format!("Unsupported: {description}")),
            _ => None,
        })
        .unwrap_or_default()
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

pub fn reaction_label(k: &ReactionKind) -> String {
    match k {
        ReactionKind::Love => "Love".into(),
        ReactionKind::Like => "Like".into(),
        ReactionKind::Dislike => "Dislike".into(),
        ReactionKind::Laugh => "Laugh".into(),
        ReactionKind::Emphasize => "Emphasize".into(),
        ReactionKind::Question => "Question".into(),
        ReactionKind::Emoji(e) => e.clone(),
    }
}

/// The six classic tapbacks, in picker order (`ReactionKind::arg` names;
/// the UI has an icon for each).
pub const TAPBACKS: [&str; 6] = ["love", "like", "dislike", "laugh", "emphasize", "question"];

/// What the picker sends (a tapback name, or typed text) -> the react
/// request's `kind`: one of [`TAPBACKS`], or an emoji (any non-ASCII
/// text). `None` for anything else.
pub fn tapback_kind(input: &str) -> Option<String> {
    let t = input.trim();
    let lower = t.to_ascii_lowercase();
    if TAPBACKS.contains(&lower.as_str()) {
        return Some(lower);
    }
    (!t.is_empty() && !t.is_ascii() && t.chars().count() <= 8).then(|| t.to_string())
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

/// MIME type from a file name (`None` = let the daemon default).
pub fn guess_mime(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    Some(match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "webp" => "image/webp",
        "tif" | "tiff" => "image/tiff",
        "mov" => "video/quicktime",
        "mp4" | "m4v" => "video/mp4",
        "m4a" => "audio/mp4",
        "caf" => "audio/x-caf",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "vcf" => "text/vcard",
        "zip" => "application/zip",
        _ => return None,
    })
}

/// Pictures the app decodes itself.
pub fn inline_image(mime: &str) -> bool {
    matches!(mime.to_ascii_lowercase().as_str(), "image/jpeg" | "image/jpg" | "image/png" | "image/gif" | "image/webp")
}

pub fn human_size(n: u64) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if f < K {
        format!("{n} B")
    } else if f < K * K {
        format!("{:.0} KB", f / K)
    } else if f < K * K * K {
        format!("{:.1} MB", f / K / K)
    } else {
        format!("{:.1} GB", f / K / K / K)
    }
}

/// The status footer's dot: connected, on the way, or a problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusLevel {
    Ok = 0,
    Busy = 1,
    Bad = 2,
}

/// The status footer (kmessages StatusFooter.qml): "Connected as
/// me@example.com" when the bridge and imessaged are up, else what is
/// wrong. `daemon` is `None` until the first status arrives.
pub fn status_line(bridge_ok: bool, bridge_seen: bool, daemon: Option<&ConnectionState>, daemon_reachable: bool, own: Option<&str>) -> (String, StatusLevel) {
    if !bridge_ok {
        return if bridge_seen { ("Bridge reconnecting…".into(), StatusLevel::Busy) } else { ("Connecting to the bridge…".into(), StatusLevel::Busy) };
    }
    if !daemon_reachable {
        return ("imessaged is not reachable from the bridge".into(), StatusLevel::Bad);
    }
    match daemon {
        None | Some(ConnectionState::Connecting) => ("Connecting…".into(), StatusLevel::Busy),
        Some(ConnectionState::Registering) => ("Registering…".into(), StatusLevel::Busy),
        Some(ConnectionState::Connected) => match own {
            Some(h) => (format!("Connected as {h}"), StatusLevel::Ok),
            None => ("Connected".into(), StatusLevel::Ok),
        },
        Some(ConnectionState::Disconnected) => ("imessaged: disconnected".into(), StatusLevel::Bad),
        Some(ConnectionState::NeedsLogin) => ("Not signed in (run imsg login on the host)".into(), StatusLevel::Bad),
        Some(ConnectionState::NeedsTwoFactor) => ("Verification code needed".into(), StatusLevel::Bad),
        Some(ConnectionState::Error(d)) => (format!("imessaged error: {}", imessage_common::redact::redact_line(d)), StatusLevel::Bad),
    }
}

/// A `connection_state` event's state ("connected", "needs_login", …);
/// `None` for "error" (no detail in the event) and unknown ones.
pub fn parse_connection_state(state: &str) -> Option<ConnectionState> {
    serde_json::from_value(serde_json::json!({ "state": state })).ok()
}

pub fn connection_label(state: &str) -> String {
    match state {
        "needs_login" => "needs login (run imsg login on the host)".into(),
        "needs_two_factor" => "needs a 2FA code".into(),
        "" => "unknown".into(),
        other => other.replace('_', " "),
    }
}

// ----- times -----

fn local(ms: u64) -> DateTime<Local> {
    Local.timestamp_millis_opt(ms as i64).single().unwrap_or_else(Local::now)
}

/// Chat list: `14:05` today, `Yesterday`, `Mon` this week, else `12 Mar`
/// (`12 Mar 2024` in other years).
pub fn list_time(ms: u64, now: DateTime<Local>) -> String {
    let t = local(ms);
    let days = (now.date_naive() - t.date_naive()).num_days();
    if days <= 0 {
        t.format("%H:%M").to_string()
    } else if days == 1 {
        "Yesterday".into()
    } else if days < 7 {
        t.format("%a").to_string()
    } else if t.year() == now.year() {
        t.format("%-d %b").to_string()
    } else {
        t.format("%-d %b %Y").to_string()
    }
}

/// Transcript header: `Today 14:05`, `Yesterday 09:12`, `Monday 18:30`,
/// `12 March 2026, 14:05`.
pub fn header_time(ms: u64, now: DateTime<Local>) -> String {
    let t = local(ms);
    let days = (now.date_naive() - t.date_naive()).num_days();
    if days <= 0 {
        t.format("Today %H:%M").to_string()
    } else if days == 1 {
        t.format("Yesterday %H:%M").to_string()
    } else if days < 7 {
        t.format("%A %H:%M").to_string()
    } else {
        t.format("%-d %B %Y, %H:%M").to_string()
    }
}

pub fn short_time(ms: u64) -> String {
    local(ms).format("%H:%M").to_string()
}

// ----- views -----

/// One chat list row.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatView {
    pub id: String,
    pub title: String,
    pub initials: String,
    pub snippet: String,
    pub time: String,
    pub unread: u32,
    pub pinned: bool,
    pub is_group: bool,
    pub sms: bool,
    pub typing: bool,
    /// "Pinned" above the first pinned chat and "Conversations" above the
    /// first other one, only when something is pinned (kmessages).
    pub section: Option<&'static str>,
    /// Avatar tint ([`tint_index`] of the chat id).
    pub tint: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PartView {
    /// A picture the app shows inline.
    Image { att_id: String, filename: String, size: u64 },
    /// Anything else: a pill with Open (click) and Save.
    File { att_id: String, filename: String, mime: String, size: u64 },
    /// A link card; `image` = its preview picture (an attachment id).
    Link { url: String, title: String, site: String, image: Option<String> },
}

/// One tapback chip: everyone who reacted with one kind.
#[derive(Debug, Clone, PartialEq)]
pub struct ReactionView {
    /// `ReactionKind::arg`: a [`TAPBACKS`] name or the emoji.
    pub kind: String,
    /// "Love" … or the emoji.
    pub label: String,
    pub count: usize,
    /// One of them is ours (blue chip; clicking it takes it back).
    pub mine: bool,
    /// "You, Bob, Carol" (hover).
    pub senders: String,
}

/// One bubble (with what goes around it).
#[derive(Debug, Clone, PartialEq)]
pub struct MsgView {
    pub id: String,
    /// A time header above (first message, or after a long gap).
    pub header: Option<String>,
    /// Sender name above the bubble (groups, incoming, first of a run).
    pub sender: Option<String>,
    /// The sender's name on every message ("You" for ours): "Bob unsent
    /// a message", accessibility.
    pub who: String,
    /// Incoming in a group: room for the 24 px sender avatar on the left.
    pub in_group: bool,
    /// The avatar is drawn by the last bubble of the run.
    pub show_avatar: bool,
    pub sender_uri: String,
    pub sender_initials: String,
    pub sender_tint: i32,
    pub from_me: bool,
    pub sms: bool,
    pub text: Option<String>,
    pub subject: Option<String>,
    pub parts: Vec<PartView>,
    pub unsent: bool,
    pub edited: bool,
    pub unverified: bool,
    pub effect: Option<String>,
    pub reply_quote: Option<String>,
    pub reactions: Vec<ReactionView>,
    /// Tapbacks allowed (not on SMS or unsent messages).
    pub can_react: bool,
    /// Our current tapback (`ReactionKind::arg`), if any.
    pub my_reaction: Option<String>,
    /// Under our newest outgoing message (and any failed one).
    pub delivery: Option<String>,
    /// Not delivered (red label).
    pub failed: bool,
    /// Shown on hover.
    pub time: String,
    pub first_in_run: bool,
    pub last_in_run: bool,
}

impl State {
    pub fn is_me(&self, h: &Handle) -> bool {
        self.me.contains(&h.uri)
    }

    pub fn name(&self, h: &Handle) -> String {
        if self.is_me(h) { "You".into() } else { handle_name(&self.contacts, h) }
    }

    /// The name for a bare URI (typing events, profile updates): the
    /// handle as a chat lists it (with its shared profile) if any.
    pub fn name_for_uri(&self, uri: &str) -> String {
        let h = self.chats.iter().flat_map(|c| c.participants.iter()).find(|h| h.uri == uri).cloned().unwrap_or_else(|| Handle::new(uri));
        self.name(&h)
    }

    pub fn chat(&self, id: &str) -> Option<&Chat> {
        self.chats.iter().find(|c| c.id.0 == id)
    }

    pub fn title(&self, id: &str) -> String {
        match self.chat(id) {
            Some(c) => chat_title(&self.contacts, c),
            None => Handle::new(id).address().to_string(),
        }
    }

    pub fn total_unread(&self) -> u32 {
        self.chats.iter().map(|c| c.unread_count).sum()
    }

    pub fn set_status(&mut self, s: Status) {
        self.me = s.registration.handles.iter().map(|h| h.uri.clone()).chain(s.registration.other_handles.iter().cloned()).collect();
        self.status = Some(s);
    }

    fn sort_chats(&mut self) {
        self.chats.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then_with(|| a.pin_order.unwrap_or(u32::MAX).cmp(&b.pin_order.unwrap_or(u32::MAX)))
                .then_with(|| b.last_message_at.unwrap_or(0).cmp(&a.last_message_at.unwrap_or(0)))
                .then_with(|| a.id.0.cmp(&b.id.0))
        });
    }

    /// A full reload replaces the list.
    pub fn set_chats(&mut self, chats: Vec<Chat>) {
        self.chats = chats;
        self.sort_chats();
    }

    /// `true` if the chat is new to the list.
    pub fn merge_chat(&mut self, chat: Chat) -> bool {
        let new = match self.chats.iter().position(|c| c.id == chat.id) {
            Some(i) => {
                self.chats[i] = chat;
                false
            }
            None => {
                self.chats.push(chat);
                true
            }
        };
        self.sort_chats();
        new
    }

    /// Our handle for the status footer: the daemon's sender handle,
    /// else the first registered one, as a bare address.
    pub fn own_handle(&self) -> Option<String> {
        let r = &self.status.as_ref()?.registration;
        r.sender.as_ref().or(r.handles.first()).map(|h| h.address().to_string())
    }

    /// Chats matching the search box (title, address or last message).
    pub fn chat_views(&self, now: DateTime<Local>, now_i: Instant) -> Vec<ChatView> {
        let mut v = self.chat_views_unsectioned(now, now_i);
        if v.iter().any(|c| c.pinned) {
            if let Some(first) = v.iter_mut().find(|c| c.pinned) {
                first.section = Some("Pinned");
            }
            if let Some(first) = v.iter_mut().find(|c| !c.pinned) {
                first.section = Some("Conversations");
            }
        }
        v
    }

    fn chat_views_unsectioned(&self, now: DateTime<Local>, now_i: Instant) -> Vec<ChatView> {
        let q = self.filter.trim().to_lowercase();
        self.chats
            .iter()
            .filter_map(|c| {
                let title = chat_title(&self.contacts, c);
                let preview = self.previews.get(&c.id.0);
                if !q.is_empty()
                    && !title.to_lowercase().contains(&q)
                    && !c.participants.iter().any(|p| p.uri.to_lowercase().contains(&q))
                    && !preview.is_some_and(|p| p.text.to_lowercase().contains(&q))
                {
                    return None;
                }
                let snippet = match preview {
                    Some(p) if p.from_me && !p.text.is_empty() => format!("You: {}", p.text),
                    Some(p) => p.text.clone(),
                    None => String::new(),
                };
                Some(ChatView {
                    id: c.id.0.clone(),
                    initials: initials(&title),
                    title,
                    snippet,
                    time: c.last_message_at.map(|t| list_time(t, now)).unwrap_or_default(),
                    unread: c.unread_count,
                    pinned: c.pinned,
                    is_group: c.is_group,
                    sms: preview.is_some_and(|p| p.service == Service::Sms),
                    typing: self.typing_in(&c.id.0, now_i).is_some(),
                    section: None,
                    tint: tint_index(&c.id.0),
                })
            })
            .collect()
    }

    // ----- previews -----

    pub fn apply_preview(&mut self, m: &Message) {
        let cur = self.previews.get(&m.chat_id.0);
        let same = cur.is_some_and(|p| p.message_id == m.id.0);
        if !same && cur.is_some_and(|p| p.at > m.sent_at) {
            return;
        }
        let from_me = m.is_from_me || self.is_me(&m.sender);
        self.previews.insert(m.chat_id.0.clone(), Preview { text: snippet(m), at: m.sent_at, message_id: m.id.0.clone(), from_me, service: m.service });
    }

    /// Chats (in list order, the first `limit`) whose preview is missing
    /// or older than their last activity and not being fetched; marks
    /// them pending.
    pub fn take_stale_previews(&mut self, limit: usize) -> Vec<String> {
        let mut out = Vec::new();
        for c in self.chats.iter().take(limit) {
            let id = &c.id.0;
            if self.preview_pending.contains(id) {
                continue;
            }
            let stale = match self.previews.get(id) {
                None => true,
                Some(p) => c.last_message_at.is_some_and(|at| at > p.at),
            };
            if stale {
                out.push(id.clone());
            }
        }
        self.preview_pending.extend(out.iter().cloned());
        out
    }

    pub fn preview_done(&mut self, chat_id: &str, m: Option<Message>) {
        self.preview_pending.remove(chat_id);
        match m {
            Some(m) => self.apply_preview(&m),
            // Remember "nothing" so it is not fetched again and again.
            None => {
                let at = self.chat(chat_id).and_then(|c| c.last_message_at).unwrap_or(0);
                self.previews.entry(chat_id.to_string()).or_insert(Preview { text: String::new(), at, message_id: String::new(), from_me: false, service: Service::Imessage });
            }
        }
    }

    // ----- typing -----

    pub fn set_typing(&mut self, chat_id: &str, handle: &str, typing: bool, now: Instant) {
        if typing {
            self.typing.insert(chat_id.to_string(), (handle.to_string(), now));
        } else if self.typing.get(chat_id).is_some_and(|(h, _)| h == handle) {
            self.typing.remove(chat_id);
        }
    }

    /// "Bob is typing…" for a chat, if anyone is.
    pub fn typing_in(&self, chat_id: &str, now: Instant) -> Option<String> {
        let (handle, at) = self.typing.get(chat_id)?;
        (now.duration_since(*at) < TYPING_TTL).then(|| format!("{} is typing…", first_name(&self.name_for_uri(handle))))
    }

    // ----- the open chat -----

    pub fn open_id(&self) -> Option<&str> {
        self.open.as_ref().map(|o| o.id.as_str())
    }

    /// Open a chat (fresh transcript; the caller loads the newest page).
    pub fn open_chat(&mut self, id: &str) {
        self.open = Some(OpenChat { id: id.to_string(), loading: true, ..Default::default() });
    }

    /// Resync: the open chat's newest page is loaded again; keep what is
    /// shown until it arrives.
    pub fn reload_open(&mut self) -> Option<String> {
        let o = self.open.as_mut()?;
        o.loading = true;
        o.loading_older = false;
        Some(o.id.clone())
    }

    /// A page arrived. `older` = it was asked for with a cursor.
    pub fn apply_page(&mut self, chat_id: &str, page: MessagePage, older: bool) -> Change {
        let Some(o) = self.open.as_mut().filter(|o| o.id == chat_id) else { return Change::None };
        if older {
            o.loading_older = false;
            if !o.loaded {
                return Change::None;
            }
            let known: HashSet<String> = o.messages.iter().map(|m| m.id.0.clone()).collect();
            let mut older_msgs: Vec<Message> = page.messages.into_iter().filter(|m| !known.contains(&m.id.0)).collect();
            o.next_cursor = page.next_cursor;
            if older_msgs.is_empty() {
                return Change::None;
            }
            older_msgs.append(&mut o.messages);
            o.messages = older_msgs;
            return Change::Prepended;
        }
        o.loading = false;
        let newest = page.messages.first().map(|m| m.sent_at).unwrap_or(0);
        // On a resync, keep older pages already loaded that end before
        // the new page (they did not change position), so scrolling back
        // does not start over.
        let keep: Vec<Message> = if o.loaded && !page.messages.is_empty() {
            let ids: HashSet<&str> = page.messages.iter().map(|m| m.id.0.as_str()).collect();
            o.messages.iter().filter(|m| m.sent_at < newest && !ids.contains(m.id.0.as_str())).cloned().collect()
        } else {
            Vec::new()
        };
        if keep.is_empty() {
            o.next_cursor = page.next_cursor;
        }
        let mut msgs = keep;
        msgs.extend(page.messages);
        for m in std::mem::take(&mut o.pending_live) {
            if let Some(i) = msgs.iter().position(|x| x.id == m.id) {
                msgs[i] = m;
            } else {
                msgs.push(m);
            }
        }
        msgs.sort_by_key(|m| m.sent_at);
        o.messages = msgs;
        o.loaded = true;
        Change::Reset
    }

    pub fn page_failed(&mut self, chat_id: &str, older: bool) {
        if let Some(o) = self.open.as_mut().filter(|o| o.id == chat_id) {
            if older {
                o.loading_older = false;
            } else {
                o.loading = false;
            }
        }
    }

    /// The cursor for the next older page, if one should be fetched now
    /// (marks it loading).
    pub fn begin_older(&mut self) -> Option<(String, String)> {
        let o = self.open.as_mut()?;
        if !o.loaded || o.loading || o.loading_older {
            return None;
        }
        let c = o.next_cursor.clone()?;
        o.loading_older = true;
        Some((o.id.clone(), c))
    }

    pub fn has_older(&self) -> bool {
        self.open.as_ref().is_some_and(|o| o.next_cursor.is_some())
    }

    /// A live new or changed message; also updates the preview.
    pub fn merge_message(&mut self, m: Message, updated: bool) -> Change {
        if !updated || self.previews.get(&m.chat_id.0).is_some_and(|p| p.message_id == m.id.0) {
            self.apply_preview(&m);
        }
        if !updated && self.typing.get(&m.chat_id.0).is_some_and(|(h, _)| *h == m.sender.uri) {
            self.typing.remove(&m.chat_id.0);
        }
        let Some(o) = self.open.as_mut().filter(|o| o.id == m.chat_id.0) else { return Change::None };
        if !o.loaded {
            o.pending_live.push(m);
            return Change::None;
        }
        if let Some(i) = o.messages.iter().position(|x| x.id == m.id) {
            if o.messages[i] == m {
                return Change::None;
            }
            o.messages[i] = m;
            return Change::Updated;
        }
        if updated {
            // An update for a message on a page we do not have.
            return Change::None;
        }
        let at_end = o.messages.last().is_none_or(|l| l.sent_at <= m.sent_at);
        o.messages.push(m);
        if at_end {
            Change::Appended
        } else {
            o.messages.sort_by_key(|x| x.sent_at);
            Change::Updated
        }
    }

    /// Newest incoming message id of the open chat (for read receipts).
    pub fn open_has_unread(&self) -> bool {
        self.open_id().and_then(|id| self.chat(id)).is_some_and(|c| c.unread_count > 0)
    }

    /// The open transcript as bubbles.
    pub fn transcript(&self, now: DateTime<Local>) -> Vec<MsgView> {
        let Some(o) = &self.open else { return vec![] };
        let is_group = self.chat(&o.id).map(|c| c.is_group).unwrap_or_else(|| o.id.starts_with("group"));
        let msgs = &o.messages;
        let mine = |m: &Message| m.is_from_me || self.is_me(&m.sender);
        let last_mine = msgs.iter().rposition(mine);
        let by_id: HashMap<&str, &Message> = msgs.iter().map(|m| (m.id.0.as_str(), m)).collect();
        let mut out: Vec<MsgView> = Vec::with_capacity(msgs.len());
        for (i, m) in msgs.iter().enumerate() {
            let prev = i.checked_sub(1).map(|j| &msgs[j]);
            let next = msgs.get(i + 1);
            let header = match prev {
                None => true,
                Some(p) => m.sent_at.saturating_sub(p.sent_at) > HEADER_GAP_MS,
            }
            .then(|| header_time(m.sent_at, now));
            let same_run = |a: &Message, b: &Message| mine(a) == mine(b) && a.sender.uri == b.sender.uri && b.sent_at.saturating_sub(a.sent_at) <= RUN_GAP_MS;
            let first_in_run = header.is_some() || prev.is_none_or(|p| !same_run(p, m));
            let last_in_run = next.is_none_or(|n| !same_run(m, n) || n.sent_at.saturating_sub(m.sent_at) > HEADER_GAP_MS);
            let from_me = mine(m);
            let who = if from_me { "You".to_string() } else { self.name(&m.sender) };
            let sender = (is_group && !from_me && first_in_run).then(|| who.clone());
            let mut parts = Vec::new();
            for p in &m.parts {
                match p {
                    MessagePart::Attachment { attachment_id, mime, filename, size } => {
                        if inline_image(mime) {
                            parts.push(PartView::Image { att_id: attachment_id.0.clone(), filename: filename.clone(), size: *size });
                        } else {
                            parts.push(PartView::File { att_id: attachment_id.0.clone(), filename: filename.clone(), mime: mime.clone(), size: *size });
                        }
                    }
                    MessagePart::Link { url, title, site_name, image_attachment_id, .. } => parts.push(PartView::Link {
                        url: url.clone(),
                        title: title.clone().unwrap_or_default(),
                        site: site_name.clone().or_else(|| imessage_common::model::link_host(url)).unwrap_or_default(),
                        image: image_attachment_id.as_ref().map(|a| a.0.clone()),
                    }),
                    _ => {}
                }
            }
            let text = if m.unsent || is_only_link(m) { None } else { m.text.as_deref().map(clean_text).filter(|t| !t.trim().is_empty()) };
            let text = text.or_else(|| {
                // Nothing else to show: say what it was.
                (!m.unsent && parts.is_empty()).then(|| snippet(m)).filter(|s| !s.is_empty())
            });
            let reply_quote = m.reply_to.as_ref().map(|r| match by_id.get(r.0.as_str()) {
                Some(q) => {
                    let s: String = snippet(q).chars().take(80).collect();
                    format!("{}: {s}", self.name(&q.sender))
                }
                None => "Reply to an earlier message".into(),
            });
            let mut reactions: Vec<ReactionView> = Vec::new();
            let mut senders: Vec<Vec<String>> = Vec::new();
            for r in m.reactions.iter().filter(|r| r.added) {
                let kind = r.kind.arg();
                let mine = self.is_me(&r.sender);
                let who = first_name(&self.name(&r.sender));
                let i = match reactions.iter().position(|v| v.kind == kind) {
                    Some(i) => i,
                    None => {
                        reactions.push(ReactionView { kind, label: reaction_label(&r.kind), count: 0, mine: false, senders: String::new() });
                        senders.push(vec![]);
                        reactions.len() - 1
                    }
                };
                reactions[i].count += 1;
                reactions[i].mine |= mine;
                if mine {
                    senders[i].insert(0, who);
                } else {
                    senders[i].push(who);
                }
            }
            for (v, who) in reactions.iter_mut().zip(senders) {
                v.senders = who.join(", ");
            }
            let can_react = m.service != Service::Sms && !m.unsent;
            let my_reaction = self.my_reaction(m);
            let delivery = if from_me && m.delivery == DeliveryState::Failed {
                Some(delivery_label(DeliveryState::Failed).to_string())
            } else if Some(i) == last_mine && !m.unsent {
                Some(delivery_label(m.delivery).to_string())
            } else {
                None
            };
            let in_group = is_group && !from_me;
            out.push(MsgView {
                id: m.id.0.clone(),
                header,
                sender,
                sender_initials: initials(&who),
                sender_tint: tint_index(&m.sender.uri),
                sender_uri: m.sender.uri.clone(),
                who,
                in_group,
                show_avatar: in_group && last_in_run && !m.unsent,
                from_me,
                sms: m.service == Service::Sms,
                text,
                subject: m.subject.clone().filter(|s| !s.trim().is_empty()),
                parts,
                unsent: m.unsent,
                edited: m.edited && !m.unsent,
                unverified: !m.verified,
                effect: m.effect.as_deref().map(effect_name),
                reply_quote,
                reactions,
                can_react,
                my_reaction,
                delivery,
                failed: from_me && m.delivery == DeliveryState::Failed,
                time: short_time(m.sent_at),
                first_in_run,
                last_in_run,
            });
        }
        out
    }

    // ----- tapbacks -----

    /// Our current tapback on `m` (`ReactionKind::arg`): one per person.
    pub fn my_reaction(&self, m: &Message) -> Option<String> {
        m.reactions.iter().find(|r| r.added && self.is_me(&r.sender)).map(|r| r.kind.arg())
    }

    /// The react request for picking `input` on an open-chat message:
    /// `(chat id, kind, remove)`. Picking our current tapback takes it
    /// back; anything else replaces it. `None` for SMS / unsent / unknown
    /// messages and input that is no tapback ([`tapback_kind`]).
    pub fn reaction_request(&self, msg_id: &str, input: &str) -> Option<(String, String, bool)> {
        let o = self.open.as_ref()?;
        let m = o.messages.iter().find(|m| m.id.0 == msg_id)?;
        if m.service == Service::Sms || m.unsent {
            return None;
        }
        let kind = tapback_kind(input)?;
        let remove = self.my_reaction(m).as_deref() == Some(kind.as_str());
        Some((m.chat_id.0.clone(), kind, remove))
    }

    // ----- new message -----

    /// Contacts matching what is typed after the last comma: `(name, address)`.
    pub fn contact_suggestions(&self, input: &str, limit: usize) -> Vec<(String, String)> {
        let q = input.rsplit([',', ';']).next().unwrap_or("").trim().to_lowercase();
        if q.is_empty() {
            return vec![];
        }
        let mut out = Vec::new();
        for p in self.contacts.people() {
            let name_hit = p.name.to_lowercase().contains(&q);
            for a in p.phones.iter().chain(p.emails.iter()) {
                if name_hit || a.to_lowercase().contains(&q) {
                    out.push((p.name.clone(), a.clone()));
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
        out
    }

    /// Recipients from the new-message box: addresses, or exact contact
    /// names (their first number, else first email).
    pub fn resolve_recipients(&self, input: &str) -> Result<Vec<String>, String> {
        let mut out: Vec<String> = Vec::new();
        for tok in input.split([',', ';']).map(str::trim).filter(|t| !t.is_empty()) {
            let h = if let Some(h) = Handle::try_normalize(tok) {
                h
            } else if let Some(p) = self.contacts.people().iter().find(|p| p.name.eq_ignore_ascii_case(tok)) {
                match p.phones.first().or(p.emails.first()).and_then(|a| Handle::try_normalize(a)) {
                    Some(h) => h,
                    None => return Err(format!("{tok} has no phone number or email address")),
                }
            } else {
                return Err(format!("Not a phone number, email address or contact: {tok}"));
            };
            if !out.contains(&h) {
                out.push(h);
            }
        }
        if out.is_empty() {
            return Err("Enter at least one recipient.".into());
        }
        Ok(out)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use imessage_common::contacts::Person;
    use imessage_common::model::{ChatId, MessageId, Reaction};

    pub fn chat(id: &str, at: u64) -> Chat {
        Chat {
            id: ChatId(id.into()),
            participants: vec![Handle::new(id)],
            display_name: None,
            is_group: id.starts_with("group"),
            last_message_at: Some(at),
            unread_count: 0,
            background: None,
            icon_path: None,
            pinned: false,
            pin_order: None,
        }
    }

    pub fn message(id: &str, chat: &str, at: u64, text: &str) -> Message {
        Message {
            id: MessageId(id.into()),
            chat_id: ChatId(chat.into()),
            sender: Handle::new(if chat.starts_with("group") { "bob@example.com" } else { chat }),
            sent_at: at,
            text: Some(text.into()),
            parts: vec![MessagePart::Text { text: text.into() }],
            subject: None,
            reply_to: None,
            is_from_me: false,
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
            Person { name: "Bob Smith".into(), phones: vec!["+1 555 000 1111".into()], emails: vec!["bob@example.com".into()] },
            Person { name: "Carol".into(), phones: vec![], emails: vec!["carol@example.com".into()] },
        ])
    }

    fn page(ms: Vec<Message>, cursor: Option<&str>) -> MessagePage {
        MessagePage { messages: ms, next_cursor: cursor.map(str::to_string) }
    }

    #[test]
    fn chats_order_pinned_first_then_activity() {
        let mut s = State::default();
        let mut p1 = chat("tel:+1", 1);
        p1.pinned = true;
        p1.pin_order = Some(1);
        let mut p0 = chat("tel:+2", 2);
        p0.pinned = true;
        p0.pin_order = Some(0);
        s.set_chats(vec![chat("tel:+3", 50), p1, chat("tel:+4", 100), p0]);
        let ids: Vec<&str> = s.chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["tel:+2", "tel:+1", "tel:+4", "tel:+3"]);
        // An update moves a chat; a new one is reported.
        assert!(!s.merge_chat(chat("tel:+3", 200)));
        assert_eq!(s.chats[2].id.as_str(), "tel:+3");
        assert!(s.merge_chat(chat("tel:+9", 1)));
        assert_eq!(s.chats.len(), 5);
    }

    #[test]
    fn names_and_titles() {
        let c = contacts();
        assert_eq!(handle_name(&c, &Handle::new("bob@example.com")), "Bob Smith");
        assert_eq!(handle_name(&c, &Handle::new("+15550001111")), "Bob Smith");
        let mut shared = Handle::new("dan@example.com");
        shared.profile = Some(HandleProfile { name: Some("Danny".into()), ..Default::default() });
        assert_eq!(handle_name(&c, &shared), "Danny");
        assert_eq!(handle_name(&c, &Handle::new("+1 205 555 0100")), "+12055550100");
        let mut g = chat("group:G1", 1);
        g.participants = vec![Handle::new("bob@example.com"), Handle::new("carol@example.com"), shared];
        assert_eq!(chat_title(&c, &g), "Bob, Carol & Danny");
        g.display_name = Some(" Team ".into());
        assert_eq!(chat_title(&c, &g), "Team");
        assert_eq!(initials("Bob Smith"), "BS");
        assert_eq!(initials("Bob, Carol & Danny"), "BC");
        assert_eq!(initials("+12055550100"), "#");
        assert_eq!(initials("  "), "?");
        assert_eq!(initials("émile zola"), "ÉZ");
    }

    #[test]
    fn filter_previews_and_views() {
        let now = Local::now();
        let mut s = State { contacts: contacts(), ..Default::default() };
        let mut bob = chat("mailto:bob@example.com", now.timestamp_millis() as u64);
        bob.unread_count = 2;
        s.set_chats(vec![bob, chat("mailto:carol@example.com", 5)]);
        assert_eq!(s.take_stale_previews(10).len(), 2);
        assert!(s.take_stale_previews(10).is_empty());
        let mut m = message("M1", "mailto:carol@example.com", 5, "lunch  at\nnoon?");
        m.is_from_me = true;
        s.preview_done("mailto:carol@example.com", Some(m));
        s.preview_done("mailto:bob@example.com", None);
        let v = s.chat_views(now, Instant::now());
        assert_eq!(v[0].title, "Bob Smith");
        assert_eq!(v[0].unread, 2);
        assert_eq!(v[0].time.len(), 5); // HH:MM today
        assert_eq!(v[1].snippet, "You: lunch at noon?");
        s.filter = "noon".into();
        assert_eq!(s.chat_views(now, Instant::now()).len(), 1);
        s.filter = "BOB".into();
        assert_eq!(s.chat_views(now, Instant::now())[0].id, "mailto:bob@example.com");
        assert_eq!(s.total_unread(), 2);
    }

    #[test]
    fn paging_live_merge_and_resync() {
        let c = "group:G1";
        let mut s = State::default();
        s.set_chats(vec![chat(c, 10)]);
        s.open_chat(c);
        assert_eq!(s.begin_older(), None); // not loaded yet
        // A live message while the first page is in flight is kept.
        assert_eq!(s.merge_message(message("M9", c, 900, "live"), false), Change::None);
        assert_eq!(s.apply_page(c, page(vec![message("M5", c, 500, "a"), message("M6", c, 600, "b")], Some("cur")), false), Change::Reset);
        let ids = |s: &State| s.open.as_ref().unwrap().messages.iter().map(|m| m.id.0.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&s), ["M5", "M6", "M9"]);
        // Older page.
        assert_eq!(s.begin_older(), Some((c.to_string(), "cur".to_string())));
        assert_eq!(s.begin_older(), None);
        assert_eq!(s.apply_page(c, page(vec![message("M1", c, 100, "old"), message("M5", c, 500, "a")], None), true), Change::Prepended);
        assert_eq!(ids(&s), ["M1", "M5", "M6", "M9"]);
        assert!(!s.has_older());
        // Live append, update, duplicate.
        assert_eq!(s.merge_message(message("M10", c, 1000, "new"), false), Change::Appended);
        let mut upd = message("M6", c, 600, "b");
        upd.edited = true;
        assert_eq!(s.merge_message(upd.clone(), true), Change::Updated);
        assert_eq!(s.merge_message(upd, true), Change::None);
        assert_eq!(s.merge_message(message("MX", c, 1, "unknown page"), true), Change::None);
        assert_eq!(s.merge_message(message("Z", "tel:+1", 1, "other chat"), false), Change::None);
        // Resync: the newest page again; the older page stays.
        assert_eq!(s.reload_open().as_deref(), Some(c));
        assert_eq!(s.apply_page(c, page(vec![message("M9", c, 900, "live"), message("M10", c, 1000, "new"), message("M11", c, 1100, "missed")], Some("c2")), false), Change::Reset);
        assert_eq!(ids(&s), ["M1", "M5", "M6", "M9", "M10", "M11"]);
        assert!(!s.has_older()); // kept the older cursor state (none left)
        // Pages for a chat that is no longer open are dropped.
        assert_eq!(s.apply_page("tel:+1", page(vec![], None), false), Change::None);
    }

    #[test]
    fn grouping_delivery_reactions_and_replies() {
        let c = "group:G1";
        let mut s = State { contacts: contacts(), ..Default::default() };
        s.me.insert("mailto:me@example.com".into());
        s.set_chats(vec![chat(c, 10)]);
        s.open_chat(c);
        let t0 = 1_700_000_000_000u64;
        let a = message("A", c, t0, "hi");
        let b = message("B", c, t0 + 60_000, "there");
        let mut r1 = message("R1", c, t0 + 120_000, "yo");
        r1.is_from_me = true;
        r1.sender = Handle::new("me@example.com");
        r1.delivery = DeliveryState::Failed;
        r1.reply_to = Some(MessageId("A".into()));
        let mut r2 = message("R2", c, t0 + 130_000, "https://example.com/x");
        r2.is_from_me = true;
        r2.delivery = DeliveryState::Read;
        r2.parts.push(MessagePart::Link {
            url: "https://example.com/x".into(),
            original_url: None,
            title: Some("A story".into()),
            summary: None,
            site_name: None,
            image_attachment_id: None,
            icon_attachment_id: None,
        });
        r2.reactions = vec![
            Reaction { kind: ReactionKind::Love, sender: Handle::new("bob@example.com"), added: true },
            Reaction { kind: ReactionKind::Love, sender: Handle::new("carol@example.com"), added: true },
            Reaction { kind: ReactionKind::Laugh, sender: Handle::new("dan@x.y"), added: false },
        ];
        let mut late = message("L", c, t0 + 2 * 3_600_000, "later");
        late.parts.push(MessagePart::Attachment { attachment_id: imessage_common::model::AttachmentId("L:1".into()), mime: "image/jpeg".into(), filename: "a.jpg".into(), size: 10 });
        late.parts.push(MessagePart::Attachment { attachment_id: imessage_common::model::AttachmentId("L:2".into()), mime: "image/heic".into(), filename: "b.heic".into(), size: 10 });
        let mut gone = message("U", c, t0 + 2 * 3_600_000 + 1000, "x");
        gone.unsent = true;
        gone.text = None;
        gone.parts.clear();
        s.apply_page(c, page(vec![a, b, r1, r2, late, gone], None), false);
        let v = s.transcript(Local::now());
        assert!(v[0].header.is_some() && v[0].first_in_run && !v[0].last_in_run);
        assert_eq!(v[0].sender.as_deref(), Some("Bob Smith"));
        assert!(v[1].sender.is_none() && v[1].last_in_run && v[1].header.is_none());
        // Group avatars: room on every incoming row, drawn by the run's last.
        assert!(v[0].in_group && !v[0].show_avatar && v[1].in_group && v[1].show_avatar);
        assert_eq!((v[1].who.as_str(), v[1].sender_initials.as_str()), ("Bob Smith", "BS"));
        assert_eq!(v[1].sender_tint, tint_index(&Handle::new("bob@example.com").uri));
        assert!(v[2].from_me && v[2].first_in_run && v[2].sender.is_none());
        assert!(!v[2].in_group && !v[2].show_avatar && v[2].who == "You");
        assert_eq!(v[2].delivery.as_deref(), Some("Not delivered"));
        assert!(v[2].failed && !v[3].failed);
        assert_eq!(v[2].reply_quote.as_deref(), Some("Bob Smith: hi"));
        // Link-only text shows the card alone; the last outgoing gets its label.
        assert_eq!(v[3].text, None);
        assert_eq!(v[3].parts, vec![PartView::Link { url: "https://example.com/x".into(), title: "A story".into(), site: "example.com".into(), image: None }]);
        assert_eq!(v[3].delivery.as_deref(), Some("Read"));
        assert_eq!(
            v[3].reactions,
            vec![ReactionView { kind: "love".into(), label: "Love".into(), count: 2, mine: false, senders: "Bob, Carol".into() }]
        );
        assert!(v[3].can_react && v[3].my_reaction.is_none());
        // A long gap: a new header and run; JPEG inline, HEIC as a file.
        assert!(v[4].header.is_some() && v[4].first_in_run);
        assert!(matches!(v[4].parts[0], PartView::Image { .. }));
        assert!(matches!(&v[4].parts[1], PartView::File { mime, .. } if mime == "image/heic"));
        assert!(v[5].unsent && v[5].text.is_none() && v[5].delivery.is_none());
        assert!(!v[5].can_react && !v[5].show_avatar);
    }

    #[test]
    fn link_preview_picture() {
        let c = "mailto:eve@example.org";
        let mut s = State::default();
        s.set_chats(vec![chat(c, 10)]);
        s.open_chat(c);
        let mut m = message("E", c, 1_000, "https://example.org/a");
        m.parts.push(MessagePart::Link {
            url: "https://example.org/a".into(),
            original_url: None,
            title: Some("A".into()),
            summary: None,
            site_name: Some("Example".into()),
            image_attachment_id: Some(imessage_common::model::AttachmentId("E:link-image".into())),
            icon_attachment_id: None,
        });
        s.apply_page(c, page(vec![m], None), false);
        let v = s.transcript(Local::now());
        assert_eq!(v[0].parts, vec![PartView::Link { url: "https://example.org/a".into(), title: "A".into(), site: "Example".into(), image: Some("E:link-image".into()) }]);
        // 1:1 chats: no group avatar gutter.
        assert!(!v[0].in_group && !v[0].show_avatar);
    }

    #[test]
    fn sections_only_when_something_is_pinned() {
        let now = Local::now();
        let mut s = State::default();
        s.set_chats(vec![chat("tel:+1", 1), chat("tel:+2", 2)]);
        assert!(s.chat_views(now, Instant::now()).iter().all(|v| v.section.is_none()));
        let mut p = chat("tel:+3", 3);
        p.pinned = true;
        s.merge_chat(p);
        let v = s.chat_views(now, Instant::now());
        let sections: Vec<Option<&str>> = v.iter().map(|v| v.section).collect();
        assert_eq!(sections, [Some("Pinned"), Some("Conversations"), None]);
        // Everything pinned: no "Conversations" header.
        for c in s.chats.iter_mut() {
            c.pinned = true;
        }
        let sections: Vec<Option<&str>> = s.chat_views(now, Instant::now()).iter().map(|v| v.section).collect();
        assert_eq!(sections, [Some("Pinned"), None, None]);
        // A search that hides the pinned chats hides the headers too.
        s.chats[0].pinned = false;
        s.filter = s.chats[0].id.0.clone();
        assert!(s.chat_views(now, Instant::now()).iter().all(|v| v.section.is_none()));
    }

    #[test]
    fn avatar_tints_are_stable() {
        // Deterministic across runs (no random hasher) and case-insensitive.
        assert_eq!(tint_index("mailto:bob@example.com"), tint_index("MAILTO:Bob@Example.com"));
        let all: HashSet<i32> = (0..200).map(|i| tint_index(&format!("tel:+1555000{i:04}"))).collect();
        assert!(all.iter().all(|t| (0..TINTS as i32).contains(t)));
        assert_eq!(all.len(), TINTS as usize, "all tints get used");
        // Pinned values: a change would recolour everyone's avatars.
        assert_eq!(tint_index(""), (0x811c_9dc5u32 % TINTS) as i32);
        let s = State { chats: vec![chat("mailto:bob@example.com", 1)], ..Default::default() };
        assert_eq!(s.chat_views(Local::now(), Instant::now())[0].tint, tint_index("mailto:bob@example.com"));
    }

    #[test]
    fn own_handle_and_status_line() {
        let mut s = State::default();
        assert_eq!(s.own_handle(), None);
        let mut st: Status = serde_json::from_value(serde_json::json!({
            "version": "1", "connection": {"state": "connected"}, "registered": true, "unread": null,
            "registration": {"handles": [{"uri": "mailto:me@example.com"}, {"uri": "tel:+15550001111"}], "registered_at": null, "expires_at": null}
        }))
        .unwrap();
        s.set_status(st.clone());
        assert_eq!(s.own_handle().as_deref(), Some("me@example.com"));
        st.registration.sender = Some(Handle::new("tel:+15550001111"));
        s.set_status(st);
        assert_eq!(s.own_handle().as_deref(), Some("+15550001111"));

        let c = ConnectionState::Connected;
        assert_eq!(status_line(true, true, Some(&c), true, Some("me@example.com")), ("Connected as me@example.com".into(), StatusLevel::Ok));
        assert_eq!(status_line(true, true, Some(&c), true, None).1, StatusLevel::Ok);
        assert_eq!(status_line(false, false, Some(&c), true, None), ("Connecting to the bridge…".into(), StatusLevel::Busy));
        assert_eq!(status_line(false, true, Some(&c), true, None), ("Bridge reconnecting…".into(), StatusLevel::Busy));
        assert_eq!(status_line(true, true, Some(&c), false, None).1, StatusLevel::Bad);
        assert_eq!(status_line(true, true, None, true, None).1, StatusLevel::Busy);
        assert_eq!(status_line(true, true, Some(&ConnectionState::NeedsTwoFactor), true, None), ("Verification code needed".into(), StatusLevel::Bad));
        assert_eq!(parse_connection_state("needs_login"), Some(ConnectionState::NeedsLogin));
        assert_eq!(parse_connection_state("connected"), Some(ConnectionState::Connected));
        assert_eq!(parse_connection_state("error"), None);
        assert_eq!(parse_connection_state("bogus"), None);
    }

    #[test]
    fn reactions_mine_counts_and_toggle() {
        let c = "group:G1";
        let mut s = State { contacts: contacts(), ..Default::default() };
        s.me.insert("mailto:me@example.com".into());
        s.set_chats(vec![chat(c, 10)]);
        s.open_chat(c);
        let me = Handle::new("mailto:me@example.com");
        let mut a = message("A", c, 1_000, "tent?");
        a.reactions = vec![
            Reaction { kind: ReactionKind::Like, sender: Handle::new("carol@example.com"), added: true },
            Reaction { kind: ReactionKind::Like, sender: me.clone(), added: true },
            Reaction { kind: ReactionKind::Emoji("🔥".into()), sender: Handle::new("bob@example.com"), added: true },
        ];
        let mut b = message("B", c, 2_000, "fire");
        b.reactions = vec![Reaction { kind: ReactionKind::Emoji("🔥".into()), sender: me.clone(), added: true }];
        let mut sms = message("S", c, 3_000, "sms");
        sms.service = Service::Sms;
        let mut gone = message("U", c, 4_000, "x");
        gone.unsent = true;
        s.apply_page(c, page(vec![a, b, sms, gone], None), false);
        let v = s.transcript(Local::now());
        // Mine first among the senders; the chip is mine when any is.
        assert_eq!(v[0].reactions[0], ReactionView { kind: "like".into(), label: "Like".into(), count: 2, mine: true, senders: "You, Carol".into() });
        assert_eq!(v[0].reactions[1], ReactionView { kind: "🔥".into(), label: "🔥".into(), count: 1, mine: false, senders: "Bob".into() });
        assert_eq!(v[0].my_reaction.as_deref(), Some("like"));
        assert_eq!(v[1].my_reaction.as_deref(), Some("🔥"));
        assert!(v[0].can_react && !v[2].can_react && !v[3].can_react);
        let open = &s.open.as_ref().unwrap().messages;
        assert_eq!(s.my_reaction(&open[0]).as_deref(), Some("like"));
        assert_eq!(s.my_reaction(&open[2]), None);

        // Same kind again takes it back; another kind (or an emoji) replaces it.
        assert_eq!(s.reaction_request("A", "like"), Some((c.to_string(), "like".to_string(), true)));
        assert_eq!(s.reaction_request("A", "Love"), Some((c.to_string(), "love".to_string(), false)));
        assert_eq!(s.reaction_request("A", " 🎉 "), Some((c.to_string(), "🎉".to_string(), false)));
        assert_eq!(s.reaction_request("B", "🔥"), Some((c.to_string(), "🔥".to_string(), true)));
        assert_eq!(s.reaction_request("B", "like"), Some((c.to_string(), "like".to_string(), false)));
        // Not on SMS, unsent or unknown messages; not for plain text.
        assert_eq!(s.reaction_request("S", "love"), None);
        assert_eq!(s.reaction_request("U", "love"), None);
        assert_eq!(s.reaction_request("nope", "love"), None);
        assert_eq!(s.reaction_request("A", "lol"), None);
        assert_eq!(s.reaction_request("A", ""), None);
    }

    #[test]
    fn typing_expires_and_clears_on_message() {
        let mut s = State { contacts: contacts(), ..Default::default() };
        let t = Instant::now();
        s.set_typing("tel:+15550001111", "tel:+15550001111", true, t);
        assert_eq!(s.typing_in("tel:+15550001111", t).as_deref(), Some("Bob is typing…"));
        assert_eq!(s.typing_in("tel:+15550001111", t + TYPING_TTL), None);
        s.merge_message(message("M", "tel:+15550001111", 1, "hi"), false);
        assert_eq!(s.typing_in("tel:+15550001111", t), None);
        s.set_typing("c", "x", true, t);
        s.set_typing("c", "y", false, t);
        assert!(s.typing_in("c", t).is_some());
        s.set_typing("c", "x", false, t);
        assert!(s.typing_in("c", t).is_none());
    }

    #[test]
    fn recipients_and_suggestions() {
        let s = State { contacts: contacts(), ..Default::default() };
        assert_eq!(s.resolve_recipients("bob smith, +1 (205) 555-0100; Carol").unwrap(), ["tel:+15550001111", "tel:+12055550100", "mailto:carol@example.com"]);
        assert_eq!(s.resolve_recipients("bob@example.com, Bob@Example.com").unwrap(), ["mailto:bob@example.com"]);
        assert!(s.resolve_recipients("nobody").is_err());
        assert!(s.resolve_recipients(" , ").is_err());
        assert_eq!(s.contact_suggestions("x@y.z, car", 5), vec![("Carol".to_string(), "carol@example.com".to_string())]);
        assert_eq!(s.contact_suggestions("bob", 5).len(), 2);
        assert!(s.contact_suggestions("", 5).is_empty());
    }

    #[test]
    fn small_helpers() {
        assert_eq!(clean_text("\u{fffc} look\u{fffc} "), "look");
        assert_eq!(guess_mime("A.JPG"), Some("image/jpeg"));
        assert_eq!(guess_mime("x"), None);
        assert!(inline_image("image/png") && !inline_image("image/heic"));
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2 KB");
        assert_eq!(human_size(5 * 1024 * 1024 + 1), "5.0 MB");
        assert_eq!(effect_name("com.apple.messages.effect.CKConfettiEffect"), "confetti");
        assert_eq!(connection_label("needs_two_factor"), "needs a 2FA code");
        assert_eq!(connection_label("connected"), "connected");
        let now = Local.with_ymd_and_hms(2026, 10, 22, 12, 0, 0).unwrap();
        let ms = |d: DateTime<Local>| d.timestamp_millis() as u64;
        assert_eq!(list_time(ms(Local.with_ymd_and_hms(2026, 10, 22, 9, 5, 0).unwrap()), now), "09:05");
        assert_eq!(list_time(ms(Local.with_ymd_and_hms(2026, 10, 21, 9, 5, 0).unwrap()), now), "Yesterday");
        assert_eq!(list_time(ms(Local.with_ymd_and_hms(2026, 3, 2, 9, 5, 0).unwrap()), now), "2 Mar");
        assert_eq!(list_time(ms(Local.with_ymd_and_hms(2024, 3, 2, 9, 5, 0).unwrap()), now), "2 Mar 2024");
        assert_eq!(header_time(ms(Local.with_ymd_and_hms(2026, 10, 22, 9, 5, 0).unwrap()), now), "Today 09:05");
        assert_eq!(header_time(ms(Local.with_ymd_and_hms(2026, 3, 2, 9, 5, 0).unwrap()), now), "2 March 2026, 09:05");
        let mut m = message("M", "tel:+1", 1, "");
        m.text = None;
        m.parts = vec![MessagePart::Attachment { attachment_id: imessage_common::model::AttachmentId("M:1".into()), mime: "image/png".into(), filename: "p.png".into(), size: 1 }];
        assert_eq!(snippet(&m), "Photo");
        assert_eq!(tapback_kind("QUESTION").as_deref(), Some("question"));
        assert_eq!(tapback_kind("👍🏽").as_deref(), Some("👍🏽"));
        assert_eq!(tapback_kind("ok"), None);
    }
}
