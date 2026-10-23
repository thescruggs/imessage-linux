//! A runnable fake imsg-bridge with canned chats, for developing and
//! screenshotting the portable client without a daemon or an Apple ID
//! (tools/app-screenshots.sh, docs/PORTABLE-WINDOWS.md).
//!
//! ```console
//! $ cargo run -p imsg-remote --example fake_bridge [-- <port>]
//! imsg+bridge://127.0.0.1:41234#Tq3x…
//! ```
//!
//! The pairing string is the first line on stdout. The fixture mirrors
//! kmessages/tests/fake_daemon.py: Bob (30 messages over three days, a PNG
//! with my "love" tapback, pinned), the "Weekend Trip" group (👍×2 + 🔥,
//! edited / delivered sent messages, a PDF, a mention, group photo), Dave
//! (unverified sender, an unsent message), Eve (link previews, a failed
//! send) and Alex (a text-message chat).
//!
//! Lines on stdin inject live events:
//!   recv <chat id> <text>        an incoming message from the chat's first participant
//!   typing <chat id> on|off      a typing indicator
//!
//! `FAKE_BRIDGE_TYPING=<chat id>`: every event stream starts with that
//! chat's first participant typing (screenshots).

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{Message as WsMessage, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::sync::broadcast;

const TOKEN: &str = "Tq3xY0abcdefghijklmnopqrstuvwxyz-_ABCDEFGHI";
const ME: &str = "mailto:me@example.com";
const BOB: &str = "mailto:bob@example.com";
const CAROL: &str = "mailto:carol@example.com";
const DAVE: &str = "tel:+15551234567";
const EVE: &str = "mailto:eve@example.org";
const ALEX: &str = "tel:+15559876543";
const TEAM: &str = "group:5B0E2C1A-3F4D-4E21-9A55-0C7E1B2D3F40";

const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

#[derive(Default)]
struct Store {
    /// Newest activity first when listed; kept in insertion order here.
    chats: Vec<Value>,
    /// Chat id -> messages, oldest first.
    messages: HashMap<String, Vec<Value>>,
    /// Attachment id -> (mime, bytes).
    files: HashMap<String, (String, Vec<u8>)>,
    /// Handle -> shared photo.
    photos: HashMap<String, Vec<u8>>,
    seq: u64,
}

struct Fake {
    store: Mutex<Store>,
    events: broadcast::Sender<String>,
}

type Shared = Arc<Fake>;

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A gradient PNG (`hue` picks the blue channel), like fake_daemon.py's.
fn png(width: u32, height: u32, hue: u8) -> Vec<u8> {
    let img = image::RgbImage::from_fn(width, height, |x, y| image::Rgb([(x * 255 / width) as u8, (y * 255 / height) as u8, hue]));
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Png).expect("PNG encodes");
    out.into_inner()
}

fn handle(uri: &str, display: Option<&str>) -> Value {
    let mut h = json!({ "uri": uri });
    if let Some(d) = display {
        h["display"] = json!(d);
    }
    h
}

fn chat(id: &str, participants: Vec<Value>, display_name: Option<&str>, is_group: bool) -> Value {
    json!({"id": id, "participants": participants, "display_name": display_name, "is_group": is_group,
           "last_message_at": null, "unread_count": 0, "pinned": false, "pin_order": null})
}

/// A `link` part; its image / icon are attachments without parts of their own.
fn link_part(mid: &str, url: &str, title: &str, summary: Option<&str>, site: Option<&str>, image: bool, icon: bool) -> Value {
    json!({"type": "link", "url": url, "original_url": null, "title": title, "summary": summary, "site_name": site,
           "image_attachment_id": image.then(|| format!("{mid}:link-image")),
           "icon_attachment_id": icon.then(|| format!("{mid}:link-icon"))})
}

/// What a message is made of, beyond its text.
#[derive(Default)]
struct Opts {
    from_me: bool,
    delivery: Option<&'static str>,
    parts: Option<Vec<Value>>,
    reactions: Vec<Value>,
    unverified: bool,
    edited: bool,
    sms: bool,
}

impl Store {
    fn guid(&mut self) -> String {
        let id = self.next_id();
        self.seq += 1;
        id
    }

    /// The id the next [`Self::msg`] gets (for its attachment ids).
    fn next_id(&self) -> String {
        format!("{:08X}-FAKE-4000-8000-{:012X}", self.seq + 1, self.seq + 1)
    }

    fn msg(&mut self, chat_id: &str, sender: &str, sent_at: u64, text: Option<&str>, o: Opts) -> String {
        let id = self.guid();
        let parts = o.parts.unwrap_or_else(|| text.map(|t| vec![json!({"type": "text", "text": t})]).unwrap_or_default());
        let m = json!({
            "id": id, "chat_id": chat_id, "sender": {"uri": if o.from_me { ME } else { sender }}, "sent_at": sent_at,
            "text": text, "parts": parts, "subject": null, "reply_to": null, "is_from_me": o.from_me,
            "delivery": o.delivery.unwrap_or("delivered"), "edited": o.edited, "unsent": false, "reactions": o.reactions,
            "effect": null, "verified": !o.unverified, "service": if o.sms { "sms" } else { "imessage" },
        });
        self.messages.entry(chat_id.to_string()).or_default().push(m);
        id
    }

    fn last_mut(&mut self, chat_id: &str) -> &mut Value {
        self.messages.get_mut(chat_id).and_then(|v| v.last_mut()).expect("a message")
    }

    fn chat_mut(&mut self, id: &str) -> Option<&mut Value> {
        self.chats.iter_mut().find(|c| c["id"] == id)
    }

    fn touch(&mut self, id: &str) {
        let last = self.messages.get(id).and_then(|v| v.iter().filter_map(|m| m["sent_at"].as_u64()).max());
        if let Some(c) = self.chat_mut(id) {
            c["last_message_at"] = json!(last);
        }
    }

    fn unread(&self) -> u64 {
        self.chats.iter().filter_map(|c| c["unread_count"].as_u64()).sum()
    }

    fn find_mut(&mut self, chat_id: &str, message_id: &str) -> Option<&mut Value> {
        self.messages.get_mut(chat_id)?.iter_mut().find(|m| m["id"] == message_id)
    }

    fn seed(&mut self, now: u64) {
        let pic = |att: &str, name: &str, mime: &str, size: u64| json!({"type": "attachment", "attachment_id": att, "mime": mime, "filename": name, "size": size});

        // Bob: 30 messages over three days, then a picture with my tapback.
        let mut bob = chat(BOB, vec![json!({"uri": BOB, "display": "Bob Smith", "profile": {"name": "Bob Smith", "photo": "profiles/bob.jpg", "updated_at": 1}})], None, false);
        bob["pinned"] = json!(true);
        bob["pin_order"] = json!(0);
        self.chats.push(bob);
        self.photos.insert(BOB.into(), png(160, 160, 90));
        let start = now - 2 * DAY - 3 * HOUR;
        for i in 0..30u64 {
            let from_me = i % 3 == 1;
            let t = if i < 27 { start + i * 97 * MINUTE } else { now - (30 - i) * 2 * MINUTE };
            let text = match i {
                27 => "Did you see the photos from the trip?".to_string(),
                28 => "Yes! They look amazing 😍 https://example.com/trip".to_string(),
                _ if from_me => format!("My reply {i:02}"),
                _ => format!("Bob message {i:02}"),
            };
            self.msg(BOB, BOB, t, Some(&text), Opts { from_me, delivery: Some(if from_me { "read" } else { "delivered" }), ..Default::default() });
        }
        let att = format!("{}:1", self.next_id());
        let beach = png(320, 200, 200);
        self.msg(BOB, BOB, now - MINUTE, Some("Here's the best one"), Opts {
            parts: Some(vec![json!({"type": "text", "text": "Here's the best one"}), pic(&att, "beach.png", "image/png", beach.len() as u64)]),
            reactions: vec![json!({"kind": "love", "sender": {"uri": ME}, "added": true})],
            ..Default::default()
        });
        self.files.insert(att, ("image/png".into(), beach));

        // The group.
        let mut team = chat(TEAM, vec![handle(BOB, Some("Bob Smith")), handle(CAROL, None), handle(DAVE, None)], Some("Weekend Trip"), true);
        team["icon_path"] = json!("group-photos/team.png");
        self.chats.push(team);
        self.photos.insert(TEAM.into(), png(160, 160, 30));
        let t0 = now - 5 * HOUR;
        self.msg(TEAM, CAROL, t0, Some("Who's bringing the tent?"), Opts::default());
        self.msg(TEAM, CAROL, t0 + MINUTE, Some("I can bring snacks"), Opts::default());
        self.msg(TEAM, DAVE, t0 + 2 * MINUTE, Some("I have a tent for 4"), Opts {
            reactions: vec![
                json!({"kind": "like", "sender": {"uri": CAROL}, "added": true}),
                json!({"kind": "like", "sender": {"uri": BOB}, "added": true}),
                json!({"kind": {"emoji": "🔥"}, "sender": {"uri": ME}, "added": true}),
            ],
            ..Default::default()
        });
        self.msg(TEAM, ME, t0 + 3 * MINUTE, Some("Perfect, I'll book the site"), Opts { from_me: true, ..Default::default() });
        self.msg(TEAM, ME, t0 + 4 * MINUTE, Some("Leaving Friday at 6"), Opts { from_me: true, edited: true, ..Default::default() });
        let att = format!("{}:1", self.next_id());
        let pdf = b"%PDF-1.4\n% fake packing list\n".repeat(64);
        self.msg(TEAM, CAROL, now - 20 * MINUTE, Some("Here's the packing list"), Opts {
            parts: Some(vec![json!({"type": "text", "text": "Here's the packing list"}), pic(&att, "packing-list.pdf", "application/pdf", 120_334)]),
            ..Default::default()
        });
        self.files.insert(att, ("application/pdf".into(), pdf));
        self.msg(TEAM, CAROL, now - 19 * MINUTE, Some("@Dave don't forget the stove"), Opts {
            parts: Some(vec![json!({"type": "mention", "handle": DAVE, "text": "@Dave"}), json!({"type": "text", "text": " don't forget the stove"})]),
            ..Default::default()
        });
        self.chat_mut(TEAM).unwrap()["unread_count"] = json!(2);

        // Dave: unknown sender, an unsent message.
        self.chats.push(chat(DAVE, vec![handle(DAVE, None)], None, false));
        self.msg(DAVE, DAVE, now - 3 * DAY, Some("Call me when you land"), Opts { unverified: true, ..Default::default() });
        self.msg(DAVE, ME, now - 3 * DAY + HOUR, None, Opts { from_me: true, ..Default::default() });
        self.last_mut(DAVE)["unsent"] = json!(true);
        self.msg(DAVE, DAVE, now - 2 * DAY, Some("Sure thing"), Opts { unverified: true, ..Default::default() });
        self.chat_mut(DAVE).unwrap()["unread_count"] = json!(1);

        // Eve: link previews (one URL-only with image + icon, one with text
        // and icon only), a failed app message.
        self.chats.push(chat(EVE, vec![handle(EVE, Some("Eve Adams"))], None, false));
        let typed = "https://example.org/plasma";
        let id = self.next_id();
        // The typed short URL redirected to the page.
        let mut link = link_part(&id, "https://www.example.org/blog/plasma-release", "Plasma: what's new this spring", Some("Highlights of the release, with screenshots."), Some("Example Blog"), true, true);
        link["original_url"] = json!(typed);
        self.msg(EVE, EVE, now - 10 * DAY, Some(typed), Opts {
            parts: Some(vec![json!({"type": "text", "text": typed}), link]),
            ..Default::default()
        });
        self.files.insert(format!("{id}:link-image"), ("image/png".into(), png(600, 315, 150)));
        self.files.insert(format!("{id}:link-icon"), ("image/png".into(), png(64, 64, 250)));
        let url = "https://cooking.example.net/pancakes";
        let text = format!("Saturday breakfast? {url}");
        let id = self.next_id();
        self.msg(EVE, ME, now - 10 * DAY + 5 * MINUTE, Some(&text), Opts {
            from_me: true,
            delivery: Some("read"),
            parts: Some(vec![json!({"type": "text", "text": text}), link_part(&id, url, "Fluffy pancakes", None, None, false, true)]),
            ..Default::default()
        });
        self.files.insert(format!("{id}:link-icon"), ("image/png".into(), png(64, 64, 40)));
        self.msg(EVE, ME, now - 9 * DAY, Some("Happy birthday! 🎉"), Opts { from_me: true, delivery: Some("read"), ..Default::default() });
        self.msg(EVE, EVE, now - 9 * DAY + 30 * MINUTE, Some("Thank you!!"), Opts::default());
        self.msg(EVE, ME, now - 9 * DAY + 31 * MINUTE, None, Opts {
            from_me: true,
            delivery: Some("failed"),
            parts: Some(vec![json!({"type": "unsupported", "description": "app message"})]),
            ..Default::default()
        });

        // Alex: a text-message (SMS) chat.
        self.chats.push(chat(ALEX, vec![handle(ALEX, None)], None, false));
        let s = Opts { sms: true, ..Default::default() };
        self.msg(ALEX, ALEX, now - 26 * HOUR, Some("Hey, it's Alex – new number"), s);
        self.msg(ALEX, ME, now - 26 * HOUR + 3 * MINUTE, Some("Got it, saved!"), Opts { from_me: true, sms: true, delivery: Some("sent"), ..Default::default() });
        self.msg(ALEX, ALEX, now - 25 * HOUR, Some("Are we still on for lunch Thursday?"), Opts { sms: true, ..Default::default() });
        self.msg(ALEX, ME, now - 25 * HOUR + 2 * MINUTE, Some("Yes, 12:30 at the usual place"), Opts { from_me: true, sms: true, delivery: Some("sent"), ..Default::default() });

        let ids: Vec<String> = self.chats.iter().map(|c| c["id"].as_str().unwrap().to_string()).collect();
        for id in ids {
            self.touch(&id);
        }
    }
}

fn not_found(what: &str) -> Response {
    (StatusCode::NOT_FOUND, axum::Json(json!({"error": "NotFound", "message": what}))).into_response()
}

async fn auth(req: Request, next: Next) -> Response {
    let ok = req.headers().get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {TOKEN}"));
    if !ok {
        return (StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "Unauthorized", "message": "unknown token"}))).into_response();
    }
    next.run(req).await
}

async fn status(State(f): State<Shared>) -> Response {
    let unread = f.store.lock().unwrap().unread();
    axum::Json(json!({"version": "0.1.14-fake", "connection": {"state": "connected"}, "registered": true,
        "registration": {"handles": [{"uri": ME}], "registered_at": 1, "expires_at": 2}, "unread": unread,
        "sms": {"state": "active", "number": "+15550000000"}}))
    .into_response()
}

async fn chats(State(f): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let limit: usize = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(500);
    let offset: usize = q.get("offset").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut all = f.store.lock().unwrap().chats.clone();
    all.sort_by_key(|c| std::cmp::Reverse(c["last_message_at"].as_u64().unwrap_or(0)));
    axum::Json(all.into_iter().skip(offset).take(limit).collect::<Vec<_>>()).into_response()
}

async fn get_chat(State(f): State<Shared>, Path(id): Path<String>) -> Response {
    match f.store.lock().unwrap().chat_mut(&id) {
        Some(c) => axum::Json(c.clone()).into_response(),
        None => not_found("no such chat"),
    }
}

/// Oldest first; `before` = index into the chat's messages.
async fn messages(State(f): State<Shared>, Path(id): Path<String>, Query(q): Query<HashMap<String, String>>) -> Response {
    let s = f.store.lock().unwrap();
    if !s.chats.iter().any(|c| c["id"] == id) {
        return (StatusCode::NOT_FOUND, axum::Json(json!({"error": "ChatNotFound", "message": format!("no chat {id}")}))).into_response();
    }
    let all = s.messages.get(&id).cloned().unwrap_or_default();
    let limit: usize = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(60);
    let end = q.get("before").and_then(|v| v.parse::<usize>().ok()).unwrap_or(all.len()).min(all.len());
    let start = end.saturating_sub(limit);
    let next = (start > 0).then(|| start.to_string());
    axum::Json(json!({"messages": &all[start..end], "next_cursor": next})).into_response()
}

async fn read(State(f): State<Shared>, Path(id): Path<String>) -> Response {
    let mut s = f.store.lock().unwrap();
    if let Some(c) = s.chat_mut(&id)
        && c["unread_count"] != 0
    {
        c["unread_count"] = json!(0);
        let _ = f.events.send(json!({"type": "chat_updated", "chat": c.clone()}).to_string());
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn no_content() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

fn reaction_kind(k: &str) -> Value {
    match k {
        "love" | "like" | "dislike" | "laugh" | "emphasize" | "question" => json!(k),
        other => json!({"emoji": other}),
    }
}

async fn react(State(f): State<Shared>, Path((chat_id, msg_id)): Path<(String, String)>, axum::Json(b): axum::Json<Value>) -> Response {
    let kind = b["kind"].as_str().unwrap_or_default().to_string();
    let remove = b["remove"].as_bool().unwrap_or(false);
    let mut s = f.store.lock().unwrap();
    let Some(m) = s.find_mut(&chat_id, &msg_id) else { return not_found("no such message") };
    // One tapback per person: a new one replaces mine.
    let mut rs: Vec<Value> = m["reactions"].as_array().cloned().unwrap_or_default();
    rs.retain(|r| r["sender"]["uri"] != ME);
    if !remove {
        rs.push(json!({"kind": reaction_kind(&kind), "sender": {"uri": ME}, "added": true}));
    }
    m["reactions"] = json!(rs);
    let _ = f.events.send(json!({"type": "message_updated", "chat_id": chat_id, "message": m.clone()}).to_string());
    StatusCode::NO_CONTENT.into_response()
}

fn chat_for(s: &Store, b: &Value) -> Option<String> {
    if let Some(id) = b["chat_id"].as_str() {
        return Some(id.to_string());
    }
    let p = b["participants"].as_array()?.first()?.as_str()?;
    let uri = if p.contains('@') { format!("mailto:{p}") } else { format!("tel:{}", p.chars().filter(|c| c.is_ascii_digit() || *c == '+').collect::<String>()) };
    s.chats.iter().any(|c| c["id"] == uri).then_some(uri)
}

fn sent(f: &Fake, s: &mut Store, chat_id: &str, text: Option<&str>, parts: Option<Vec<Value>>) -> Response {
    let sms = s.messages.get(chat_id).and_then(|v| v.last()).is_some_and(|m| m["service"] == "sms");
    let at = now_ms();
    let id = s.msg(chat_id, ME, at, text, Opts { from_me: true, parts, sms, ..Default::default() });
    s.touch(chat_id);
    let m = s.find_mut(chat_id, &id).cloned().unwrap();
    let _ = f.events.send(json!({"type": "message_received", "chat_id": chat_id, "message": m}).to_string());
    axum::Json(json!({"id": id, "chat_id": chat_id, "sent_at": at, "sender": {"uri": ME}, "service": if sms { "sms" } else { "imessage" }})).into_response()
}

async fn send(State(f): State<Shared>, axum::Json(b): axum::Json<Value>) -> Response {
    let text = b["text"].as_str().unwrap_or_default().to_string();
    if text == "__fail__" {
        return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(json!({"error": "Transient", "message": "try again"}))).into_response();
    }
    let mut s = f.store.lock().unwrap();
    let Some(chat_id) = chat_for(&s, &b) else { return not_found("no such chat (the fake only sends to existing chats)") };
    sent(&f, &mut s, &chat_id, Some(&text), None)
}

async fn send_file(State(f): State<Shared>, Query(q): Query<HashMap<String, String>>, body: Bytes) -> Response {
    let mut s = f.store.lock().unwrap();
    let Some(chat_id) = q.get("chat_id").cloned() else { return not_found("the fake only sends files to a chat_id") };
    let name = q.get("filename").cloned().unwrap_or_else(|| "file".into());
    let mime = q.get("mime").cloned().unwrap_or_else(|| "application/octet-stream".into());
    let att = format!("{}:1", s.guid());
    let part = json!({"type": "attachment", "attachment_id": att, "mime": mime, "filename": name, "size": body.len()});
    s.files.insert(att, (mime, body.to_vec()));
    sent(&f, &mut s, &chat_id, None, Some(vec![part]))
}

async fn attachment(State(f): State<Shared>, Path(id): Path<String>) -> Response {
    match f.store.lock().unwrap().files.get(&id) {
        Some((mime, data)) => ([("content-type", mime.clone())], data.clone()).into_response(),
        None => not_found("no such attachment"),
    }
}

async fn photo(State(f): State<Shared>, Path(h): Path<String>) -> Response {
    match f.store.lock().unwrap().photos.get(&h) {
        Some(p) => ([("content-type", "image/png")], p.clone()).into_response(),
        None => not_found("no shared photo"),
    }
}

async fn profile(State(f): State<Shared>, Path(h): Path<String>) -> Response {
    let s = f.store.lock().unwrap();
    let p = s.chats.iter().flat_map(|c| c["participants"].as_array().cloned().unwrap_or_default()).find(|p| p["uri"] == h).map(|p| p["profile"].clone());
    axum::Json(p.unwrap_or(Value::Null)).into_response()
}

async fn contacts(h: HeaderMap) -> Response {
    if h.get("if-none-match").and_then(|v| v.to_str().ok()) == Some("\"fake-1\"") {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    let people = json!([
        {"name": "Bob Smith", "phones": [], "emails": ["bob@example.com"]},
        {"name": "Carol Diaz", "phones": [], "emails": ["carol@example.com"]},
        {"name": "Dave Lee", "phones": ["+1 555 123 4567"], "emails": []},
        {"name": "Eve Adams", "phones": [], "emails": ["eve@example.org"]},
        {"name": "Alex Kim", "phones": ["(555) 987-6543"], "emails": []},
    ]);
    ([("etag", "\"fake-1\"")], axum::Json(people)).into_response()
}

async fn events(State(f): State<Shared>, ws: WebSocketUpgrade) -> Response {
    let mut rx = f.events.subscribe();
    ws.on_upgrade(move |mut socket| async move {
        let hello = json!({"type": "hello", "bridge": "0.1.14-fake", "daemon_connected": true}).to_string();
        if socket.send(WsMessage::Text(hello.into())).await.is_err() {
            return;
        }
        let typing = std::env::var("FAKE_BRIDGE_TYPING").ok().and_then(|chat_id| {
            let s = f.store.lock().unwrap();
            let from = s.chats.iter().find(|c| c["id"] == chat_id.as_str())?["participants"][0]["uri"].as_str()?.to_string();
            Some(json!({"type": "typing", "chat_id": chat_id, "handle": from, "typing": true}).to_string())
        });
        if let Some(t) = typing
            && socket.send(WsMessage::Text(t.into())).await.is_err()
        {
            return;
        }
        loop {
            tokio::select! {
                frame = rx.recv() => match frame {
                    Ok(t) => if socket.send(WsMessage::Text(t.into())).await.is_err() { return },
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                },
                incoming = socket.recv() => if !matches!(incoming, Some(Ok(_))) { return },
            }
        }
    })
}

/// `recv <chat id> <text>` / `typing <chat id> on|off` from stdin.
async fn stdin_events(f: Shared) {
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut it = line.splitn(3, ' ');
        let (cmd, chat_id, rest) = (it.next().unwrap_or(""), it.next().unwrap_or("").to_string(), it.next().unwrap_or(""));
        let mut s = f.store.lock().unwrap();
        let Some(from) = s.chat_mut(&chat_id).and_then(|c| c["participants"][0]["uri"].as_str().map(str::to_string)) else {
            eprintln!("fake_bridge: no chat {chat_id:?}");
            continue;
        };
        match cmd {
            "recv" => {
                let sms = s.messages.get(&chat_id).and_then(|v| v.last()).is_some_and(|m| m["service"] == "sms");
                let id = s.msg(&chat_id, &from, now_ms(), Some(rest), Opts { sms, ..Default::default() });
                s.touch(&chat_id);
                if let Some(c) = s.chat_mut(&chat_id) {
                    c["unread_count"] = json!(c["unread_count"].as_u64().unwrap_or(0) + 1);
                }
                let m = s.find_mut(&chat_id, &id).cloned().unwrap();
                let _ = f.events.send(json!({"type": "message_received", "chat_id": chat_id, "message": m}).to_string());
            }
            "typing" => {
                let _ = f.events.send(json!({"type": "typing", "chat_id": chat_id, "handle": from, "typing": rest != "off"}).to_string());
            }
            _ => eprintln!("fake_bridge: commands are `recv <chat> <text>` and `typing <chat> on|off`"),
        }
    }
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args().nth(1).and_then(|p| p.parse().ok()).unwrap_or(0);
    let mut store = Store::default();
    store.seed(now_ms());
    let fake = Arc::new(Fake { store: Mutex::new(store), events: broadcast::channel(64).0 });
    let app = Router::new()
        .route("/v1/status", get(status))
        .route("/v1/chats", get(chats))
        .route("/v1/chats/{id}", get(get_chat))
        .route("/v1/chats/{id}/messages", get(messages))
        .route("/v1/chats/{id}/messages/{msg}/react", post(react))
        .route("/v1/chats/{id}/typing", post(no_content))
        .route("/v1/chats/{id}/read", post(read))
        .route("/v1/chats/{id}/icon", get(photo))
        .route("/v1/handles/{h}/photo", get(photo))
        .route("/v1/handles/{h}/profile", get(profile))
        .route("/v1/attachments/{id}", get(attachment))
        .route("/v1/contacts", get(contacts))
        .route("/v1/send", post(send))
        .route("/v1/send-file", post(send_file))
        .route("/v1/events", get(events))
        .layer(middleware::from_fn(auth))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("bind 127.0.0.1");
    let addr = listener.local_addr().unwrap();
    println!("imsg+bridge://{addr}#{TOKEN}");
    tokio::spawn(stdin_events(fake));
    axum::serve(listener, app).await.expect("serve");
}
