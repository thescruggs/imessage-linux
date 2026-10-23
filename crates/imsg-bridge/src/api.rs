//! HTTP API v1 (docs/BRIDGE.md, Endpoints): thin handlers that call
//! `org.imessage.Daemon1` and return its JSON verbatim, plus the file,
//! upload and contacts endpoints. Every route sits behind
//! [`crate::auth::require_auth`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::StreamExt;
use imessage_common::platform::fs::{create_private_dir_all, private_options};
use imsg_client::Client;
use rand::RngCore as _;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt as _;

use crate::error::ApiError;
use crate::{Bridge, VERSION, files};

/// Ordinary daemon calls.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(25);
/// Sending a file, `SubmitTwoFactor`, `DownloadAttachment`.
pub const LONG_TIMEOUT: Duration = imsg_client::LONG_CALL_TIMEOUT;
/// After a send-file call timed out the daemon may still be reading the
/// upload; it is removed this much later instead of at once.
const UPLOAD_GRACE: Duration = Duration::from_secs(600);

type ApiResult = Result<Response, ApiError>;

pub fn router(bridge: Arc<Bridge>) -> Router {
    Router::new()
        .route("/v1/version", get(version))
        .route("/v1/status", get(status))
        .route("/v1/chats", get(chats))
        .route("/v1/chats/{chat_id}", get(chat))
        .route("/v1/chats/{chat_id}/messages", get(messages))
        .route("/v1/chats/{chat_id}/messages/{message_id}/react", post(react))
        .route("/v1/chats/{chat_id}/messages/{message_id}/edit", post(edit))
        .route("/v1/chats/{chat_id}/messages/{message_id}/unsend", post(unsend))
        .route("/v1/chats/{chat_id}/typing", post(typing))
        .route("/v1/chats/{chat_id}/read", post(read))
        .route("/v1/chats/{chat_id}/pin", post(pin))
        .route("/v1/chats/{chat_id}/icon", get(chat_icon))
        .route("/v1/chats/{chat_id}/background", get(chat_background))
        .route("/v1/pins", post(pins))
        .route("/v1/search", get(search))
        .route("/v1/send", post(send))
        .route("/v1/send-file", post(send_file))
        .route("/v1/lookup", post(lookup))
        .route("/v1/handles/{handle}/profile", get(profile))
        .route("/v1/handles/{handle}/photo", get(photo))
        .route("/v1/sms", get(sms))
        .route("/v1/two-factor", post(two_factor))
        .route("/v1/contacts", get(contacts))
        .route("/v1/attachments/{attachment_id}", get(attachment))
        .route("/v1/events", get(crate::events::ws))
        .fallback(|| async { ApiError::not_found("no such endpoint") })
        .layer(middleware::from_fn_with_state(bridge.clone(), crate::auth::require_auth))
        .with_state(bridge)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// One daemon call on the shared connection, bounded by `limit`.
async fn call<T, F, Fut>(b: &Bridge, limit: Duration, what: &str, f: F) -> Result<T, ApiError>
where
    F: FnOnce(Arc<Client>) -> Fut,
    Fut: Future<Output = zbus::Result<T>>,
{
    let c = b.daemon.client().await?;
    match tokio::time::timeout(limit, f(c)).await {
        Ok(r) => r.map_err(|e| imsg_client::map_zbus(e).into()),
        Err(_) => Err(ApiError::new("Timeout", format!("imessaged did not answer {what} within {} s (it may still finish)", limit.as_secs()))),
    }
}

/// The daemon's JSON reply, unchanged.
fn verbatim(raw: String) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], raw).into_response()
}

fn no_content() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

/// The query string's pairs, decoded (`+` = space); repeated keys kept.
fn query(uri: &Uri) -> Vec<(String, String)> {
    let decode = |s: &str| percent_encoding::percent_decode_str(&s.replace('+', " ")).decode_utf8_lossy().into_owned();
    uri.query()
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (decode(k), decode(v)),
            None => (decode(p), String::new()),
        })
        .collect()
}

fn param<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// `?<key>=` as a count: absent / 0 = `default`, capped at `max`.
fn count(q: &[(String, String)], key: &str, default: u32, max: u32) -> Result<u32, ApiError> {
    match param(q, key).filter(|v| !v.is_empty()) {
        None => Ok(default),
        Some(v) => match v.parse::<u32>() {
            Ok(0) => Ok(default),
            Ok(n) => Ok(n.min(max)),
            Err(_) => Err(ApiError::bad_request(format!("`{key}` must be a non-negative integer"))),
        },
    }
}

/// A JSON request body (an empty body counts as `{}`).
fn body_json<T: DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    let body: &[u8] = if body.iter().all(u8::is_ascii_whitespace) { b"{}" } else { body };
    serde_json::from_slice(body).map_err(|e| ApiError::bad_request(format!("bad JSON body: {e}")))
}

/// `(chat_id_or_empty, participants_json)` from a chat id or handles:
/// exactly one of the two.
fn target(chat_id: Option<&str>, participants: &[String]) -> Result<(String, String), ApiError> {
    match (chat_id.filter(|c| !c.is_empty()), participants.is_empty()) {
        (Some(c), true) => Ok((c.to_string(), String::new())),
        (None, false) => Ok((String::new(), serde_json::to_string(participants).expect("strings serialize"))),
        _ => Err(ApiError::bad_request("give either a chat id or participants")),
    }
}

fn parse_value(raw: &str) -> Result<Value, ApiError> {
    serde_json::from_str(raw).map_err(|e| ApiError::new("DaemonError", format!("imessaged sent malformed JSON ({e})")))
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

async fn version(State(b): State<Arc<Bridge>>) -> Response {
    let daemon = call(&b, Duration::from_secs(5), "Version", |c| async move { c.proxy.version().await }).await.ok();
    axum::Json(json!({"api": 1, "bridge": VERSION, "daemon": daemon})).into_response()
}

async fn status(State(b): State<Arc<Bridge>>) -> ApiResult {
    Ok(verbatim(call(&b, CALL_TIMEOUT, "GetStatus", |c| async move { c.proxy.get_status().await }).await?))
}

async fn chats(State(b): State<Arc<Bridge>>, uri: Uri) -> ApiResult {
    let q = query(&uri);
    let limit = count(&q, "limit", 50, 500)?;
    let offset = count(&q, "offset", 0, u32::MAX)?;
    Ok(verbatim(call(&b, CALL_TIMEOUT, "ListChats", |c| async move { c.proxy.list_chats(limit, offset).await }).await?))
}

async fn get_chat_raw(b: &Bridge, chat_id: String) -> Result<String, ApiError> {
    call(b, CALL_TIMEOUT, "GetChat", |c| async move { c.proxy.get_chat(&chat_id).await }).await
}

async fn chat(State(b): State<Arc<Bridge>>, Path(chat_id): Path<String>) -> ApiResult {
    Ok(verbatim(get_chat_raw(&b, chat_id).await?))
}

async fn messages(State(b): State<Arc<Bridge>>, Path(chat_id): Path<String>, uri: Uri) -> ApiResult {
    let q = query(&uri);
    let limit = count(&q, "limit", 60, 500)?;
    let before = param(&q, "before").unwrap_or("").to_string();
    Ok(verbatim(call(&b, CALL_TIMEOUT, "ListMessages", |c| async move { c.proxy.list_messages(&chat_id, &before, limit).await }).await?))
}

async fn search(State(b): State<Arc<Bridge>>, uri: Uri) -> ApiResult {
    let q = query(&uri);
    let limit = count(&q, "limit", 100, 500)?;
    let text = param(&q, "q").filter(|t| !t.is_empty()).ok_or_else(|| ApiError::bad_request("`q` is required"))?.to_string();
    Ok(verbatim(call(&b, CALL_TIMEOUT, "Search", |c| async move { c.proxy.search(&text, limit).await }).await?))
}

async fn profile(State(b): State<Arc<Bridge>>, Path(handle): Path<String>) -> ApiResult {
    Ok(verbatim(call(&b, CALL_TIMEOUT, "GetHandleProfile", |c| async move { c.proxy.get_handle_profile(&handle).await }).await?))
}

async fn sms(State(b): State<Arc<Bridge>>) -> ApiResult {
    Ok(verbatim(call(&b, CALL_TIMEOUT, "GetSmsForwarding", |c| async move { c.proxy.get_sms_forwarding().await }).await?))
}

// ---------------------------------------------------------------------------
// Sending and changing
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SendReq {
    chat_id: Option<String>,
    #[serde(default)]
    participants: Vec<String>,
    text: String,
    reply_to: Option<String>,
    service: Option<String>,
}

async fn send(State(b): State<Arc<Bridge>>, body: Bytes) -> ApiResult {
    let r: SendReq = body_json(&body)?;
    let (chat, participants) = target(r.chat_id.as_deref(), &r.participants)?;
    let (reply_to, service) = (r.reply_to.unwrap_or_default(), r.service.unwrap_or_default());
    let raw = call(&b, CALL_TIMEOUT, "SendTextAs", |c| async move { c.proxy.send_text_as(&chat, &participants, &r.text, &reply_to, &service).await }).await?;
    Ok(verbatim(raw))
}

/// A private upload directory, removed when dropped (at once, or after
/// [`UPLOAD_GRACE`] if the daemon may still be reading it).
struct UploadDir {
    dir: PathBuf,
    grace: bool,
}

impl Drop for UploadDir {
    fn drop(&mut self) {
        let dir = std::mem::take(&mut self.dir);
        if self.grace
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            rt.spawn(async move {
                tokio::time::sleep(UPLOAD_GRACE).await;
                let _ = tokio::fs::remove_dir_all(&dir).await;
            });
            return;
        }
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            log::warn!("cannot remove upload {}: {e}", dir.display());
        }
    }
}

async fn send_file(State(b): State<Arc<Bridge>>, uri: Uri, headers: HeaderMap, body: Body) -> ApiResult {
    let q = query(&uri);
    let to: Vec<String> = q.iter().filter(|(k, _)| k == "to").map(|(_, v)| v.clone()).filter(|v| !v.is_empty()).collect();
    let (chat, participants) = target(param(&q, "chat_id"), &to)?;
    let filename = files::sanitize_filename(param(&q, "filename").unwrap_or("file"));
    let mime = param(&q, "mime").filter(|m| !m.is_empty()).map(str::to_string).or_else(|| files::mime_for(&filename).map(|m| m.split(';').next().unwrap_or(m).to_string())).unwrap_or_default();
    let service = param(&q, "service").unwrap_or("").to_string();
    let limit = b.cfg.max_upload_bytes();
    let too_large = || ApiError::new("TooLarge", format!("uploads are limited to {} MiB", b.cfg.max_upload_mib));
    let declared = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > limit) {
        return Err(too_large());
    }

    // uploads/<random>/<filename>: the daemon takes the attachment's name
    // from the path, and concurrent uploads of the same name must not clash.
    let mut rnd = [0u8; 12];
    rand::rng().fill_bytes(&mut rnd);
    let dir = b.paths.uploads_dir().join(rnd.iter().map(|x| format!("{x:02x}")).collect::<String>());
    create_private_dir_all(&dir).map_err(|e| ApiError::new("Store", format!("cannot create the upload directory: {e}")))?;
    let mut upload = UploadDir { dir: dir.clone(), grace: false };
    let path = dir.join(&filename);
    let file = private_options().write(true).create_new(true).open(&path).map_err(|e| ApiError::new("Store", format!("cannot create the upload: {e}")))?;
    let mut file = tokio::fs::File::from_std(file);
    let mut stream = body.into_data_stream();
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ApiError::bad_request(format!("upload interrupted: {e}")))?;
        total += chunk.len() as u64;
        if total > limit {
            return Err(too_large());
        }
        file.write_all(&chunk).await.map_err(|e| ApiError::new("Store", format!("cannot write the upload: {e}")))?;
    }
    file.flush().await.map_err(|e| ApiError::new("Store", e.to_string()))?;
    drop(file);

    let path_arg = path.to_string_lossy().into_owned();
    let r = call(&b, LONG_TIMEOUT, "SendFileAs", |c| async move { c.proxy.send_file_as(&chat, &participants, &path_arg, &mime, &service).await }).await;
    upload.grace = matches!(&r, Err(e) if e.name == "Timeout");
    drop(upload);
    Ok(verbatim(r?))
}

#[derive(Deserialize)]
struct ReactReq {
    kind: String,
    #[serde(default)]
    remove: bool,
}

async fn react(State(b): State<Arc<Bridge>>, Path((chat_id, message_id)): Path<(String, String)>, body: Bytes) -> ApiResult {
    let r: ReactReq = body_json(&body)?;
    call(&b, CALL_TIMEOUT, "React", |c| async move { c.proxy.react(&chat_id, &message_id, &r.kind, r.remove).await }).await?;
    Ok(no_content())
}

#[derive(Deserialize)]
struct EditReq {
    text: String,
}

async fn edit(State(b): State<Arc<Bridge>>, Path((chat_id, message_id)): Path<(String, String)>, body: Bytes) -> ApiResult {
    let r: EditReq = body_json(&body)?;
    call(&b, CALL_TIMEOUT, "Edit", |c| async move { c.proxy.edit(&chat_id, &message_id, &r.text).await }).await?;
    Ok(no_content())
}

async fn unsend(State(b): State<Arc<Bridge>>, Path((chat_id, message_id)): Path<(String, String)>) -> ApiResult {
    call(&b, CALL_TIMEOUT, "Unsend", |c| async move { c.proxy.unsend(&chat_id, &message_id).await }).await?;
    Ok(no_content())
}

#[derive(Deserialize)]
struct TypingReq {
    typing: bool,
}

async fn typing(State(b): State<Arc<Bridge>>, Path(chat_id): Path<String>, body: Bytes) -> ApiResult {
    let r: TypingReq = body_json(&body)?;
    call(&b, CALL_TIMEOUT, "SetTyping", |c| async move { c.proxy.set_typing(&chat_id, r.typing).await }).await?;
    Ok(no_content())
}

#[derive(Deserialize)]
struct ReadReq {
    up_to: Option<String>,
}

async fn read(State(b): State<Arc<Bridge>>, Path(chat_id): Path<String>, body: Bytes) -> ApiResult {
    let r: ReadReq = body_json(&body)?;
    let up_to = r.up_to.unwrap_or_default();
    call(&b, CALL_TIMEOUT, "MarkRead", |c| async move { c.proxy.mark_read(&chat_id, &up_to).await }).await?;
    Ok(no_content())
}

#[derive(Deserialize)]
struct PinReq {
    pinned: bool,
    order: Option<i64>,
}

async fn pin(State(b): State<Arc<Bridge>>, Path(chat_id): Path<String>, body: Bytes) -> ApiResult {
    let r: PinReq = body_json(&body)?;
    // Negative = "last for a new pin, unchanged for a pinned one" (DBUS.md).
    let order = r.order.map(|o| o.clamp(-1, i64::from(i32::MAX)) as i32).unwrap_or(-1);
    Ok(verbatim(call(&b, CALL_TIMEOUT, "SetChatPinned", |c| async move { c.proxy.set_chat_pinned(&chat_id, r.pinned, order).await }).await?))
}

#[derive(Deserialize)]
struct PinsReq {
    chat_ids: Vec<String>,
}

async fn pins(State(b): State<Arc<Bridge>>, body: Bytes) -> ApiResult {
    let r: PinsReq = body_json(&body)?;
    let arg = serde_json::to_string(&r.chat_ids).expect("strings serialize");
    Ok(verbatim(call(&b, CALL_TIMEOUT, "ReorderPins", |c| async move { c.proxy.reorder_pins(&arg).await }).await?))
}

#[derive(Deserialize)]
struct LookupReq {
    handles: Vec<String>,
}

async fn lookup(State(b): State<Arc<Bridge>>, body: Bytes) -> ApiResult {
    let r: LookupReq = body_json(&body)?;
    let arg = serde_json::to_string(&r.handles).expect("strings serialize");
    Ok(verbatim(call(&b, CALL_TIMEOUT, "LookupHandles", |c| async move { c.proxy.lookup_handles(&arg).await }).await?))
}

#[derive(Deserialize)]
struct TwoFactorReq {
    code: String,
}

async fn two_factor(State(b): State<Arc<Bridge>>, body: Bytes) -> ApiResult {
    let r: TwoFactorReq = body_json(&body)?;
    let result = call(&b, LONG_TIMEOUT, "SubmitTwoFactor", |c| async move { c.proxy.submit_two_factor(&r.code).await }).await?;
    Ok(axum::Json(json!({"result": result})).into_response())
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

async fn attachment(State(b): State<Arc<Bridge>>, Path(id): Path<String>, headers: HeaderMap) -> ApiResult {
    let path = call(&b, LONG_TIMEOUT, "DownloadAttachment", |c| async move { c.proxy.download_attachment(&id).await }).await?;
    files::serve(&b.cfg.daemon_state_dir, &path, &headers).await
}

async fn photo(State(b): State<Arc<Bridge>>, Path(handle): Path<String>, headers: HeaderMap) -> ApiResult {
    let raw = call(&b, CALL_TIMEOUT, "GetHandleProfile", |c| async move { c.proxy.get_handle_profile(&handle).await }).await?;
    let v = parse_value(&raw)?;
    let path = v["photo"].as_str().ok_or_else(|| ApiError::not_found("no shared photo"))?;
    files::serve(&b.cfg.daemon_state_dir, path, &headers).await
}

async fn chat_icon(State(b): State<Arc<Bridge>>, Path(chat_id): Path<String>, headers: HeaderMap) -> ApiResult {
    let v = parse_value(&get_chat_raw(&b, chat_id).await?)?;
    let path = v["icon_path"].as_str().ok_or_else(|| ApiError::not_found("no group photo"))?;
    files::serve(&b.cfg.daemon_state_dir, path, &headers).await
}

async fn chat_background(State(b): State<Arc<Bridge>>, Path(chat_id): Path<String>, headers: HeaderMap) -> ApiResult {
    let v = parse_value(&get_chat_raw(&b, chat_id).await?)?;
    let path = v["background"]["path"].as_str().ok_or_else(|| ApiError::not_found("no background picture"))?;
    files::serve(&b.cfg.daemon_state_dir, path, &headers).await
}

// ---------------------------------------------------------------------------
// Contacts
// ---------------------------------------------------------------------------

/// Every vCard under `dir` (same rule as `imessage_common::contacts`).
fn vcard_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            vcard_files(&p, out);
        } else if p.extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case("vcf") || x.eq_ignore_ascii_case("vcard")) {
            out.push(p);
        }
    }
}

/// `(etag, JSON body)`, rebuilt only when the vCards changed.
fn contacts_json(b: &Bridge) -> (String, Bytes) {
    let mut found = Vec::new();
    for d in &b.cfg.contacts_dirs {
        vcard_files(d, &mut found);
    }
    let newest = found
        .iter()
        .filter_map(|f| std::fs::metadata(f).ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .max()
        .unwrap_or(0);
    let etag = format!("W/\"{:x}-{newest:x}\"", found.len());
    let mut cache = b.contacts.lock().unwrap();
    if let Some((tag, body)) = cache.as_ref()
        && *tag == etag
    {
        return (etag, body.clone());
    }
    let contacts = imessage_common::contacts::Contacts::load(&b.cfg.contacts_dirs);
    let people: Vec<Value> = contacts.people().iter().map(|p| json!({"name": p.name, "phones": p.phones, "emails": p.emails})).collect();
    let body = Bytes::from(serde_json::to_vec(&people).expect("JSON values serialize"));
    *cache = Some((etag.clone(), body.clone()));
    (etag, body)
}

async fn contacts(State(b): State<Arc<Bridge>>, headers: HeaderMap) -> ApiResult {
    let b2 = b.clone();
    let (etag, body) = tokio::task::spawn_blocking(move || contacts_json(&b2)).await.map_err(|e| ApiError::new("Store", e.to_string()))?;
    let base = Response::builder().header(header::ETAG, &etag).header(header::CACHE_CONTROL, "private, no-cache");
    if files::etag_matches(headers.get(header::IF_NONE_MATCH), &etag) {
        return Ok(base.status(StatusCode::NOT_MODIFIED).body(Body::empty()).expect("valid response"));
    }
    Ok(base.header(header::CONTENT_TYPE, "application/json").body(Body::from(body)).expect("valid response"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_strings() {
        let uri: Uri = "/v1/send-file?to=bob%40example.com&to=tel%3A%2B1555&filename=a+b.jpg&x".parse().unwrap();
        let q = query(&uri);
        assert_eq!(q.iter().filter(|(k, _)| k == "to").count(), 2);
        assert_eq!(param(&q, "to"), Some("bob@example.com"));
        assert_eq!(param(&q, "filename"), Some("a b.jpg"));
        assert_eq!(param(&q, "x"), Some(""));
        assert_eq!(count(&q, "limit", 50, 500).unwrap(), 50);
        let q = query(&"/?limit=900".parse().unwrap());
        assert_eq!(count(&q, "limit", 50, 500).unwrap(), 500);
        assert!(count(&query(&"/?limit=-1".parse().unwrap()), "limit", 50, 500).is_err());
    }

    #[test]
    fn targets() {
        assert_eq!(target(Some("group:X"), &[]).unwrap(), ("group:X".into(), String::new()));
        assert_eq!(target(None, &["a@b.c".into()]).unwrap(), (String::new(), r#"["a@b.c"]"#.into()));
        assert!(target(Some("x"), &["a@b.c".into()]).is_err());
        assert!(target(None, &[]).is_err());
    }
}
