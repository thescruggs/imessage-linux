//! Daemon wiring without D-Bus: engine events -> store -> signals, group
//! re-seeding after a restart, typing timeouts, shutdown checkpoint.

use std::sync::Arc;
use std::time::Duration;

use imessage_common::model::{Chat, ChatId, DeliveryState, Handle, Message, MessageId, MessagePart, TypingEvent};
use imessaged::config::DaemonConfig;
use imessaged::daemon::Daemon;
use imessaged::engine::mock::{MockCall, MockEngine};
use imessaged::engine::{ChatTarget, Engine, EngineEvent};
use imessaged::hub::DaemonSignal;
use imessaged::keyring::StateKey;
use imessaged::store::{Store, StoreHandle};
use tokio::sync::broadcast;

const BOB: &str = "mailto:bob@example.com";

fn store(dir: &std::path::Path) -> StoreHandle {
    StoreHandle::new(Store::open(dir, StateKey::from_bytes([3; 32])).unwrap())
}

fn incoming(id: &str, chat: &Chat, from: &str, at: u64, text: &str) -> EngineEvent {
    EngineEvent::MessageReceived {
        message: Message {
            id: MessageId::new(id),
            chat_id: chat.id.clone(),
            sender: Handle::new(from),
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
        chat: chat.clone(),
        attachments: vec![],
    }
}

async fn next_matching(rx: &mut broadcast::Receiver<DaemonSignal>, f: impl Fn(&DaemonSignal) -> bool) -> DaemonSignal {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let s = rx.recv().await.unwrap();
            if f(&s) {
                return s;
            }
        }
    })
    .await
    .expect("signal not seen within 5s")
}

#[tokio::test]
async fn events_reach_store_then_signals_and_groups_are_reseeded() {
    let dir = tempfile::tempdir().unwrap();
    let group = Chat {
        id: ChatId::group("AAAA-1"),
        participants: vec![Handle::new(BOB), Handle::new("carol@example.com")],
        display_name: Some("Team".into()),
        is_group: true,
        last_message_at: Some(10),
        unread_count: 0,
        background: None,
        icon_path: None,
    };

    // first run: a group message arrives
    {
        let mock = Arc::new(MockEngine::new());
        let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone())
            .with_store(store(dir.path()))
            .with_typing_timeout(Duration::from_millis(150))
            .start()
            .await
            .unwrap();
        let mut sig = running.signals();
        mock.inject(incoming("G1", &group, BOB, 10, "hello team"));
        let s = next_matching(&mut sig, |s| matches!(s, DaemonSignal::MessageReceived { .. })).await;
        // the signal fires only after the row is stored
        let DaemonSignal::MessageReceived { message, .. } = s else { unreachable!() };
        let st = running.store().unwrap();
        let id = message.id.clone();
        assert!(st.call(move |s| s.get_message(&id)).await.unwrap().is_some());
        let c = next_matching(&mut sig, |s| matches!(s, DaemonSignal::ChatUpdated(_))).await;
        assert!(matches!(c, DaemonSignal::ChatUpdated(c) if c.unread_count == 1));

        // typing: shown, then cleared by the timeout
        mock.inject(EngineEvent::Typing(TypingEvent { chat_id: group.id.clone(), sender: Handle::new(BOB), typing: true }));
        next_matching(&mut sig, |s| matches!(s, DaemonSignal::Typing { typing: true, .. })).await;
        assert_eq!(running.hub().typing_now().len(), 1);
        next_matching(&mut sig, |s| matches!(s, DaemonSignal::Typing { typing: false, .. })).await;
        assert!(running.hub().typing_now().is_empty());

        mock.inject(incoming("G2", &group, BOB, 11, "another"));
        next_matching(&mut sig, |s| matches!(s, DaemonSignal::MessageReceived { message, .. } if message.id.0 == "G2")).await;

        // connection state is relayed, not stored
        mock.set_connection_state(imessage_common::model::ConnectionState::Connecting);
        next_matching(&mut sig, |s| matches!(s, DaemonSignal::ConnectionStateChanged(_))).await;

        running.shutdown().await.unwrap();
        // closed store: WAL folded back into the db
        let wal = dir.path().join("messages.db-wal");
        assert!(!wal.exists() || std::fs::metadata(&wal).unwrap().len() == 0, "WAL not checkpointed");
        assert_eq!(mock.calls().last(), Some(&MockCall::Shutdown));
    }

    // restart with a fresh engine: the stored group is taught to it, so
    // ChatTarget::Chat(group) resolves
    let mock = Arc::new(MockEngine::new());
    let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone()).with_store(store(dir.path())).start().await.unwrap();
    assert!(mock.calls().contains(&MockCall::UpsertChat(group.id.clone())));
    mock.login("me@example.com", "pw").await.unwrap();
    let sent = running.hub().send_text(ChatTarget::Chat(group.id.clone()), "back again", None).await.unwrap();
    assert_eq!(sent.chat_id, group.id);
    let st = running.store().unwrap();
    let gid = group.id.clone();
    let page = st.call(move |s| s.list_messages(&gid, None, 10)).await.unwrap();
    assert_eq!(page.messages.len(), 3);
    assert!(page.messages[2].is_from_me);
    assert_eq!(page.messages[2].sender, Handle::new("me@example.com"));
    running.shutdown().await.unwrap();
}

/// A group created in the store after startup is re-seeded on ChatNotFound.
#[tokio::test]
async fn chat_not_found_reseeds_from_store() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(MockEngine::new());
    let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone()).with_store(store(dir.path())).start().await.unwrap();
    mock.login("me@example.com", "pw").await.unwrap();
    let g = Chat { id: ChatId::group("BBBB-2"), participants: vec![Handle::new(BOB)], display_name: None, is_group: true, last_message_at: None, unread_count: 0, background: None, icon_path: None };
    let st = running.store().unwrap();
    let g2 = g.clone();
    st.call(move |s| s.upsert_chat(&g2)).await.unwrap();
    let sent = running.hub().send_text(ChatTarget::Chat(g.id.clone()), "hi", None).await.unwrap();
    assert_eq!(sent.chat_id, g.id);
    assert!(mock.calls().contains(&MockCall::UpsertChat(g.id.clone())));
    running.shutdown().await.unwrap();
}

/// A message from the typing person clears their indicator. The typing
/// timeout is an hour, so only the message can have cleared it.
#[tokio::test]
async fn typing_is_cleared_by_a_message_from_the_same_sender() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(MockEngine::new());
    let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone())
        .with_store(store(dir.path()))
        .with_typing_timeout(Duration::from_secs(3600))
        .start()
        .await
        .unwrap();
    let mut sig = running.signals();
    let chat = Chat { id: ChatId::direct(BOB), participants: vec![Handle::new(BOB)], display_name: None, is_group: false, last_message_at: None, unread_count: 0, background: None, icon_path: None };
    mock.inject(EngineEvent::Typing(TypingEvent { chat_id: chat.id.clone(), sender: Handle::new(BOB), typing: true }));
    next_matching(&mut sig, |s| matches!(s, DaemonSignal::Typing { typing: true, .. })).await;
    assert_eq!(running.hub().typing_now().len(), 1);
    mock.inject(incoming("T1", &chat, BOB, 11, "done typing"));
    let s = next_matching(&mut sig, |s| matches!(s, DaemonSignal::Typing { .. } | DaemonSignal::MessageReceived { .. })).await;
    assert!(matches!(s, DaemonSignal::Typing { typing: false, ref handle, .. } if *handle == Handle::new(BOB)), "typing cleared before the message is announced: {s:?}");
    assert!(running.hub().typing_now().is_empty());
    running.shutdown().await.unwrap();
}

/// B1 / M1 through the daemon: a peer's edit / unsend of our message and a
/// peer's "read" of their own message change nothing and signal nothing;
/// the same from our own devices does apply.
#[tokio::test]
async fn stranger_edit_unsend_and_read_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(MockEngine::new());
    let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone()).with_store(store(dir.path())).start().await.unwrap();
    mock.login("me@example.com", "pw").await.unwrap();
    let bob = ChatId::direct(BOB);
    let chat = Chat { id: bob.clone(), participants: vec![Handle::new(BOB)], display_name: None, is_group: false, last_message_at: None, unread_count: 0, background: None, icon_path: None };
    let mut sig = running.signals();
    let own = running.hub().send_text(ChatTarget::Chat(bob.clone()), "my words", None).await.unwrap().id;
    mock.inject(incoming("IN1", &chat, BOB, 2_000_000_000_000, "bob's words"));
    next_matching(&mut sig, |s| matches!(s, DaemonSignal::MessageReceived { message, .. } if message.id.0 == "IN1")).await;

    mock.inject(MockEngine::edited(&bob, &own, BOB, "bob rewrote this", 2_000_000_000_001));
    mock.inject(MockEngine::unsent(&bob, &own, "carol@example.com", 2_000_000_000_002));
    mock.inject(MockEngine::read(Some(&bob), &MessageId::new("IN1"), BOB, 2_000_000_000_003));
    // events are handled strictly in order: once this one is through, so are the above
    mock.inject(incoming("SENTINEL", &chat, BOB, 2_000_000_000_004, "sentinel"));
    loop {
        let s = next_matching(&mut sig, |_| true).await;
        match s {
            DaemonSignal::MessageReceived { message, .. } if message.id.0 == "SENTINEL" => break,
            DaemonSignal::MessageUpdated { message, .. } => panic!("stranger event changed {}", message.id),
            DaemonSignal::ChatUpdated(c) => assert_eq!(c.unread_count, 1, "peer cleared our unread state"),
            _ => {}
        }
    }
    let st = running.store().unwrap();
    let o = own.clone();
    let m = st.call(move |s| s.get_message(&o)).await.unwrap().unwrap();
    assert!(!m.edited && !m.unsent);
    assert_eq!(m.text.as_deref(), Some("my words"));
    assert_eq!(st.call(|s| s.get_chat(&ChatId::direct(BOB))).await.unwrap().unwrap().unread_count, 2);

    // the same from our own devices applies
    mock.inject(MockEngine::edited_from_me(&bob, &own, "me@example.com", "my better words", 2_000_000_000_005));
    next_matching(&mut sig, |s| matches!(s, DaemonSignal::MessageUpdated { message, .. } if message.edited)).await;
    mock.inject(MockEngine::read_by_me(Some(&bob), &MessageId::new("SENTINEL"), "me@example.com", 2_000_000_000_006));
    next_matching(&mut sig, |s| matches!(s, DaemonSignal::ChatUpdated(c) if c.unread_count == 0)).await;
    mock.inject(MockEngine::unsent_from_me(&bob, &own, "me@example.com", 2_000_000_000_007));
    next_matching(&mut sig, |s| matches!(s, DaemonSignal::MessageUpdated { message, .. } if message.unsent)).await;
    running.shutdown().await.unwrap();
}

/// A sent file is copied into the attachments dir, so it stays downloadable
/// after the user deletes the original.
#[tokio::test]
async fn sent_attachment_survives_deleting_the_original() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mock = Arc::new(MockEngine::new());
    let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone()).with_store(store(dir.path())).start().await.unwrap();
    mock.login("me@example.com", "pw").await.unwrap();
    let original = outside.path().join("linux-test.png");
    std::fs::write(&original, b"\x89PNG not really").unwrap();
    let hub = running.hub();
    let sent = hub.send_file(ChatTarget::Participants(vec![BOB.into()]), original.to_str().unwrap(), "image/png").await.unwrap();
    std::fs::remove_file(&original).unwrap();

    let id = format!("{}:0", sent.id.as_str());
    let path = hub.download_attachment(&id).await.unwrap();
    let p = std::path::Path::new(&path);
    assert!(p.starts_with(hub.attachments_dir()), "{path}");
    assert_eq!(p.file_name().unwrap(), "linux-test.png");
    assert_eq!(std::fs::read(p).unwrap(), b"\x89PNG not really");
    assert_eq!(std::fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(p.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
    // no temp files left behind
    assert_eq!(std::fs::read_dir(p.parent().unwrap()).unwrap().count(), 1);
    running.shutdown().await.unwrap();
}

/// B1: a background announcing a huge size is neither stored nor fetched,
/// and asking for the chat afterwards does not start a download either.
#[tokio::test]
async fn huge_background_is_not_fetched() {
    use imessaged::engine::BackgroundRef;
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(MockEngine::new());
    let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone()).with_store(store(dir.path())).start().await.unwrap();
    mock.login("me@example.com", "pw").await.unwrap();
    let chat = Chat { id: ChatId::direct(BOB), participants: vec![Handle::new(BOB)], display_name: None, is_group: false, last_message_at: None, unread_count: 0, background: None, icon_path: None };
    let mut sig = running.signals();
    mock.inject(incoming("H1", &chat, BOB, 2_000_000_000_000, "hi"));
    let huge = BackgroundRef { background_id: "HUGE".into(), payload_version: 1, seq: 1, size: 1 << 40, locator: "{\"v\":1,\"plist\":\"K\"}".into() };
    mock.inject(EngineEvent::ChatBackground { chat_id: chat.id.clone(), background: Some(huge), by: Handle::new(BOB), at: 2_000_000_000_001 });
    mock.inject(incoming("H2", &chat, BOB, 2_000_000_000_002, "sentinel"));
    next_matching(&mut sig, |s| matches!(s, DaemonSignal::MessageReceived { message, .. } if message.id.0 == "H2")).await;
    let c = running.hub().get_chat(&chat.id.0).await.unwrap();
    assert!(c.background.is_none());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!mock.calls().iter().any(|c| matches!(c, MockCall::DownloadAttachment { .. })), "{:?}", mock.calls());
    running.shutdown().await.unwrap();
}

/// S2: a background that replaces the one being downloaded (its own fetch
/// request finds the chat in flight) is downloaded right after.
#[tokio::test]
async fn background_replaced_while_downloading_is_fetched() {
    use imessaged::engine::BackgroundRef;
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(MockEngine::new());
    let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone()).with_store(store(dir.path())).start().await.unwrap();
    mock.login("me@example.com", "pw").await.unwrap();
    mock.set_download_delay(Duration::from_millis(300));
    let chat = Chat { id: ChatId::direct(BOB), participants: vec![Handle::new(BOB)], display_name: None, is_group: false, last_message_at: None, unread_count: 0, background: None, icon_path: None };
    let mut sig = running.signals();
    mock.inject(incoming("R1", &chat, BOB, 2_000_000_000_000, "hi"));
    let bg = |id: &str| BackgroundRef { background_id: id.into(), payload_version: 1, seq: 1, size: 10, locator: "{\"v\":1,\"plist\":\"K\"}".into() };
    mock.inject(EngineEvent::ChatBackground { chat_id: chat.id.clone(), background: Some(bg("A")), by: Handle::new(BOB), at: 2_000_000_000_001 });
    next_matching(&mut sig, |s| matches!(s, DaemonSignal::ChatUpdated(c) if c.background.as_ref().is_some_and(|b| b.id == "A"))).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    mock.inject(EngineEvent::ChatBackground { chat_id: chat.id.clone(), background: Some(bg("B")), by: Handle::new(BOB), at: 2_000_000_000_002 });
    let s = next_matching(&mut sig, |s| matches!(s, DaemonSignal::ChatUpdated(c) if c.background.as_ref().is_some_and(|b| b.payload.is_some()))).await;
    let DaemonSignal::ChatUpdated(c) = s else { unreachable!() };
    let b = c.background.unwrap();
    assert_eq!(b.id, "B");
    assert!(b.payload.as_deref().unwrap().ends_with("/B"));
    let downloads: Vec<_> = mock.calls().into_iter().filter(|c| matches!(c, MockCall::DownloadAttachment { .. })).collect();
    assert_eq!(downloads.len(), 2, "{downloads:?}");
    let bg_dir = running.hub().backgrounds_dir();
    let leftover: Vec<_> = std::fs::read_dir(bg_dir.join(std::fs::read_dir(&bg_dir).unwrap().next().unwrap().unwrap().file_name()))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(leftover, vec!["B".to_string()], "A's download was discarded");
    running.shutdown().await.unwrap();
}
