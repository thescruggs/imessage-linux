//! The D-Bus API end to end on a private `dbus-daemon` with `MockEngine`,
//! one focused test per area. Prints `SKIP:` and passes when `dbus-daemon`
//! is not installed.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use imessage_common::model::{
    Attachment, AttachmentId, Chat, ChatId, ConnectionState, DeliveryState, Handle, Message, MessageId, MessagePart, TypingEvent,
};
use imessaged::config::DaemonConfig;
use imessaged::daemon::{Daemon, RunningDaemon};
use imessaged::dbus::{BUS_NAME, BusChoice, INTERFACE, OBJECT_PATH};
use imessaged::engine::mock::{MockCall, MockEngine};
use imessaged::engine::{BackgroundRef, ChatTarget, Engine, EngineEvent};
use imessaged::keyring::StateKey;
use imessaged::store::{Store, StoreHandle};
use serde_json::Value;
use zbus::zvariant::OwnedValue;

const BOB: &str = "mailto:bob@example.com";

/// Private bus + daemon (MockEngine, real store) + a client proxy.
struct Harness {
    bus: common::PrivateBus,
    dir: tempfile::TempDir,
    mock: Arc<MockEngine>,
    running: Option<RunningDaemon>,
    conn: zbus::Connection,
    p: zbus::Proxy<'static>,
}

impl Harness {
    /// `None` (test skipped) when `dbus-daemon` is not installed.
    async fn start() -> Option<Harness> {
        let bus = common::PrivateBus::start()?;
        let dir = tempfile::tempdir().unwrap();
        let mock = Arc::new(MockEngine::new());
        let store = StoreHandle::new(Store::open(dir.path(), StateKey::from_bytes([5; 32])).unwrap());
        let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone())
            .with_store(store)
            .with_dbus(BusChoice::Address(bus.address.clone()))
            .with_typing_timeout(Duration::from_millis(200))
            .start()
            .await
            .unwrap();
        let conn = zbus::connection::Builder::address(bus.address.as_str()).unwrap().build().await.unwrap();
        common::wait_for_name(&conn, BUS_NAME, 5).await;
        let p = proxy(&conn).await;
        Some(Harness { bus, dir, mock, running: Some(running), conn, p })
    }

    /// Started and logged in (directly on the mock, no 2FA).
    async fn logged_in() -> Option<Harness> {
        let h = Harness::start().await?;
        h.mock.login("me@example.com", "pw").await.unwrap();
        Some(h)
    }

    async fn call_json<B>(&self, method: &str, body: &B) -> Value
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        json(&self.p.call::<_, _, String>(method, body).await.unwrap_or_else(|e| panic!("{method}: {e}")))
    }

    async fn shutdown(mut self) {
        self.running.take().unwrap().shutdown().await.unwrap();
    }
}

async fn proxy(conn: &zbus::Connection) -> zbus::Proxy<'static> {
    zbus::proxy::Builder::new(conn)
        .destination(BUS_NAME)
        .unwrap()
        .path(OBJECT_PATH)
        .unwrap()
        .interface(INTERFACE)
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap()
}

fn json(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|e| panic!("bad JSON {s:?}: {e}"))
}

fn error_name(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(name, _, _) => name.to_string(),
        other => panic!("not a method error: {other:?}"),
    }
}

/// Next signal on `stream` whose body passes `f`, within 5 s.
async fn next_signal<T, F>(stream: &mut zbus::proxy::SignalStream<'_>, f: F) -> T
where
    T: serde::de::DeserializeOwned + zbus::zvariant::Type,
    F: Fn(&T) -> bool,
{
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let msg = stream.next().await.expect("signal stream ended");
            let body: T = msg.body().deserialize().unwrap();
            if f(&body) {
                return body;
            }
        }
    })
    .await
    .expect("signal not received within 5s")
}

fn bob_chat(at: u64) -> Chat {
    Chat { id: ChatId::direct(BOB), participants: vec![Handle::new(BOB)], display_name: None, is_group: false, last_message_at: Some(at), unread_count: 0, background: None, icon_path: None }
}

/// An incoming message from Bob, optionally with one attachment (part 1).
fn incoming(id: &str, at: u64, text: &str, attachment: Option<(&str, &str)>) -> EngineEvent {
    let mut parts = vec![MessagePart::Text { text: text.into() }];
    let mut attachments = vec![];
    if let Some((filename, locator)) = attachment {
        let att_id = AttachmentId::for_part(&MessageId::new(id), 1);
        parts.push(MessagePart::Attachment { attachment_id: att_id.clone(), mime: "image/png".into(), filename: filename.into(), size: 3 });
        attachments.push(Attachment {
            id: att_id,
            message_id: MessageId::new(id),
            mime: "image/png".into(),
            filename: filename.into(),
            size: 3,
            local_path: None,
            mmcs: locator.into(),
        });
    }
    EngineEvent::MessageReceived {
        message: Message {
            id: MessageId::new(id),
            chat_id: ChatId::direct(BOB),
            sender: Handle::new(BOB),
            sent_at: at,
            text: Some(text.into()),
            parts,
            subject: None,
            reply_to: None,
            is_from_me: false,
            delivery: DeliveryState::Delivered,
            edited: false,
            unsent: false,
            reactions: vec![],
            effect: None,
            verified: false,
        },
        chat: bob_chat(at),
        attachments,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn name_is_exclusive_and_released_on_shutdown() {
    let Some(h) = Harness::start().await else { return };
    let v: String = h.p.call("Version", &()).await.unwrap();
    assert_eq!(v, env!("CARGO_PKG_VERSION"));
    let second = Daemon::with_engine(DaemonConfig::new(h.dir.path().into()), Arc::new(MockEngine::new()))
        .with_store(StoreHandle::new(Store::open(tempfile::tempdir().unwrap().path(), StateKey::from_bytes([5; 32])).unwrap()))
        .with_dbus(BusChoice::Address(h.bus.address.clone()))
        .start()
        .await;
    assert!(second.is_err(), "second instance must fail to claim {BUS_NAME}");
    let conn = h.conn.clone();
    h.shutdown().await;
    assert!(!common::has_owner(&conn, BUS_NAME).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_then_two_factor() {
    let Some(h) = Harness::start().await else { return };
    h.mock.set_require_two_factor(true);
    let p = &h.p;
    let mut two_factor = p.receive_signal("TwoFactorRequired").await.unwrap();
    let mut conn_state = p.receive_signal("ConnectionStateChanged").await.unwrap();

    let st = h.call_json("GetStatus", &()).await;
    assert_eq!(st["connection"]["state"], "needs_login");
    assert_eq!(st["registered"], false);
    assert_eq!(st["version"], env!("CARGO_PKG_VERSION"));
    assert!(st.get("handles").is_none(), "handles only live under registration");
    assert!(!p.get_property::<bool>("Registered").await.unwrap());

    let r: String = p.call("Login", &("me@example.com", "hunter2")).await.unwrap();
    assert_eq!(r, "two_factor_required");
    let () = next_signal(&mut two_factor, |_: &()| true).await;
    assert_eq!(p.get_property::<String>("ConnectionState").await.unwrap(), "needs_two_factor");
    let e = p.call::<_, _, String>("SubmitTwoFactor", &("12",)).await.unwrap_err();
    assert_eq!(error_name(&e), "org.imessage.Daemon1.Error.BadTwoFactorCode");
    let r: String = p.call("SubmitTwoFactor", &("123456",)).await.unwrap();
    assert_eq!(r, "ok");
    next_signal(&mut conn_state, |(s,): &(String,)| s == "connected").await;
    assert!(p.get_property::<bool>("Registered").await.unwrap());
    let st = h.call_json("GetStatus", &()).await;
    assert_eq!(st["registered"], true);
    assert_eq!(st["registration"]["handles"][0]["uri"], "mailto:me@example.com");
    let e = p.call::<_, _, String>("SubmitTwoFactor", &("123456",)).await.unwrap_err();
    assert_eq!(error_name(&e), "org.imessage.Daemon1.Error.NoPendingTwoFactor");

    let calls = format!("{:?}", h.mock.calls());
    assert!(!calls.contains("hunter2") && !calls.contains("123456"), "secrets never reach the call log");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_text_round_trip_and_signal() {
    let Some(h) = Harness::start().await else { return };
    let p = &h.p;
    let e = p.call::<_, _, String>("SendText", &("", "[\"bob@example.com\"]", "x", "")).await.unwrap_err();
    assert_eq!(error_name(&e), "org.imessage.Daemon1.Error.NotLoggedIn");
    h.mock.login("me@example.com", "pw").await.unwrap();
    let mut received = p.receive_signal("MessageReceived").await.unwrap();
    let mut updated = p.receive_signal("MessageUpdated").await.unwrap();

    let sent = h.call_json("SendText", &("", "[\"Bob@Example.com\"]", "hello bob", "")).await;
    assert_eq!(sent["chat_id"], BOB);
    assert_eq!(sent["sender"]["uri"], "mailto:me@example.com");
    let sent_id = sent["id"].as_str().unwrap().to_string();
    assert!(h.mock.calls().iter().any(|c| matches!(c, MockCall::SendText { text, chat: ChatTarget::Participants(p), .. } if text == "hello bob" && p == &vec!["Bob@Example.com".to_string()])));
    let (chat_id, mj): (String, String) = next_signal(&mut received, |(_, m): &(String, String)| json(m)["id"] == sent_id.as_str()).await;
    assert_eq!(chat_id, BOB);
    let m = json(&mj);
    assert_eq!(m["is_from_me"], true);
    assert_eq!(m["text"], "hello bob");
    assert_eq!(m["sender"]["uri"], "mailto:me@example.com", "sender = the handle the engine sent from");
    let page = h.call_json("ListMessages", &(BOB, "", 50u32)).await;
    assert_eq!(page["messages"].as_array().unwrap().len(), 1);
    assert_eq!(page["messages"][0]["id"], sent_id.as_str());

    // reply_to + a delivery receipt -> MessageUpdated
    let sent2 = h.call_json("SendText", &(BOB, "", "second", sent_id.as_str())).await;
    let id2 = sent2["id"].as_str().unwrap().to_string();
    assert!(h.mock.calls().iter().any(|c| matches!(c, MockCall::SendText { reply_to: Some(r), .. } if r.0 == sent_id)));
    h.mock.inject(EngineEvent::Delivered(imessage_common::model::DeliveryReceipt {
        chat_id: Some(ChatId::direct(BOB)),
        message_id: MessageId::new(&id2),
        recipient: Handle::new(BOB),
        delivered_at: 3,
    }));
    next_signal(&mut updated, |(_, m): &(String, String)| json(m)["id"] == id2.as_str() && json(m)["delivery"] == "delivered").await;
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_messages_after_injected_events() {
    let Some(h) = Harness::logged_in().await else { return };
    let p = &h.p;
    let mut received = p.receive_signal("MessageReceived").await.unwrap();
    let mut chat_updated = p.receive_signal("ChatUpdated").await.unwrap();
    h.mock.inject(incoming("IN-1", 2_000_000_000_000, "look", Some(("../cat.png", "{\"v\":1,\"plist\":\"FILE-KEY\"}"))));
    h.mock.inject(incoming("IN-2", 2_000_000_000_001, "again", None));
    let (_, mj): (String, String) = next_signal(&mut received, |(_, m): &(String, String)| json(m)["id"] == "IN-1").await;
    assert_eq!(json(&mj)["verified"], false, "unverified flag survives store + D-Bus");
    assert!(!mj.contains("FILE-KEY"), "locator never leaves the daemon");
    next_signal(&mut chat_updated, |(c,): &(String,)| json(c)["unread_count"] == 2).await;

    let chats = h.call_json("ListChats", &(10u32, 0u32)).await;
    assert_eq!(chats.as_array().unwrap().len(), 1);
    assert_eq!(chats[0]["unread_count"], 2);
    let c = h.call_json("GetChat", &(BOB,)).await;
    assert_eq!(c["participants"][0]["uri"], BOB);
    let e = p.call::<_, _, String>("GetChat", &("mailto:nobody@x.y",)).await.unwrap_err();
    assert_eq!(error_name(&e), "org.imessage.Daemon1.Error.NotFound");

    let page = h.call_json("ListMessages", &(BOB, "", 1u32)).await;
    assert_eq!(page["messages"][0]["id"], "IN-2");
    let cursor = page["next_cursor"].as_str().unwrap().to_string();
    let older = h.call_json("ListMessages", &(BOB, cursor.as_str(), 1u32)).await;
    assert_eq!(older["messages"][0]["id"], "IN-1");
    assert!(older["next_cursor"].is_null());
    let e = p.call::<_, _, String>("ListMessages", &(BOB, "garbage", 1u32)).await.unwrap_err();
    assert_eq!(error_name(&e), "org.imessage.Daemon1.Error.InvalidInput");
    let hits = h.call_json("Search", &("LOOK", 10u32)).await;
    assert_eq!(hits.as_array().unwrap().len(), 1);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_attachment() {
    let Some(h) = Harness::logged_in().await else { return };
    let p = &h.p;
    let mut received = p.receive_signal("MessageReceived").await.unwrap();
    h.mock.inject(incoming("IN-1", 2_000_000_000_000, "look", Some(("../cat.png", "{\"v\":1,\"plist\":\"FILE-KEY\"}"))));
    next_signal(&mut received, |(_, m): &(String, String)| json(m)["id"] == "IN-1").await;
    let att_id = "IN-1:1";

    // concurrent calls download once and agree on the path
    let arg = (att_id,);
    let (a, b, c) = tokio::join!(
        p.call::<_, _, String>("DownloadAttachment", &arg),
        p.call::<_, _, String>("DownloadAttachment", &arg),
        p.call::<_, _, String>("DownloadAttachment", &arg),
    );
    let path = a.unwrap();
    assert_eq!(b.unwrap(), path);
    assert_eq!(c.unwrap(), path);
    assert!(path.starts_with(h.dir.path().join("attachments").to_str().unwrap()), "{path}");
    assert!(path.ends_with("/IN-1_1/cat.png"), "{path}");
    assert_eq!(std::fs::read(&path).unwrap(), MockEngine::ATTACHMENT_PAYLOAD);
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    let again: String = p.call("DownloadAttachment", &(att_id,)).await.unwrap();
    assert_eq!(again, path);
    assert_eq!(h.mock.calls().iter().filter(|c| matches!(c, MockCall::DownloadAttachment { .. })).count(), 1);
    let e = p.call::<_, _, String>("DownloadAttachment", &("nope:0",)).await.unwrap_err();
    assert_eq!(error_name(&e), "org.imessage.Daemon1.Error.NotFound");
    assert!(!format!("{:?}", h.mock.calls()).contains("FILE-KEY"));
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_backgrounds() {
    let Some(h) = Harness::logged_in().await else { return };
    let p = &h.p;
    let mut updated = p.receive_signal("ChatUpdated").await.unwrap();
    let mut received = p.receive_signal("MessageReceived").await.unwrap();
    h.mock.inject(incoming("IN-1", 2_000_000_000_000, "hi", None));
    next_signal(&mut received, |(_, m): &(String, String)| json(m)["id"] == "IN-1").await;
    let chat = ChatId::direct(BOB);

    // Received: stored, then downloaded eagerly; a second ChatUpdated carries the path.
    let bg = BackgroundRef { background_id: "BG-1".into(), payload_version: 1, seq: 1, size: 3, locator: "{\"v\":1,\"plist\":\"BG-KEY\"}".into() };
    h.mock.inject(EngineEvent::ChatBackground { chat_id: chat.clone(), background: Some(bg), by: Handle::new(BOB), at: 2_000_000_000_100 });
    let (c,): (String,) = next_signal(&mut updated, |(c,): &(String,)| !json(c)["background"]["payload"].is_null()).await;
    let b = &json(&c)["background"];
    assert_eq!((b["id"].as_str(), b["version"].as_u64(), b["set_by"].as_str()), (Some("BG-1"), Some(1), Some(BOB)));
    let path = b["payload"].as_str().unwrap().to_string();
    assert!(b["path"].is_null(), "the mock payload holds no picture");
    assert!(path.starts_with(h.dir.path().join("backgrounds").to_str().unwrap()) && path.ends_with("/BG-1"), "{path}");
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    let got: String = p.call("GetChat", &(chat.0.as_str(),)).await.unwrap();
    assert_eq!(json(&got)["background"]["payload"].as_str(), Some(path.as_str()));

    // Imported locally: replaces it (the old file goes).
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
    let src = h.dir.path().join("wallpaper");
    std::fs::write(&src, png).unwrap();
    let c: String = p.call("ImportChatBackground", &(chat.0.as_str(), src.to_str().unwrap())).await.unwrap();
    let ib = &json(&c)["background"];
    let ipath = ib["path"].as_str().unwrap().to_string();
    assert!(ib["id"].as_str().unwrap().starts_with("local-"));
    assert!(ipath.ends_with(".png") && ipath.starts_with(ib["payload"].as_str().unwrap()), "{ipath}");
    assert_eq!(std::fs::read(&ipath).unwrap(), png);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!std::path::Path::new(&path).exists(), "replaced background file removed");
    let e = p.call::<_, _, String>("ImportChatBackground", &(chat.0.as_str(), "relative.png")).await.unwrap_err();
    assert_eq!(error_name(&e), "org.imessage.Daemon1.Error.InvalidInput");

    // Removed over iMessage.
    h.mock.inject(EngineEvent::ChatBackground { chat_id: chat.clone(), background: None, by: Handle::new(BOB), at: 2_000_000_000_200 });
    next_signal(&mut updated, |(c,): &(String,)| json(c)["background"].is_null()).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!std::path::Path::new(&ipath).exists());
    assert!(!format!("{:?}", h.mock.calls()).contains("BG-KEY"));
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_profiles() {
    use imessaged::engine::{FetchedProfile, ShareRef};
    let Some(h) = Harness::logged_in().await else { return };
    let p = &h.p;
    let st: String = p.call("GetStatus", &()).await.unwrap();
    assert_eq!(json(&st)["profiles"]["state"], "ready");
    let mut profiles = p.receive_signal("HandleProfileUpdated").await.unwrap();
    let mut received = p.receive_signal("MessageReceived").await.unwrap();
    h.mock.inject(incoming("PR-1", 2_000_000_000_000, "hi", None));
    next_signal(&mut received, |(_, m): &(String, String)| json(m)["id"] == "PR-1").await;
    let none: String = p.call("GetHandleProfile", &(BOB,)).await.unwrap();
    assert_eq!(none, "null");

    h.mock.set_profile("REC", FetchedProfile { name: "Bobby".into(), first: "Bob".into(), last: String::new(), image: Some(b"\x89PNG\r\n\x1a\nxx".to_vec()) });
    let share = ShareRef { record_key: "REC".into(), decryption_key: b"PROFILE-KEY".to_vec(), poster: None };
    h.mock.inject(EngineEvent::ProfileShared { handle: Handle::new(BOB), share: Some(share), at: 2_000_000_000_001 });
    let (who, pj): (String, String) = next_signal(&mut profiles, |_: &(String, String)| true).await;
    assert_eq!(who, BOB);
    let prof = json(&pj);
    assert_eq!((prof["name"].as_str(), prof["first"].as_str(), prof["last"].as_str()), (Some("Bobby"), Some("Bob"), None));
    let photo = prof["photo"].as_str().unwrap();
    assert!(photo.starts_with(h.dir.path().join("profiles").to_str().unwrap()) && photo.ends_with(".png"), "{photo}");
    let got: String = p.call("GetHandleProfile", &("bob@example.com",)).await.unwrap();
    assert_eq!(json(&got), prof);
    let chat: String = p.call("GetChat", &(BOB,)).await.unwrap();
    assert_eq!(json(&chat)["participants"][0]["profile"], prof);
    assert_eq!(json(&chat)["participants"][0]["uri"], BOB);
    let e = p.call::<_, _, String>("GetHandleProfile", &("nobody",)).await.unwrap_err();
    assert_eq!(error_name(&e), "org.imessage.Daemon1.Error.InvalidHandle");

    h.mock.inject(EngineEvent::ProfileShared { handle: Handle::new(BOB), share: None, at: 2_000_000_000_002 });
    let (_, pj): (String, String) = next_signal(&mut profiles, |_: &(String, String)| true).await;
    assert_eq!(pj, "null");
    assert!(!format!("{:?}", h.mock.calls()).contains("PROFILE-KEY"));
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn property_change_signals() {
    let Some(h) = Harness::start().await else { return };
    let p = &h.p;
    let props = zbus::fdo::PropertiesProxy::builder(&h.conn).destination(BUS_NAME).unwrap().path(OBJECT_PATH).unwrap().build().await.unwrap();
    let mut prop_changes = props.receive_properties_changed().await.unwrap();
    assert_eq!(p.get_property::<String>("ConnectionState").await.unwrap(), "needs_login");

    h.mock.set_connection_state(ConnectionState::Connecting);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let ch = prop_changes.next().await.unwrap();
            let args = ch.args().unwrap();
            if let Some(v) = args.changed_properties().get("ConnectionState")
                && String::try_from(OwnedValue::try_from(v.try_clone().unwrap()).unwrap()).unwrap() == "connecting"
            {
                break;
            }
        }
    })
    .await
    .expect("PropertiesChanged(ConnectionState=connecting) not received");
    assert_eq!(p.get_property::<String>("ConnectionState").await.unwrap(), "connecting");

    h.mock.login("me@example.com", "pw").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let ch = prop_changes.next().await.unwrap();
            let args = ch.args().unwrap();
            if args.changed_properties().contains_key("Registered") {
                break;
            }
        }
    })
    .await
    .expect("PropertiesChanged(Registered) not received");
    assert!(p.get_property::<bool>("Registered").await.unwrap());
    h.shutdown().await;
}

/// Engine errors are relayed as `BackgroundError`; so is an overflow of the
/// daemon's event queue. Current-thread runtime: the pump cannot run while
/// this test injects, so the queue overflows deterministically.
#[tokio::test(flavor = "current_thread")]
async fn background_error_signal() {
    let Some(h) = Harness::start().await else { return };
    let mut errors = h.p.receive_signal("BackgroundError").await.unwrap();
    h.mock.inject(EngineEvent::Error("registration rejected".into()));
    let (m,): (String,) = next_signal(&mut errors, |_: &(String,)| true).await;
    assert_eq!(m, "registration rejected");

    for _ in 0..(imessaged::engine::EVENT_CHANNEL_CAPACITY + 100) {
        h.mock.inject(EngineEvent::TwoFactorRequired);
    }
    let (m,): (String,) = next_signal(&mut errors, |(m,): &(String,)| m.contains("dropped")).await;
    assert!(m.contains("resync"), "{m}");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_operations() {
    let Some(h) = Harness::logged_in().await else { return };
    let p = &h.p;
    let mut received = p.receive_signal("MessageReceived").await.unwrap();
    let mut updated = p.receive_signal("MessageUpdated").await.unwrap();
    let mut chat_updated = p.receive_signal("ChatUpdated").await.unwrap();
    let mut typing = p.receive_signal("Typing").await.unwrap();
    let sent_id = h.call_json("SendText", &(BOB, "", "hello bob", "")).await["id"].as_str().unwrap().to_string();
    h.mock.inject(incoming("IN-1", 2_000_000_000_000, "hi", None));
    next_signal(&mut received, |(_, m): &(String, String)| json(m)["id"] == "IN-1").await;

    // React / Edit / Unsend on our message -> MessageUpdated
    let () = p.call("React", &(BOB, sent_id.as_str(), "love", false)).await.unwrap();
    let (_, mj): (String, String) = next_signal(&mut updated, |(_, m): &(String, String)| json(m)["id"] == sent_id.as_str()).await;
    assert_eq!(json(&mj)["reactions"][0]["kind"], "love");
    assert_eq!(json(&mj)["reactions"][0]["sender"]["uri"], "mailto:me@example.com");
    let e = p.call::<_, _, ()>("React", &(BOB, sent_id.as_str(), "nonsense", false)).await.unwrap_err();
    assert_eq!(error_name(&e), "org.imessage.Daemon1.Error.InvalidInput");
    let () = p.call("Edit", &(BOB, sent_id.as_str(), "hello, bob")).await.unwrap();
    let (_, mj): (String, String) = next_signal(&mut updated, |(_, m): &(String, String)| json(m)["edited"] == true).await;
    assert_eq!(json(&mj)["text"], "hello, bob");
    let e = p.call::<_, _, ()>("Edit", &(BOB, "IN-1", "not mine")).await.unwrap_err();
    assert_eq!(error_name(&e), "org.imessage.Daemon1.Error.InvalidInput");
    let () = p.call("Unsend", &(BOB, sent_id.as_str())).await.unwrap();
    let (_, mj): (String, String) = next_signal(&mut updated, |(_, m): &(String, String)| json(m)["unsent"] == true).await;
    assert!(json(&mj)["text"].is_null());

    // MarkRead -> ChatUpdated(unread 0) + read receipt
    let () = p.call("MarkRead", &(BOB, "IN-1")).await.unwrap();
    next_signal(&mut chat_updated, |(c,): &(String,)| json(c)["unread_count"] == 0).await;
    assert!(h.mock.calls().iter().any(|c| matches!(c, MockCall::MarkRead { up_to, .. } if up_to.0 == "IN-1")));

    // never a read receipt for a message from one of our other_handles
    // (a row stored before the address was configured: is_from_me = false)
    h.mock.set_other_handles(vec!["tel:+15551234567".into()]);
    let EngineEvent::MessageReceived { mut message, chat, attachments } = incoming("IN-PHONE", 2_000_000_000_500, "from my phone", None) else { unreachable!() };
    message.sender = Handle::new("tel:+15551234567");
    h.mock.inject(EngineEvent::MessageReceived { message, chat, attachments });
    next_signal(&mut received, |(_, m): &(String, String)| json(m)["id"] == "IN-PHONE").await;
    h.mock.clear_calls();
    for up_to in ["", "IN-PHONE"] {
        let () = p.call("MarkRead", &(BOB, up_to)).await.unwrap();
    }
    assert!(!h.mock.calls().iter().any(|c| matches!(c, MockCall::MarkRead { up_to, .. } if up_to.0 == "IN-PHONE")), "{:?}", h.mock.calls());
    h.mock.set_other_handles(vec![]);

    // typing in and out
    let () = p.call("SetTyping", &(BOB, true)).await.unwrap();
    assert!(h.mock.calls().iter().any(|c| matches!(c, MockCall::SetTyping { typing: true, .. })));
    h.mock.inject(EngineEvent::Typing(TypingEvent { chat_id: ChatId::direct(BOB), sender: Handle::new(BOB), typing: true }));
    next_signal(&mut typing, |(_, hd, t): &(String, String, bool)| hd == BOB && *t).await;
    next_signal(&mut typing, |(_, _, t): &(String, String, bool)| !*t).await; // 200 ms timeout

    // LookupHandles
    h.mock.set_reachable("bob@example.com", true);
    let r = h.call_json("LookupHandles", &("[\"bob@example.com\",\"zed@example.com\"]",)).await;
    assert_eq!(r[0]["handle"], BOB);
    assert_eq!(r[0]["reachable"], true);
    assert_eq!(r[1]["reachable"], false);
    let e = p.call::<_, _, String>("LookupHandles", &("not json",)).await.unwrap_err();
    assert_eq!(error_name(&e), "org.imessage.Daemon1.Error.InvalidInput");

    // SendFile
    let f = h.dir.path().join("note.txt");
    std::fs::write(&f, b"file body").unwrap();
    let s = h.call_json("SendFile", &(BOB, "", f.to_str().unwrap(), "text/plain")).await;
    let (_, mj): (String, String) = next_signal(&mut received, |(_, m): &(String, String)| json(m)["id"] == s["id"]).await;
    assert_eq!(json(&mj)["parts"][0]["filename"], "note.txt");
    let e = p.call::<_, _, String>("SendFile", &(BOB, "", "relative.txt", "")).await.unwrap_err();
    assert_eq!(error_name(&e), "org.imessage.Daemon1.Error.InvalidInput");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout() {
    let Some(h) = Harness::logged_in().await else { return };
    let p = &h.p;
    let mut conn_state = p.receive_signal("ConnectionStateChanged").await.unwrap();
    assert!(p.get_property::<bool>("Registered").await.unwrap());
    let () = p.call("Logout", &()).await.unwrap();
    next_signal(&mut conn_state, |(s,): &(String,)| s == "needs_login").await;
    assert!(!p.get_property::<bool>("Registered").await.unwrap());
    assert!(h.mock.calls().contains(&MockCall::Logout));
    let st = h.call_json("GetStatus", &()).await;
    assert_eq!(st["registered"], false);
    assert!(st["registration"]["handles"].as_array().unwrap().is_empty());
    let e = p.call::<_, _, String>("SendText", &(BOB, "", "x", "")).await.unwrap_err();
    assert_eq!(error_name(&e), "org.imessage.Daemon1.Error.NotLoggedIn");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn purge_invalid_chats() {
    let Some(h) = Harness::start().await else { return };
    let p = &h.p;
    let batch = serde_json::json!({
        "chats": [{"id": "46339(smsft)", "participants": [], "is_group": false, "unread_count": 0}],
    });
    let e = p.call::<_, _, String>("ImportMessages", &(batch.to_string(),)).await.unwrap_err();
    assert_eq!(error_name(&e), "org.imessage.Daemon1.Error.InvalidInput");
    assert!(e.to_string().contains("46339(smsft)"), "{e}");
    let s: String = p.call("PurgeInvalidChats", &(true,)).await.unwrap();
    let v = json(&s);
    assert_eq!((v["dry_run"].as_bool(), v["chats"].as_array().map(Vec::len), v["messages"].as_u64()), (Some(true), Some(0), Some(0)));
    let s: String = p.call("PurgeInvalidChats", &(false,)).await.unwrap();
    assert_eq!(json(&s)["dry_run"], false);
    h.shutdown().await;
}
