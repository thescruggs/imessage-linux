//! `ImportMessages` (one-time history import) through the hub, with the
//! mock engine: idempotency, read state, signals, attachments, reactions.

use std::sync::Arc;
use std::time::Duration;

use imessage_common::import::{ImportAttachment, ImportBatch, ImportMessage, ImportReaction};
use imessage_common::model::{AttachmentId, Chat, ChatId, DeliveryState, Handle, Message, MessageId, MessagePart, Reaction, ReactionKind};
use imessaged::config::DaemonConfig;
use imessaged::daemon::Daemon;
use imessaged::engine::mock::{MockCall, MockEngine};
use imessaged::engine::{ChatTarget, Engine, EngineEvent};
use imessaged::hub::{DaemonSignal, OpError};
use imessaged::keyring::StateKey;
use imessaged::store::{Store, StoreHandle};

const BOB: &str = "mailto:bob@example.com";
const ME: &str = "mailto:me@example.com";

fn store(dir: &std::path::Path) -> StoreHandle {
    StoreHandle::new(Store::open(dir, StateKey::from_bytes([5; 32])).unwrap())
}

fn msg(id: &str, chat: &ChatId, from: &str, at: u64, text: &str, from_me: bool) -> Message {
    Message {
        id: MessageId::new(id),
        chat_id: chat.clone(),
        sender: Handle::new(from),
        sent_at: at,
        text: Some(text.into()),
        parts: vec![MessagePart::Text { text: text.into() }],
        subject: None,
        reply_to: None,
        is_from_me: from_me,
        delivery: if from_me { DeliveryState::Read } else { DeliveryState::Delivered },
        edited: false,
        unsent: false,
        reactions: vec![],
        effect: None,
        verified: true,
    }
}

fn chat(id: &ChatId, participants: &[&str], name: Option<&str>) -> Chat {
    Chat {
        id: id.clone(),
        participants: participants.iter().map(|p| Handle::new(p)).collect(),
        display_name: name.map(String::from),
        is_group: id.is_group(),
        last_message_at: None,
        unread_count: 0,
        background: None,
        icon_path: None,
    }
}

fn batch(photo: &std::path::Path) -> ImportBatch {
    let bob = ChatId::direct(BOB);
    let team = ChatId::group("6A1B2C3D-0000-4000-8000-000000000001");
    let mut with_photo = msg("IMP-2", &bob, ME, 2_000, "look", true);
    let att = AttachmentId::for_part(&with_photo.id, 0);
    with_photo.parts.insert(0, MessagePart::Attachment { attachment_id: att.clone(), mime: "image/png".into(), filename: "IMG_1.png".into(), size: 4 });
    ImportBatch {
        chats: vec![chat(&bob, &[BOB], None), chat(&team, &[BOB, "tel:+15550001111"], Some("Team"))],
        messages: vec![
            ImportMessage { message: msg("IMP-1", &bob, BOB, 1_000, "hello from the mac", false), attachments: vec![] },
            ImportMessage {
                message: with_photo,
                attachments: vec![ImportAttachment {
                    id: att,
                    mime: "image/png".into(),
                    filename: "IMG_1.png".into(),
                    size: 4,
                    source_path: Some(photo.to_string_lossy().into_owned()),
                }],
            },
            ImportMessage { message: msg("IMP-3", &team, "tel:+15550001111", 3_000, "team news", false), attachments: vec![] },
        ],
        reactions: vec![
            ImportReaction { message_id: MessageId::new("IMP-3"), reaction: Reaction { kind: ReactionKind::Love, sender: Handle::new(ME), added: true }, at: 3_500 },
            ImportReaction { message_id: MessageId::new("OLDER-THAN-RANGE"), reaction: Reaction { kind: ReactionKind::Like, sender: Handle::new(BOB), added: true }, at: 3_600 },
        ],
    }
}

#[tokio::test]
async fn import_twice_inserts_once_without_message_signals() {
    let dir = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let photo = staging.path().join("IMG_1.png");
    std::fs::write(&photo, b"\x89PNG").unwrap();
    let mock = Arc::new(MockEngine::new());
    let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone()).with_store(store(dir.path())).start().await.unwrap();
    let hub = running.hub();
    let mut sig = running.signals();

    let r = hub.import_messages(batch(&photo)).await.unwrap();
    assert_eq!((r.inserted, r.skipped_existing, r.chats_created, r.attachments, r.attachments_without_file), (3, 0, 2, 1, 0));
    assert_eq!((r.reactions, r.reactions_orphaned), (1, 1));

    // one ChatUpdated per chat, never MessageReceived
    let mut updated = vec![];
    while let Ok(Ok(s)) = tokio::time::timeout(Duration::from_millis(200), sig.recv()).await {
        match s {
            DaemonSignal::ChatUpdated(c) => updated.push(c),
            DaemonSignal::MessageReceived { .. } | DaemonSignal::MessageUpdated { .. } => panic!("import must not announce messages: {s:?}"),
            _ => {}
        }
    }
    updated.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(updated.iter().map(|c| c.id.0.as_str()).collect::<Vec<_>>(), vec!["group:6A1B2C3D-0000-4000-8000-000000000001", BOB]);
    assert!(updated.iter().all(|c| c.unread_count == 0), "imported rows are read");
    assert_eq!(updated[0].display_name.as_deref(), Some("Team"));
    assert_eq!(updated[1].last_message_at, Some(2_000));

    // rows look like native history
    let st = running.store().unwrap();
    let page = st.call(|s| s.list_messages(&ChatId::direct(BOB), None, 10)).await.unwrap();
    assert_eq!(page.messages.iter().map(|m| m.id.0.as_str()).collect::<Vec<_>>(), vec!["IMP-1", "IMP-2"]);
    assert!(page.messages[1].is_from_me);
    let team = st.call(|s| s.get_message(&MessageId::new("IMP-3"))).await.unwrap().unwrap();
    assert_eq!(team.reactions, vec![Reaction { kind: ReactionKind::Love, sender: Handle::new(ME), added: true }]);
    assert_eq!(st.call(|s| s.total_unread()).await.unwrap(), 0);

    // the attachment is a private copy, independent of the staging file
    std::fs::remove_file(&photo).unwrap();
    let path = hub.download_attachment("IMP-2:0").await.unwrap();
    assert!(std::path::Path::new(&path).starts_with(hub.attachments_dir()), "{path}");
    assert_eq!(std::fs::read(&path).unwrap(), b"\x89PNG");

    // the engine learned the group: sending to it works
    assert!(mock.calls().contains(&MockCall::UpsertChat(ChatId::group("6A1B2C3D-0000-4000-8000-000000000001"))));
    mock.login("me@example.com", "pw").await.unwrap();
    hub.send_text(ChatTarget::Chat(ChatId::group("6A1B2C3D-0000-4000-8000-000000000001")), "hi team", None).await.unwrap();

    // second run: everything is skipped (the staging file is gone, too)
    let r = hub.import_messages(batch(&photo)).await.unwrap();
    assert_eq!((r.inserted, r.skipped_existing, r.chats_created, r.attachments), (0, 3, 0, 0));
    assert_eq!((r.reactions, r.reactions_orphaned), (1, 1));
    let page = st.call(|s| s.list_messages(&ChatId::direct(BOB), None, 10)).await.unwrap();
    assert_eq!(page.messages.len(), 2);
    assert_eq!(std::fs::read(&path).unwrap(), b"\x89PNG");
    running.shutdown().await.unwrap();
}

/// A message the daemon already has live is left exactly as it is.
#[tokio::test]
async fn existing_live_message_is_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(MockEngine::new());
    let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone()).with_store(store(dir.path())).start().await.unwrap();
    let mut sig = running.signals();
    let bob = ChatId::direct(BOB);
    mock.inject(EngineEvent::MessageReceived {
        message: msg("LIVE-1", &bob, BOB, 5_000, "live text", false),
        chat: chat(&bob, &[BOB], None),
        attachments: vec![],
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(sig.recv().await.unwrap(), DaemonSignal::MessageReceived { .. }) {}
    })
    .await
    .unwrap();

    let b = ImportBatch {
        chats: vec![chat(&bob, &[BOB], None)],
        messages: vec![
            ImportMessage { message: msg("LIVE-1", &bob, BOB, 5_000, "mac text", false), attachments: vec![] },
            ImportMessage { message: msg("OLD-1", &bob, BOB, 4_000, "older", false), attachments: vec![] },
        ],
        reactions: vec![
            // on the live message: announced
            ImportReaction { message_id: MessageId::new("LIVE-1"), reaction: Reaction { kind: ReactionKind::Like, sender: Handle::new(ME), added: true }, at: 5_100 },
            // on a message this call inserts: not (it was never announced)
            ImportReaction { message_id: MessageId::new("OLD-1"), reaction: Reaction { kind: ReactionKind::Love, sender: Handle::new(ME), added: true }, at: 4_100 },
        ],
    };
    let r = running.hub().import_messages(b.clone()).await.unwrap();
    assert_eq!((r.inserted, r.skipped_existing, r.chats_created, r.reactions), (1, 1, 0, 2));
    let mut updated = vec![];
    while let Ok(Ok(s)) = tokio::time::timeout(Duration::from_millis(200), sig.recv()).await {
        if let DaemonSignal::MessageUpdated { message, .. } = s {
            updated.push(message);
        }
    }
    assert_eq!(updated.len(), 1, "{updated:?}");
    assert_eq!((updated[0].id.0.as_str(), updated[0].reactions.len()), ("LIVE-1", 1));
    // re-running changes nothing and announces nothing
    running.hub().import_messages(b).await.unwrap();
    while let Ok(Ok(s)) = tokio::time::timeout(Duration::from_millis(200), sig.recv()).await {
        assert!(!matches!(s, DaemonSignal::MessageUpdated { .. }), "{s:?}");
    }
    let st = running.store().unwrap();
    let live = st.call(|s| s.get_message(&MessageId::new("LIVE-1"))).await.unwrap().unwrap();
    assert_eq!(live.text.as_deref(), Some("live text"));
    // the live message stays unread; the imported one is read
    assert_eq!(st.call(move |s| s.get_chat(&bob)).await.unwrap().unwrap().unread_count, 1);
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_batches_store_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(MockEngine::new());
    let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone()).with_store(store(dir.path())).start().await.unwrap();
    let hub = running.hub();
    let bob = ChatId::direct(BOB);
    let stray = ImportAttachment { id: AttachmentId("IMP-9:0".into()), mime: "image/png".into(), filename: "x.png".into(), size: 1, source_path: None };
    let b = ImportBatch {
        chats: vec![],
        messages: vec![ImportMessage { message: msg("IMP-9", &bob, BOB, 1, "x", false), attachments: vec![stray] }],
        reactions: vec![],
    };
    assert!(matches!(hub.import_messages(b).await, Err(OpError::InvalidInput(_))));
    let mut m = msg("IMP-10", &bob, BOB, 1, "x", false);
    let att = AttachmentId::for_part(&m.id, 0);
    m.parts.push(MessagePart::Attachment { attachment_id: att.clone(), mime: "a/b".into(), filename: "f".into(), size: 1 });
    let rel = ImportAttachment { id: att, mime: "a/b".into(), filename: "f".into(), size: 1, source_path: Some("relative/f".into()) };
    let b = ImportBatch { chats: vec![], messages: vec![ImportMessage { message: m, attachments: vec![rel] }], reactions: vec![] };
    assert!(matches!(hub.import_messages(b).await, Err(OpError::InvalidInput(_))));
    let st = running.store().unwrap();
    assert!(st.call(|s| s.list_chats(10, 0)).await.unwrap().is_empty());

    // a missing source file is not fatal: stored without a file
    let mut m = msg("IMP-11", &bob, BOB, 1, "x", false);
    let att = AttachmentId::for_part(&m.id, 1);
    m.parts.push(MessagePart::Attachment { attachment_id: att.clone(), mime: "a/b".into(), filename: "f".into(), size: 1 });
    let gone = ImportAttachment { id: att, mime: "a/b".into(), filename: "f".into(), size: 1, source_path: Some("/nonexistent/imsg-test/f".into()) };
    let b = ImportBatch { chats: vec![], messages: vec![ImportMessage { message: m, attachments: vec![gone] }], reactions: vec![] };
    let r = hub.import_messages(b).await.unwrap();
    assert_eq!((r.inserted, r.chats_created, r.attachments, r.attachments_without_file), (1, 1, 0, 1));
    running.shutdown().await.unwrap();
}

/// S4: a source that is a symlink (or FIFO) is not followed / opened; the
/// attachment is stored without a file.
#[tokio::test]
async fn import_skips_symlinked_and_fifo_sources() {
    let dir = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let secret = staging.path().join("secret");
    std::fs::write(&secret, b"top secret").unwrap();
    let link = staging.path().join("IMG_1.png");
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    let mock = Arc::new(MockEngine::new());
    let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone()).with_store(store(dir.path())).start().await.unwrap();
    let r = running.hub().import_messages(batch(&link)).await.unwrap();
    assert_eq!((r.inserted, r.attachments, r.attachments_without_file), (3, 0, 1));

    let dir2 = tempfile::tempdir().unwrap();
    let fifo = staging.path().join("fifo.png");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let running2 = Daemon::with_engine(DaemonConfig::new(dir2.path().into()), mock.clone()).with_store(store(dir2.path())).start().await.unwrap();
    let r = tokio::time::timeout(Duration::from_secs(5), running2.hub().import_messages(batch(&fifo))).await.expect("a FIFO must not block").unwrap();
    assert_eq!((r.attachments, r.attachments_without_file), (0, 1));
    running.shutdown().await.unwrap();
    running2.shutdown().await.unwrap();
}

/// Every chat id and handle must be a valid IDS URI / group id; the first
/// bad one is named and nothing is stored.
#[tokio::test]
async fn invalid_ids_and_handles_reject_the_batch() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(MockEngine::new());
    let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone()).with_store(store(dir.path())).start().await.unwrap();
    let hub = running.hub();
    let raw = ChatId("46339(smsft)".into());
    let short = ChatId::direct("tel:46339");
    let reject = |b: ImportBatch, want: &'static str| {
        let hub = hub.clone();
        async move {
            match hub.import_messages(b).await {
                Err(OpError::InvalidInput(e)) => assert!(e.contains(want), "{e} should name {want}"),
                other => panic!("{want}: {other:?}"),
            }
        }
    };
    let one = |chats: Vec<Chat>, m: Message| ImportBatch { chats, messages: vec![ImportMessage { message: m, attachments: vec![] }], reactions: vec![] };
    reject(one(vec![chat(&raw, &[], None)], msg("V-1", &short, "tel:46339", 1, "x", false)), "46339(smsft)").await;
    reject(one(vec![], msg("V-2", &raw, "tel:46339", 1, "x", false)), "46339(smsft)").await;
    let mut tagged = chat(&short, &[], None);
    tagged.participants = vec![Handle { uri: "+12125550199(smsfp)".into(), display: None, profile: None }];
    reject(one(vec![tagged], msg("V-3", &short, "tel:46339", 1, "x", false)), "+12125550199(smsfp)").await;
    let mut m = msg("V-4", &short, "tel:46339", 1, "x", false);
    m.sender = Handle { uri: "urn:biz:1234".into(), display: None, profile: None };
    reject(one(vec![], m), "urn:biz:1234").await;
    let mut b = one(vec![], msg("V-5", &short, "tel:46339", 1, "x", false));
    b.reactions.push(ImportReaction { message_id: MessageId::new("V-5"), reaction: Reaction { kind: ReactionKind::Like, sender: Handle::new("mom"), added: true }, at: 2 });
    reject(b, "mom").await;
    let st = running.store().unwrap();
    assert!(st.call(|s| s.list_chats(10, 0)).await.unwrap().is_empty());
    // the normalised short code is fine
    let r = hub.import_messages(one(vec![chat(&short, &["tel:46339"], None)], msg("V-6", &short, "tel:46339", 1, "x", false))).await.unwrap();
    assert_eq!(r.inserted, 1);
    running.shutdown().await.unwrap();
}

/// Empty group rows go at startup and after an import; PurgeInvalidChats
/// removes bad chats with their files (dry run first).
#[tokio::test]
async fn housekeeping_and_purge_invalid_chats() {
    let dir = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let photo = staging.path().join("IMG_1.png");
    std::fs::write(&photo, b"\x89PNG").unwrap();
    let s = store(dir.path());
    let ghost = ChatId::group("B5E489CA-0000-4000-8000-000000000001");
    s.store().upsert_chat(&chat(&ghost, &[], None)).unwrap();
    let mock = Arc::new(MockEngine::new());
    let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone()).with_store(s.clone()).start().await.unwrap();
    assert!(s.store().get_chat(&ghost).unwrap().is_none(), "removed at startup");
    assert!(!mock.calls().contains(&MockCall::UpsertChat(ghost.clone())), "not taught to the engine");
    let hub = running.hub();
    hub.import_messages(batch(&photo)).await.unwrap();

    // a bad chat as an older import left it (written directly to the store)
    let raw = ChatId("46339(smsft)".into());
    let ghost2 = ChatId::group("8DE60804-0000-4000-8000-000000000001");
    s.store().upsert_chat(&chat(&ghost2, &[], None)).unwrap();
    let mut m = msg("RAW-1", &raw, "46339(smsft)", 5_000, "your code", false);
    let att = AttachmentId::for_part(&m.id, 1);
    m.parts.push(MessagePart::Attachment { attachment_id: att.clone(), mime: "image/png".into(), filename: "c.png".into(), size: 4 });
    let kept = hub.attachments_dir().join("RAW-1/c.png");
    std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
    std::fs::write(&kept, b"x").unwrap();
    let a = imessage_common::model::Attachment {
        id: att,
        message_id: m.id.clone(),
        mime: "image/png".into(),
        filename: "c.png".into(),
        size: 4,
        local_path: Some(kept.to_string_lossy().into_owned()),
        mmcs: String::new(),
    };
    s.store().insert_message(&m, &[a]).unwrap();

    // an import (even an empty one) also sweeps empty groups
    hub.import_messages(ImportBatch::default()).await.unwrap();
    assert!(s.store().get_chat(&ghost2).unwrap().is_none(), "removed after an import");

    let dry = hub.purge_invalid_chats(true).await.unwrap();
    assert_eq!((dry.chats.len(), dry.messages, dry.attachments, dry.files), (1, 1, 1, 1));
    assert_eq!(dry.chats[0].id, raw);
    assert!(kept.exists() && s.store().get_chat(&raw).unwrap().is_some(), "dry run deletes nothing");
    let done = hub.purge_invalid_chats(false).await.unwrap();
    assert!(!done.dry_run && done.chats.len() == 1);
    assert!(!kept.exists(), "the copied file is removed");
    assert!(s.store().get_chat(&raw).unwrap().is_none());
    assert!(s.store().get_message(&MessageId::new("RAW-1")).unwrap().is_none());
    // the imported, valid history is untouched
    assert_eq!(s.store().list_chats(10, 0).unwrap().len(), 2);
    running.shutdown().await.unwrap();
}

/// An empty group snapshot (no members, messages or name) is dropped in
/// the import's transaction: no ChatUpdated for a row that is gone.
#[tokio::test]
async fn empty_group_snapshot_is_not_announced() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(MockEngine::new());
    let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone()).with_store(store(dir.path())).start().await.unwrap();
    let mut sig = running.signals();
    let bob = ChatId::direct(BOB);
    let ghost = ChatId::group("7A1B2C3D-0000-4000-8000-000000000002");
    let b = ImportBatch {
        chats: vec![chat(&ghost, &[], None), chat(&bob, &[BOB], None)],
        messages: vec![ImportMessage { message: msg("IMP-20", &bob, BOB, 1_000, "hi", false), attachments: vec![] }],
        reactions: vec![],
    };
    let r = running.hub().import_messages(b).await.unwrap();
    assert_eq!((r.inserted, r.chats_created), (1, 1));
    let mut updated = vec![];
    while let Ok(Ok(s)) = tokio::time::timeout(Duration::from_millis(200), sig.recv()).await {
        if let DaemonSignal::ChatUpdated(c) = s {
            updated.push(c.id);
        }
    }
    assert_eq!(updated, vec![bob]);
    let st = running.store().unwrap();
    assert!(st.call(move |s| s.get_chat(&ghost)).await.unwrap().is_none());
    running.shutdown().await.unwrap();
}
