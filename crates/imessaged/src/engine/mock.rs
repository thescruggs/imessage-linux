//! In-memory [`Engine`] for tests (this crate's and Phase 2b's integration
//! tests). Records every call, lets tests inject events, and answers with
//! configurable canned results. No network, no rustpush state.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use imessage_common::model::{
    Chat, ChatId, ConnectionState, DeliveryState, Handle, MessageId, MessagePart, ProfilesState, ProfilesStatus, ReactionKind,
    ReadReceipt, RegistrationState, Service, SmsForwarding, SmsState,
};
use tokio::sync::broadcast;

use super::{
    ChatTarget, Engine, EngineError, EngineEvent, EngineResult, FetchedProfile, LoginOutcome, SentMessage, ShareRef, EVENT_CHANNEL_CAPACITY,
};

/// Fixed send timestamp (ms) reported by the mock.
pub const MOCK_SENT_AT: u64 = 1_700_000_000_000;

/// One recorded call. Passwords and 2FA codes are not recorded.
#[derive(Debug, Clone, PartialEq)]
pub enum MockCall {
    Login { apple_id: String },
    SubmitTwoFactor,
    Logout,
    Register,
    SendText { chat: ChatTarget, text: String, reply_to: Option<MessageId> },
    SendAttachment { chat: ChatTarget, path: PathBuf, mime: String },
    React { chat: ChatTarget, message_id: MessageId, kind: ReactionKind, remove: bool },
    Edit { chat: ChatTarget, message_id: MessageId, new_text: String },
    Unsend { chat: ChatTarget, message_id: MessageId },
    SetTyping { chat: ChatTarget, typing: bool },
    MarkRead { chat: ChatTarget, up_to: MessageId },
    LookupHandles(Vec<String>),
    /// The locator is secret (decryption key); only the destination is recorded.
    DownloadAttachment { dest: PathBuf },
    /// The decryption key is secret; only the record is recorded.
    FetchProfile { record_key: String },
    UpsertChat(ChatId),
    /// A text message (`Service::Sms`) send: text, or an attachment path.
    SendSms { chat: ChatTarget, text: Option<String>, path: Option<PathBuf> },
    SetSmsForwarding(bool),
    Shutdown,
}

#[derive(Debug)]
struct Inner {
    calls: Vec<MockCall>,
    connection: ConnectionState,
    registration: RegistrationState,
    require_two_factor: bool,
    two_factor_pending: bool,
    reachable: HashMap<String, bool>,
    fail_next: Option<String>,
    chats: HashMap<ChatId, Chat>,
    next_id: u64,
    next_group: u64,
    download_delay: std::time::Duration,
    profiles: HashMap<String, FetchedProfile>,
    profiles_status: ProfilesStatus,
    sms: SmsForwarding,
}

pub struct MockEngine {
    inner: Mutex<Inner>,
    events: broadcast::Sender<EngineEvent>,
}

impl Default for MockEngine {
    fn default() -> Self {
        MockEngine::new()
    }
}

impl MockEngine {
    /// What [`Engine::download_attachment`] writes.
    pub const ATTACHMENT_PAYLOAD: &'static [u8] = b"mock attachment payload";

    pub fn new() -> MockEngine {
        MockEngine {
            inner: Mutex::new(Inner {
                calls: vec![],
                connection: ConnectionState::NeedsLogin,
                registration: RegistrationState::default(),
                require_two_factor: false,
                two_factor_pending: false,
                reachable: HashMap::new(),
                fail_next: None,
                chats: HashMap::new(),
                next_id: 1,
                next_group: 1,
                download_delay: std::time::Duration::ZERO,
                profiles: HashMap::new(),
                profiles_status: ProfilesStatus { state: ProfilesState::Ready, detail: None },
                sms: SmsForwarding::default(),
            }),
            events: broadcast::channel(EVENT_CHANNEL_CAPACITY).0,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Deliver an event to all subscribers as if the engine produced it.
    pub fn inject(&self, event: EngineEvent) {
        let _ = self.events.send(event);
    }
    /// Set the Text Message Forwarding state (and announce it, as the real
    /// engine does when the iPhone confirms).
    pub fn set_sms(&self, sms: SmsForwarding) {
        self.lock().sms = sms.clone();
        self.inject(EngineEvent::SmsForwardingChanged(sms));
    }
    /// SMS forwarding `active` for `number`.
    pub fn activate_sms(&self, number: &str) {
        self.set_sms(SmsForwarding { state: SmsState::Active, number: Some(Handle::normalize(number)), since: Some(MOCK_SENT_AT), detail: None });
    }
    pub fn calls(&self) -> Vec<MockCall> {
        self.lock().calls.clone()
    }
    pub fn clear_calls(&self) {
        self.lock().calls.clear();
    }
    pub fn set_require_two_factor(&self, v: bool) {
        self.lock().require_two_factor = v;
    }
    pub fn set_connection_state(&self, s: ConnectionState) {
        self.lock().connection = s.clone();
        self.inject(EngineEvent::ConnectionChanged(s));
    }
    /// The daemon's `other_handles` as `registration_state` reports them.
    pub fn set_other_handles(&self, handles: Vec<String>) {
        self.lock().registration.other_handles = handles;
    }
    pub fn set_reachable(&self, handle: &str, reachable: bool) {
        self.lock().reachable.insert(Handle::normalize(handle), reachable);
    }
    /// Make every `download_attachment` take this long (to test races).
    pub fn set_download_delay(&self, d: std::time::Duration) {
        self.lock().download_delay = d;
    }
    /// What [`Engine::fetch_profile`] returns for `record_key` (unknown
    /// records fail with `Protocol`).
    pub fn set_profile(&self, record_key: &str, profile: FetchedProfile) {
        self.lock().profiles.insert(record_key.to_string(), profile);
    }
    /// `profiles_status`; anything but `Ready` makes `fetch_profile` fail
    /// with `Unsupported`.
    pub fn set_profiles_status(&self, s: ProfilesStatus) {
        self.lock().profiles_status = s;
    }
    /// Make the next fallible call fail with `EngineError::Protocol(msg)`.
    pub fn fail_next(&self, msg: impl Into<String>) {
        self.lock().fail_next = Some(msg.into());
    }

    fn record(&self, call: MockCall) -> EngineResult<()> {
        let mut g = self.lock();
        g.calls.push(call);
        match g.fail_next.take() {
            Some(m) => Err(EngineError::Protocol(m)),
            None => Ok(()),
        }
    }

    fn new_id(&self) -> MessageId {
        let mut g = self.lock();
        let n = g.next_id;
        g.next_id += 1;
        MessageId(format!("00000000-0000-4000-8000-{n:012X}"))
    }

    fn logged_in(&self) -> EngineResult<()> {
        if self.lock().registration.is_registered() { Ok(()) } else { Err(EngineError::NotLoggedIn) }
    }

    fn finish_login(&self, apple_id: &str) {
        let reg = RegistrationState {
            handles: vec![Handle::new(apple_id)],
            registered_at: Some(1_700_000_000),
            expires_at: Some(1_700_000_000 + 30 * 86400),
            sender: Some(Handle::new(apple_id)),
            other_handles: vec![],
        };
        {
            let mut g = self.lock();
            g.registration = reg.clone();
            g.connection = ConnectionState::Connected;
        }
        self.inject(EngineEvent::RegistrationChanged(reg));
        self.inject(EngineEvent::ConnectionChanged(ConnectionState::Connected));
    }

    /// Same id scheme as the real engine: 1:1 = the other handle, new
    /// participant sets get a minted `group:<GUID>` (deterministic here:
    /// `00000000-0000-4000-9000-<n>`) that is reused for the same set.
    fn resolve_chat_id(&self, chat: &ChatTarget) -> EngineResult<ChatId> {
        match chat {
            ChatTarget::Participants(p) => {
                let mut others: Vec<String> = Vec::new();
                for h in p {
                    let n = Handle::try_normalize(h).ok_or_else(|| EngineError::InvalidHandle(h.clone()))?;
                    if !others.contains(&n) {
                        others.push(n);
                    }
                }
                match others.len() {
                    0 => Err(EngineError::InvalidInput("no recipients".into())),
                    1 => Ok(ChatId::direct(&others[0])),
                    _ => {
                        let mut g = self.lock();
                        let mut want = others.clone();
                        want.sort();
                        let existing = g.chats.values().find(|c| {
                            let mut have: Vec<String> = c.participants.iter().map(|h| h.uri.clone()).collect();
                            have.sort();
                            c.is_group && have == want
                        });
                        if let Some(c) = existing {
                            return Ok(c.id.clone());
                        }
                        let n = g.next_group;
                        g.next_group += 1;
                        let id = ChatId::group(&format!("00000000-0000-4000-9000-{n:012X}"));
                        let chat = Chat {
                            id: id.clone(),
                            participants: others.iter().map(|o| Handle::new(o)).collect(),
                            display_name: None,
                            is_group: true,
                            last_message_at: None,
                            unread_count: 0,
                            background: None,
                            icon_path: None,
                        };
                        g.chats.insert(id.clone(), chat);
                        Ok(id)
                    }
                }
            }
            ChatTarget::Chat(id) if id.is_group() => {
                if self.lock().chats.contains_key(id) { Ok(id.clone()) } else { Err(EngineError::ChatNotFound(id.clone())) }
            }
            ChatTarget::Chat(id) => Handle::try_normalize(&id.0).map(|h| ChatId::direct(&h)).ok_or_else(|| EngineError::InvalidHandle(id.0.clone())),
        }
    }

    fn sent(&self, chat: &ChatTarget, report: bool) -> EngineResult<SentMessage> {
        self.logged_in()?;
        let chat_id = self.resolve_chat_id(chat)?;
        let id = self.new_id();
        if report {
            self.inject(EngineEvent::MessageSent { id: id.clone(), chat_id: Some(chat_id.clone()), delivery: DeliveryState::Sent, error: None, at: MOCK_SENT_AT });
        }
        let sender = self.lock().registration.handles.first().cloned().unwrap_or_else(|| Handle::new(""));
        Ok(SentMessage { id, chat_id, sent_at: MOCK_SENT_AT, sender, service: Default::default() })
    }

    fn sent_sms(&self, chat: &ChatTarget) -> EngineResult<SentMessage> {
        self.logged_in()?;
        let sms = self.lock().sms.clone();
        if !sms.is_active() {
            return Err(EngineError::Unsupported("Text Message Forwarding is not active".into()));
        }
        // A group text is keyed by its numbers; the mock need not know it.
        let mut sent = match chat {
            ChatTarget::Chat(id) if id.0.starts_with(ChatId::GROUP_PARTICIPANTS_PREFIX) => {
                let id2 = self.new_id();
                self.inject(EngineEvent::MessageSent { id: id2.clone(), chat_id: Some(id.clone()), delivery: DeliveryState::Sent, error: None, at: MOCK_SENT_AT });
                let sender = self.lock().registration.handles.first().cloned().unwrap_or_else(|| Handle::new(""));
                SentMessage { id: id2, chat_id: id.clone(), sent_at: MOCK_SENT_AT, sender, service: Service::Sms }
            }
            _ => self.sent(chat, true)?,
        };
        sent.service = Service::Sms;
        Ok(sent)
    }

    // Event constructors for tests. Like the real engine's conversion, the
    // ownership flags (`from_me` / `by_me`) default to `false` (= a peer);
    // the `*_from_me` / `*_by_me` variants model another of our devices.

    /// `Edited` text of `message_id` by `sender` (a peer: `from_me = false`).
    pub fn edited(chat_id: &ChatId, message_id: &MessageId, sender: &str, text: &str, at: u64) -> EngineEvent {
        Self::edit_event(chat_id, message_id, sender, text, at, false)
    }

    /// [`MockEngine::edited`] from one of our own devices (`from_me = true`).
    pub fn edited_from_me(chat_id: &ChatId, message_id: &MessageId, sender: &str, text: &str, at: u64) -> EngineEvent {
        Self::edit_event(chat_id, message_id, sender, text, at, true)
    }

    fn edit_event(chat_id: &ChatId, message_id: &MessageId, sender: &str, text: &str, at: u64, from_me: bool) -> EngineEvent {
        EngineEvent::Edited {
            chat_id: chat_id.clone(),
            message_id: message_id.clone(),
            part: 0,
            new_parts: vec![MessagePart::Text { text: text.to_string() }],
            new_text: Some(text.to_string()),
            sender: Handle::new(sender),
            from_me,
            at,
        }
    }

    /// `Unsent` of `message_id` by `sender` (a peer: `from_me = false`).
    pub fn unsent(chat_id: &ChatId, message_id: &MessageId, sender: &str, at: u64) -> EngineEvent {
        EngineEvent::Unsent { chat_id: chat_id.clone(), message_id: message_id.clone(), part: 0, sender: Handle::new(sender), from_me: false, at }
    }

    /// [`MockEngine::unsent`] from one of our own devices (`from_me = true`).
    pub fn unsent_from_me(chat_id: &ChatId, message_id: &MessageId, sender: &str, at: u64) -> EngineEvent {
        EngineEvent::Unsent { chat_id: chat_id.clone(), message_id: message_id.clone(), part: 0, sender: Handle::new(sender), from_me: true, at }
    }

    /// Read receipt from a peer (`by_me = false`).
    pub fn read(chat_id: Option<&ChatId>, message_id: &MessageId, reader: &str, at: u64) -> EngineEvent {
        EngineEvent::Read(ReadReceipt { chat_id: chat_id.cloned(), message_id: message_id.clone(), reader: Handle::new(reader), read_at: at, by_me: false })
    }

    /// Read on another of our own devices (`by_me = true`).
    pub fn read_by_me(chat_id: Option<&ChatId>, message_id: &MessageId, reader: &str, at: u64) -> EngineEvent {
        EngineEvent::Read(ReadReceipt { chat_id: chat_id.cloned(), message_id: message_id.clone(), reader: Handle::new(reader), read_at: at, by_me: true })
    }
}

#[async_trait]
impl Engine for MockEngine {
    async fn login(&self, apple_id: &str, _password: &str) -> EngineResult<LoginOutcome> {
        self.record(MockCall::Login { apple_id: apple_id.to_string() })?;
        if self.lock().require_two_factor {
            {
                let mut g = self.lock();
                g.two_factor_pending = true;
                g.connection = ConnectionState::NeedsTwoFactor;
            }
            self.inject(EngineEvent::TwoFactorRequired);
            self.inject(EngineEvent::ConnectionChanged(ConnectionState::NeedsTwoFactor));
            // remember who for submit_two_factor
            self.lock().registration.handles = vec![Handle::new(apple_id)];
            return Ok(LoginOutcome::TwoFactorRequired);
        }
        self.finish_login(apple_id);
        Ok(LoginOutcome::Ok)
    }

    async fn submit_two_factor(&self, code: &str) -> EngineResult<LoginOutcome> {
        self.record(MockCall::SubmitTwoFactor)?;
        let who = {
            let g = self.lock();
            if !g.two_factor_pending {
                return Err(EngineError::NoPendingTwoFactor);
            }
            g.registration.handles.first().map(|h| h.address().to_string()).unwrap_or_default()
        };
        if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
            return Err(EngineError::BadTwoFactorCode);
        }
        self.lock().two_factor_pending = false;
        self.finish_login(&who);
        Ok(LoginOutcome::Ok)
    }

    async fn logout(&self) -> EngineResult<()> {
        self.record(MockCall::Logout)?;
        {
            let mut g = self.lock();
            g.registration = RegistrationState::default();
            g.two_factor_pending = false;
            g.connection = ConnectionState::NeedsLogin;
        }
        self.inject(EngineEvent::RegistrationChanged(RegistrationState::default()));
        self.inject(EngineEvent::ConnectionChanged(ConnectionState::NeedsLogin));
        Ok(())
    }

    async fn register(&self) -> EngineResult<RegistrationState> {
        self.record(MockCall::Register)?;
        self.logged_in()?;
        Ok(self.lock().registration.clone())
    }

    async fn registration_state(&self) -> RegistrationState {
        self.lock().registration.clone()
    }

    async fn connection_state(&self) -> ConnectionState {
        self.lock().connection.clone()
    }

    async fn handles(&self) -> Vec<Handle> {
        self.lock().registration.handles.clone()
    }

    async fn send_text(&self, chat: &ChatTarget, text: &str, reply_to: Option<&MessageId>) -> EngineResult<SentMessage> {
        self.record(MockCall::SendText { chat: chat.clone(), text: text.to_string(), reply_to: reply_to.cloned() })?;
        self.sent(chat, true)
    }

    async fn send_attachment(&self, chat: &ChatTarget, path: &Path, mime: &str) -> EngineResult<SentMessage> {
        self.record(MockCall::SendAttachment { chat: chat.clone(), path: path.to_path_buf(), mime: mime.to_string() })?;
        self.sent(chat, true)
    }

    async fn send_text_via(&self, chat: &ChatTarget, text: &str, reply_to: Option<&MessageId>, service: Service) -> EngineResult<SentMessage> {
        if service == Service::Imessage {
            return self.send_text(chat, text, reply_to).await;
        }
        self.record(MockCall::SendSms { chat: chat.clone(), text: Some(text.to_string()), path: None })?;
        self.sent_sms(chat)
    }

    async fn send_attachment_via(&self, chat: &ChatTarget, path: &Path, mime: &str, service: Service) -> EngineResult<SentMessage> {
        if service == Service::Imessage {
            return self.send_attachment(chat, path, mime).await;
        }
        self.record(MockCall::SendSms { chat: chat.clone(), text: None, path: Some(path.to_path_buf()) })?;
        self.sent_sms(chat)
    }

    async fn sms_forwarding(&self) -> SmsForwarding {
        self.lock().sms.clone()
    }

    async fn set_sms_forwarding(&self, enable: bool) -> EngineResult<SmsForwarding> {
        self.record(MockCall::SetSmsForwarding(enable))?;
        self.logged_in()?;
        let number = self.lock().registration.other_handles.iter().find(|h| h.starts_with("tel:")).cloned();
        let Some(number) = number else {
            return Err(EngineError::InvalidInput("no phone number to relay through (set sms_relay_number)".into()));
        };
        let state = if enable { SmsState::Requested } else { SmsState::Off };
        let sms = SmsForwarding { state, number: Some(number), since: Some(MOCK_SENT_AT), detail: None };
        self.set_sms(sms.clone());
        Ok(sms)
    }

    async fn react(&self, chat: &ChatTarget, message_id: &MessageId, kind: ReactionKind, remove: bool) -> EngineResult<SentMessage> {
        self.record(MockCall::React { chat: chat.clone(), message_id: message_id.clone(), kind, remove })?;
        self.sent(chat, false)
    }

    async fn edit(&self, chat: &ChatTarget, message_id: &MessageId, new_text: &str) -> EngineResult<SentMessage> {
        self.record(MockCall::Edit { chat: chat.clone(), message_id: message_id.clone(), new_text: new_text.to_string() })?;
        self.sent(chat, false)
    }

    async fn unsend(&self, chat: &ChatTarget, message_id: &MessageId) -> EngineResult<SentMessage> {
        self.record(MockCall::Unsend { chat: chat.clone(), message_id: message_id.clone() })?;
        self.sent(chat, false)
    }

    async fn set_typing(&self, chat: &ChatTarget, typing: bool) -> EngineResult<()> {
        self.record(MockCall::SetTyping { chat: chat.clone(), typing })?;
        self.logged_in()
    }

    async fn mark_read(&self, chat: &ChatTarget, up_to: &MessageId) -> EngineResult<()> {
        self.record(MockCall::MarkRead { chat: chat.clone(), up_to: up_to.clone() })?;
        self.logged_in()
    }

    async fn lookup_handles(&self, handles: Vec<String>) -> EngineResult<Vec<(String, bool)>> {
        self.record(MockCall::LookupHandles(handles.clone()))?;
        self.logged_in()?;
        let g = self.lock();
        handles
            .iter()
            .map(|h| {
                let n = Handle::try_normalize(h).ok_or_else(|| EngineError::InvalidHandle(h.clone()))?;
                let ok = g.reachable.get(&n).copied().unwrap_or(false);
                Ok((n, ok))
            })
            .collect()
    }

    async fn download_attachment(&self, _locator: &str, dest: &Path, max_bytes: u64) -> EngineResult<()> {
        self.record(MockCall::DownloadAttachment { dest: dest.to_path_buf() })?;
        if MockEngine::ATTACHMENT_PAYLOAD.len() as u64 > max_bytes {
            return Err(EngineError::InvalidInput(format!("attachment too large (limit {max_bytes})")));
        }
        let delay = self.lock().download_delay;
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let dest = dest.to_path_buf();
        tokio::task::spawn_blocking(move || {
            crate::state::atomic_write_in_place(&dest, MockEngine::ATTACHMENT_PAYLOAD)
                .map_err(|e| EngineError::InvalidInput(format!("cannot write {}: {e}", dest.display())))
        })
        .await
        .map_err(|e| EngineError::Protocol(e.to_string()))?
    }

    async fn fetch_profile(&self, share: &ShareRef) -> EngineResult<FetchedProfile> {
        self.record(MockCall::FetchProfile { record_key: share.record_key.clone() })?;
        let delay = self.lock().download_delay;
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let g = self.lock();
        if g.profiles_status.state != ProfilesState::Ready {
            return Err(EngineError::Unsupported("shared profiles are not available".into()));
        }
        g.profiles.get(&share.record_key).cloned().ok_or_else(|| EngineError::Protocol("no such profile record".into()))
    }

    async fn profiles_status(&self) -> ProfilesStatus {
        self.lock().profiles_status.clone()
    }

    async fn upsert_chat(&self, chat: &Chat) {
        let _ = self.record(MockCall::UpsertChat(chat.id.clone()));
        self.lock().chats.insert(chat.id.clone(), chat.clone());
    }

    fn events(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }

    async fn shutdown(&self) {
        let _ = self.record(MockCall::Shutdown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use imessage_common::model::TypingEvent;

    #[tokio::test]
    async fn smoke_login_2fa_send_and_events() {
        let e = MockEngine::new();
        let mut rx = e.events();
        assert_eq!(e.connection_state().await, ConnectionState::NeedsLogin);
        assert!(matches!(e.send_text(&ChatTarget::Participants(vec!["a@b.c".into()]), "x", None).await, Err(EngineError::NotLoggedIn)));

        e.set_require_two_factor(true);
        assert_eq!(e.login("Me@Example.com", "hunter2").await.unwrap(), LoginOutcome::TwoFactorRequired);
        assert_eq!(rx.recv().await.unwrap(), EngineEvent::TwoFactorRequired);
        assert!(matches!(e.submit_two_factor("12").await, Err(EngineError::BadTwoFactorCode)));
        assert_eq!(e.submit_two_factor("123456").await.unwrap(), LoginOutcome::Ok);
        assert_eq!(e.connection_state().await, ConnectionState::Connected);
        assert_eq!(e.handles().await, vec![Handle::new("me@example.com")]);

        let target = ChatTarget::Participants(vec!["Bob@x.com".into()]);
        let sent = e.send_text(&target, "hi", None).await.unwrap();
        assert_eq!(sent.chat_id, ChatId::direct("bob@x.com"));
        assert_eq!(sent.sent_at, MOCK_SENT_AT);
        assert_eq!(sent.sender, Handle::new("me@example.com"));
        let sent_id = sent.id.clone();
        e.react(&target, &sent_id, ReactionKind::Love, false).await.unwrap();

        // n5: groups get a minted group:<GUID>, reused for the same set
        let grp = e.send_text(&ChatTarget::Participants(vec!["c@x.com".into(), "Bob@x.com".into()]), "g", None).await.unwrap();
        assert_eq!(grp.chat_id.0, "group:00000000-0000-4000-9000-000000000001");
        let again = e.send_text(&ChatTarget::Participants(vec!["bob@x.com".into(), "c@x.com".into()]), "g2", None).await.unwrap();
        assert_eq!(again.chat_id, grp.chat_id);
        assert_eq!(e.send_text(&ChatTarget::Chat(grp.chat_id.clone()), "g3", None).await.unwrap().chat_id, grp.chat_id);
        assert!(matches!(e.send_text(&ChatTarget::Chat(ChatId::group("unknown")), "x", None).await, Err(EngineError::ChatNotFound(_))));
        // n6: garbage handles are rejected
        assert!(matches!(e.send_text(&ChatTarget::Participants(vec!["bob".into()]), "x", None).await, Err(EngineError::InvalidHandle(_))));
        assert!(matches!(e.lookup_handles(vec!["".into()]).await, Err(EngineError::InvalidHandle(_))));

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.bin");
        e.download_attachment("{\"v\":1,\"plist\":\"secret\"}", &dest, 1 << 20).await.unwrap();
        assert!(e.download_attachment("{}", &dest, 3).await.is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), MockEngine::ATTACHMENT_PAYLOAD);

        e.set_reachable("bob@x.com", true);
        let r = e.lookup_handles(vec!["BOB@x.com".into(), "z@q.r".into()]).await.unwrap();
        assert_eq!(r, vec![("mailto:bob@x.com".into(), true), ("mailto:z@q.r".into(), false)]);

        e.fail_next("boom");
        assert!(matches!(e.set_typing(&target, true).await, Err(EngineError::Protocol(m)) if m == "boom"));

        let ev = EngineEvent::Typing(TypingEvent { chat_id: ChatId::direct("bob@x.com"), sender: Handle::new("bob@x.com"), typing: true });
        e.inject(ev.clone());
        // drain until our injected event
        let mut seen_sent = false;
        loop {
            let got = rx.recv().await.unwrap();
            if matches!(&got, EngineEvent::MessageSent { id, .. } if *id == sent_id) {
                seen_sent = true;
            }
            if got == ev { break; }
        }
        assert!(seen_sent);

        let calls = e.calls();
        assert!(calls.contains(&MockCall::Login { apple_id: "Me@Example.com".into() }));
        assert!(calls.iter().any(|c| matches!(c, MockCall::SendText { text, .. } if text == "hi")));
        // passwords / codes / attachment locators are never recorded
        assert!(!format!("{calls:?}").contains("hunter2") && !format!("{calls:?}").contains("123456"));
        assert!(!format!("{calls:?}").contains("secret"));
    }
}
