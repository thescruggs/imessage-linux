//! imsg-remote against a tiny fake bridge (axum): a few calls, errors,
//! downloads, uploads and the event stream across a reconnect.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{Message as WsMessage, WebSocketUpgrade};
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use imsg_remote::{ContactsReply, EventOptions, RemoteClient, RemoteEvent, Target};
use serde_json::json;

const TOKEN: &str = "Tq3xY0abcdefghijklmnopqrstuvwxyz-_ABCDEFGHI";

#[derive(Default)]
struct Fake {
    ws_sessions: AtomicUsize,
    last_upload: std::sync::Mutex<Option<(String, usize)>>,
    reacts: std::sync::Mutex<Vec<(String, String, serde_json::Value)>>,
}

fn authed(h: &HeaderMap) -> bool {
    h.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {TOKEN}"))
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "Unauthorized", "message": "unknown token"}))).into_response()
}

fn chat(id: &str) -> serde_json::Value {
    json!({"id": id, "participants": [{"uri": "tel:+15550001111"}], "display_name": null, "is_group": id.starts_with("group:"),
           "last_message_at": 1700000000000u64, "unread_count": 1, "pinned": false, "pin_order": null})
}

fn message(id: &str, chat_id: &str, text: &str) -> serde_json::Value {
    json!({"id": id, "chat_id": chat_id, "sender": {"uri": "tel:+15550001111"}, "sent_at": 1700000000000u64, "text": text,
           "parts": [{"type": "text", "text": text}], "subject": null, "reply_to": null, "is_from_me": false, "delivery": "delivered",
           "edited": false, "unsent": false, "reactions": [], "effect": null, "service": "sms"})
}

async fn status(h: HeaderMap) -> Response {
    if !authed(&h) {
        return unauthorized();
    }
    axum::Json(json!({"version": "0.1.14", "connection": {"state": "connected"}, "registered": true,
        "registration": {"handles": [{"uri": "mailto:me@example.com"}], "registered_at": 1, "expires_at": 2}, "unread": 3}))
    .into_response()
}

async fn chats(h: HeaderMap, Query(q): Query<std::collections::HashMap<String, String>>) -> Response {
    if !authed(&h) {
        return unauthorized();
    }
    assert_eq!(q.get("limit").map(String::as_str), Some("500"));
    axum::Json(json!([chat("group:ABC"), chat("tel:+15550001111")])).into_response()
}

async fn messages(h: HeaderMap, Path(id): Path<String>, Query(q): Query<std::collections::HashMap<String, String>>) -> Response {
    if !authed(&h) {
        return unauthorized();
    }
    if id != "group:ABC" {
        return (StatusCode::NOT_FOUND, axum::Json(json!({"error": "ChatNotFound", "message": format!("no chat {id}")}))).into_response();
    }
    let older = q.get("before").map(String::as_str) == Some("1700-1");
    let page = if older {
        json!({"messages": [message("M0", &id, "first")], "next_cursor": null})
    } else {
        json!({"messages": [message("M1", &id, "hello")], "next_cursor": "1700-1"})
    };
    axum::Json(page).into_response()
}

async fn attachment(h: HeaderMap, Path(id): Path<String>) -> Response {
    if !authed(&h) {
        return unauthorized();
    }
    if id == "M1:1" { (StatusCode::OK, vec![7u8; 100_000]).into_response() } else { StatusCode::NOT_FOUND.into_response() }
}

async fn contacts(h: HeaderMap) -> Response {
    if h.get("if-none-match").and_then(|v| v.to_str().ok()) == Some("\"v1\"") {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    ([("etag", "\"v1\"")], axum::Json(json!([{"name": "Bob Smith", "phones": ["+1 555 000 1111"], "emails": []}]))).into_response()
}

async fn send_file(State(s): State<Arc<Fake>>, h: HeaderMap, RawQuery(q): RawQuery, body: Bytes) -> Response {
    if !authed(&h) {
        return unauthorized();
    }
    *s.last_upload.lock().unwrap() = Some((q.unwrap_or_default(), body.len()));
    axum::Json(json!({"id": "S1", "chat_id": "group:ABC", "sent_at": 1, "sender": {"uri": "mailto:me@example.com"}, "service": "imessage"}))
        .into_response()
}

async fn send(h: HeaderMap, axum::Json(b): axum::Json<serde_json::Value>) -> Response {
    if !authed(&h) {
        return unauthorized();
    }
    assert_eq!(b["participants"], json!(["bob@example.com"]));
    assert_eq!(b["service"], "auto");
    axum::Json(json!({"id": "S2", "chat_id": "mailto:bob@example.com", "sent_at": 1, "sender": {"uri": "mailto:me@example.com"}})).into_response()
}

async fn react(State(s): State<Arc<Fake>>, h: HeaderMap, Path((chat, msg)): Path<(String, String)>, axum::Json(b): axum::Json<serde_json::Value>) -> Response {
    if !authed(&h) {
        return unauthorized();
    }
    s.reacts.lock().unwrap().push((chat, msg, b));
    StatusCode::NO_CONTENT.into_response()
}

async fn read(h: HeaderMap, Path(id): Path<String>) -> Response {
    if !authed(&h) {
        return unauthorized();
    }
    assert_eq!(id, "tel:+15550001111");
    StatusCode::NO_CONTENT.into_response()
}

/// Session 1: hello, a message, then close. Session 2+: hello, stay open.
async fn events(State(s): State<Arc<Fake>>, h: HeaderMap, ws: WebSocketUpgrade) -> Response {
    if !authed(&h) {
        return unauthorized();
    }
    let n = s.ws_sessions.fetch_add(1, Ordering::SeqCst);
    ws.on_upgrade(move |mut socket| async move {
        let hello = json!({"type": "hello", "bridge": "0.1.14", "daemon_connected": n > 0}).to_string();
        if socket.send(WsMessage::Text(hello.into())).await.is_err() {
            return;
        }
        if n == 0 {
            let m = json!({"type": "message_received", "chat_id": "group:ABC", "message": message("M2", "group:ABC", "live")}).to_string();
            let _ = socket.send(WsMessage::Text(m.into())).await;
            let _ = socket.send(WsMessage::Text(json!({"type": "unknown_future_frame"}).to_string().into())).await;
            let _ = socket.send(WsMessage::Text(json!({"type": "daemon", "state": "lost", "detail": "bus"}).to_string().into())).await;
            let _ = socket.send(WsMessage::Close(None)).await;
        } else {
            while let Some(Ok(_)) = socket.recv().await {}
        }
    })
}

async fn serve() -> (String, Arc<Fake>) {
    let state = Arc::new(Fake::default());
    let app = Router::new()
        .route("/v1/status", get(status))
        .route("/v1/chats", get(chats))
        .route("/v1/chats/{id}/messages", get(messages))
        .route("/v1/chats/{id}/read", post(read))
        .route("/v1/attachments/{id}", get(attachment))
        .route("/v1/contacts", get(contacts))
        .route("/v1/send", post(send))
        .route("/v1/send-file", post(send_file))
        .route("/v1/chats/{id}/messages/{msg}/react", post(react))
        .route("/v1/events", get(events))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state)
}

#[tokio::test]
async fn calls_errors_and_files() {
    let (url, fake) = serve().await;
    let c = RemoteClient::new(&url, TOKEN).unwrap();

    let s = c.status().await.unwrap();
    assert_eq!(s.unread, Some(3));
    let chats = c.all_chats().await.unwrap();
    assert_eq!(chats.len(), 2);
    assert_eq!(chats[0].id.0, "group:ABC");

    // Path ids round-trip through percent-encoding; paging by cursor.
    let page = c.messages("group:ABC", None, 60).await.unwrap();
    assert_eq!(page.messages[0].text.as_deref(), Some("hello"));
    let older = c.messages("group:ABC", page.next_cursor.as_deref(), 60).await.unwrap();
    assert_eq!(older.messages[0].id.0, "M0");
    assert_eq!(older.next_cursor, None);
    let e = c.messages("group:NOPE", None, 60).await.unwrap_err();
    assert!(e.is("ChatNotFound"), "{e:?}");
    assert_eq!(e.status, Some(404));
    c.mark_read("tel:+15550001111", None).await.unwrap();

    // Bad token.
    let bad = RemoteClient::new(&url, "x").unwrap();
    assert!(bad.status().await.unwrap_err().is("Unauthorized"));

    // Downloads: memory and streamed to disk.
    assert_eq!(c.download("M1:1").await.unwrap().len(), 100_000);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("a.bin");
    assert_eq!(c.download_to("M1:1", &dest).await.unwrap(), 100_000);
    assert_eq!(std::fs::read(&dest).unwrap().len(), 100_000);
    assert!(c.download_to("M9:1", &dir.path().join("b.bin")).await.unwrap_err().is("NotFound"));
    assert!(!dir.path().join("b.bin.part").exists());

    // Contacts with ETag.
    let ContactsReply::Fresh { contacts, etag } = c.contacts(None).await.unwrap() else { panic!() };
    assert_eq!(contacts[0].name, "Bob Smith");
    assert_eq!(c.contacts(etag.as_deref()).await.unwrap(), ContactsReply::NotModified);

    // Sends.
    let to = Target::Participants(vec!["bob@example.com".into()]);
    assert_eq!(c.send_text(&to, "hi", None, None).await.unwrap().id, "S2");
    let src = dir.path().join("photo 1.jpg");
    std::fs::write(&src, vec![1u8; 5000]).unwrap();
    let sent = c.send_file(&Target::Chat("group:ABC".into()), &src, None, Some("image/jpeg"), None).await.unwrap();
    assert_eq!(sent.id, "S1");
    let (q, n) = fake.last_upload.lock().unwrap().clone().unwrap();
    assert_eq!(n, 5000);
    assert_eq!(q, "chat_id=group%3AABC&filename=photo+1.jpg&mime=image%2Fjpeg&service=auto");

    // Tapbacks: a named kind, then an emoji removed.
    c.react("group:ABC", "M1", "love", false).await.unwrap();
    c.react("group:ABC", "M1", "🔥", true).await.unwrap();
    let reacts = fake.reacts.lock().unwrap().clone();
    assert_eq!(
        reacts,
        vec![
            ("group:ABC".to_string(), "M1".to_string(), json!({"kind": "love", "remove": false})),
            ("group:ABC".to_string(), "M1".to_string(), json!({"kind": "🔥", "remove": true})),
        ]
    );
}

async fn next(ev: &mut imsg_remote::EventStream) -> RemoteEvent {
    tokio::time::timeout(Duration::from_secs(5), ev.next_event()).await.expect("event in time").expect("stream open")
}

#[tokio::test]
async fn events_reconnect_and_say_connected_again() {
    let (url, fake) = serve().await;
    let c = RemoteClient::new(&url, TOKEN).unwrap();
    let opts = EventOptions { initial_backoff: Duration::from_millis(50), max_backoff: Duration::from_millis(200), ..Default::default() };
    let mut ev = c.events_with(opts);
    assert_eq!(next(&mut ev).await, RemoteEvent::Connected { bridge: "0.1.14".into(), daemon_connected: false });
    match next(&mut ev).await {
        RemoteEvent::MessageReceived { chat_id, message } => {
            assert_eq!(chat_id, "group:ABC");
            assert_eq!(message.text.as_deref(), Some("live"));
        }
        other => panic!("{other:?}"),
    }
    // The unknown frame is skipped.
    assert_eq!(next(&mut ev).await, RemoteEvent::Daemon { connected: false, detail: Some("bus".into()) });
    assert!(matches!(next(&mut ev).await, RemoteEvent::Lost(_)));
    let again = next(&mut ev).await;
    assert_eq!(again, RemoteEvent::Connected { bridge: "0.1.14".into(), daemon_connected: true });
    assert!(again.needs_resync());
    assert_eq!(fake.ws_sessions.load(Ordering::SeqCst), 2);

    // A refused token: Lost with a clear reason, retried.
    let bad = RemoteClient::new(&url, "nope").unwrap();
    let mut ev = bad.events_with(opts);
    match next(&mut ev).await {
        RemoteEvent::Lost(r) => assert!(r.contains("token"), "{r}"),
        other => panic!("{other:?}"),
    }
}
