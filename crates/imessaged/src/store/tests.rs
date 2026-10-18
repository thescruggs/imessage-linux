use std::os::unix::fs::PermissionsExt;

use imessage_common::model::{DeliveryReceipt, ReadReceipt, TypingEvent};

use super::*;

const BOB: &str = "mailto:bob@example.com";
const CAROL: &str = "mailto:carol@example.com";
const ME: &str = "mailto:me@example.com";

fn key() -> StateKey {
    StateKey::from_bytes([9; 32])
}

fn open(dir: &Path) -> Store {
    Store::open(dir, key()).unwrap()
}

fn direct_chat(other: &str) -> Chat {
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

fn msg(id: &str, chat: &ChatId, sender: &str, at: u64, text: &str) -> Message {
    Message {
        id: MessageId::new(id),
        chat_id: chat.clone(),
        sender: Handle::new(sender),
        sent_at: at,
        text: Some(text.into()),
        parts: vec![MessagePart::Text { text: text.into() }],
        subject: None,
        reply_to: None,
        is_from_me: sender == ME,
        delivery: if sender == ME { DeliveryState::Sent } else { DeliveryState::Delivered },
        edited: false,
        unsent: false,
        reactions: vec![],
        effect: None,
        verified: true,
        service: Default::default(),
    }
}

fn received(m: Message, chat: Chat) -> EngineEvent {
    EngineEvent::MessageReceived { message: m, chat, attachments: vec![] }
}

#[test]
fn migrates_from_empty_and_reopens_idempotently() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    assert_eq!(s.schema_version().unwrap(), latest_schema_version());
    s.upsert_chat(&direct_chat(BOB)).unwrap();
    s.close().unwrap();
    assert!(matches!(s.list_chats(10, 0), Err(StoreError::Closed)));

    let s = open(dir.path());
    assert_eq!(s.schema_version().unwrap(), latest_schema_version());
    assert_eq!(s.list_chats(10, 0).unwrap().len(), 1, "data survives reopen");
    let n: i64 = s.lock().conn.as_ref().unwrap().query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0)).unwrap();
    assert_eq!(n, MIGRATIONS.len() as i64, "migrations applied exactly once");
    let fk: i64 = s.lock().conn.as_ref().unwrap().query_row("PRAGMA foreign_keys", [], |r| r.get(0)).unwrap();
    assert_eq!(fk, 1);
    let jm: String = s.lock().conn.as_ref().unwrap().query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
    assert_eq!(jm, "wal");
}

#[test]
fn refuses_newer_schema() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    s.lock().conn.as_ref().unwrap().execute("INSERT INTO schema_migrations VALUES (999, 0)", []).unwrap();
    s.close().unwrap();
    assert!(matches!(Store::open(dir.path(), key()), Err(StoreError::TooNew { found: 999, .. })));
}

#[test]
fn files_are_private() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    s.upsert_chat(&direct_chat(BOB)).unwrap();
    for name in ["messages.db", "messages.db-wal", "messages.db-shm"] {
        let p = dir.path().join(name);
        assert!(p.exists(), "{name} missing");
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600, "{name}");
    }
    // a loosened db file is tightened on open
    s.close().unwrap();
    fs::set_permissions(dir.path().join(DB_FILE), fs::Permissions::from_mode(0o644)).unwrap();
    let _s = open(dir.path());
    assert_eq!(fs::metadata(dir.path().join(DB_FILE)).unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn every_event_type_is_persisted() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let bob = ChatId::direct(BOB);

    // MessageReceived (incoming, with an attachment)
    let mut m1 = msg("A1", &bob, BOB, 1000, "hello");
    let att = Attachment {
        id: AttachmentId::for_part(&m1.id, 1),
        message_id: m1.id.clone(),
        mime: "image/png".into(),
        filename: "cat.png".into(),
        size: 42,
        local_path: None,
        mmcs: "{\"v\":1,\"plist\":\"SECRET-KEY\"}".into(),
    };
    m1.parts.push(MessagePart::Attachment { attachment_id: att.id.clone(), mime: att.mime.clone(), filename: att.filename.clone(), size: 42 });
    let mut c = direct_chat(BOB);
    c.last_message_at = Some(1000);
    let up = s.apply_event(&EngineEvent::MessageReceived { message: m1.clone(), chat: c, attachments: vec![att.clone()] }).unwrap();
    assert!(matches!(&up[0], StoreUpdate::NewMessage(m) if m.id == m1.id && m.parts.len() == 2));
    assert!(matches!(&up[1], StoreUpdate::ChatUpdated(c) if c.unread_count == 1 && c.last_message_at == Some(1000)));
    // redelivery is not new
    let up = s.apply_event(&received(m1.clone(), direct_chat(BOB))).unwrap();
    assert!(matches!(&up[0], StoreUpdate::MessageUpdated(_)));
    assert_eq!(s.get_chat(&bob).unwrap().unwrap().unread_count, 1);

    // our own message + MessageSent / Delivered / Read
    let mine = msg("B1", &bob, ME, 2000, "hi bob");
    s.insert_message(&mine, &[]).unwrap();
    assert!(s.apply_event(&EngineEvent::MessageSent { id: mine.id.clone(), chat_id: Some(bob.clone()), delivery: DeliveryState::Sent, error: None, at: 2000 }).unwrap().is_empty(), "no change");
    let up = s.apply_event(&EngineEvent::Delivered(DeliveryReceipt { chat_id: None, message_id: mine.id.clone(), recipient: Handle::new(BOB), delivered_at: 2100 })).unwrap();
    assert!(matches!(&up[0], StoreUpdate::MessageUpdated(m) if m.delivery == DeliveryState::Delivered));
    let up = s.apply_event(&EngineEvent::Read(ReadReceipt { chat_id: None, message_id: mine.id.clone(), reader: Handle::new(BOB), read_at: 2200, by_me: false })).unwrap();
    assert!(matches!(&up[0], StoreUpdate::MessageUpdated(m) if m.delivery == DeliveryState::Read));
    // never backwards
    s.apply_event(&EngineEvent::Delivered(DeliveryReceipt { chat_id: None, message_id: mine.id.clone(), recipient: Handle::new(BOB), delivered_at: 2300 })).unwrap();
    assert_eq!(s.get_message(&mine.id).unwrap().unwrap().delivery, DeliveryState::Read);
    // failure report
    let failing = msg("B2", &bob, ME, 2500, "x");
    s.insert_message(&failing, &[]).unwrap();
    s.apply_event(&EngineEvent::MessageSent { id: failing.id.clone(), chat_id: Some(bob.clone()), delivery: DeliveryState::Failed, error: Some("nope".into()), at: 2501 }).unwrap();
    assert_eq!(s.get_message(&failing.id).unwrap().unwrap().delivery, DeliveryState::Failed);

    // Read receipt for an incoming message = read on another of our devices
    let up = s.apply_event(&EngineEvent::Read(ReadReceipt { chat_id: Some(bob.clone()), message_id: m1.id.clone(), reader: Handle::new(ME), read_at: 3000, by_me: true })).unwrap();
    assert!(matches!(&up[0], StoreUpdate::ChatUpdated(c) if c.unread_count == 0));

    // Reaction add, replace, stale, remove
    let r = |kind: ReactionKind, added: bool| Reaction { kind, sender: Handle::new(BOB), added };
    let react = |kind, added, at| EngineEvent::Reaction { chat_id: bob.clone(), message_id: mine.id.clone(), reaction: r(kind, added), at };
    let up = s.apply_event(&react(ReactionKind::Love, true, 4000)).unwrap();
    assert!(matches!(&up[0], StoreUpdate::MessageUpdated(m) if m.reactions == vec![r(ReactionKind::Love, true)]));
    s.apply_event(&react(ReactionKind::Emoji("🔥".into()), true, 4100)).unwrap();
    assert_eq!(s.get_message(&mine.id).unwrap().unwrap().reactions, vec![r(ReactionKind::Emoji("🔥".into()), true)]);
    assert!(s.apply_event(&react(ReactionKind::Like, true, 3900)).unwrap().is_empty(), "older add ignored");
    assert!(s.apply_event(&react(ReactionKind::Love, false, 4200)).unwrap().is_empty(), "removing a reaction they don't have");
    s.apply_event(&react(ReactionKind::Emoji("🔥".into()), false, 4300)).unwrap();
    assert!(s.get_message(&mine.id).unwrap().unwrap().reactions.is_empty());
    assert!(s.apply_event(&react(ReactionKind::Emoji("🔥".into()), true, 4250)).unwrap().is_empty(), "tombstone wins over older add");

    // Edited (only by the sender; older edit ignored)
    let edit = |by: &str, text: &str, at| EngineEvent::Edited {
        chat_id: bob.clone(),
        message_id: m1.id.clone(),
        part: 0,
        new_parts: vec![MessagePart::Text { text: text.into() }],
        new_text: Some(text.into()),
        sender: Handle::new(by),
        from_me: false,
        at,
    };
    assert!(s.apply_event(&edit(CAROL, "evil", 5000)).unwrap().is_empty());
    let up = s.apply_event(&edit(BOB, "hello there", 5000)).unwrap();
    assert!(matches!(&up[0], StoreUpdate::MessageUpdated(m) if m.edited && m.text.as_deref() == Some("hello there") && m.parts.len() == 2));
    assert!(s.apply_event(&edit(BOB, "older", 4999)).unwrap().is_empty());

    // Unsent: content and attachments gone, file returned for deletion
    s.set_attachment_path(&att.id, Some("/tmp/x/cat.png")).unwrap();
    let up = s.apply_event(&EngineEvent::Unsent { chat_id: bob.clone(), message_id: m1.id.clone(), part: 0, sender: Handle::new(BOB), from_me: false, at: 6000 }).unwrap();
    assert!(matches!(&up[0], StoreUpdate::MessageUpdated(m) if m.unsent && m.text.is_none() && m.parts.is_empty()));
    assert!(up.contains(&StoreUpdate::FilesRemoved(vec!["/tmp/x/cat.png".into()])));
    assert!(s.get_attachment(&att.id).unwrap().is_none());
    // a redelivered original does not resurrect it
    s.apply_event(&received(m1.clone(), direct_chat(BOB))).unwrap();
    assert!(s.get_message(&m1.id).unwrap().unwrap().unsent);
    assert_eq!(s.get_message(&m1.id).unwrap().unwrap().text, None);

    // ChatRenamed / ParticipantsChanged with ordering
    let g = ChatId::group("g1");
    let up = s.apply_event(&EngineEvent::ChatRenamed { chat_id: g.clone(), name: Some("Team".into()), by: Handle::new(BOB), at: 7000 }).unwrap();
    assert!(matches!(&up[0], StoreUpdate::ChatUpdated(c) if c.display_name.as_deref() == Some("Team") && c.is_group));
    assert!(s.apply_event(&EngineEvent::ChatRenamed { chat_id: g.clone(), name: Some("Old".into()), by: Handle::new(BOB), at: 6999 }).unwrap().is_empty());
    let up = s
        .apply_event(&EngineEvent::ParticipantsChanged { chat_id: g.clone(), participants: vec![Handle::new(BOB), Handle::new(CAROL)], by: Handle::new(BOB), at: 7000 })
        .unwrap();
    assert!(matches!(&up[0], StoreUpdate::ChatUpdated(c) if c.participants == vec![Handle::new(BOB), Handle::new(CAROL)]));
    // an old message carrying an old snapshot does not undo the rename / membership
    let old_snapshot = Chat { id: g.clone(), participants: vec![Handle::new(BOB)], display_name: Some("Ancient".into()), is_group: true, last_message_at: Some(10), unread_count: 0, background: None, icon_path: None, pinned: false, pin_order: None };
    s.apply_event(&received(msg("G0", &g, BOB, 10, "old"), old_snapshot)).unwrap();
    let gc = s.get_chat(&g).unwrap().unwrap();
    assert_eq!(gc.display_name.as_deref(), Some("Team"));
    assert_eq!(gc.participants.len(), 2);
    let up = s.apply_event(&EngineEvent::ChatRenamed { chat_id: g.clone(), name: None, by: Handle::new(BOB), at: 8000 }).unwrap();
    assert!(matches!(&up[0], StoreUpdate::ChatUpdated(c) if c.display_name.is_none()));
    assert_eq!(s.group_chats().unwrap().len(), 1);

    // in-memory only
    for ev in [
        EngineEvent::Typing(TypingEvent { chat_id: bob.clone(), sender: Handle::new(BOB), typing: true }),
        EngineEvent::ConnectionChanged(imessage_common::model::ConnectionState::Connected),
        EngineEvent::TwoFactorRequired,
        EngineEvent::Error("x".into()),
    ] {
        assert!(s.apply_event(&ev).unwrap().is_empty());
    }

    // survives a reopen
    s.close().unwrap();
    let s = open(dir.path());
    assert_eq!(s.get_message(&mine.id).unwrap().unwrap().delivery, DeliveryState::Read);
    assert_eq!(s.get_chat(&g).unwrap().unwrap().participants.len(), 2);
}

fn attachment_for(m: &Message, local_path: Option<&str>) -> Attachment {
    Attachment {
        id: AttachmentId::for_part(&m.id, 1),
        message_id: m.id.clone(),
        mime: "image/png".into(),
        filename: "a.png".into(),
        size: 1,
        local_path: local_path.map(str::to_string),
        mmcs: "{\"v\":1,\"plist\":\"K\"}".into(),
    }
}

fn edit_ev(chat: &ChatId, id: &MessageId, by: &str, from_me: bool, text: &str, at: u64) -> EngineEvent {
    EngineEvent::Edited {
        chat_id: chat.clone(),
        message_id: id.clone(),
        part: 0,
        new_parts: vec![MessagePart::Text { text: text.into() }],
        new_text: Some(text.into()),
        sender: Handle::new(by),
        from_me,
        at,
    }
}

fn unsend_ev(chat: &ChatId, id: &MessageId, by: &str, from_me: bool, at: u64) -> EngineEvent {
    EngineEvent::Unsent { chat_id: chat.clone(), message_id: id.clone(), part: 0, sender: Handle::new(by), from_me, at }
}

/// B1: a stranger (any peer) can neither edit nor unsend one of our own
/// messages; only our own devices can.
#[test]
fn edit_from_a_stranger_on_an_own_message_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let bob = ChatId::direct(BOB);
    let mine = msg("OWN1", &bob, ME, 1000, "original");
    s.insert_message(&mine, &[]).unwrap();
    for by in [BOB, CAROL] {
        assert!(s.apply_event(&edit_ev(&bob, &mine.id, by, false, "hijacked", 2000)).unwrap().is_empty(), "edit by {by}");
    }
    let m = s.get_message(&mine.id).unwrap().unwrap();
    assert!(!m.edited);
    assert_eq!(m.text.as_deref(), Some("original"));
}

#[test]
fn unsend_from_a_stranger_on_an_own_message_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let bob = ChatId::direct(BOB);
    let mine = msg("OWN2", &bob, ME, 1000, "keep me");
    let att = attachment_for(&mine, Some("/home/me/a.png"));
    s.insert_message(&mine, std::slice::from_ref(&att)).unwrap();
    for by in [BOB, CAROL] {
        assert!(s.apply_event(&unsend_ev(&bob, &mine.id, by, false, 2000)).unwrap().is_empty(), "unsend by {by}");
    }
    let m = s.get_message(&mine.id).unwrap().unwrap();
    assert!(!m.unsent);
    assert_eq!(m.text.as_deref(), Some("keep me"));
    // attachment row, its locator and its file reference are untouched
    let back = s.get_attachment(&att.id).unwrap().expect("attachment row kept");
    assert_eq!(back.mmcs, att.mmcs);
    assert_eq!(back.local_path.as_deref(), Some("/home/me/a.png"));
}

#[test]
fn own_device_edit_and_unsend_of_own_message_apply() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let bob = ChatId::direct(BOB);
    let mine = msg("OWN3", &bob, ME, 1000, "typo");
    s.insert_message(&mine, &[]).unwrap();
    // any of our handles may edit our own message
    let up = s.apply_event(&edit_ev(&bob, &mine.id, "mailto:me-alt@example.com", true, "fixed", 2000)).unwrap();
    assert!(matches!(&up[0], StoreUpdate::MessageUpdated(m) if m.edited && m.text.as_deref() == Some("fixed")));
    let up = s.apply_event(&unsend_ev(&bob, &mine.id, ME, true, 3000)).unwrap();
    assert!(matches!(&up[0], StoreUpdate::MessageUpdated(m) if m.unsent && m.text.is_none()));

    // ...but "our device" cannot edit / unsend someone else's message
    let theirs = msg("THEIRS1", &bob, BOB, 1000, "bob's");
    s.insert_message(&theirs, &[]).unwrap();
    assert!(s.apply_event(&edit_ev(&bob, &theirs.id, ME, true, "rewritten", 2000)).unwrap().is_empty());
    assert!(s.apply_event(&unsend_ev(&bob, &theirs.id, ME, true, 2000)).unwrap().is_empty());
    assert_eq!(s.get_message(&theirs.id).unwrap().unwrap().text.as_deref(), Some("bob's"));
}

/// M1: read receipts only do what their origin allows.
#[test]
fn read_receipts_respect_who_read() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let bob = ChatId::direct(BOB);
    let theirs = msg("R1", &bob, BOB, 1000, "unread");
    s.insert_message(&theirs, &[]).unwrap();
    let mine = msg("R2", &bob, ME, 1001, "mine");
    s.insert_message(&mine, &[]).unwrap();
    let read = |id: &MessageId, reader: &str, by_me| EngineEvent::Read(ReadReceipt { chat_id: Some(bob.clone()), message_id: id.clone(), reader: Handle::new(reader), read_at: 2000, by_me });

    // a peer cannot mark (their own) message read on our side
    assert!(s.apply_event(&read(&theirs.id, BOB, false)).unwrap().is_empty());
    assert_eq!(s.get_chat(&bob).unwrap().unwrap().unread_count, 1);
    // our own device reading our own message: nothing to do
    assert!(s.apply_event(&read(&mine.id, ME, true)).unwrap().is_empty());
    assert_eq!(s.get_message(&mine.id).unwrap().unwrap().delivery, DeliveryState::Sent);
    // a peer reading our message -> delivery Read
    let up = s.apply_event(&read(&mine.id, BOB, false)).unwrap();
    assert!(matches!(&up[0], StoreUpdate::MessageUpdated(m) if m.delivery == DeliveryState::Read));
    // we read theirs on another device -> local read marker
    let up = s.apply_event(&read(&theirs.id, ME, true)).unwrap();
    assert!(matches!(&up[0], StoreUpdate::ChatUpdated(c) if c.unread_count == 0));
}

/// m1: a redelivered unsent message does not bring back attachment rows or
/// their sealed locators.
#[test]
fn redelivery_of_unsent_message_does_not_recreate_attachments() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let bob = ChatId::direct(BOB);
    let m = msg("U1", &bob, BOB, 1000, "oops");
    let att = attachment_for(&m, None);
    s.apply_event(&EngineEvent::MessageReceived { message: m.clone(), chat: direct_chat(BOB), attachments: vec![att.clone()] }).unwrap();
    s.apply_event(&unsend_ev(&bob, &m.id, BOB, false, 2000)).unwrap();
    assert!(s.get_attachment(&att.id).unwrap().is_none());
    s.apply_event(&EngineEvent::MessageReceived { message: m.clone(), chat: direct_chat(BOB), attachments: vec![att.clone()] }).unwrap();
    assert!(s.get_attachment(&att.id).unwrap().is_none(), "attachment row resurrected");
    assert!(s.list_attachments(&m.id).unwrap().is_empty());
    let n: i64 = s.lock().conn.as_ref().unwrap().query_row("SELECT COUNT(*) FROM attachments", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "no sealed locator left behind");
}

/// m2: an edit / unsend that beats its original is applied when the
/// original arrives.
#[test]
fn early_edit_and_unsend_apply_when_the_original_arrives() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let bob = ChatId::direct(BOB);

    // unsend first, then the original (with an attachment)
    let m = msg("LATE1", &bob, BOB, 1000, "gone");
    let att = attachment_for(&m, None);
    assert!(s.apply_event(&unsend_ev(&bob, &m.id, BOB, false, 2000)).unwrap().is_empty());
    let up = s.apply_event(&EngineEvent::MessageReceived { message: m.clone(), chat: direct_chat(BOB), attachments: vec![att.clone()] }).unwrap();
    assert!(matches!(&up[0], StoreUpdate::NewMessage(m) if m.unsent && m.text.is_none() && m.parts.is_empty()));
    assert!(s.get_attachment(&att.id).unwrap().is_none());
    assert_eq!(s.get_chat(&bob).unwrap().unwrap().unread_count, 0, "an unsent message is not unread");

    // two edits out of order, then the original: the newest edit wins
    let m2 = msg("LATE2", &bob, BOB, 1000, "first draft");
    s.apply_event(&edit_ev(&bob, &m2.id, BOB, false, "final", 3000)).unwrap();
    s.apply_event(&edit_ev(&bob, &m2.id, BOB, false, "second", 2500)).unwrap();
    let up = s.apply_event(&received(m2.clone(), direct_chat(BOB))).unwrap();
    assert!(matches!(&up[0], StoreUpdate::NewMessage(m) if m.edited && m.text.as_deref() == Some("final")));

    // a held edit from a stranger is still checked against the real sender
    let m3 = msg("LATE3", &bob, BOB, 1000, "authentic");
    s.apply_event(&edit_ev(&bob, &m3.id, CAROL, false, "forged", 2000)).unwrap();
    s.apply_event(&received(m3.clone(), direct_chat(BOB))).unwrap();
    let got = s.get_message(&m3.id).unwrap().unwrap();
    assert!(!got.edited);
    assert_eq!(got.text.as_deref(), Some("authentic"));

    // the hold is bounded
    for i in 0..(EARLY_MOD_CAP + 10) {
        s.apply_event(&unsend_ev(&bob, &MessageId::new(format!("NEVER{i}")), BOB, false, 1)).unwrap();
    }
    assert_eq!(s.lock().early.mods.len(), EARLY_MOD_CAP);
}

#[test]
fn delivery_report_before_row_is_applied_on_insert() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let bob = ChatId::direct(BOB);
    let mut m = msg("E1", &bob, ME, 1, "x");
    m.delivery = DeliveryState::Pending;
    assert!(s.update_delivery(&m.id, DeliveryState::Delivered, None).unwrap().is_none());
    let (stored, new) = s.insert_message(&m, &[]).unwrap();
    assert!(new);
    assert_eq!(stored.delivery, DeliveryState::Delivered);
}

#[test]
fn cursor_pagination_is_stable() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let bob = ChatId::direct(BOB);
    // 25 messages, several sharing a timestamp
    for i in 0..25u64 {
        s.insert_message(&msg(&format!("M{i:02}"), &bob, BOB, 1000 + i / 3, &format!("n{i}")), &[]).unwrap();
    }
    let mut seen = vec![];
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = s.list_messages(&bob, cursor.as_deref(), 10).unwrap();
        pages += 1;
        // a new message arriving mid-pagination must not shift older pages
        if pages == 1 {
            s.insert_message(&msg("NEW", &bob, BOB, 5000, "late"), &[]).unwrap();
        }
        let ids: Vec<_> = page.messages.iter().map(|m| m.id.0.clone()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "page is oldest-first");
        seen.splice(0..0, ids);
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    let want: Vec<_> = (0..25).map(|i| format!("M{i:02}")).collect();
    assert_eq!(seen, want);
    assert!(matches!(s.list_messages(&bob, Some("garbage"), 10), Err(StoreError::InvalidInput(_))));
    assert_eq!(s.list_messages(&bob, None, 0).unwrap().messages.len(), 26, "0 = default page size");
}

#[test]
fn search_and_unread() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let bob = ChatId::direct(BOB);
    let carol = ChatId::direct(CAROL);
    s.insert_message(&msg("S1", &bob, BOB, 1, "Lunch at noon?"), &[]).unwrap();
    s.insert_message(&msg("S2", &bob, BOB, 2, "100% sure"), &[]).unwrap();
    s.insert_message(&msg("S3", &carol, CAROL, 3, "lunch tomorrow"), &[]).unwrap();
    s.insert_message(&msg("S4", &carol, ME, 4, "ok lunch"), &[]).unwrap();
    let r: Vec<_> = s.search("LUNCH", 10).unwrap().into_iter().map(|m| m.id.0).collect();
    assert_eq!(r, vec!["S4", "S3", "S1"]);
    assert_eq!(s.search("100%", 10).unwrap().len(), 1, "% is literal");
    assert_eq!(s.search("1_0", 10).unwrap().len(), 0, "_ is literal");
    assert!(s.search("  ", 10).unwrap().is_empty());
    assert_eq!(s.search("lunch", 1).unwrap().len(), 1);

    assert_eq!(s.total_unread().unwrap(), 3);
    assert_eq!(s.unread_counts().unwrap(), vec![(bob.clone(), 2), (carol.clone(), 1)]);
    let c = s.mark_read(&bob, Some(&MessageId::new("S1"))).unwrap();
    assert_eq!(c.unread_count, 1);
    let c = s.mark_read(&bob, None).unwrap();
    assert_eq!(c.unread_count, 0);
    assert!(matches!(s.mark_read(&bob, Some(&MessageId::new("S3"))), Err(StoreError::NotFound(_))), "message of another chat");
    // unsending an unread message drops it from the count
    s.apply_unsend(&MessageId::new("S3"), 0, Some(&Handle::new(CAROL)), false, 9).unwrap();
    assert_eq!(s.get_chat(&carol).unwrap().unwrap().unread_count, 0);
    assert!(s.search("tomorrow", 10).unwrap().is_empty());

    let chats = s.list_chats(10, 0).unwrap();
    assert_eq!(chats[0].id, carol, "newest activity first");
    assert_eq!(s.list_chats(10, 1).unwrap().len(), 1);
}

#[test]
fn attachment_locator_is_sealed_and_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let bob = ChatId::direct(BOB);
    let m = msg("AT1", &bob, BOB, 1, "pic");
    let secret = "{\"v\":1,\"plist\":\"TOP-SECRET-FILE-KEY\"}";
    let a = Attachment {
        id: AttachmentId::for_part(&m.id, 0),
        message_id: m.id.clone(),
        mime: "image/jpeg".into(),
        filename: "p.jpg".into(),
        size: 7,
        local_path: None,
        mmcs: secret.into(),
    };
    s.insert_message(&m, std::slice::from_ref(&a)).unwrap();
    let back = s.get_attachment(&a.id).unwrap().unwrap();
    assert_eq!(back.mmcs, secret);
    assert!(!format!("{back:?}").contains("TOP-SECRET"));
    assert!(s.list_attachments(&m.id).unwrap()[0].mmcs.is_empty());
    s.set_attachment_path(&a.id, Some("/x/p.jpg")).unwrap();
    assert_eq!(s.get_attachment(&a.id).unwrap().unwrap().local_path.as_deref(), Some("/x/p.jpg"));
    s.close().unwrap();
    // never in plaintext on disk (db + wal)
    for name in ["messages.db", "messages.db-wal"] {
        if let Ok(bytes) = fs::read(dir.path().join(name)) {
            assert!(!bytes.windows(10).any(|w| w == b"TOP-SECRET"), "{name} leaks the locator");
        }
    }
    // wrong key (state key rotated) cannot read it: reported as an
    // undownloadable attachment, not a corrupt database
    let other = Store::open(dir.path(), StateKey::from_bytes([1; 32])).unwrap();
    assert!(matches!(other.get_attachment(&a.id), Err(StoreError::NotFound(m)) if m.contains("state key changed")));
    other.close().unwrap();
    let s = open(dir.path());
    assert_eq!(s.delete_attachment(&a.id).unwrap().as_deref(), Some("/x/p.jpg"));
    assert!(s.get_attachment(&a.id).unwrap().is_none());
}

#[test]
fn purge_and_vacuum() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let bob = ChatId::direct(BOB);
    let carol = ChatId::direct(CAROL);
    for i in 0..5 {
        s.insert_message(&msg(&format!("P{i}"), &bob, BOB, i, "x"), &[]).unwrap();
    }
    s.insert_message(&msg("C1", &carol, CAROL, 100, "y"), &[]).unwrap();
    s.apply_reaction(&MessageId::new("P1"), &Reaction { kind: ReactionKind::Like, sender: Handle::new(ME), added: true }, 5).unwrap();
    let (n, _) = s.purge_before(3).unwrap();
    assert_eq!(n, 3);
    assert_eq!(s.get_chat(&bob).unwrap().unwrap().unread_count, 2);
    s.purge_chat(&bob).unwrap();
    assert!(s.get_chat(&bob).unwrap().is_none());
    assert!(s.get_message(&MessageId::new("P4")).unwrap().is_none());
    s.vacuum().unwrap();
    assert_eq!(s.list_chats(10, 0).unwrap().len(), 1);
}

#[tokio::test]
async fn async_handle_runs_on_blocking_pool() {
    let dir = tempfile::tempdir().unwrap();
    let h = StoreHandle::new(open(dir.path()));
    let c = h.call(|s| s.upsert_chat(&direct_chat(BOB))).await.unwrap();
    assert_eq!(c.id, ChatId::direct(BOB));
    assert_eq!(h.call(|s| s.list_chats(5, 0)).await.unwrap().len(), 1);
}

fn bg(id: &str) -> BackgroundRef {
    BackgroundRef { background_id: id.into(), payload_version: 1, seq: 1, size: 10, locator: format!("SECRET-{id}") }
}

fn bg_event(chat: &ChatId, b: Option<BackgroundRef>, at: u64) -> EngineEvent {
    EngineEvent::ChatBackground { chat_id: chat.clone(), background: b, by: Handle::new(BOB), at }
}

#[test]
fn chat_background_set_replace_remove() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let bob = ChatId::direct(BOB);
    s.upsert_chat(&direct_chat(BOB)).unwrap();

    let u = s.apply_event(&bg_event(&bob, Some(bg("A")), 100)).unwrap();
    let [StoreUpdate::ChatUpdated(c)] = &u[..] else { panic!("{u:?}") };
    let b = c.background.clone().unwrap();
    assert_eq!((b.id.as_str(), b.path, b.version, b.set_by.as_deref(), b.set_at), ("A", None, 1, Some(BOB), 100));
    // The locator is sealed at rest and opens again.
    let raw: Vec<u8> = s.lock().conn.as_ref().unwrap().query_row("SELECT locator FROM chat_backgrounds", [], |r| r.get(0)).unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains("SECRET"));
    let dl = s.chat_background_locator(&bob).unwrap().unwrap();
    assert_eq!((dl.background_id.as_str(), dl.locator.as_str(), dl.size, dl.local_path.as_deref()), ("A", "SECRET-A", 10, None));
    assert!(!format!("{dl:?}").contains("SECRET"));
    // Redelivery is a no-op.
    assert!(s.apply_event(&bg_event(&bob, Some(bg("A")), 100)).unwrap().is_empty());

    let c = s.set_background_files(&bob, "A", Some("/x/A"), Some("/x/A.heic")).unwrap().unwrap();
    assert_eq!(s.chat_background_locator(&bob).unwrap().unwrap().local_path.as_deref(), Some("/x/A"));
    let b = c.background.unwrap();
    assert_eq!((b.path.as_deref(), b.payload.as_deref()), (Some("/x/A.heic"), Some("/x/A")));
    assert!(s.set_background_files(&bob, "stale", Some("/x/B"), Some("/x/B.heic")).unwrap().is_none());

    // An older change does not override; a newer one replaces and hands back the old file.
    assert!(s.apply_event(&bg_event(&bob, Some(bg("OLD")), 50)).unwrap().is_empty());
    let u = s.apply_event(&bg_event(&bob, Some(bg("B")), 200)).unwrap();
    assert!(matches!(&u[..], [StoreUpdate::ChatUpdated(c), StoreUpdate::FilesRemoved(f)] if c.background.as_ref().unwrap().id == "B" && f == &vec!["/x/A".to_string(), "/x/A.heic".to_string()]));
    assert_eq!(s.list_chats(10, 0).unwrap()[0].background.as_ref().unwrap().id, "B");

    // Remove, then a late set from before the removal is ignored.
    s.set_background_files(&bob, "B", Some("/x/B"), Some("/x/B.heic")).unwrap();
    let u = s.apply_event(&bg_event(&bob, None, 300)).unwrap();
    assert!(matches!(&u[..], [StoreUpdate::ChatUpdated(c), StoreUpdate::FilesRemoved(f)] if c.background.is_none() && f == &vec!["/x/B".to_string(), "/x/B.heic".to_string()]));
    assert!(s.apply_event(&bg_event(&bob, Some(bg("C")), 250)).unwrap().is_empty());
    assert!(s.apply_event(&bg_event(&bob, None, 400)).unwrap().is_empty(), "nothing left to remove");
    assert!(s.get_chat(&bob).unwrap().unwrap().background.is_none());
    assert!(s.chat_background_locator(&bob).unwrap().is_none());
    let u = s.apply_event(&bg_event(&bob, Some(bg("D")), 500)).unwrap();
    assert_eq!(u.len(), 1);

    // Purging the chat hands back the background file too.
    s.set_background_files(&bob, "D", Some("/x/D"), Some("/x/D.heic")).unwrap();
    let purged = s.purge_chat(&bob).unwrap();
    assert!(purged.contains(&"/x/D".to_string()) && purged.contains(&"/x/D.heic".to_string()));
}

/// B1: a peer announcing a huge background is not stored (so never fetched).
#[test]
fn oversized_background_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let bob = ChatId::direct(BOB);
    s.upsert_chat(&direct_chat(BOB)).unwrap();
    let huge = BackgroundRef { size: 1 << 40, ..bg("HUGE") };
    assert!(s.apply_event(&bg_event(&bob, Some(huge), 100)).unwrap().is_empty());
    assert!(s.get_chat(&bob).unwrap().unwrap().background.is_none());
    assert!(s.chat_background_locator(&bob).unwrap().is_none());
    // At the limit is fine.
    let ok = BackgroundRef { size: crate::engine::MAX_BACKGROUND_BYTES, ..bg("OK") };
    assert_eq!(s.apply_event(&bg_event(&bob, Some(ok), 200)).unwrap().len(), 1);
}

/// S6: an import snapshot does not clobber live membership: undated, or
/// older than the chat's newest stored message, it leaves participants
/// (and the name) alone; a newer one applies.
#[test]
fn import_snapshot_does_not_clobber_live_participants() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let g = ChatId::group("IMPORT-1");
    let live = Chat {
        id: g.clone(),
        participants: vec![Handle::new(BOB), Handle::new("mailto:carol@example.com")],
        display_name: Some("Live".into()),
        is_group: true,
        last_message_at: Some(5_000),
        unread_count: 0,
        background: None,
        icon_path: None,
        pinned: false,
        pin_order: None,
    };
    s.upsert_chat(&live).unwrap();
    let names = |s: &Store| s.get_chat(&g).unwrap().unwrap().participants.into_iter().map(|h| h.uri).collect::<Vec<_>>();
    let snapshot = |at: Option<u64>| Chat {
        participants: vec![Handle::new("mailto:dave@example.com")],
        display_name: Some("Mac".into()),
        last_message_at: at,
        ..live.clone()
    };
    s.import(&[snapshot(None)], &[], &[]).unwrap();
    assert_eq!(names(&s), vec![BOB.to_string(), "mailto:carol@example.com".into()], "undated");
    s.import(&[snapshot(Some(4_000))], &[], &[]).unwrap();
    assert_eq!(names(&s), vec![BOB.to_string(), "mailto:carol@example.com".into()], "older than live");
    assert_eq!(s.get_chat(&g).unwrap().unwrap().display_name.as_deref(), Some("Live"));
    s.import(&[snapshot(Some(6_000))], &[], &[]).unwrap();
    assert_eq!(names(&s), vec!["mailto:dave@example.com".to_string()], "newer than live");
    // A new chat takes the undated snapshot's members.
    let fresh = Chat { id: ChatId::group("IMPORT-2"), ..snapshot(None) };
    s.import(&[fresh], &[], &[]).unwrap();
    assert_eq!(s.get_chat(&ChatId::group("IMPORT-2")).unwrap().unwrap().participants.len(), 1);
}

/// An imported background is dated by the import: an older iMessage
/// change delivered late does not undo it, a newer one does.
#[test]
fn imported_background_is_dated() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let bob = ChatId::direct(BOB);
    s.upsert_chat(&direct_chat(BOB)).unwrap();
    let u = s.import_chat_background(&bob, "local-1", 1, "/x/L", Some("/x/L.png"), 1_000).unwrap();
    assert!(matches!(&u[..], [StoreUpdate::ChatUpdated(c)] if c.background.as_ref().unwrap().set_at == 1_000));
    assert!(s.apply_event(&bg_event(&bob, None, 500)).unwrap().is_empty(), "older removal");
    assert!(s.apply_event(&bg_event(&bob, Some(bg("OLD")), 900)).unwrap().is_empty(), "older set");
    assert_eq!(s.get_chat(&bob).unwrap().unwrap().background.unwrap().id, "local-1");
    let u = s.apply_event(&bg_event(&bob, None, 2_000)).unwrap();
    assert!(matches!(&u[..], [StoreUpdate::ChatUpdated(c), StoreUpdate::FilesRemoved(_)] if c.background.is_none()));
}

fn group_chat(guid: &str, participants: &[&str], name: Option<&str>) -> Chat {
    Chat {
        id: ChatId::group(guid),
        participants: participants.iter().map(|p| Handle::new(p)).collect(),
        display_name: name.map(String::from),
        is_group: true,
        last_message_at: None,
        unread_count: 0,
        background: None,
        icon_path: None,
        pinned: false,
        pin_order: None,
    }
}

/// Empty group rows (live 1:1 traffic carrying a group id used to create
/// them) go; groups with members, messages, a name or a background stay.
#[test]
fn empty_groups_are_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    for g in ["GHOST-1", "GHOST-2"] {
        s.upsert_chat(&group_chat(g, &[], None)).unwrap();
    }
    s.upsert_chat(&group_chat("MEMBERS", &[BOB, CAROL], None)).unwrap();
    s.upsert_chat(&group_chat("NAMED", &[], Some("Crew"))).unwrap();
    s.insert_message(&msg("G1", &ChatId::group("WITH-MSG"), BOB, 1, "x"), &[]).unwrap();
    s.upsert_chat(&group_chat("BG", &[], None)).unwrap();
    s.apply_event(&bg_event(&ChatId::group("BG"), Some(bg("B")), 5)).unwrap();
    s.upsert_chat(&direct_chat(BOB)).unwrap();
    assert_eq!(s.delete_empty_groups().unwrap(), 2);
    let mut ids: Vec<String> = s.list_chats(50, 0).unwrap().into_iter().map(|c| c.id.0).collect();
    ids.sort();
    assert_eq!(ids, vec!["group:BG", "group:MEMBERS", "group:NAMED", "group:WITH-MSG", BOB]);
    assert_eq!(s.delete_empty_groups().unwrap(), 0);
}

/// An import drops empty groups inside its transaction: a group snapshot
/// with no members and nothing in it is neither listed in the outcome nor
/// counted as created, and stray empty rows from before go too.
#[test]
fn import_drops_empty_groups_before_reporting() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    s.upsert_chat(&group_chat("GHOST-OLD", &[], None)).unwrap();
    let bob = direct_chat(BOB);
    let out = s.import(&[group_chat("GHOST-NEW", &[], None), group_chat("KEEP", &[BOB, CAROL], None), bob.clone()], &[], &[]).unwrap();
    let ids: Vec<&str> = out.chats.iter().map(|c| c.id.0.as_str()).collect();
    assert_eq!(ids, vec!["group:KEEP", BOB]);
    assert_eq!(out.result.chats_created, 2);
    assert_eq!(out.removed_empty_groups, 2);
    assert!(s.get_chat(&ChatId::group("GHOST-NEW")).unwrap().is_none());
    assert!(s.get_chat(&ChatId::group("GHOST-OLD")).unwrap().is_none());
}

/// Chats with ids that are not valid chat ids (raw Mac SMS handles) go
/// with their messages, reactions, attachments and files; a dry run
/// counts the same and changes nothing.
#[test]
fn purge_invalid_chats() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let bad = ChatId("46339(smsft)".into());
    let bad2 = ChatId("+12125550199(smsfp)".into());
    let good = ChatId::direct("tel:46339");
    s.upsert_chat(&Chat { id: bad.clone(), participants: vec![Handle::new("46339(smsft)")], ..direct_chat(BOB) }).unwrap();
    let m1 = msg("B1", &bad, "46339(smsft)", 1, "code 123");
    s.insert_message(&m1, &[attachment_for(&m1, Some("/state/attachments/B1/a.png"))]).unwrap();
    s.insert_message(&msg("B2", &bad, ME, 2, "ok"), &[]).unwrap();
    s.apply_reaction(&MessageId::new("B2"), &Reaction { kind: ReactionKind::Like, sender: Handle::new(BOB), added: true }, 3).unwrap();
    s.insert_message(&msg("B3", &bad2, "+12125550199(smsfp)", 4, "x"), &[]).unwrap();
    s.insert_message(&msg("G1", &good, "tel:46339", 5, "y"), &[]).unwrap();
    s.upsert_chat(&group_chat("FINE", &[BOB, CAROL], None)).unwrap();

    let dry = s.purge_invalid_chats(true).unwrap();
    assert!(dry.dry_run);
    assert_eq!(dry.chats.iter().map(|c| c.id.0.as_str()).collect::<Vec<_>>(), vec!["+12125550199(smsfp)", "46339(smsft)"]);
    assert_eq!((dry.messages, dry.reactions, dry.attachments, dry.files, dry.handles), (3, 1, 1, 1, 2));
    assert_eq!(dry.paths, vec!["/state/attachments/B1/a.png".to_string()]);
    assert_eq!(s.list_chats(50, 0).unwrap().len(), 4, "dry run changes nothing");
    assert!(s.get_message(&MessageId::new("B1")).unwrap().is_some());

    let done = s.purge_invalid_chats(false).unwrap();
    assert_eq!(PurgeSummary { dry_run: true, ..done.clone() }, dry);
    for m in ["B1", "B2", "B3"] {
        assert!(s.get_message(&MessageId::new(m)).unwrap().is_none(), "{m}");
    }
    assert!(s.get_attachment(&AttachmentId::for_part(&m1.id, 1)).unwrap().is_none());
    let mut ids: Vec<String> = s.list_chats(50, 0).unwrap().into_iter().map(|c| c.id.0).collect();
    ids.sort();
    assert_eq!(ids, vec!["group:FINE", "tel:46339"]);
    let json = serde_json::to_value(&done).unwrap();
    assert_eq!(json["messages"], 3);
    assert!(json.get("paths").is_none());
    let again = s.purge_invalid_chats(false).unwrap();
    assert!(again.chats.is_empty() && again.handles == 0);
}

fn share(key: &str) -> crate::engine::ShareRef {
    crate::engine::ShareRef { record_key: key.into(), decryption_key: b"SECRETKEY-123456".to_vec(), poster: None }
}

#[test]
fn shared_profiles_record_fetch_clear() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    s.upsert_chat(&direct_chat(BOB)).unwrap();
    let bob = Handle::new(BOB);

    assert_eq!(s.record_profile_share(&bob, Some(&share("R1")), 100).unwrap(), ShareOutcome::Changed);
    // key sealed at rest, opens again
    let raw: Vec<u8> = s.lock().conn.as_ref().unwrap().query_row("SELECT decryption_key FROM handle_profiles", [], |r| r.get(0)).unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains("SECRETKEY"));
    assert_eq!(s.profile_share(BOB).unwrap().unwrap(), share("R1"));
    // nothing to show yet
    assert_eq!(s.handle_profile(BOB).unwrap(), None);
    assert!(s.get_chat(&ChatId::direct(BOB)).unwrap().unwrap().participants[0].profile.is_none());
    // the same record again, not fetched yet
    assert_eq!(s.record_profile_share(&bob, Some(&share("R1")), 110).unwrap(), ShareOutcome::Unfetched);
    assert_eq!(s.stale_profiles(&[BOB.to_string(), CAROL.to_string()], 0).unwrap(), vec![BOB.to_string()]);

    // a fetch for an older record is refused
    assert!(s.set_profile_fetched(BOB, "R0", ("Bob", "", ""), None, 5).unwrap().is_none());
    let (p, replaced) = s.set_profile_fetched(BOB, "R1", ("Bobby", "Bob", "Smith"), Some("/p/R1.jpg"), 1_000).unwrap().unwrap();
    let p = p.unwrap();
    assert_eq!((p.name.as_deref(), p.first.as_deref(), p.last.as_deref(), p.photo.as_deref(), p.updated_at), (Some("Bobby"), Some("Bob"), Some("Smith"), Some("/p/R1.jpg"), 1_000));
    assert!(replaced.is_empty());
    let c = s.get_chat(&ChatId::direct(BOB)).unwrap().unwrap();
    assert_eq!(c.participants[0].profile.as_ref(), Some(&p));
    assert!(serde_json::to_string(&c).unwrap().contains(r#""profile":{"name":"Bobby""#));
    assert_eq!(s.record_profile_share(&bob, Some(&share("R1")), 120).unwrap(), ShareOutcome::Nothing);
    assert!(s.stale_profiles(&[BOB.to_string()], 500).unwrap().is_empty());
    assert_eq!(s.stale_profiles(&[BOB.to_string()], 2_000).unwrap(), vec![BOB.to_string()], "older than a day");
    assert_eq!(s.chats_with_participant(BOB).unwrap().len(), 1);

    // a new record: the old name stays until the fetch, whose photo replaces the old one
    assert_eq!(s.record_profile_share(&bob, Some(&share("R2")), 200).unwrap(), ShareOutcome::Changed);
    assert_eq!(s.handle_profile(BOB).unwrap().unwrap().name.as_deref(), Some("Bobby"));
    let (_, replaced) = s.set_profile_fetched(BOB, "R2", ("Bob S.", "", ""), Some("/p/R2.png"), 2_000).unwrap().unwrap();
    assert_eq!(replaced, vec!["/p/R1.jpg".to_string()]);

    // an older share does not win; a stop clears and hands back the photo
    assert_eq!(s.record_profile_share(&bob, Some(&share("R1")), 150).unwrap(), ShareOutcome::Nothing);
    assert_eq!(s.record_profile_share(&bob, None, 300).unwrap(), ShareOutcome::Cleared { files: vec!["/p/R2.png".into()], had_profile: true });
    assert_eq!(s.handle_profile(BOB).unwrap(), None);
    assert!(s.profile_share(BOB).unwrap().is_none());
    // a late share from before the stop is ignored; a second stop has nothing to clear
    assert_eq!(s.record_profile_share(&bob, Some(&share("R2")), 250).unwrap(), ShareOutcome::Nothing);
    assert_eq!(s.record_profile_share(&bob, None, 310).unwrap(), ShareOutcome::Cleared { files: vec![], had_profile: false });
    // a stop from someone who never shared leaves a tombstone
    assert_eq!(s.record_profile_share(&Handle::new(CAROL), None, 50).unwrap(), ShareOutcome::Cleared { files: vec![], had_profile: false });
    assert_eq!(s.record_profile_share(&Handle::new(CAROL), Some(&share("C1")), 40).unwrap(), ShareOutcome::Nothing);
}

#[test]
fn group_icons_set_download_remove() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let g = ChatId::group("G-ICON");
    let icon = |id: &str, size: u64| crate::engine::IconRef { icon_id: id.into(), size, locator: format!("SECRET-{id}") };
    let ev = |i: Option<crate::engine::IconRef>, at| EngineEvent::ChatIcon { chat_id: g.clone(), icon: i, by: Handle::new(BOB), at };

    let u = s.apply_event(&ev(Some(icon("I1", 10)), 100)).unwrap();
    assert!(matches!(&u[..], [StoreUpdate::ChatUpdated(c)] if c.icon_path.is_none()));
    assert!(s.apply_event(&ev(Some(icon("I1", 10)), 100)).unwrap().is_empty(), "redelivery");
    let dl = s.chat_icon_locator(&g).unwrap().unwrap();
    assert_eq!((dl.icon_id.as_str(), dl.locator.as_str(), dl.size), ("I1", "SECRET-I1", 10));
    assert!(!format!("{dl:?}").contains("SECRET"));
    assert!(s.set_icon_file(&g, "other", "/i/x").unwrap().is_none());
    let c = s.set_icon_file(&g, "I1", "/i/I1.jpg").unwrap().unwrap();
    assert_eq!(c.icon_path.as_deref(), Some("/i/I1.jpg"));
    assert!(serde_json::to_string(&c).unwrap().contains(r#""icon_path":"/i/I1.jpg""#));
    // too large / older: ignored
    assert!(s.apply_event(&ev(Some(icon("BIG", crate::engine::MAX_ICON_BYTES + 1)), 200)).unwrap().is_empty());
    assert!(s.apply_event(&ev(Some(icon("OLD", 10)), 50)).unwrap().is_empty());
    // replaced: old file handed back
    let u = s.apply_event(&ev(Some(icon("I2", 10)), 200)).unwrap();
    assert!(matches!(&u[..], [StoreUpdate::ChatUpdated(c), StoreUpdate::FilesRemoved(f)] if c.icon_path.is_none() && f == &vec!["/i/I1.jpg".to_string()]));
    s.set_icon_file(&g, "I2", "/i/I2.png").unwrap();
    // removed
    let u = s.apply_event(&ev(None, 300)).unwrap();
    assert!(matches!(&u[..], [StoreUpdate::ChatUpdated(c), StoreUpdate::FilesRemoved(f)] if c.icon_path.is_none() && f == &vec!["/i/I2.png".to_string()]));
    assert!(s.chat_icon_locator(&g).unwrap().is_none());
    assert!(s.apply_event(&ev(None, 400)).unwrap().is_empty());
    assert!(s.apply_event(&ev(Some(icon("I3", 10)), 350)).unwrap().is_empty(), "older than the removal");
    // a group with only an icon is not "empty"
    s.apply_event(&ev(Some(icon("I4", 10)), 500)).unwrap();
    assert_eq!(s.delete_empty_groups().unwrap(), 0);
}

#[test]
fn message_service_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let chat = direct_chat("+15555550123");
    let mut sms = msg("S1", &chat.id, "tel:+15555550123", 10, "via the phone");
    sms.service = Service::Sms;
    s.apply_event(&received(sms, chat.clone())).unwrap();
    s.apply_event(&received(msg("I1", &chat.id, "tel:+15555550123", 20, "over iMessage"), chat.clone())).unwrap();
    // one thread for both services
    let page = s.list_messages(&chat.id, None, 10).unwrap();
    let services: Vec<Service> = page.messages.iter().map(|m| m.service).collect();
    assert_eq!(services, vec![Service::Sms, Service::Imessage]);
    assert_eq!(s.get_message(&MessageId::new("S1")).unwrap().unwrap().service, Service::Sms);
    // JSON carries it
    let j = serde_json::to_value(s.get_message(&MessageId::new("S1")).unwrap().unwrap()).unwrap();
    assert_eq!(j["service"], "sms");
}

/// Pins: insert at a position, move, unpin, reorder; dense pin_order;
/// ChatUpdated only for chats whose pin changed; unknown chats refused.
#[test]
fn pins() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let (bob, carol, dave) = (ChatId::direct(BOB), ChatId::direct(CAROL), ChatId::direct("tel:+15551234567"));
    for h in [BOB, CAROL, "tel:+15551234567"] {
        s.upsert_chat(&direct_chat(h)).unwrap();
    }
    let order = |s: &Store| s.pinned_chats().unwrap().iter().map(|c| (c.id.0.clone(), c.pin_order)).collect::<Vec<_>>();
    let changed = |u: &[StoreUpdate]| {
        let mut v: Vec<String> = u.iter().filter_map(|u| if let StoreUpdate::ChatUpdated(c) = u { Some(c.id.0.clone()) } else { None }).collect();
        v.sort();
        v
    };
    let c = s.get_chat(&bob).unwrap().unwrap();
    assert!(!c.pinned && c.pin_order.is_none());

    let u = s.set_chat_pinned(&bob, true, None).unwrap();
    assert_eq!(changed(&u), vec![BOB.to_string()]);
    let c = s.get_chat(&bob).unwrap().unwrap();
    assert!(c.pinned && c.pin_order == Some(0));
    // carol at the front: bob moves down
    let u = s.set_chat_pinned(&carol, true, Some(0)).unwrap();
    assert_eq!(changed(&u), vec![BOB.to_string(), CAROL.to_string()]);
    assert_eq!(order(&s), vec![(CAROL.into(), Some(0)), (BOB.into(), Some(1))]);
    // past the end = last
    s.set_chat_pinned(&dave, true, Some(99)).unwrap();
    assert_eq!(order(&s).iter().map(|(i, _)| i.as_str()).collect::<Vec<_>>(), [CAROL, BOB, "tel:+15551234567"]);
    // the same again: no change, no signal
    assert!(s.set_chat_pinned(&dave, true, Some(2)).unwrap().is_empty());
    assert!(s.set_chat_pinned(&dave, true, None).unwrap().is_empty());
    // no order on an already pinned chat: it stays where it is (bob is in the middle)
    assert!(s.set_chat_pinned(&bob, true, None).unwrap().is_empty());
    assert_eq!(order(&s)[1].0, BOB);
    // unpin the middle one: the rest close up
    let u = s.set_chat_pinned(&bob, false, None).unwrap();
    assert_eq!(changed(&u), vec![BOB.to_string(), "tel:+15551234567".to_string()]);
    assert_eq!(order(&s), vec![(CAROL.into(), Some(0)), ("tel:+15551234567".into(), Some(1))]);
    let c = s.get_chat(&bob).unwrap().unwrap();
    assert!(!c.pinned && c.pin_order.is_none());
    assert!(s.set_chat_pinned(&bob, false, None).unwrap().is_empty(), "unpinning an unpinned chat changes nothing");

    // reorder: listed first (bob gets pinned), the others keep their order after
    let u = s.reorder_pins(&[bob.clone(), dave.clone(), bob.clone()]).unwrap();
    assert_eq!(changed(&u), vec![BOB.to_string(), CAROL.to_string()], "dave stays at 1");
    assert_eq!(order(&s), vec![(BOB.into(), Some(0)), ("tel:+15551234567".into(), Some(1)), (CAROL.into(), Some(2))]);
    assert!(s.reorder_pins(std::slice::from_ref(&bob)).unwrap().is_empty());
    // unknown chats: refused, nothing changes
    assert!(matches!(s.reorder_pins(&[carol.clone(), ChatId::direct("mailto:nobody@example.com")]), Err(StoreError::NotFound(_))));
    assert!(matches!(s.set_chat_pinned(&ChatId::direct("mailto:nobody@example.com"), true, None), Err(StoreError::NotFound(_))));
    assert_eq!(order(&s)[0].0, BOB);
    // pins survive a reopen and upserts
    s.upsert_chat(&direct_chat(BOB)).unwrap();
    s.close().unwrap();
    let s = open(dir.path());
    assert_eq!(order(&s).len(), 3);
    assert!(s.list_chats(10, 0).unwrap().iter().filter(|c| c.pinned).count() == 3);
    // a purged chat leaves the pins
    s.purge_chat(&dave).unwrap();
    assert_eq!(order(&s).iter().map(|(i, _)| i.as_str()).collect::<Vec<_>>(), [BOB, CAROL]);
}
