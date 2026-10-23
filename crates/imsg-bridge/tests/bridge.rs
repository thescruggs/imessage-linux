//! `imsg-bridge`'s router against a real `imessaged` (in-process,
//! `MockEngine`) on a private `dbus-daemon`, served on 127.0.0.1 with
//! `require_whois = false`. Prints `SKIP:` and passes when `dbus-daemon`
//! is not installed.

#[path = "../../imsg/tests/common/mod.rs"]
mod common;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use imessage_common::model::{Chat, ChatId, DeliveryState, Handle, Message, MessageId, MessagePart};
use imessaged::config::DaemonConfig;
use imessaged::daemon::{Daemon, RunningDaemon};
use imessaged::dbus::{BUS_NAME, BusChoice};
use imessaged::engine::mock::{MockCall, MockEngine};
use imessaged::engine::{ChatTarget, Engine, EngineEvent};
use imessaged::keyring::StateKey;
use imessaged::store::{Store, StoreHandle};
use imsg_bridge::tokens::TokenStore;
use imsg_bridge::{Bridge, Config, Paths};
use imsg_client::Bus;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const BOB: &str = "mailto:bob@example.com";

struct Harness {
    _bus: common::PrivateBus,
    state: tempfile::TempDir,
    bridge_dir: tempfile::TempDir,
    mock: Arc<MockEngine>,
    running: Option<RunningDaemon>,
    bridge: Arc<Bridge>,
    addr: SocketAddr,
    token: String,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Harness {
    async fn start() -> Option<Harness> {
        let bus = common::PrivateBus::start()?;
        let state = tempfile::tempdir().unwrap();
        let mock = Arc::new(MockEngine::new());
        let store = StoreHandle::new(Store::open(state.path(), StateKey::from_bytes([7; 32])).unwrap());
        let running = Daemon::with_engine(DaemonConfig::new(state.path().into()), mock.clone())
            .with_store(store)
            .with_dbus(BusChoice::Address(bus.address.clone()))
            .start()
            .await
            .unwrap();
        let conn = zbus::connection::Builder::address(bus.address.as_str()).unwrap().build().await.unwrap();
        common::wait_for_name(&conn, BUS_NAME, 5).await;
        mock.login("me@example.com", "pw").await.unwrap();

        let bridge_dir = tempfile::tempdir().unwrap();
        let contacts = bridge_dir.path().join("vcards");
        std::fs::create_dir_all(&contacts).unwrap();
        std::fs::write(contacts.join("bob.vcf"), "BEGIN:VCARD\nVERSION:3.0\nFN:Bob Smith\nEMAIL:bob@example.com\nTEL:+1 555 0100\nEND:VCARD\n").unwrap();
        let cfg = Config::parse(&format!(
            "listen = [\"127.0.0.1:7227\"]\nrequire_whois = false\nmax_upload_mib = 1\ndaemon_state_dir = {:?}\ncontacts_dirs = [{:?}]\n",
            state.path().display().to_string(),
            contacts.display().to_string()
        ))
        .unwrap();
        let paths = Paths::under(bridge_dir.path());
        let token = TokenStore::new(paths.tokens_file()).create("test-device").unwrap();
        let bridge = Bridge::new(cfg, paths, Bus::Address(bus.address.clone()), None).unwrap();
        let events = bridge.start_events();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = imsg_bridge::router(bridge.clone()).into_make_service_with_connect_info::<SocketAddr>();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        // Wait until the shared signal subscription is up.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !bridge.daemon.is_connected() {
            assert!(Instant::now() < deadline, "bridge never subscribed to the daemon");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Some(Harness { _bus: bus, state, bridge_dir, mock, running: Some(running), bridge, addr, token, tasks: vec![events, server] })
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.token)
    }

    async fn get(&self, path: &str) -> Resp {
        request(self.addr, "GET", path, &[("Authorization", &self.auth())], b"").await
    }

    async fn post(&self, path: &str, body: &str) -> Resp {
        request(self.addr, "POST", path, &[("Authorization", &self.auth()), ("Content-Type", "application/json")], body.as_bytes()).await
    }

    fn uploads(&self) -> PathBuf {
        self.bridge.paths.uploads_dir()
    }

    async fn shutdown(mut self) {
        for t in &self.tasks {
            t.abort();
        }
        if let Some(r) = self.running.take() {
            let _ = r.shutdown().await;
        }
    }
}

#[derive(Debug)]
struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {:?}", String::from_utf8_lossy(&self.body)))
    }
    fn error(&self) -> String {
        self.json()["error"].as_str().unwrap_or_default().to_string()
    }
}

/// One HTTP/1.0 request (the server answers without chunking and closes).
async fn request(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Resp {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut req = format!("{method} {path} HTTP/1.0\r\nHost: {addr}\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    s.write_all(body).await.unwrap();
    raw_response(s).await
}

async fn raw_response(mut s: tokio::net::TcpStream) -> Resp {
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), s.read_to_end(&mut buf)).await.expect("response within 30 s").unwrap();
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").expect("header end");
    let head = String::from_utf8_lossy(&buf[..split]).into_owned();
    let mut lines = head.lines();
    let status = lines.next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
    let headers = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
    Resp { status, headers, body: buf[split + 4..].to_vec() }
}

/// One percent-encoded path segment.
fn seg(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn incoming(id: &str, at: u64, text: &str) -> EngineEvent {
    EngineEvent::MessageReceived {
        message: Message {
            id: MessageId::new(id),
            chat_id: ChatId::direct(BOB),
            sender: Handle::new(BOB),
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
            service: Default::default(),
        },
        chat: Chat { id: ChatId::direct(BOB), participants: vec![Handle::new(BOB)], display_name: None, is_group: false, last_message_at: Some(at), unread_count: 0, background: None, icon_path: None, pinned: false, pin_order: None },
        attachments: vec![],
    }
}

/// Inject a message and wait until the daemon has stored it.
async fn inject_and_wait(h: &Harness, id: &str, at: u64, text: &str) {
    h.mock.inject(incoming(id, at, text));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let r = h.get(&format!("/v1/chats/{}/messages", seg(BOB))).await;
        if r.status == 200 && r.json()["messages"].as_array().is_some_and(|m| m.iter().any(|m| m["id"] == id)) {
            return;
        }
        assert!(Instant::now() < deadline, "message {id} was not stored");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tokens_are_required() {
    let Some(h) = Harness::start().await else { return };
    let r = request(h.addr, "GET", "/v1/status", &[], b"").await;
    assert_eq!((r.status, r.error().as_str()), (401, "Unauthorized"));
    let t = Instant::now();
    let r = request(h.addr, "GET", "/v1/status", &[("Authorization", "Bearer wrong")], b"").await;
    assert_eq!((r.status, r.error().as_str()), (401, "Unauthorized"));
    assert!(t.elapsed() >= Duration::from_millis(900), "failed auth must be delayed");
    // Unknown paths are refused before routing, too.
    assert_eq!(request(h.addr, "GET", "/nope", &[], b"").await.status, 401);
    assert_eq!(h.get("/nope").await.status, 404);

    let r = h.get("/v1/version").await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v["api"], 1);
    assert_eq!(v["bridge"], imsg_bridge::VERSION);
    assert!(v["daemon"].is_string(), "{v}");

    // Use is recorded; the access log names the token, never the secret.
    let deadline = Instant::now() + Duration::from_secs(5);
    let store = TokenStore::new(h.bridge.paths.tokens_file());
    while store.load().unwrap()[0].last_peer.is_none() {
        assert!(Instant::now() < deadline, "last_peer never recorded");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let log = std::fs::read_to_string(h.bridge.paths.access_log()).unwrap();
    assert!(log.contains("token=test-device") && log.contains("reason=unknown-token") && log.contains("reason=no-token"), "{log}");
    assert!(!log.contains(&h.token));

    // Revoking takes effect on the next request.
    assert!(store.revoke("test-device").unwrap());
    assert_eq!(h.get("/v1/status").await.status, 401);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_pass_through_and_errors_map() {
    let Some(h) = Harness::start().await else { return };
    let r = h.get("/v1/status").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert_eq!(r.json()["registered"], true);

    inject_and_wait(&h, "M1", 1_000, "hello there").await;
    let chats = h.get("/v1/chats?limit=10").await.json();
    assert_eq!(chats.as_array().unwrap().len(), 1);
    assert_eq!(chats[0]["id"], BOB);
    let chat = h.get(&format!("/v1/chats/{}", seg(BOB))).await.json();
    assert_eq!(chat["id"], BOB);
    let page = h.get(&format!("/v1/chats/{}/messages?limit=5", seg(BOB))).await.json();
    assert_eq!(page["messages"][0]["text"], "hello there");
    let found = h.get("/v1/search?q=hello").await.json();
    assert_eq!(found[0]["id"], "M1");
    assert_eq!(h.get("/v1/sms").await.status, 200);
    let r = h.get(&format!("/v1/handles/{}/profile", seg(BOB))).await;
    assert_eq!((r.status, r.json()), (200, Value::Null));

    // Error mapping: the daemon's short name and its status.
    let r = h.get(&format!("/v1/chats/{}", seg("mailto:nobody@example.com"))).await;
    assert_eq!((r.status, r.error().as_str()), (404, "NotFound"), "{:?}", r.json());
    assert!(r.json()["message"].as_str().unwrap().contains("nobody"));
    let r = h.get(&format!("/v1/handles/{}/photo", seg(BOB))).await;
    assert_eq!((r.status, r.error().as_str()), (404, "NotFound"));
    let r = h.post("/v1/lookup", r#"{"handles": ["not a handle"]}"#).await;
    assert_eq!((r.status, r.error().as_str()), (400, "InvalidHandle"));
    let r = h.post("/v1/send", r#"{"text": "no target"}"#).await;
    assert_eq!((r.status, r.error().as_str()), (400, "BadRequest"));
    let r = h.post("/v1/send", "not json").await;
    assert_eq!((r.status, r.error().as_str()), (400, "BadRequest"));
    assert_eq!(h.get("/v1/search").await.error(), "BadRequest");

    // Contacts, with a working ETag.
    let r = h.get("/v1/contacts").await;
    assert_eq!(r.status, 200);
    let people = r.json();
    assert_eq!(people[0]["name"], "Bob Smith");
    assert_eq!(people[0]["emails"][0], "bob@example.com");
    let etag = r.header("etag").unwrap().to_string();
    let r = request(h.addr, "GET", "/v1/contacts", &[("Authorization", &h.auth()), ("If-None-Match", &etag)], b"").await;
    assert_eq!(r.status, 304);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sending_text_reactions_and_pins() {
    let Some(h) = Harness::start().await else { return };
    inject_and_wait(&h, "M1", 1_000, "hi").await;
    let r = h.post("/v1/send", &format!(r#"{{"chat_id": "{BOB}", "text": "hello bob", "reply_to": "M1"}}"#)).await;
    assert_eq!(r.status, 200, "{:?}", r.json());
    let sent = r.json();
    assert_eq!(sent["chat_id"], BOB);
    assert!(sent["id"].is_string());
    assert!(h.mock.calls().iter().any(|c| matches!(c, MockCall::SendText { text, reply_to: Some(r), chat: ChatTarget::Chat(_) } if text == "hello bob" && r.0 == "M1")));

    let r = h.post("/v1/send", r#"{"participants": ["carol@example.com"], "text": "new chat"}"#).await;
    assert_eq!(r.status, 200);
    assert!(h.mock.calls().iter().any(|c| matches!(c, MockCall::SendText { text, chat: ChatTarget::Participants(_), .. } if text == "new chat")));

    let r = h.post(&format!("/v1/chats/{}/messages/M1/react", seg(BOB)), r#"{"kind": "love"}"#).await;
    assert_eq!(r.status, 204);
    assert!(h.mock.calls().iter().any(|c| matches!(c, MockCall::React { remove: false, .. })));
    assert_eq!(h.post(&format!("/v1/chats/{}/typing", seg(BOB)), r#"{"typing": true}"#).await.status, 204);
    assert_eq!(h.post(&format!("/v1/chats/{}/read", seg(BOB)), "").await.status, 204);
    let r = h.post(&format!("/v1/chats/{}/pin", seg(BOB)), r#"{"pinned": true}"#).await;
    assert_eq!((r.status, r.json()["pinned"].clone()), (200, Value::Bool(true)));
    let r = h.post("/v1/pins", &format!(r#"{{"chat_ids": ["{BOB}"]}}"#)).await;
    assert_eq!(r.json()[0]["id"], BOB);
    let r = h.post("/v1/two-factor", r#"{"code": "123456"}"#).await;
    assert_eq!((r.status, r.error().as_str()), (409, "NoPendingTwoFactor"));
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_upload_download_range_and_containment() {
    let Some(h) = Harness::start().await else { return };
    inject_and_wait(&h, "M1", 1_000, "hi").await;

    // Upload: SendFile gets a private copy, which is gone afterwards.
    let r = request(
        h.addr,
        "POST",
        &format!("/v1/send-file?chat_id={}&filename={}", seg(BOB), seg("../note.txt")),
        &[("Authorization", &h.auth())],
        b"hello",
    )
    .await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let sent_id = r.json()["id"].as_str().unwrap().to_string();
    let uploaded = h
        .mock
        .calls()
        .iter()
        .find_map(|c| match c {
            MockCall::SendAttachment { path, mime, .. } => Some((path.clone(), mime.clone())),
            _ => None,
        })
        .expect("SendFile reached the engine");
    assert_eq!(uploaded.0.file_name().unwrap(), "note.txt");
    assert!(uploaded.0.starts_with(h.uploads()) || uploaded.0.starts_with(h.state.path()), "{uploaded:?}");
    assert_eq!(uploaded.1, "text/plain");
    assert_eq!(std::fs::read_dir(h.uploads()).unwrap().count(), 0, "upload not cleaned up");

    // Too large: refused from Content-Length alone.
    let mut s = tokio::net::TcpStream::connect(h.addr).await.unwrap();
    let req = format!("POST /v1/send-file?chat_id={} HTTP/1.0\r\nAuthorization: {}\r\nContent-Length: {}\r\n\r\n", seg(BOB), h.auth(), 2 * 1024 * 1024);
    s.write_all(req.as_bytes()).await.unwrap();
    let r = raw_response(s).await;
    assert_eq!((r.status, r.error().as_str()), (413, "TooLarge"));

    // Download the daemon's copy of the attachment.
    let page = h.get(&format!("/v1/chats/{}/messages", seg(BOB))).await.json();
    let msg = page["messages"].as_array().unwrap().iter().find(|m| m["id"] == sent_id.as_str()).expect("sent message stored").clone();
    let att = msg["parts"].as_array().unwrap().iter().find_map(|p| p["attachment_id"].as_str()).unwrap().to_string();
    let url = format!("/v1/attachments/{}", seg(&att));
    let r = h.get(&url).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"hello");
    assert_eq!(r.header("content-type"), Some("text/plain; charset=utf-8"));
    assert_eq!(r.header("content-disposition"), Some("inline; filename*=UTF-8''note.txt"));
    assert_eq!(r.header("content-length"), Some("5"));
    let etag = r.header("etag").unwrap().to_string();
    assert!(etag.starts_with("W/"));

    let r = request(h.addr, "GET", &url, &[("Authorization", &h.auth()), ("Range", "bytes=1-3")], b"").await;
    assert_eq!(r.status, 206);
    assert_eq!(r.body, b"ell");
    assert_eq!(r.header("content-range"), Some("bytes 1-3/5"));
    let r = request(h.addr, "GET", &url, &[("Authorization", &h.auth()), ("Range", "bytes=9-")], b"").await;
    assert_eq!((r.status, r.header("content-range")), (416, Some("bytes */5")));
    let r = request(h.addr, "GET", &url, &[("Authorization", &h.auth()), ("If-None-Match", &etag)], b"").await;
    assert_eq!(r.status, 304);
    assert!(r.body.is_empty());

    // Containment: replace the daemon's copy with a symlink out of the state dir.
    let copy = std::fs::read_dir(h.state.path().join("attachments")).unwrap().flatten().find_map(|d| {
        let f = d.path().join("note.txt");
        f.exists().then_some(f)
    });
    let copy = copy.expect("daemon copy under attachments/");
    let secret = h.bridge_dir.path().join("secret.txt");
    std::fs::write(&secret, b"top secret").unwrap();
    std::fs::remove_file(&copy).unwrap();
    std::os::unix::fs::symlink(&secret, &copy).unwrap();
    let r = h.get(&url).await;
    assert_eq!((r.status, r.error().as_str()), (404, "NotFound"));
    assert!(!String::from_utf8_lossy(&r.body).contains("top secret"));
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_hello_and_events() {
    let Some(h) = Harness::start().await else { return };
    // No token: the upgrade is refused.
    let e = tokio_tungstenite::connect_async(format!("ws://{}/v1/events", h.addr)).await.unwrap_err();
    assert!(matches!(&e, tungstenite::Error::Http(r) if r.status() == 401), "{e:?}");

    let mut req = format!("ws://{}/v1/events", h.addr).into_client_request().unwrap();
    req.headers_mut().insert("Authorization", h.auth().parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let mut next = async || -> Value {
        loop {
            let m = tokio::time::timeout(Duration::from_secs(10), ws.next()).await.expect("frame within 10 s").unwrap().unwrap();
            if let tungstenite::Message::Text(t) = m {
                return serde_json::from_str(&t).unwrap();
            }
        }
    };
    let hello = next().await;
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["bridge"], imsg_bridge::VERSION);
    assert_eq!(hello["daemon_connected"], true);

    h.mock.inject(incoming("M9", 9_000, "over the wire"));
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut got_msg, mut got_chat) = (false, false);
    while !(got_msg && got_chat) {
        assert!(Instant::now() < deadline, "no message_received / chat_updated");
        let f = next().await;
        match f["type"].as_str().unwrap() {
            "message_received" => {
                assert_eq!(f["chat_id"], BOB);
                // Embedded as an object, not a JSON string.
                assert_eq!(f["message"]["text"], "over the wire");
                got_msg = true;
            }
            "chat_updated" => {
                assert_eq!(f["chat"]["id"], BOB);
                got_chat = true;
            }
            _ => {}
        }
    }

    h.mock.set_connection_state(imessage_common::model::ConnectionState::Connecting);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "no connection_state frame");
        let f = next().await;
        if f["type"] == "connection_state" {
            assert_eq!(f["state"], "connecting");
            break;
        }
    }
    h.shutdown().await;
}
