//! Shared names and photos and group photos through the daemon with the
//! mock engine: share -> fetch -> photo file -> signals, refresh on GetChat,
//! stop sharing, group photo download.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use imessage_common::model::{Chat, ChatId, DeliveryState, Handle, Message, MessageId, MessagePart, ProfilesState, ProfilesStatus};
use imessaged::config::DaemonConfig;
use imessaged::daemon::Daemon;
use imessaged::engine::mock::{MockCall, MockEngine};
use imessaged::engine::{Engine, EngineEvent, FetchedProfile, IconRef, ShareRef};
use imessaged::hub::DaemonSignal;
use imessaged::keyring::StateKey;
use imessaged::store::{Store, StoreHandle};
use tokio::sync::broadcast;

const BOB: &str = "mailto:bob@example.com";
const CAROL: &str = "tel:+15555550123";
const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];

fn store(dir: &std::path::Path) -> StoreHandle {
    StoreHandle::new(Store::open(dir, StateKey::from_bytes([5; 32])).unwrap())
}

fn chat(id: ChatId, who: &[&str]) -> Chat {
    Chat {
        is_group: id.is_group(),
        id,
        participants: who.iter().map(|h| Handle::new(h)).collect(),
        display_name: None,
        last_message_at: None,
        unread_count: 0,
        background: None,
        icon_path: None,
        pinned: false,
        pin_order: None,
    }
}

fn incoming(id: &str, chat: &Chat, from: &str, at: u64) -> EngineEvent {
    EngineEvent::MessageReceived {
        message: Message {
            id: MessageId::new(id),
            chat_id: chat.id.clone(),
            sender: Handle::new(from),
            sent_at: at,
            text: Some("hi".into()),
            parts: vec![MessagePart::Text { text: "hi".into() }],
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

fn share(key: &str) -> ShareRef {
    ShareRef { record_key: key.into(), decryption_key: vec![1; 16], poster: None }
}

fn shared(who: &str, key: Option<&str>, at: u64) -> EngineEvent {
    EngineEvent::ProfileShared { handle: Handle::new(who), share: key.map(share), at }
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

fn fetches(mock: &MockEngine) -> usize {
    mock.calls().iter().filter(|c| matches!(c, MockCall::FetchProfile { .. })).count()
}

#[tokio::test]
async fn shared_profile_is_fetched_stored_and_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(MockEngine::new());
    let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone()).with_store(store(dir.path())).start().await.unwrap();
    mock.login("me@example.com", "pw").await.unwrap();
    mock.set_profile("R1", FetchedProfile { name: "Bobby".into(), first: "Bob".into(), last: "Smith".into(), image: Some(JPEG.to_vec()) });
    let c = chat(ChatId::direct(BOB), &[BOB]);
    let mut sig = running.signals();
    mock.inject(incoming("P1", &c, BOB, 2_000_000_000_000));
    mock.inject(shared(BOB, Some("R1"), 2_000_000_000_000));

    let s = next_matching(&mut sig, |s| matches!(s, DaemonSignal::HandleProfileUpdated { .. })).await;
    let DaemonSignal::HandleProfileUpdated { handle, profile: Some(p) } = s else { panic!("{s:?}") };
    assert_eq!(handle, BOB);
    assert_eq!((p.name.as_deref(), p.first.as_deref(), p.last.as_deref()), (Some("Bobby"), Some("Bob"), Some("Smith")));
    let photo = p.photo.clone().unwrap();
    assert!(photo.starts_with(&running.hub().profiles_dir().to_string_lossy().into_owned()), "{photo}");
    assert!(photo.ends_with("/R1.jpg"), "{photo}");
    assert_eq!(std::fs::read(&photo).unwrap(), JPEG);
    #[cfg(unix)]
    assert_eq!(std::fs::metadata(&photo).unwrap().permissions().mode() & 0o777, 0o600);
    // the chat is announced again with the profile in its participants
    let s = next_matching(&mut sig, |s| matches!(s, DaemonSignal::ChatUpdated(c) if c.participants[0].profile.is_some())).await;
    let DaemonSignal::ChatUpdated(ch) = s else { unreachable!() };
    assert_eq!(ch.participants[0].profile.as_ref(), Some(&p));
    assert_eq!(running.hub().get_handle_profile("Bob@Example.com").await.unwrap(), Some(p.clone()));
    assert_eq!(fetches(&mock), 1);

    // the same share again (every message carries it): no new fetch, and
    // opening the chat does not refetch a fresh profile
    mock.inject(shared(BOB, Some("R1"), 2_000_000_000_100));
    mock.inject(incoming("P2", &c, BOB, 2_000_000_000_100));
    next_matching(&mut sig, |s| matches!(s, DaemonSignal::MessageReceived { message, .. } if message.id.0 == "P2")).await;
    running.hub().get_chat(BOB).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fetches(&mock), 1);

    // the photo file went missing: opening the chat fetches again
    std::fs::remove_file(&photo).unwrap();
    running.hub().get_chat(BOB).await.unwrap();
    next_matching(&mut sig, |s| matches!(s, DaemonSignal::HandleProfileUpdated { .. })).await;
    assert_eq!(fetches(&mock), 2);
    assert!(std::path::Path::new(&photo).exists());

    // stop sharing: cleared, photo deleted, signal with null
    mock.inject(shared(BOB, None, 2_000_000_000_200));
    let s = next_matching(&mut sig, |s| matches!(s, DaemonSignal::HandleProfileUpdated { .. })).await;
    assert_eq!(s, DaemonSignal::HandleProfileUpdated { handle: BOB.into(), profile: None });
    assert!(!std::path::Path::new(&photo).exists());
    assert_eq!(running.hub().get_handle_profile(BOB).await.unwrap(), None);
    assert!(running.hub().get_chat(BOB).await.unwrap().participants[0].profile.is_none());
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn unavailable_cloudkit_does_not_retry_in_a_loop() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(MockEngine::new());
    let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone()).with_store(store(dir.path())).start().await.unwrap();
    mock.login("me@example.com", "pw").await.unwrap();
    mock.set_profiles_status(ProfilesStatus { state: ProfilesState::NeedsLogin, detail: Some("Apple wants a fresh sign-in".into()) });
    assert_eq!(running.hub().status().await.profiles.state, ProfilesState::NeedsLogin);
    let c = chat(ChatId::direct(CAROL), &[CAROL]);
    let mut sig = running.signals();
    mock.inject(incoming("Q1", &c, CAROL, 2_000_000_000_000));
    mock.inject(shared(CAROL, Some("RC"), 2_000_000_000_000));
    mock.inject(incoming("Q2", &c, CAROL, 2_000_000_000_001));
    next_matching(&mut sig, |s| matches!(s, DaemonSignal::MessageReceived { message, .. } if message.id.0 == "Q2")).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fetches(&mock), 1);
    // more messages with the share and opening the chat: no new attempt for a while
    mock.inject(shared(CAROL, Some("RC"), 2_000_000_000_002));
    running.hub().get_chat(CAROL).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fetches(&mock), 1);
    // a different record is worth a try at once
    mock.set_profiles_status(ProfilesStatus { state: ProfilesState::Ready, detail: None });
    mock.set_profile("RC2", FetchedProfile { name: "Carol".into(), ..Default::default() });
    mock.inject(shared(CAROL, Some("RC2"), 2_000_000_000_003));
    let s = next_matching(&mut sig, |s| matches!(s, DaemonSignal::HandleProfileUpdated { .. })).await;
    let DaemonSignal::HandleProfileUpdated { profile: Some(p), .. } = s else { panic!("{s:?}") };
    assert_eq!((p.name.as_deref(), p.photo.as_deref()), (Some("Carol"), None));
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn group_photo_is_downloaded_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(MockEngine::new());
    let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone()).with_store(store(dir.path())).start().await.unwrap();
    mock.login("me@example.com", "pw").await.unwrap();
    let g = chat(ChatId::group("G-PHOTO"), &[BOB, CAROL]);
    let mut sig = running.signals();
    mock.inject(incoming("G1", &g, BOB, 2_000_000_000_000));
    let icon = IconRef { icon_id: "ICON-1".into(), size: 23, locator: "{\"v\":1,\"plist\":\"K\"}".into() };
    mock.inject(EngineEvent::ChatIcon { chat_id: g.id.clone(), icon: Some(icon), by: Handle::new(BOB), at: 2_000_000_000_001 });
    let s = next_matching(&mut sig, |s| matches!(s, DaemonSignal::ChatUpdated(c) if c.icon_path.is_some())).await;
    let DaemonSignal::ChatUpdated(c) = s else { unreachable!() };
    let path = c.icon_path.unwrap();
    assert!(path.starts_with(&running.hub().chat_icons_dir().to_string_lossy().into_owned()), "{path}");
    assert_eq!(std::fs::read(&path).unwrap(), MockEngine::ATTACHMENT_PAYLOAD);
    assert_eq!(running.hub().get_chat(&g.id.0).await.unwrap().icon_path.as_deref(), Some(path.as_str()));

    mock.inject(EngineEvent::ChatIcon { chat_id: g.id.clone(), icon: None, by: Handle::new(BOB), at: 2_000_000_000_002 });
    next_matching(&mut sig, |s| matches!(s, DaemonSignal::ChatUpdated(c) if c.icon_path.is_none())).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!std::path::Path::new(&path).exists());
    running.shutdown().await.unwrap();
}
