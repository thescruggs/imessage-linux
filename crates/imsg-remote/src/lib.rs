//! Client for `imsg-bridge` (docs/BRIDGE.md): typed HTTP calls returning
//! the shared `imessage_common` model types, file transfers, and the
//! WebSocket event stream with reconnects. No D-Bus, no TLS (the bridge
//! listens inside the tailnet; WireGuard encrypts the traffic).
//!
//! ```no_run
//! # async fn demo() -> imsg_remote::Result<()> {
//! let p = imsg_remote::parse_pairing("imsg+bridge://100.64.0.10:7227#…").unwrap();
//! let client = imsg_remote::RemoteClient::new(&p.bridge_url, &p.token)?;
//! let chats = client.all_chats().await?;
//! let mut events = client.events();
//! while let Some(ev) = events.next_event().await { /* … */ }
//! # Ok(()) }
//! ```

mod error;
mod events;
mod pairing;

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use imessage_common::contacts::Person;
use imessage_common::model::{Chat, HandleProfile, Message, Service, SmsForwarding};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use tokio::io::AsyncWriteExt;

pub use error::{Error, Result};
pub use events::{EventOptions, EventStream, RemoteEvent};
pub use imessage_common::api::{MessagePage, Reachability, SentMessage, Status};
pub use pairing::{DEFAULT_PORT, Pairing, PairingError, parse_pairing, valid_token};

/// Everything but the unreserved characters `A-Z a-z 0-9 - . _ ~`
/// (docs/BRIDGE.md, Conventions: ids are one percent-encoded segment).
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

/// One path segment, percent-encoded.
pub fn encode_segment(s: &str) -> String {
    utf8_percent_encode(s, SEGMENT).to_string()
}

/// Daemon calls answer within 25 s (bridge timeout) plus some network.
const CALL_TIMEOUT: Duration = Duration::from_secs(35);
/// Sending a file, `SubmitTwoFactor` and downloads may take 120 s on the
/// daemon's side.
const SLOW_TIMEOUT: Duration = Duration::from_secs(130);
/// Streaming a large file to disk.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Who a send goes to: a known chat, or handles (one = 1:1, several = a
/// new or existing group).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Chat(String),
    Participants(Vec<String>),
}

/// `GET /v1/version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    pub api: u32,
    pub bridge: String,
    /// `None` if the daemon is not reachable.
    #[serde(default)]
    pub daemon: Option<String>,
}

/// `POST /v1/two-factor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwoFactorResult {
    Ok,
    /// Apple accepted the code but wants another one.
    AnotherCode,
}

/// One entry of `GET /v1/contacts` (the host's address book).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contact {
    pub name: String,
    #[serde(default)]
    pub phones: Vec<String>,
    #[serde(default)]
    pub emails: Vec<String>,
}

impl From<Contact> for Person {
    fn from(c: Contact) -> Person {
        Person { name: c.name, phones: c.phones, emails: c.emails }
    }
}

/// `GET /v1/contacts` with `If-None-Match`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContactsReply {
    /// 304: what the caller has is current.
    NotModified,
    Fresh { contacts: Vec<Contact>, etag: Option<String> },
}

/// A bridge connection: base URL + token. Cheap to clone (the HTTP
/// connection pool is shared).
#[derive(Clone)]
pub struct RemoteClient {
    http: reqwest::Client,
    /// `http://host:port`, no trailing slash, no `/v1`.
    base: Arc<str>,
    token: Arc<str>,
}

impl fmt::Debug for RemoteClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteClient").field("base", &self.base).field("token", &"<redacted>").finish()
    }
}

/// `http://host:port` from what the user typed (`host:port`, a trailing
/// `/` or `/v1`); `None` if it is not an `http(s)` URL.
pub fn normalize_base_url(url: &str) -> Option<String> {
    let mut u = url.trim().to_string();
    if !u.contains("://") {
        u = format!("http://{u}");
    }
    let lower = u.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return None;
    }
    while u.ends_with('/') {
        u.pop();
    }
    if let Some(s) = u.strip_suffix("/v1") {
        u = s.to_string();
    }
    let host = u.split_once("://").map(|(_, r)| r).unwrap_or("");
    if host.is_empty() || host.contains(['/', '?', '#', ' ']) {
        return None;
    }
    Some(u)
}

impl RemoteClient {
    /// A client for the bridge at `base_url` (`http://host:port`).
    pub fn new(base_url: &str, token: &str) -> Result<RemoteClient> {
        let base = normalize_base_url(base_url).ok_or_else(|| Error::new("InvalidUrl", format!("not a bridge URL: {base_url}")))?;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(60))
            // Inside the tailnet: never a system proxy.
            .no_proxy()
            .user_agent(concat!("imsg-remote/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::new("Transport", e.to_string()))?;
        Ok(RemoteClient { http, base: base.into(), token: token.into() })
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// `base/v1/<segments, each percent-encoded>`.
    fn url(&self, segments: &[&str]) -> String {
        let mut u = format!("{}/v1", self.base);
        for s in segments {
            u.push('/');
            u.push_str(&encode_segment(s));
        }
        u
    }

    fn req(&self, method: Method, segments: &[&str]) -> RequestBuilder {
        self.http.request(method, self.url(segments)).bearer_auth(&*self.token).timeout(CALL_TIMEOUT)
    }

    async fn send(rb: RequestBuilder) -> Result<Response> {
        let r = rb.send().await.map_err(Error::transport)?;
        if r.status().is_success() || r.status() == StatusCode::NOT_MODIFIED {
            return Ok(r);
        }
        let status = r.status().as_u16();
        let body = r.bytes().await.unwrap_or_default();
        Err(Error::from_reply(status, &body))
    }

    async fn json<T: DeserializeOwned>(rb: RequestBuilder) -> Result<T> {
        let r = Self::send(rb).await?;
        let body = r.bytes().await.map_err(Error::transport)?;
        serde_json::from_slice(&body).map_err(|e| Error::new("Decode", format!("unexpected reply from the bridge: {e}")))
    }

    async fn empty(rb: RequestBuilder) -> Result<()> {
        Self::send(rb).await.map(|_| ())
    }

    // ----- reading -----

    pub async fn version(&self) -> Result<Version> {
        Self::json(self.req(Method::GET, &["version"])).await
    }

    pub async fn status(&self) -> Result<Status> {
        Self::json(self.req(Method::GET, &["status"])).await
    }

    /// One page of chats, newest activity first (`limit` ≤ 500).
    pub async fn chats(&self, limit: u32, offset: u32) -> Result<Vec<Chat>> {
        Self::json(self.req(Method::GET, &["chats"]).query(&[("limit", limit), ("offset", offset)])).await
    }

    /// Every chat (pages of 500).
    pub async fn all_chats(&self) -> Result<Vec<Chat>> {
        let mut all = Vec::new();
        loop {
            let page = self.chats(500, all.len() as u32).await?;
            let n = page.len();
            all.extend(page);
            if n < 500 {
                return Ok(all);
            }
        }
    }

    pub async fn chat(&self, chat_id: &str) -> Result<Chat> {
        Self::json(self.req(Method::GET, &["chats", chat_id])).await
    }

    /// A page of a chat, oldest first; `before` = the previous page's
    /// `next_cursor` (`None` = the newest page).
    pub async fn messages(&self, chat_id: &str, before: Option<&str>, limit: u32) -> Result<MessagePage> {
        let mut rb = self.req(Method::GET, &["chats", chat_id, "messages"]).query(&[("limit", limit)]);
        if let Some(b) = before.filter(|b| !b.is_empty()) {
            rb = rb.query(&[("before", b)]);
        }
        Self::json(rb).await
    }

    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<Message>> {
        Self::json(self.req(Method::GET, &["search"]).query(&[("q", query)]).query(&[("limit", limit)])).await
    }

    pub async fn handle_profile(&self, handle: &str) -> Result<Option<HandleProfile>> {
        Self::json(self.req(Method::GET, &["handles", handle, "profile"])).await
    }

    pub async fn sms(&self) -> Result<SmsForwarding> {
        Self::json(self.req(Method::GET, &["sms"])).await
    }

    pub async fn lookup(&self, handles: &[String]) -> Result<Vec<Reachability>> {
        Self::json(self.req(Method::POST, &["lookup"]).json(&json!({ "handles": handles }))).await
    }

    /// The host's address book; pass the previous reply's `etag` to get
    /// `NotModified` when nothing changed.
    pub async fn contacts(&self, etag: Option<&str>) -> Result<ContactsReply> {
        let mut rb = self.req(Method::GET, &["contacts"]);
        if let Some(e) = etag {
            rb = rb.header("If-None-Match", e);
        }
        let r = Self::send(rb).await?;
        if r.status() == StatusCode::NOT_MODIFIED {
            return Ok(ContactsReply::NotModified);
        }
        let etag = r.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_string);
        let body = r.bytes().await.map_err(Error::transport)?;
        let contacts = serde_json::from_slice(&body).map_err(|e| Error::new("Decode", format!("unexpected contacts reply: {e}")))?;
        Ok(ContactsReply::Fresh { contacts, etag })
    }

    // ----- sending and changing -----

    pub async fn send_text(&self, to: &Target, text: &str, reply_to: Option<&str>, service: Option<Service>) -> Result<SentMessage> {
        let mut body = match to {
            Target::Chat(id) => json!({ "chat_id": id }),
            Target::Participants(p) => json!({ "participants": p }),
        };
        body["text"] = json!(text);
        if let Some(r) = reply_to.filter(|r| !r.is_empty()) {
            body["reply_to"] = json!(r);
        }
        body["service"] = json!(service.map(Service::as_str).unwrap_or("auto"));
        Self::json(self.req(Method::POST, &["send"]).json(&body)).await
    }

    fn send_file_req(&self, to: &Target, filename: &str, mime: Option<&str>, service: Option<Service>) -> RequestBuilder {
        let mut q: Vec<(&str, &str)> = Vec::new();
        match to {
            Target::Chat(id) => q.push(("chat_id", id)),
            Target::Participants(p) => q.extend(p.iter().map(|h| ("to", h.as_str()))),
        }
        q.push(("filename", filename));
        if let Some(m) = mime.filter(|m| !m.is_empty()) {
            q.push(("mime", m));
        }
        q.push(("service", service.map(Service::as_str).unwrap_or("auto")));
        self.req(Method::POST, &["send-file"]).query(&q).timeout(SLOW_TIMEOUT).header("Content-Type", "application/octet-stream")
    }

    /// Upload a file from memory.
    pub async fn send_file_bytes(&self, to: &Target, filename: &str, mime: Option<&str>, service: Option<Service>, data: Vec<u8>) -> Result<SentMessage> {
        Self::json(self.send_file_req(to, filename, mime, service).body(data)).await
    }

    /// Upload a file from disk, streamed. `filename` defaults to the
    /// file's name.
    pub async fn send_file(&self, to: &Target, path: &Path, filename: Option<&str>, mime: Option<&str>, service: Option<Service>) -> Result<SentMessage> {
        let file = tokio::fs::File::open(path).await.map_err(Error::io)?;
        let len = file.metadata().await.map_err(Error::io)?.len();
        let name = filename.map(str::to_string).unwrap_or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into()));
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
        let rb = self.send_file_req(to, &name, mime, service).header("Content-Length", len).body(body).timeout(TRANSFER_TIMEOUT);
        Self::json(rb).await
    }

    /// Add (`remove = false`) or take back a tapback: `love`, `like`,
    /// `dislike`, `laugh`, `emphasize`, `question`, or an emoji.
    pub async fn react(&self, chat_id: &str, message_id: &str, kind: &str, remove: bool) -> Result<()> {
        Self::empty(self.req(Method::POST, &["chats", chat_id, "messages", message_id, "react"]).json(&json!({ "kind": kind, "remove": remove }))).await
    }

    pub async fn edit(&self, chat_id: &str, message_id: &str, text: &str) -> Result<()> {
        Self::empty(self.req(Method::POST, &["chats", chat_id, "messages", message_id, "edit"]).json(&json!({ "text": text }))).await
    }

    pub async fn unsend(&self, chat_id: &str, message_id: &str) -> Result<()> {
        Self::empty(self.req(Method::POST, &["chats", chat_id, "messages", message_id, "unsend"])).await
    }

    pub async fn set_typing(&self, chat_id: &str, typing: bool) -> Result<()> {
        Self::empty(self.req(Method::POST, &["chats", chat_id, "typing"]).json(&json!({ "typing": typing }))).await
    }

    /// Mark incoming messages read up to and including `up_to` (`None` = all).
    pub async fn mark_read(&self, chat_id: &str, up_to: Option<&str>) -> Result<()> {
        Self::empty(self.req(Method::POST, &["chats", chat_id, "read"]).json(&json!({ "up_to": up_to.unwrap_or("") }))).await
    }

    pub async fn set_pinned(&self, chat_id: &str, pinned: bool, order: Option<u32>) -> Result<Chat> {
        Self::json(self.req(Method::POST, &["chats", chat_id, "pin"]).json(&json!({ "pinned": pinned, "order": order }))).await
    }

    pub async fn reorder_pins(&self, chat_ids: &[String]) -> Result<Vec<Chat>> {
        Self::json(self.req(Method::POST, &["pins"]).json(&json!({ "chat_ids": chat_ids }))).await
    }

    pub async fn two_factor(&self, code: &str) -> Result<TwoFactorResult> {
        #[derive(Deserialize)]
        struct R {
            result: String,
        }
        let r: R = Self::json(self.req(Method::POST, &["two-factor"]).json(&json!({ "code": code })).timeout(SLOW_TIMEOUT)).await?;
        Ok(if r.result == "two_factor_required" { TwoFactorResult::AnotherCode } else { TwoFactorResult::Ok })
    }

    // ----- files -----

    async fn get_file(&self, segments: &[&str]) -> Result<Option<Bytes>> {
        match Self::send(self.req(Method::GET, segments).timeout(SLOW_TIMEOUT)).await {
            Ok(r) => Ok(Some(r.bytes().await.map_err(Error::transport)?)),
            Err(e) if e.status == Some(404) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// An attachment, in memory (for pictures; see [`Self::download_to`]
    /// for large files).
    pub async fn download(&self, attachment_id: &str) -> Result<Bytes> {
        let r = Self::send(self.req(Method::GET, &["attachments", attachment_id]).timeout(SLOW_TIMEOUT)).await?;
        r.bytes().await.map_err(Error::transport)
    }

    /// Stream an attachment to `dest` (written to `dest.part`, then
    /// renamed, so a broken transfer leaves no half file under the real
    /// name). Returns the size.
    pub async fn download_to(&self, attachment_id: &str, dest: &Path) -> Result<u64> {
        let r = Self::send(self.req(Method::GET, &["attachments", attachment_id]).timeout(TRANSFER_TIMEOUT)).await?;
        let mut part = dest.as_os_str().to_owned();
        part.push(".part");
        let part = std::path::PathBuf::from(part);
        let result = async {
            let mut f = tokio::fs::File::create(&part).await.map_err(Error::io)?;
            let mut n = 0u64;
            let mut s = r.bytes_stream();
            while let Some(chunk) = s.next().await {
                let chunk = chunk.map_err(Error::transport)?;
                n += chunk.len() as u64;
                f.write_all(&chunk).await.map_err(Error::io)?;
            }
            f.flush().await.map_err(Error::io)?;
            f.sync_all().await.map_err(Error::io)?;
            drop(f);
            tokio::fs::rename(&part, dest).await.map_err(Error::io)?;
            Ok(n)
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&part).await;
        }
        result
    }

    /// The photo a contact shared, `None` if there is none.
    pub async fn handle_photo(&self, handle: &str) -> Result<Option<Bytes>> {
        self.get_file(&["handles", handle, "photo"]).await
    }

    /// A group's photo, `None` if there is none.
    pub async fn chat_icon(&self, chat_id: &str) -> Result<Option<Bytes>> {
        self.get_file(&["chats", chat_id, "icon"]).await
    }

    pub async fn chat_background(&self, chat_id: &str) -> Result<Option<Bytes>> {
        self.get_file(&["chats", chat_id, "background"]).await
    }

    // ----- events -----

    /// `ws://host:port/v1/events`.
    pub fn events_url(&self) -> String {
        let base = if let Some(r) = self.base.strip_prefix("https://") {
            format!("wss://{r}")
        } else {
            format!("ws://{}", self.base.strip_prefix("http://").unwrap_or(&self.base))
        };
        format!("{base}/v1/events")
    }

    /// The live event stream (must be called inside a tokio runtime).
    /// It starts with [`RemoteEvent::Connected`] once the bridge said
    /// hello and reconnects by itself (1 s doubling to 30 s), emitting
    /// [`RemoteEvent::Lost`] and later `Connected` again.
    pub fn events(&self) -> EventStream {
        self.events_with(EventOptions::default())
    }

    pub fn events_with(&self, opts: EventOptions) -> EventStream {
        EventStream::start(self.events_url(), self.token.to_string(), opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_are_percent_encoded() {
        assert_eq!(encode_segment("group:ABC-1"), "group%3AABC-1");
        assert_eq!(encode_segment("tel:+15551234567"), "tel%3A%2B15551234567");
        assert_eq!(encode_segment("mailto:a@b.c"), "mailto%3Aa%40b.c");
        assert_eq!(encode_segment("0F1E-AB:1"), "0F1E-AB%3A1");
        assert_eq!(encode_segment("0F1E:link-image"), "0F1E%3Alink-image");
        assert_eq!(encode_segment("group-p:mailto:a@x,tel:+1555"), "group-p%3Amailto%3Aa%40x%2Ctel%3A%2B1555");
        assert_eq!(encode_segment("a b/c?d#e%~_."), "a%20b%2Fc%3Fd%23e%25~_.");
        assert_eq!(encode_segment("Zoë"), "Zo%C3%AB");
    }

    #[test]
    fn urls() {
        let c = RemoteClient::new("http://100.64.0.10:7227/", "t").unwrap();
        assert_eq!(c.base_url(), "http://100.64.0.10:7227");
        assert_eq!(c.url(&["chats", "tel:+1555", "messages"]), "http://100.64.0.10:7227/v1/chats/tel%3A%2B1555/messages");
        assert_eq!(c.url(&["attachments", "AB-CD:2"]), "http://100.64.0.10:7227/v1/attachments/AB-CD%3A2");
        assert_eq!(c.events_url(), "ws://100.64.0.10:7227/v1/events");
        assert!(!format!("{c:?}").contains("\"t\""));
        assert_eq!(normalize_base_url("vash:7227/v1/").as_deref(), Some("http://vash:7227"));
        assert_eq!(normalize_base_url("https://vash.ts.net").as_deref(), Some("https://vash.ts.net"));
        assert_eq!(normalize_base_url("ftp://x"), None);
        assert_eq!(normalize_base_url("http://"), None);
        assert_eq!(normalize_base_url("http://a/b"), None);
        assert!(RemoteClient::new("nonsense://", "t").unwrap_err().is("InvalidUrl"));
        assert_eq!(RemoteClient::new("https://vash", "t").unwrap().events_url(), "wss://vash/v1/events");
    }
}
