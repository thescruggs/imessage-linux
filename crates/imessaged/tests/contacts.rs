//! Contact links through the daemon: a person's email and phone chats are
//! merged at startup, new messages from either address land in one chat,
//! and replies go to the address the person last wrote from.

use std::sync::Arc;
use std::time::Duration;

use imessage_common::model::{Chat, ChatId, DeliveryState, Handle, Message, MessageId, MessagePart};
use imessaged::config::DaemonConfig;
use imessaged::daemon::Daemon;
use imessaged::engine::mock::{MOCK_SENT_AT, MockCall, MockEngine};
use imessaged::engine::{ChatTarget, Engine, EngineEvent};
use imessaged::hub::DaemonSignal;
use imessaged::keyring::StateKey;
use imessaged::store::{Store, StoreHandle};
use tokio::sync::broadcast;

const MAIL: &str = "mailto:pat@example.com";
const PHONE: &str = "tel:+15555550142";
const BOB: &str = "mailto:bob@example.com";

fn store(dir: &std::path::Path) -> StoreHandle {
    StoreHandle::new(Store::open(dir, StateKey::from_bytes([5; 32])).unwrap())
}

fn direct(other: &str) -> Chat {
    Chat {
        id: ChatId::direct(other),
        participants: vec![Handle::new(other)],
        display_name: None,
        is_group: false,
        last_message_at: None,
        unread_count: 0,
        background: None,
        icon_path: None,
        pinned: false,
        pin_order: None,
    }
}

fn msg(id: &str, chat: &str, from: &str, at: u64) -> Message {
    Message {
        id: MessageId::new(id),
        chat_id: ChatId::direct(chat),
        sender: Handle::new(from),
        sent_at: at,
        text: Some(id.into()),
        parts: vec![MessagePart::Text { text: id.into() }],
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
    }
}

fn incoming(id: &str, from: &str, at: u64) -> EngineEvent {
    EngineEvent::MessageReceived { message: msg(id, from, from, at), chat: direct(from), attachments: vec![] }
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
async fn one_conversation_per_contact() {
    let dir = tempfile::tempdir().unwrap();
    let cards = dir.path().join("kpeoplevcard");
    std::fs::create_dir(&cards).unwrap();
    std::fs::write(
        cards.join("mac-1.vcf"),
        "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Pat Doe\r\nEMAIL;TYPE=INTERNET:pat@example.com\r\nTEL:(555) 555-0142\r\nEND:VCARD\r\n",
    )
    .unwrap();
    std::fs::write(cards.join("mac-2.vcf"), "BEGIN:VCARD\r\nFN:Bob\r\nEMAIL:bob@example.com\r\nEND:VCARD\r\n").unwrap();

    // history from before the link: one chat per address, the email one pinned and unread
    let st = store(dir.path());
    st.call(|s| {
        s.apply_event(&incoming("M1", MAIL, 100))?;
        s.apply_event(&incoming("P1", PHONE, 200))?;
        s.apply_event(&incoming("B1", BOB, 300))?;
        s.set_chat_pinned(&ChatId::direct(MAIL), true, None)?;
        Ok(())
    })
    .await
    .unwrap();

    let mock = Arc::new(MockEngine::new());
    mock.login("me@example.com", "pw").await.unwrap();
    let mut cfg = DaemonConfig::new(dir.path().into());
    cfg.contacts_dir = cards.clone();
    let running = Daemon::with_engine(cfg, mock.clone()).with_store(st.clone()).start().await.unwrap();
    let hub = running.hub();
    let mut sig = running.signals();

    // merged at startup into the phone number's chat
    let chats = hub.list_chats(50, 0).await.unwrap();
    let ids: Vec<&str> = chats.iter().map(|c| c.id.0.as_str()).collect();
    assert_eq!(ids, vec![BOB, PHONE], "{ids:?}");
    let person = chats.iter().find(|c| c.id.0 == PHONE).unwrap();
    assert_eq!(person.unread_count, 2);
    assert!(person.pinned, "the pin moved with the merge");
    assert_eq!(person.participants, vec![Handle::new(PHONE)]);
    let page = hub.list_messages(PHONE, "", 50).await.unwrap();
    assert_eq!(page.messages.iter().map(|m| m.id.0.as_str()).collect::<Vec<_>>(), vec!["M1", "P1"]);
    // a client still holding the old id gets the merged chat
    assert_eq!(hub.get_chat(MAIL).await.unwrap().id.0, PHONE);
    assert_eq!(hub.list_messages(MAIL, "", 50).await.unwrap().messages.len(), 2);

    // a new message to the email address lands in the same chat
    mock.inject(incoming("M2", MAIL, 400));
    let s = next_matching(&mut sig, |s| matches!(s, DaemonSignal::MessageReceived { .. })).await;
    let DaemonSignal::MessageReceived { chat_id, message } = s else { unreachable!() };
    assert_eq!((chat_id.0.as_str(), message.chat_id.0.as_str()), (PHONE, PHONE));
    assert_eq!(st.call(|s| s.get_chat(&ChatId::direct(MAIL))).await.unwrap(), None, "no chat for the email address comes back");

    // typing from either address shows in the one chat
    mock.inject(EngineEvent::Typing(imessage_common::model::TypingEvent { chat_id: ChatId::direct(MAIL), sender: Handle::new(MAIL), typing: true }));
    let s = next_matching(&mut sig, |s| matches!(s, DaemonSignal::Typing { .. })).await;
    assert!(matches!(s, DaemonSignal::Typing { chat_id, .. } if chat_id.0 == PHONE));

    // a reply goes to the address they last wrote from, filed under the person's chat
    mock.clear_calls();
    let sent = hub.send_text(ChatTarget::Chat(ChatId::direct(PHONE)), "hi", None).await.unwrap();
    assert_eq!(sent.chat_id.0, PHONE);
    assert!(mock.calls().iter().any(|c| matches!(c, MockCall::SendText { chat: ChatTarget::Chat(id), .. } if id.0 == MAIL)), "{:?}", mock.calls());
    let id = sent.id.clone();
    assert_eq!(st.call(move |s| s.get_message(&id)).await.unwrap().unwrap().chat_id.0, PHONE);
    assert_eq!(hub.get_chat(PHONE).await.unwrap().participants, vec![Handle::new(PHONE)]);

    // after they write from the phone, replies follow
    mock.inject(incoming("P2", PHONE, MOCK_SENT_AT + 500));
    next_matching(&mut sig, |s| matches!(s, DaemonSignal::MessageReceived { message, .. } if message.id.0 == "P2")).await;
    mock.clear_calls();
    hub.send_text(ChatTarget::Chat(ChatId::direct(PHONE)), "again", None).await.unwrap();
    assert!(mock.calls().iter().any(|c| matches!(c, MockCall::SendText { chat: ChatTarget::Chat(id), .. } if id.0 == PHONE)), "{:?}", mock.calls());

    // a tapback on their email message goes to the email conversation
    mock.clear_calls();
    hub.react(PHONE, "M2", "love", false).await.unwrap();
    assert!(mock.calls().iter().any(|c| matches!(c, MockCall::React { chat: ChatTarget::Chat(id), .. } if id.0 == MAIL)), "{:?}", mock.calls());

    // the read receipt goes where the newest message came from
    mock.clear_calls();
    hub.mark_read(PHONE, "").await.unwrap();
    assert_eq!(hub.get_chat(PHONE).await.unwrap().unread_count, 0);
    assert!(mock.calls().iter().any(|c| matches!(c, MockCall::MarkRead { chat: ChatTarget::Chat(id), up_to } if id.0 == PHONE && up_to.0 == "P2")), "{:?}", mock.calls());

    // an edit of my reply to their email goes to the email conversation,
    // although they wrote from the phone since
    let reply = sent.id.0.clone();
    mock.clear_calls();
    hub.edit(PHONE, &reply, "hi!").await.unwrap();
    assert!(mock.calls().iter().any(|c| matches!(c, MockCall::Edit { chat: ChatTarget::Chat(id), .. } if id.0 == MAIL)), "{:?}", mock.calls());

    // a text message only ever goes to a phone number, even right after
    // they wrote from the email address
    mock.inject(incoming("M3", MAIL, MOCK_SENT_AT + 600));
    next_matching(&mut sig, |s| matches!(s, DaemonSignal::MessageReceived { message, .. } if message.id.0 == "M3")).await;
    mock.activate_sms("tel:+15555550100");
    mock.clear_calls();
    hub.send_text_as(ChatTarget::Chat(ChatId::direct(PHONE)), "sms", None, Some(imessage_common::model::Service::Sms)).await.unwrap();
    assert!(mock.calls().iter().any(|c| matches!(c, MockCall::SendSms { chat: ChatTarget::Chat(id), .. } if id.0 == PHONE)), "{:?}", mock.calls());

    // a lone address is untouched
    mock.clear_calls();
    hub.send_text(ChatTarget::Chat(ChatId::direct(BOB)), "yo", None).await.unwrap();
    assert!(mock.calls().iter().any(|c| matches!(c, MockCall::SendText { chat: ChatTarget::Chat(id), .. } if id.0 == BOB)));

    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn switched_off_leaves_chats_apart() {
    let dir = tempfile::tempdir().unwrap();
    let cards = dir.path().join("kpeoplevcard");
    std::fs::create_dir(&cards).unwrap();
    std::fs::write(cards.join("a.vcf"), "BEGIN:VCARD\nEMAIL:pat@example.com\nTEL:+15555550142\nEND:VCARD\n").unwrap();
    let st = store(dir.path());
    st.call(|s| {
        s.apply_event(&incoming("M1", MAIL, 100))?;
        s.apply_event(&incoming("P1", PHONE, 200))?;
        Ok(())
    })
    .await
    .unwrap();
    let mock = Arc::new(MockEngine::new());
    mock.login("me@example.com", "pw").await.unwrap();
    let mut cfg = DaemonConfig::new(dir.path().into());
    cfg.contacts_dir = cards;
    cfg.link_contacts = false;
    let running = Daemon::with_engine(cfg, mock.clone()).with_store(st).start().await.unwrap();
    assert_eq!(running.hub().list_chats(50, 0).await.unwrap().len(), 2);
    running.shutdown().await.unwrap();
}
