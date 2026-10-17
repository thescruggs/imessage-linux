//! Text Message Forwarding through the hub: choosing SMS vs iMessage,
//! what text messages refuse, activation state and signals.

use std::sync::Arc;
use std::time::Duration;

use imessage_common::model::{Chat, ChatId, DeliveryState, Handle, Message, MessageId, MessagePart, Service, SmsState};
use imessaged::config::DaemonConfig;
use imessaged::daemon::Daemon;
use imessaged::engine::mock::{MockCall, MockEngine};
use imessaged::engine::{ChatTarget, Engine, EngineError, EngineEvent};
use imessaged::hub::{DaemonSignal, OpError};
use imessaged::keyring::StateKey;
use imessaged::store::{Store, StoreHandle};

const MYNUM: &str = "tel:+15555550100";
const PEER: &str = "tel:+15555550123";
const FRIEND: &str = "tel:+15555550124";

fn store(dir: &std::path::Path) -> StoreHandle {
    StoreHandle::new(Store::open(dir, StateKey::from_bytes([4; 32])).unwrap())
}

fn lookups(mock: &MockEngine) -> usize {
    mock.calls().iter().filter(|c| matches!(c, MockCall::LookupHandles(_))).count()
}

#[tokio::test]
async fn sms_routing_storage_and_limits() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(MockEngine::new());
    let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone()).with_store(store(dir.path())).start().await.unwrap();
    let hub = running.hub();
    let mut sig = running.signals();
    mock.login("me@example.com", "pw").await.unwrap();
    mock.set_other_handles(vec![MYNUM.into()]);
    mock.set_reachable(FRIEND, true);

    // forwarding off: everything is iMessage, nobody is looked up
    let sent = hub.send_text(ChatTarget::Participants(vec![PEER.into()]), "first", None).await.unwrap();
    assert_eq!(sent.service, Service::Imessage);
    assert_eq!(lookups(&mock), 0);

    // turn it on: requested, announced, in the status
    let s = hub.enable_sms_forwarding(true).await.unwrap();
    assert_eq!(s.state, SmsState::Requested);
    assert_eq!(s.number.as_deref(), Some(MYNUM));
    assert!(mock.calls().contains(&MockCall::SetSmsForwarding(true)));
    let got = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let DaemonSignal::SmsForwardingChanged(s) = sig.recv().await.unwrap() {
                return s;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(got.state, SmsState::Requested);
    assert_eq!(hub.status().await.sms.state, SmsState::Requested);
    // requested is not active: still iMessage unless asked
    assert_eq!(hub.send_text(ChatTarget::Chat(ChatId::direct(PEER)), "x", None).await.unwrap().service, Service::Imessage);

    // the iPhone confirmed
    mock.activate_sms(MYNUM);
    mock.clear_calls();
    // a number not on iMessage goes out as SMS, the row says so
    let sent = hub.send_text(ChatTarget::Chat(ChatId::direct(PEER)), "via sms", Some(MessageId::new("ignored-reply"))).await.unwrap();
    assert_eq!(sent.service, Service::Sms);
    assert!(mock.calls().iter().any(|c| matches!(c, MockCall::SendSms { text: Some(t), .. } if t == "via sms")));
    let st = running.store().unwrap();
    let id = sent.id.clone();
    let row = st.call(move |s| s.get_message(&id)).await.unwrap().unwrap();
    assert_eq!(row.service, Service::Sms);
    assert_eq!(row.reply_to, None, "text messages carry no reply reference");
    assert_eq!(row.chat_id.0, PEER);
    // the answer is cached
    assert_eq!(lookups(&mock), 1);
    hub.send_text(ChatTarget::Chat(ChatId::direct(PEER)), "again", None).await.unwrap();
    assert_eq!(lookups(&mock), 1);

    // a number on iMessage, an email address, an explicit choice
    assert_eq!(hub.send_text(ChatTarget::Participants(vec![FRIEND.into()]), "im", None).await.unwrap().service, Service::Imessage);
    assert_eq!(hub.send_text(ChatTarget::Participants(vec!["bob@example.com".into()]), "im", None).await.unwrap().service, Service::Imessage);
    assert_eq!(hub.send_text_as(ChatTarget::Participants(vec![FRIEND.into()]), "sms", None, Some(Service::Sms)).await.unwrap().service, Service::Sms);
    assert_eq!(hub.send_text_as(ChatTarget::Chat(ChatId::direct(PEER)), "im", None, Some(Service::Imessage)).await.unwrap().service, Service::Imessage);

    // an incoming text lands in the same chat as iMessage with that number
    let chat = Chat {
        id: ChatId::direct(PEER),
        participants: vec![Handle::new(PEER)],
        display_name: None,
        is_group: false,
        last_message_at: Some(20),
        unread_count: 0,
        background: None,
        icon_path: None,
    };
    let incoming = Message {
        id: MessageId::new("SMS-IN-1"),
        chat_id: chat.id.clone(),
        sender: Handle::new(PEER),
        sent_at: 9_999_999_999_999,
        text: Some("hello from android".into()),
        parts: vec![MessagePart::Text { text: "hello from android".into() }],
        subject: None,
        reply_to: None,
        is_from_me: false,
        delivery: DeliveryState::Delivered,
        edited: false,
        unsent: false,
        reactions: vec![],
        effect: None,
        verified: true,
        service: Service::Sms,
    };
    mock.inject(EngineEvent::MessageReceived { message: incoming, chat, attachments: vec![] });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let DaemonSignal::MessageReceived { message, .. } = sig.recv().await.unwrap()
                && message.id.0 == "SMS-IN-1"
            {
                assert_eq!(message.service, Service::Sms);
                return;
            }
        }
    })
    .await
    .unwrap();
    let c = ChatId::direct(PEER);
    let page = st.call(move |s| s.list_messages(&c, None, 50)).await.unwrap();
    assert!(page.messages.iter().any(|m| m.service == Service::Imessage) && page.messages.iter().any(|m| m.service == Service::Sms));

    // no tapbacks / edits / unsend / typing on text messages; no read receipt
    let unsupported = |r: Result<(), OpError>| matches!(r, Err(OpError::Engine(EngineError::Unsupported(_))));
    assert!(unsupported(hub.react(PEER, "SMS-IN-1", "love", false).await));
    assert!(unsupported(hub.edit(PEER, &sent.id.0, "changed").await));
    assert!(unsupported(hub.unsend(PEER, &sent.id.0).await));
    assert!(unsupported(hub.set_typing(PEER, true).await));
    mock.clear_calls();
    hub.mark_read(PEER, "SMS-IN-1").await.unwrap();
    assert!(!mock.calls().iter().any(|c| matches!(c, MockCall::MarkRead { .. })), "no read receipt for SMS");
    assert_eq!(st.call(|s| s.get_chat(&ChatId::direct(PEER))).await.unwrap().unwrap().unread_count, 0);

    // turned off: iMessage again
    hub.enable_sms_forwarding(false).await.unwrap();
    assert_eq!(hub.status().await.sms.state, SmsState::Off);
    assert_eq!(hub.send_text(ChatTarget::Chat(ChatId::direct(PEER)), "x", None).await.unwrap().service, Service::Imessage);
    running.shutdown().await.unwrap();
}

/// A group of numbers, one of them not on iMessage, is a group text keyed
/// by the numbers.
#[tokio::test]
async fn group_text() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(MockEngine::new());
    let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone()).with_store(store(dir.path())).start().await.unwrap();
    mock.login("me@example.com", "pw").await.unwrap();
    mock.set_other_handles(vec![MYNUM.into()]);
    mock.set_reachable(FRIEND, true);
    mock.activate_sms(MYNUM);
    let id = ChatId::group_by_participants(&[PEER.into(), FRIEND.into()]);
    let hub = running.hub();
    let sent = hub.send_text(ChatTarget::Chat(id.clone()), "all of you", None).await.unwrap();
    assert_eq!(sent.service, Service::Sms);
    assert!(mock.calls().iter().any(|c| matches!(c, MockCall::SendSms { chat: ChatTarget::Chat(c), .. } if *c == id)));
    let st = running.store().unwrap();
    let k = id.clone();
    let chat = st.call(move |s| s.get_chat(&k)).await.unwrap().unwrap();
    assert!(chat.is_group);
    assert_eq!(chat.participants.len(), 2);
    // an iMessage group with a group id is never auto-switched
    let g = ChatId::group("CCCC-3");
    assert_eq!(hub.choose_service(&ChatTarget::Chat(g), None).await, Service::Imessage);
    running.shutdown().await.unwrap();
}
