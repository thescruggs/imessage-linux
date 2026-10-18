//! `imsg-tui`'s backend and app against a real `imessaged` (in-process,
//! `MockEngine`) on a private `dbus-daemon`, as crates/imsg/tests does.
//! Prints `SKIP:` and passes when `dbus-daemon` is not installed.

#[path = "../../imsg/tests/common/mod.rs"]
mod common;

use std::sync::Arc;
use std::time::Duration;

use imessage_common::model::{Chat, ChatId, DeliveryState, Handle, Message, MessageId, MessagePart};
use imessaged::config::DaemonConfig;
use imessaged::daemon::{Daemon, RunningDaemon};
use imessaged::dbus::{BUS_NAME, BusChoice};
use imessaged::engine::mock::{MockCall, MockEngine};
use imessaged::engine::{ChatTarget, Engine, EngineEvent};
use imessaged::keyring::StateKey;
use imessaged::store::{Store, StoreHandle};
use imsg_client::{Bus, Target};
use imsg_tui::app::{App, AppEvent};
use imsg_tui::backend::{Backend, DaemonEvent};
use imsg_tui::contacts::{Contacts, Person};
use tokio::sync::mpsc;

const BOB: &str = "mailto:bob@example.com";

struct Harness {
    bus: common::PrivateBus,
    _dir: tempfile::TempDir,
    mock: Arc<MockEngine>,
    running: Option<RunningDaemon>,
}

impl Harness {
    async fn logged_in() -> Option<Harness> {
        let bus = common::PrivateBus::start()?;
        let dir = tempfile::tempdir().unwrap();
        let mock = Arc::new(MockEngine::new());
        let store = StoreHandle::new(Store::open(dir.path(), StateKey::from_bytes([7; 32])).unwrap());
        let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone())
            .with_store(store)
            .with_dbus(BusChoice::Address(bus.address.clone()))
            .start()
            .await
            .unwrap();
        let conn = zbus::connection::Builder::address(bus.address.as_str()).unwrap().build().await.unwrap();
        common::wait_for_name(&conn, BUS_NAME, 5).await;
        mock.login("me@example.com", "pw").await.unwrap();
        Some(Harness { bus, _dir: dir, mock, running: Some(running) })
    }

    fn backend(&self) -> Backend {
        Backend::new(Bus::Address(self.bus.address.clone()))
    }

    async fn shutdown(mut self) {
        if let Some(r) = self.running.take() {
            let _ = r.shutdown().await;
        }
    }
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

/// Wait for a daemon event matching `pred` (others are dropped).
async fn wait_for(rx: &mut mpsc::UnboundedReceiver<DaemonEvent>, what: &str, pred: impl Fn(&DaemonEvent) -> bool) -> DaemonEvent {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(ev)) if pred(&ev) => return ev,
            Ok(Some(_)) => {}
            Ok(None) => panic!("signal loop ended while waiting for {what}"),
            Err(_) => panic!("no {what} within 10 s"),
        }
    }
}

/// Pump the app's events until `pred` holds on the app.
async fn pump(app: &mut App, rx: &mut mpsc::UnboundedReceiver<AppEvent>, what: &str, pred: impl Fn(&App) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !pred(app) {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(ev)) => app.on_app_event(ev),
            Ok(None) => panic!("event channel closed while waiting for {what}"),
            Err(_) => panic!("app never reached: {what} (status {:?})", app.status),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backend_calls_signals_and_sending() {
    let Some(h) = Harness::logged_in().await else { return };
    let b = Backend::connect(Bus::Address(h.bus.address.clone())).await.unwrap();
    let st = b.status().await.unwrap();
    assert!(st.registered, "{st:?}");
    assert!(b.chats().await.unwrap().is_empty());

    // Subscribe, then inject: the signal arrives after the row is stored.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let b2 = b.clone();
    let loop_task = tokio::spawn(async move { b2.signal_loop(tx).await });
    wait_for(&mut rx, "Subscribed", |e| matches!(e, DaemonEvent::Subscribed)).await;
    h.mock.inject(incoming("M1", 1_000, "hello there"));
    // Both signals come, in either order.
    let (mut got_msg, mut got_chat) = (false, false);
    while !(got_msg && got_chat) {
        match wait_for(&mut rx, "MessageReceived / ChatUpdated", |e| matches!(e, DaemonEvent::MessageReceived(_) | DaemonEvent::ChatUpdated(_))).await {
            DaemonEvent::MessageReceived(m) => {
                assert_eq!(m.text.as_deref(), Some("hello there"));
                assert_eq!(m.chat_id.0, BOB);
                got_msg = true;
            }
            DaemonEvent::ChatUpdated(c) => {
                assert_eq!(c.id.0, BOB);
                got_chat = true;
            }
            _ => unreachable!(),
        }
    }

    let chats = b.chats().await.unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].unread_count, 1);
    let page = b.messages(BOB, None, 60).await.unwrap();
    assert_eq!(page.messages.len(), 1);
    assert!(page.next_cursor.is_none());
    assert_eq!(b.newest(BOB).await.unwrap().unwrap().id.0, "M1");
    assert_eq!(b.search("HELLO").await.unwrap().len(), 1);

    // Sending to a handle: the daemon announces our own row before replying.
    let sent = b.send_text(&Target::Participants(vec!["Bob@Example.com".into()]), "hi bob", Some("M1")).await.unwrap();
    assert_eq!(sent.chat_id, BOB);
    assert!(h.mock.calls().iter().any(|c| matches!(c, MockCall::SendText { text, reply_to: Some(r), chat: ChatTarget::Participants(_) } if text == "hi bob" && r.0 == "M1")));
    let ev = wait_for(&mut rx, "own MessageReceived", |e| matches!(e, DaemonEvent::MessageReceived(m) if m.is_from_me)).await;
    let DaemonEvent::MessageReceived(mine) = ev else { unreachable!() };
    assert_eq!(mine.id.0, sent.id);

    // Reacting and marking read come back as signals too.
    b.react(BOB, "M1", "love", false).await.unwrap();
    wait_for(&mut rx, "MessageUpdated with reaction", |e| matches!(e, DaemonEvent::MessageUpdated(m) if m.id.0 == "M1" && m.reactions.len() == 1)).await;
    b.mark_read(BOB).await.unwrap();
    wait_for(&mut rx, "ChatUpdated read", |e| matches!(e, DaemonEvent::ChatUpdated(c) if c.id.0 == BOB && c.unread_count == 0)).await;
    b.set_typing(BOB, true).await.unwrap();

    // A file: the daemon keeps its own copy.
    let f = h._dir.path().join("note.txt");
    std::fs::write(&f, b"hello").unwrap();
    let sent = b.send_file(&Target::Chat(BOB.into()), &f.to_string_lossy(), "text/plain").await.unwrap();
    let ev = wait_for(&mut rx, "file MessageReceived", |e| matches!(e, DaemonEvent::MessageReceived(m) if m.id.0 == sent.id)).await;
    let DaemonEvent::MessageReceived(m) = ev else { unreachable!() };
    let att = m.parts.iter().find_map(|p| match p {
        MessagePart::Attachment { attachment_id, .. } => Some(attachment_id.0.clone()),
        _ => None,
    });
    let path = b.download(&att.expect("attachment part")).await.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"hello");

    drop(rx);
    loop_task.abort();
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_loads_chats_previews_and_live_messages() {
    let Some(h) = Harness::logged_in().await else { return };
    h.mock.inject(incoming("M1", 1_000, "first"));
    h.mock.inject(incoming("M2", 2_000, "second"));
    // Injection is asynchronous: wait until the daemon has stored both.
    let b = h.backend();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !b.chats().await.unwrap().first().is_some_and(|c| c.unread_count == 2) {
        assert!(tokio::time::Instant::now() < deadline, "messages were not stored");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let contacts = Contacts::from_people(vec![Person { name: "Bob Smith".into(), phones: vec![], emails: vec!["bob@example.com".into()] }]);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(b, tx.clone(), contacts, vec![]);
    let (dtx, mut drx) = mpsc::unbounded_channel();
    let b = app.backend.clone();
    let loop_task = tokio::spawn(async move { b.signal_loop(dtx).await });
    let forward = {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = drx.recv().await {
                if tx.send(AppEvent::Daemon(ev)).is_err() {
                    break;
                }
            }
        })
    };
    // Nothing is loaded until the subscription is up.
    pump(&mut app, &mut rx, "status", |a| a.daemon.is_some() && a.daemon_ok).await;
    assert!(app.is_me(&Handle::new("me@example.com")));
    pump(&mut app, &mut rx, "chat list", |a| a.chats.len() == 1).await;
    assert_eq!(app.chat_title(&app.chats[0]), "Bob Smith");
    assert_eq!(app.chats[0].unread_count, 2);
    // The chat list fetches the newest message for its preview, and the
    // selected chat's history loads; nothing is marked read by a mere load.
    pump(&mut app, &mut rx, "preview", |a| a.previews.get(BOB).is_some_and(|p| p.text == "second")).await;
    pump(&mut app, &mut rx, "history", |a| a.current_messages().len() == 2).await;
    assert_eq!(app.current_messages()[0].text.as_deref(), Some("first"));
    assert_eq!(app.chats[0].unread_count, 2, "status {:?}", app.status);
    assert!(!app.status.contains("reconnected"), "the first subscription is not a reconnect: {:?}", app.status);
    assert!(!h.mock.calls().iter().any(|c| matches!(c, MockCall::MarkRead { .. })));
    // Typing a filter only shows chats; confirming it opens one, which
    // marks it read (a read receipt goes out).
    app.on_key(crossterm::event::Event::Key(crossterm::event::KeyCode::Char('/').into())).unwrap();
    app.on_key(crossterm::event::Event::Key(crossterm::event::KeyCode::Char('b').into())).unwrap();
    assert_eq!(app.current_chat_id().as_deref(), Some(BOB));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!h.mock.calls().iter().any(|c| matches!(c, MockCall::MarkRead { .. })), "filter keystrokes must not send read receipts");
    app.on_key(crossterm::event::Event::Key(crossterm::event::KeyCode::Enter.into())).unwrap();
    pump(&mut app, &mut rx, "read", |a| a.chats[0].unread_count == 0).await;
    assert!(h.mock.calls().iter().any(|c| matches!(c, MockCall::MarkRead { .. })));

    // A live message in the open chat lands in the transcript and the
    // preview, and is marked read at once.
    h.mock.clear_calls();
    h.mock.inject(incoming("M3", 3_000, "third"));
    pump(&mut app, &mut rx, "live message", |a| a.current_messages().len() == 3).await;
    assert_eq!(app.previews[BOB].text, "third");
    assert_eq!(app.previews[BOB].at, 3_000);
    pump(&mut app, &mut rx, "live read", |a| a.chats[0].unread_count == 0 && a.chats[0].last_message_at == Some(3_000)).await;
    assert!(h.mock.calls().iter().any(|c| matches!(c, MockCall::MarkRead { .. })));

    drop(rx);
    forward.abort();
    loop_task.abort();
    h.shutdown().await;
}
