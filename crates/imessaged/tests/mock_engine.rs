//! Proves the Engine/MockEngine surface is usable from outside the crate
//! (what Phase 2b's store / D-Bus integration tests will do).

use std::sync::Arc;

use imessage_common::model::{ChatId, Handle, TypingEvent};
use imessaged::config::DaemonConfig;
use imessaged::daemon::Daemon;
use imessaged::engine::mock::{MockCall, MockEngine};
use imessaged::engine::{ChatTarget, Engine, EngineEvent, LoginOutcome};

#[tokio::test]
async fn external_crate_can_drive_mock_engine() {
    let mock = Arc::new(MockEngine::new());
    let engine: Arc<dyn Engine> = mock.clone();
    let daemon = Daemon::with_engine(DaemonConfig::new("/tmp/unused".into()), engine.clone());
    let mut events = daemon.events();

    assert_eq!(engine.login("me@example.com", "pw").await.unwrap(), LoginOutcome::Ok);
    let sent = engine.send_text(&ChatTarget::Chat(ChatId::direct("bob@example.com")), "hello", None).await.unwrap();
    assert_eq!(sent.chat_id, ChatId::direct("bob@example.com"));
    let id = sent.id;

    let typing = EngineEvent::Typing(TypingEvent { chat_id: ChatId::direct("bob@example.com"), sender: Handle::new("bob@example.com"), typing: true });
    mock.inject(typing.clone());

    let mut got = vec![];
    loop {
        let ev = events.recv().await.unwrap();
        let done = ev == typing;
        got.push(ev);
        if done {
            break;
        }
    }
    assert!(got.iter().any(|e| matches!(e, EngineEvent::MessageSent { id: i, .. } if *i == id)));
    // Events serialize for D-Bus/JSON transport.
    assert!(serde_json::to_string(&got).unwrap().contains("\"event\":\"typing\""));

    daemon.run_until(async {}).await.unwrap();
    assert_eq!(mock.calls().last(), Some(&MockCall::Shutdown));
}
