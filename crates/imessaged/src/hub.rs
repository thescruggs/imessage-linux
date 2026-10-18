//! The daemon's core: engine + store + signal fan-out.
//!
//! Both the engine-event pump ([`crate::daemon`]) and the D-Bus interface
//! ([`crate::dbus`]) go through a [`Hub`], so every user-visible operation is
//! testable without a bus. Each operation talks only to `dyn Engine` and the
//! [`StoreHandle`]; signals go to an in-process broadcast channel
//! ([`Hub::signals`]) and, when D-Bus is up, to the bus.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use imessage_common::model::{
    Attachment, AttachmentId, Chat, ChatId, ConnectionState, DeliveryState, Handle, HandleProfile, Message, MessageId, MessagePart,
    ProfilesStatus, Reaction, ReactionKind, RegistrationState, Service, SmsForwarding,
};
use serde::Serialize;
use tokio::sync::{broadcast, watch};

use crate::engine::{ChatTarget, Engine, EngineError, LoginOutcome, SentMessage, MAX_ATTACHMENT_BYTES, MAX_BACKGROUND_BYTES};
use crate::store::{MessagePage, StoreError, StoreHandle, StoreUpdate};

/// Typing indicators are cleared after this long without a refresh.
pub const DEFAULT_TYPING_TIMEOUT: Duration = Duration::from_secs(60);
pub const ATTACHMENTS_DIR: &str = "attachments";
/// Downloaded transcript backgrounds: `<chat id>/<background id>`.
pub const BACKGROUNDS_DIR: &str = "backgrounds";
/// Backgrounds one fetch task downloads in a row (the chat's background
/// changed while the previous one was downloading).
pub const BACKGROUND_FETCH_ROUNDS: usize = 3;
/// How long an IDS answer "on iMessage" is trusted when picking iMessage or
/// SMS for a send.
pub const REACHABLE_TTL: Duration = Duration::from_secs(6 * 3600);
/// The same for "not on iMessage" (people get iPhones).
pub const UNREACHABLE_TTL: Duration = Duration::from_secs(30 * 60);

/// Everything the daemon announces (D-Bus signals mirror these 1:1).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum DaemonSignal {
    /// A new message row (incoming, or outgoing from any of our devices).
    MessageReceived { chat_id: ChatId, message: Message },
    /// Delivery / reaction / edit / unsend on an existing message.
    MessageUpdated { chat_id: ChatId, message: Message },
    ChatUpdated(Chat),
    Typing { chat_id: ChatId, handle: Handle, typing: bool },
    ConnectionStateChanged(ConnectionState),
    RegisteredChanged(bool),
    TwoFactorRequired,
    /// Background problem (D-Bus signal `BackgroundError`).
    BackgroundError(String),
    /// A contact's shared name / photo changed (`None`: they stopped
    /// sharing).
    HandleProfileUpdated { handle: String, profile: Option<HandleProfile> },
    /// Text Message Forwarding changed state.
    SmsForwardingChanged(SmsForwarding),
}

/// What an operation failed with (mapped to D-Bus error names in `dbus.rs`).
#[derive(Debug, thiserror::Error)]
pub enum OpError {
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error("store: {0}")]
    Store(StoreError),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

impl From<StoreError> for OpError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound(s) => OpError::NotFound(s),
            StoreError::InvalidInput(s) => OpError::InvalidInput(s),
            other => OpError::Store(other),
        }
    }
}

pub type OpResult<T> = Result<T, OpError>;

/// `GetStatus` payload.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    pub version: String,
    pub connection: ConnectionState,
    pub registered: bool,
    pub registration: RegistrationState,
    /// Total unread incoming messages (`null` if the store is unavailable).
    pub unread: Option<u32>,
    /// Whether contacts' shared names and photos can be fetched.
    pub profiles: ProfilesStatus,
    /// Text Message Forwarding (SMS relayed by the iPhone).
    pub sms: SmsForwarding,
    /// The daemon's `mac_host` setting: the Mac's ssh destination for
    /// `imsg sync-mac` (`null` when unset).
    pub mac_host: Option<String>,
}

/// `LookupHandles` result element.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reachability {
    pub handle: String,
    pub reachable: bool,
}

/// Snake-case state name (`connected`, `needs_login`, `error`, ...).
pub fn connection_state_name(s: &ConnectionState) -> &'static str {
    match s {
        ConnectionState::Disconnected => "disconnected",
        ConnectionState::Connecting => "connecting",
        ConnectionState::Connected => "connected",
        ConnectionState::NeedsLogin => "needs_login",
        ConnectionState::NeedsTwoFactor => "needs_two_factor",
        ConnectionState::Registering => "registering",
        ConnectionState::Error(_) => "error",
    }
}

/// `love | like | dislike | laugh | emphasize | question`, `emoji:<e>`, or a
/// bare emoji.
pub fn parse_reaction_kind(s: &str) -> OpResult<ReactionKind> {
    let t = s.trim();
    Ok(match t.to_ascii_lowercase().as_str() {
        "love" | "heart" => ReactionKind::Love,
        "like" | "thumbsup" => ReactionKind::Like,
        "dislike" | "thumbsdown" => ReactionKind::Dislike,
        "laugh" | "haha" => ReactionKind::Laugh,
        "emphasize" | "!!" => ReactionKind::Emphasize,
        "question" | "?" => ReactionKind::Question,
        _ => {
            let e = t.strip_prefix("emoji:").unwrap_or(t);
            if e.is_empty() || e.is_ascii() || e.chars().count() > 16 {
                return Err(OpError::InvalidInput(format!("unknown reaction {s:?}")));
            }
            ReactionKind::Emoji(e.to_string())
        }
    })
}

/// Longest path component we create, in bytes (NAME_MAX is 255 on Linux
/// file systems; leave room for the engine's temp-file suffix).
const MAX_COMPONENT_BYTES: usize = 200;

/// A single path component safe to create under our attachments dir:
/// no separators, no leading dots, no control characters, at most
/// [`MAX_COMPONENT_BYTES`] bytes (cut on a char boundary).
fn safe_component(s: &str, fallback: &str) -> String {
    let base = s.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base.chars().map(|c| if c.is_control() || c == ':' { '_' } else { c }).collect();
    let mut cleaned = cleaned.trim_start_matches('.').trim();
    if cleaned.len() > MAX_COMPONENT_BYTES {
        let mut end = MAX_COMPONENT_BYTES;
        while !cleaned.is_char_boundary(end) {
            end -= 1;
        }
        cleaned = cleaned[..end].trim_end();
    }
    if cleaned.is_empty() { fallback.to_string() } else { cleaned.to_string() }
}

/// Copy `src` to `dest` atomically (temp file in `dest`'s directory, mode
/// 0600, `fsync`, rename). `src` must be a regular file: it is opened
/// non-blocking and checked, so a FIFO or device cannot stall the caller.
fn copy_private(src: &Path, dest: &Path) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = dest.parent().unwrap_or_else(|| Path::new("."));
    let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "attachment".into());
    let tmp = dir.join(format!(".{name}.tmp-{}-{:08x}", std::process::id(), rand::random::<u32>()));
    let result = (|| {
        let mut input = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(src)?;
        if !input.metadata()?.is_file() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"));
        }
        let mut out = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        std::io::copy(&mut input, &mut out)?;
        out.flush()?;
        out.sync_all()?;
        drop(out);
        std::fs::rename(&tmp, dest)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Membership of `key` in an in-flight set, released on drop (so a task
/// that panics cannot leave its key claimed forever).
struct InFlight<K: std::hash::Hash + Eq> {
    set: Arc<Mutex<HashSet<K>>>,
    key: Option<K>,
}

impl<K: std::hash::Hash + Eq + Clone> InFlight<K> {
    /// `None` if `key` is already in flight.
    fn claim(set: &Arc<Mutex<HashSet<K>>>, key: K) -> Option<Self> {
        set.lock().unwrap_or_else(|p| p.into_inner()).insert(key.clone()).then(|| InFlight { set: set.clone(), key: Some(key) })
    }
}

impl<K: std::hash::Hash + Eq> Drop for InFlight<K> {
    fn drop(&mut self) {
        if let Some(k) = self.key.take() {
            self.set.lock().unwrap_or_else(|p| p.into_inner()).remove(&k);
        }
    }
}

#[derive(Default)]
struct Typing {
    next: AtomicU64,
    active: Mutex<HashMap<(ChatId, String), u64>>,
}

pub struct Hub {
    engine: Arc<dyn Engine>,
    /// Per-attachment download guard: concurrent `DownloadAttachment` calls
    /// for one id wait for the first instead of downloading again.
    downloads: Mutex<HashMap<AttachmentId, Arc<tokio::sync::Mutex<()>>>>,
    /// Chats whose background is being downloaded right now.
    background_fetches: Arc<Mutex<HashSet<ChatId>>>,
    /// Shared-profile and group-photo downloads (`hub/profiles.rs`).
    profile_fetches: profiles::ProfileFetches,
    store: Option<StoreHandle>,
    state_dir: PathBuf,
    signals: broadcast::Sender<DaemonSignal>,
    /// Set while D-Bus is serving (see `dbus.rs`); taken at shutdown to break
    /// the Hub -> interface -> Hub cycle.
    bus: Mutex<Option<crate::dbus::Emitter>>,
    typing: Typing,
    typing_timeout: Duration,
    stop: watch::Sender<bool>,
    /// IDS reachability answers (handle -> (on iMessage, when asked)) for
    /// choosing between iMessage and SMS.
    reachability: Mutex<HashMap<String, (bool, Instant)>>,
    /// `mac_host` from the configuration (reported in `GetStatus`).
    mac_host: Mutex<Option<String>>,
}

impl Hub {
    pub fn new(engine: Arc<dyn Engine>, store: Option<StoreHandle>, state_dir: PathBuf, typing_timeout: Duration) -> Arc<Hub> {
        Arc::new(Hub {
            engine,
            downloads: Mutex::new(HashMap::new()),
            background_fetches: Arc::new(Mutex::new(HashSet::new())),
            profile_fetches: profiles::ProfileFetches::default(),
            store,
            state_dir,
            signals: broadcast::channel(1024).0,
            bus: Mutex::new(None),
            typing: Typing::default(),
            typing_timeout,
            stop: watch::channel(false).0,
            reachability: Mutex::new(HashMap::new()),
            mac_host: Mutex::new(None),
        })
    }

    /// The `mac_host` setting `GetStatus` reports.
    pub fn set_mac_host(&self, host: Option<String>) {
        *self.mac_host.lock().unwrap_or_else(|p| p.into_inner()) = host;
    }

    pub fn engine(&self) -> &Arc<dyn Engine> {
        &self.engine
    }

    pub fn store(&self) -> OpResult<&StoreHandle> {
        self.store.as_ref().ok_or_else(|| OpError::Store(StoreError::Closed))
    }

    pub fn signals(&self) -> broadcast::Receiver<DaemonSignal> {
        self.signals.subscribe()
    }

    pub fn attachments_dir(&self) -> PathBuf {
        self.state_dir.join(ATTACHMENTS_DIR)
    }

    pub fn backgrounds_dir(&self) -> PathBuf {
        self.state_dir.join(BACKGROUNDS_DIR)
    }

    pub(crate) fn set_bus(&self, e: Option<crate::dbus::Emitter>) {
        *self.bus.lock().unwrap_or_else(|p| p.into_inner()) = e;
    }

    pub(crate) fn stop(&self) {
        let _ = self.stop.send(true);
        self.set_bus(None);
    }

    /// Broadcast in-process, then on D-Bus (if serving).
    pub async fn emit(&self, sig: DaemonSignal) {
        let _ = self.signals.send(sig.clone());
        let bus = self.bus.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(bus) = bus {
            bus.emit(&sig).await;
        }
    }

    /// Emit the signals for store updates (and delete files of removed
    /// attachments that live in our attachments directory).
    pub async fn publish(self: &Arc<Self>, updates: Vec<StoreUpdate>) {
        for u in updates {
            match u {
                StoreUpdate::NewMessage(m) => {
                    if !m.is_from_me {
                        // A message implies its sender stopped typing.
                        self.clear_typing(&m.chat_id, &m.sender).await;
                    }
                    self.emit(DaemonSignal::MessageReceived { chat_id: m.chat_id.clone(), message: m }).await;
                }
                StoreUpdate::MessageUpdated(m) => self.emit(DaemonSignal::MessageUpdated { chat_id: m.chat_id.clone(), message: m }).await,
                StoreUpdate::ChatUpdated(c) => self.emit(DaemonSignal::ChatUpdated(c)).await,
                StoreUpdate::FilesRemoved(files) => self.remove_files(files).await,
            }
        }
    }

    /// Only files inside `state_dir/attachments`, `backgrounds`, `profiles`
    /// and `chat_icons` are ours to delete (an outgoing attachment whose
    /// copy could not be made still points at the user's own file).
    async fn remove_files(&self, files: Vec<String>) {
        let roots = [self.attachments_dir(), self.backgrounds_dir(), self.profiles_dir(), self.chat_icons_dir()];
        let _ = tokio::task::spawn_blocking(move || {
            for f in files {
                let p = Path::new(&f);
                let Some(root) = roots.iter().find(|r| p.starts_with(r)) else { continue };
                if p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                    continue;
                }
                if let Err(e) = std::fs::remove_file(p)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    log::warn!("cannot remove {}: {e}", p.display());
                }
                if let Some(dir) = p.parent()
                    && dir != root.as_path()
                {
                    let _ = std::fs::remove_dir(dir); // only if empty
                }
            }
        })
        .await;
    }

    // ------------------------------------------------------------------
    // typing (in memory)
    // ------------------------------------------------------------------

    /// Someone started / stopped typing. A start is cleared automatically
    /// after the typing timeout unless refreshed.
    pub async fn on_typing(self: &Arc<Self>, chat_id: ChatId, handle: Handle, typing: bool) {
        let key = (chat_id.clone(), handle.uri.clone());
        if !typing {
            self.typing.active.lock().unwrap_or_else(|p| p.into_inner()).remove(&key);
            self.emit(DaemonSignal::Typing { chat_id, handle, typing: false }).await;
            return;
        }
        let generation = self.typing.next.fetch_add(1, Ordering::SeqCst) + 1;
        self.typing.active.lock().unwrap_or_else(|p| p.into_inner()).insert(key.clone(), generation);
        self.emit(DaemonSignal::Typing { chat_id: chat_id.clone(), handle: handle.clone(), typing: true }).await;
        let hub = Arc::downgrade(self);
        let timeout = self.typing_timeout;
        let mut stop = self.stop.subscribe();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(timeout) => {}
                _ = stop.wait_for(|s| *s) => return,
            }
            let Some(hub) = hub.upgrade() else { return };
            let expired = {
                let mut g = hub.typing.active.lock().unwrap_or_else(|p| p.into_inner());
                if g.get(&key) == Some(&generation) {
                    g.remove(&key);
                    true
                } else {
                    false
                }
            };
            if expired {
                hub.emit(DaemonSignal::Typing { chat_id, handle, typing: false }).await;
            }
        });
    }

    async fn clear_typing(&self, chat_id: &ChatId, handle: &Handle) {
        let was = self.typing.active.lock().unwrap_or_else(|p| p.into_inner()).remove(&(chat_id.clone(), handle.uri.clone())).is_some();
        if was {
            self.emit(DaemonSignal::Typing { chat_id: chat_id.clone(), handle: handle.clone(), typing: false }).await;
        }
    }

    /// Who is typing right now (for tests / status).
    pub fn typing_now(&self) -> Vec<(ChatId, String)> {
        let mut v: Vec<_> = self.typing.active.lock().unwrap_or_else(|p| p.into_inner()).keys().cloned().collect();
        v.sort();
        v
    }

    // ------------------------------------------------------------------
    // engine seeding
    // ------------------------------------------------------------------

    /// Teach the engine every stored group chat (2a: `ChatTarget::Chat`
    /// for a group only resolves for chats the engine has seen).
    pub async fn seed_engine_chats(&self) {
        let Ok(store) = self.store() else { return };
        match store.call(|s| s.group_chats()).await {
            Ok(chats) => {
                for c in &chats {
                    self.engine.upsert_chat(c).await;
                }
                if !chats.is_empty() {
                    log::info!("seeded the engine with {} stored group chat(s)", chats.len());
                }
            }
            Err(e) => log::warn!("cannot load stored chats: {e}"),
        }
    }

    /// After `ChatNotFound` for a stored chat: upsert it and say whether a
    /// retry makes sense.
    async fn reseed(&self, target: &ChatTarget) -> bool {
        let ChatTarget::Chat(id) = target else { return false };
        let Ok(store) = self.store() else { return false };
        let id2 = id.clone();
        match store.call(move |s| s.get_chat(&id2)).await {
            Ok(Some(c)) => {
                self.engine.upsert_chat(&c).await;
                true
            }
            _ => false,
        }
    }

    // ------------------------------------------------------------------
    // operations (D-Bus methods)
    // ------------------------------------------------------------------

    pub async fn login(&self, apple_id: &str, password: &str) -> OpResult<LoginOutcome> {
        Ok(self.engine.login(apple_id, password).await?)
    }

    pub async fn submit_two_factor(&self, code: &str) -> OpResult<LoginOutcome> {
        Ok(self.engine.submit_two_factor(code).await?)
    }

    pub async fn logout(&self) -> OpResult<()> {
        Ok(self.engine.logout().await?)
    }

    pub async fn status(&self) -> Status {
        let registration = self.engine.registration_state().await;
        let unread = match self.store() {
            Ok(s) => s.call(|s| s.total_unread()).await.ok(),
            Err(_) => None,
        };
        Status {
            version: env!("CARGO_PKG_VERSION").to_string(),
            connection: self.engine.connection_state().await,
            registered: registration.is_registered(),
            registration,
            unread,
            profiles: self.engine.profiles_status().await,
            sms: self.engine.sms_forwarding().await,
            mac_host: self.mac_host.lock().unwrap_or_else(|p| p.into_inner()).clone(),
        }
    }

    /// Ask the iPhone to relay text messages to us (or stop).
    pub async fn enable_sms_forwarding(&self, enable: bool) -> OpResult<SmsForwarding> {
        Ok(self.engine.set_sms_forwarding(enable).await?)
    }

    pub async fn sms_forwarding(&self) -> SmsForwarding {
        self.engine.sms_forwarding().await
    }

    /// Register with IDS again right now (e.g. to pick up a handle the
    /// account gained) and return the new registration.
    pub async fn reregister(&self) -> OpResult<RegistrationState> {
        Ok(self.engine.register().await?)
    }

    /// Our own addresses (registered + `other_handles`), normalized.
    async fn own_handles(&self) -> HashSet<String> {
        let reg = self.engine.registration_state().await;
        let sms = self.engine.sms_forwarding().await;
        reg.handles.iter().map(|h| h.uri.clone()).chain(reg.other_handles.iter().cloned()).chain(sms.number).map(|h| Handle::normalize(&h)).collect()
    }

    /// The phone numbers a send to `target` goes to, if it is a 1:1 chat
    /// with a phone number, a text-message group (`group-p:` of numbers) or
    /// a participant list of numbers only; `None` otherwise (an email
    /// address, an iMessage group with a group id).
    async fn phone_recipients(&self, target: &ChatTarget) -> Option<Vec<String>> {
        let raw: Vec<String> = match target {
            ChatTarget::Participants(p) => p.iter().map(|h| Handle::try_normalize(h)).collect::<Option<Vec<_>>>()?,
            ChatTarget::Chat(id) if id.group_guid().is_some() => return None,
            ChatTarget::Chat(id) => match id.0.strip_prefix(ChatId::GROUP_PARTICIPANTS_PREFIX) {
                Some(list) => list.split(',').map(str::to_string).collect(),
                None => vec![id.0.clone()],
            },
        };
        let ours = self.own_handles().await;
        let mut others: Vec<String> = raw.into_iter().filter(|h| !ours.contains(h)).collect();
        others.dedup();
        (!others.is_empty() && others.iter().all(|h| h.starts_with("tel:"))).then_some(others)
    }

    /// Which of `handles` are on iMessage, from the cache or IDS.
    async fn reachable(&self, handles: &[String]) -> OpResult<HashMap<String, bool>> {
        let now = Instant::now();
        let mut out = HashMap::new();
        let mut ask = vec![];
        {
            let cache = self.reachability.lock().unwrap_or_else(|p| p.into_inner());
            for h in handles {
                match cache.get(h) {
                    Some((ok, at)) if now.duration_since(*at) < if *ok { REACHABLE_TTL } else { UNREACHABLE_TTL } => {
                        out.insert(h.clone(), *ok);
                    }
                    _ => ask.push(h.clone()),
                }
            }
        }
        if !ask.is_empty() {
            let answers = self.engine.lookup_handles(ask).await?;
            let mut cache = self.reachability.lock().unwrap_or_else(|p| p.into_inner());
            for (h, ok) in answers {
                cache.insert(h.clone(), (ok, now));
                out.insert(h, ok);
            }
        }
        Ok(out)
    }

    /// iMessage or SMS for a send. An explicit `hint` wins. Otherwise SMS
    /// when Text Message Forwarding is active and the recipients are phone
    /// numbers of which at least one is not on iMessage (Messages.app's
    /// rule); iMessage when anything is uncertain (lookup failed).
    pub async fn choose_service(&self, target: &ChatTarget, hint: Option<Service>) -> Service {
        if let Some(s) = hint {
            return s;
        }
        if !self.engine.sms_forwarding().await.is_active() {
            return Service::Imessage;
        }
        let Some(numbers) = self.phone_recipients(target).await else { return Service::Imessage };
        match self.reachable(&numbers).await {
            Ok(r) if numbers.iter().any(|n| r.get(n) == Some(&false)) => Service::Sms,
            Ok(_) => Service::Imessage,
            Err(e) => {
                log::warn!("cannot tell whether the recipients are on iMessage ({e}); sending as iMessage");
                Service::Imessage
            }
        }
    }

    /// Text messages have no tapbacks, edits, unsend or typing indicators.
    async fn refuse_sms(&self, id: &MessageId, what: &str) -> OpResult<()> {
        match self.stored_message(id).await? {
            Some(m) if m.service == Service::Sms => Err(EngineError::Unsupported(format!("{what} is not available for text messages (SMS)")).into()),
            _ => Ok(()),
        }
    }

    pub async fn list_chats(&self, limit: u32, offset: u32) -> OpResult<Vec<Chat>> {
        Ok(self.store()?.call(move |s| s.list_chats(limit, offset)).await?)
    }

    /// Pin or unpin a chat (local only). `order`: position among the pins
    /// (0 = first), `None` = last. Emits `ChatUpdated` for every chat whose
    /// pin changed and returns the chat.
    pub async fn set_chat_pinned(self: &Arc<Self>, chat_id: &str, pinned: bool, order: Option<u32>) -> OpResult<Chat> {
        let id = ChatId(chat_id.to_string());
        let k = id.clone();
        let updates = self.store()?.call(move |s| s.set_chat_pinned(&k, pinned, order)).await?;
        self.publish(updates).await;
        let k = id.clone();
        self.store()?.call(move |s| s.get_chat(&k)).await?.ok_or_else(|| OpError::NotFound(format!("chat {id}")))
    }

    /// Put `ids` first among the pins, in this order (pinning them); the
    /// other pins follow. Returns all pinned chats in order.
    pub async fn reorder_pins(self: &Arc<Self>, ids: Vec<String>) -> OpResult<Vec<Chat>> {
        let ids: Vec<ChatId> = ids.into_iter().map(ChatId).collect();
        let updates = self.store()?.call(move |s| s.reorder_pins(&ids)).await?;
        self.publish(updates).await;
        Ok(self.store()?.call(|s| s.pinned_chats()).await?)
    }

    /// A chat whose background has not been downloaded (or whose file went
    /// missing) gets another download attempt in the background; a
    /// `ChatUpdated` follows when it lands. The same goes for the group
    /// photo, and participants' shared profiles older than a day (or never
    /// fetched) are fetched again (`HandleProfileUpdated` when they land).
    pub async fn get_chat(self: &Arc<Self>, id: &str) -> OpResult<Chat> {
        let id = ChatId(id.to_string());
        let k = id.clone();
        let chat = self.store()?.call(move |s| s.get_chat(&k)).await?.ok_or_else(|| OpError::NotFound(format!("chat {id}")))?;
        if let Some(bg) = &chat.background {
            let present = match &bg.payload {
                Some(p) => tokio::fs::metadata(p).await.is_ok_and(|m| m.is_file()),
                None => false,
            };
            if !present {
                self.spawn_background_fetch(chat.id.clone());
            }
        }
        self.refresh_chat_extras(&chat).await;
        Ok(chat)
    }

    /// Make the paintable image for a background payload: `<payload>.<ext>`
    /// next to it (0600), or `None` if the payload holds no picture.
    async fn background_image(payload: PathBuf) -> Option<String> {
        tokio::task::spawn_blocking(move || {
            let len = std::fs::metadata(&payload).map_err(|e| log::warn!("{}: {e}", payload.display())).ok()?.len();
            if len > MAX_BACKGROUND_BYTES {
                log::warn!("{}: background payload of {len} bytes is over the limit; not reading it", payload.display());
                return None;
            }
            let bytes = std::fs::read(&payload).map_err(|e| log::warn!("{}: {e}", payload.display())).ok()?;
            let Some(img) = crate::background::extract_image(&bytes) else {
                log::info!("{}: no picture in this background payload ({} bytes)", payload.display(), bytes.len());
                return None;
            };
            let mut name = payload.file_name()?.to_os_string();
            name.push(format!(".{}", img.ext));
            let dest = payload.with_file_name(name);
            // A plain image is its own picture: link it rather than write
            // the same bytes again (each name is still deleted separately).
            let linked = img.data.len() == bytes.len() && {
                let _ = std::fs::remove_file(&dest);
                std::fs::hard_link(&payload, &dest).is_ok()
            };
            if !linked {
                crate::state::atomic_write_in_place(&dest, &img.data).map_err(|e| log::warn!("{}: {e}", dest.display())).ok()?;
            }
            Some(dest.to_string_lossy().into_owned())
        })
        .await
        .ok()
        .flatten()
    }

    /// Set a chat's background locally from a file the daemon can read
    /// (e.g. pulled from a Mac's Messages data), without telling anyone.
    /// The file is copied into `state_dir/backgrounds/<chat>/`. A later
    /// background change from iMessage replaces it.
    pub async fn import_chat_background(self: &Arc<Self>, chat_id: &str, path: &str) -> OpResult<Chat> {
        let id = ChatId(chat_id.to_string());
        let k = id.clone();
        self.store()?.call(move |s| s.get_chat(&k)).await?.ok_or_else(|| OpError::NotFound(format!("chat {id}")))?;
        let src = PathBuf::from(path);
        let meta = tokio::fs::metadata(&src).await.ok().filter(|m| m.is_file());
        let Some(meta) = meta.filter(|_| src.is_absolute()) else {
            return Err(OpError::InvalidInput(format!("{path}: not an absolute path to a readable file")));
        };
        if meta.len() > MAX_BACKGROUND_BYTES {
            return Err(OpError::InvalidInput(format!("{path}: larger than {MAX_BACKGROUND_BYTES} bytes")));
        }
        let bid = format!("local-{:016x}", rand::random::<u64>());
        let root = self.backgrounds_dir();
        let dir = root.join(safe_component(&id.0, "chat"));
        let dest = dir.join(&bid);
        let (d, s2, d2) = (dir.clone(), src.clone(), dest.clone());
        tokio::task::spawn_blocking(move || {
            crate::state::ensure_private_dir(&root)?;
            crate::state::ensure_private_dir(&d)?;
            copy_private(&s2, &d2)
        })
        .await
        .map_err(|e| OpError::Store(StoreError::Task(e.to_string())))?
        .map_err(|source| OpError::Store(StoreError::Io { path: dest.clone(), source }))?;
        let p = dest.to_string_lossy().into_owned();
        let image = Self::background_image(dest.clone()).await;
        let k = id.clone();
        let mut ours = vec![p.clone()];
        ours.extend(image.clone());
        // Dated now: a late redelivery of an older iMessage change (a
        // removal, say) does not undo the import; a newer one still wins.
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        let store = self.store()?;
        let updates = match store.call(move |s| s.import_chat_background(&k, &bid, 1, &p, image.as_deref(), now)).await {
            Ok(u) => u,
            Err(e) => {
                self.remove_files(ours).await;
                return Err(e.into());
            }
        };
        let chat = updates.iter().find_map(|u| if let StoreUpdate::ChatUpdated(c) = u { Some(c.clone()) } else { None });
        self.publish(updates).await;
        chat.ok_or_else(|| OpError::NotFound(format!("chat {id}")))
    }

    /// Download the chat's current background (if any, and not already
    /// in flight) without blocking the caller. Failures are logged. A
    /// background that replaces the one being downloaded is fetched next
    /// (its own fetch request found this one in flight), up to
    /// [`BACKGROUND_FETCH_ROUNDS`] in a row.
    pub fn spawn_background_fetch(self: &Arc<Self>, chat_id: ChatId) {
        let Some(in_flight) = InFlight::claim(&self.background_fetches, chat_id.clone()) else { return };
        let hub = self.clone();
        tokio::spawn(async move {
            // Released on every exit, a panic included.
            let _in_flight = in_flight;
            let mut attempted: Option<String> = None;
            for round in 0..BACKGROUND_FETCH_ROUNDS {
                let k = chat_id.clone();
                let dl = match hub.store() {
                    Ok(s) => s.call(move |s| s.chat_background_locator(&k)).await,
                    Err(e) => return log::warn!("chat {chat_id}: background download: {e}"),
                };
                let dl = match dl {
                    Ok(Some(dl)) => dl,
                    Ok(None) => return,
                    Err(e) => return log::warn!("chat {chat_id}: background download: {e}"),
                };
                // After the first round only a different, not yet
                // downloaded background is worth another go.
                if round > 0 && (dl.local_path.is_some() || attempted.as_deref() == Some(dl.background_id.as_str())) {
                    return;
                }
                attempted = Some(dl.background_id.clone());
                match hub.fetch_background(&chat_id, dl).await {
                    Ok(Some(chat)) => hub.emit(DaemonSignal::ChatUpdated(chat)).await,
                    Ok(None) => {}
                    Err(e) => log::warn!("chat {chat_id}: background download failed: {e}"),
                }
            }
        });
    }

    /// Download background `dl` of the chat to
    /// `state_dir/backgrounds/<chat id>/<background id>` (0600, dirs 0700).
    /// `Ok(None)`: nothing to do (too large, or it changed meanwhile).
    async fn fetch_background(&self, chat_id: &ChatId, dl: crate::store::BackgroundDownload) -> OpResult<Option<Chat>> {
        if dl.size > MAX_BACKGROUND_BYTES {
            log::warn!("chat {chat_id}: background {} announces {} bytes (limit {MAX_BACKGROUND_BYTES}); not downloading", dl.background_id, dl.size);
            return Ok(None);
        }
        let (bid, locator) = (dl.background_id, dl.locator);
        let root = self.backgrounds_dir();
        let dir = root.join(safe_component(&chat_id.0, "chat"));
        let dest = dir.join(safe_component(&bid, "background"));
        let d = dir.clone();
        tokio::task::spawn_blocking(move || crate::state::ensure_private_dir(&root).and_then(|_| crate::state::ensure_private_dir(&d)))
            .await
            .map_err(|e| OpError::Store(StoreError::Task(e.to_string())))?
            .map_err(|source| OpError::Store(StoreError::Io { path: dir.clone(), source }))?;
        self.engine.download_attachment(&locator, &dest, MAX_BACKGROUND_BYTES).await?;
        let path = dest.to_string_lossy().into_owned();
        log::info!("chat {chat_id}: background {bid} downloaded ({} bytes)", tokio::fs::metadata(&dest).await.map(|m| m.len()).unwrap_or(0));
        let image = Self::background_image(dest.clone()).await;
        let k = chat_id.clone();
        let (p, i) = (path.clone(), image.clone());
        let updated = self.store()?.call(move |s| s.set_background_files(&k, &bid, Some(&p), i.as_deref())).await?;
        if updated.is_none() {
            // Replaced while we downloaded: these files are nobody's.
            let _ = tokio::fs::remove_file(&dest).await;
            if let Some(i) = image {
                let _ = tokio::fs::remove_file(i).await;
            }
        }
        Ok(updated)
    }

    pub async fn list_messages(&self, chat_id: &str, before: &str, limit: u32) -> OpResult<MessagePage> {
        let (chat, before) = (ChatId(chat_id.to_string()), before.to_string());
        Ok(self.store()?.call(move |s| s.list_messages(&chat, Some(&before), limit)).await?)
    }

    pub async fn search(&self, query: &str, limit: u32) -> OpResult<Vec<Message>> {
        let q = query.to_string();
        Ok(self.store()?.call(move |s| s.search(&q, limit)).await?)
    }

    /// `chat_id` wins when non-empty; otherwise `participants` (1 = 1:1 chat,
    /// more = group).
    pub fn target(chat_id: &str, participants: &[String]) -> OpResult<ChatTarget> {
        if !chat_id.trim().is_empty() {
            Ok(ChatTarget::Chat(ChatId(chat_id.trim().to_string())))
        } else if !participants.is_empty() {
            Ok(ChatTarget::Participants(participants.to_vec()))
        } else {
            Err(OpError::InvalidInput("either a chat id or participants are required".into()))
        }
    }

    /// Store a message we just sent (and its chat), then announce it.
    async fn record_outgoing(self: &Arc<Self>, target: &ChatTarget, msg: Message, attachments: Vec<Attachment>) -> OpResult<()> {
        let participants: Vec<Handle> = match target {
            ChatTarget::Participants(p) => {
                let mut v: Vec<Handle> = vec![];
                for h in p {
                    let h = Handle::new(h);
                    if !v.contains(&h) {
                        v.push(h);
                    }
                }
                v
            }
            ChatTarget::Chat(id) if !id.is_group() => vec![Handle::new(&id.0)],
            ChatTarget::Chat(id) => match id.0.strip_prefix(ChatId::GROUP_PARTICIPANTS_PREFIX) {
                Some(list) => list.split(',').map(Handle::new).collect(),
                None => vec![],
            },
        };
        // A text message's chat may differ from what the target names (our
        // own numbers dropped, a group of numbers keyed by them): store the
        // chat the engine sent to with the people in it.
        let participants = if msg.chat_id.0.starts_with(ChatId::GROUP_PARTICIPANTS_PREFIX) {
            msg.chat_id.0[ChatId::GROUP_PARTICIPANTS_PREFIX.len()..].split(',').map(Handle::new).collect()
        } else if !msg.chat_id.is_group() && msg.service == Service::Sms {
            vec![Handle::new(&msg.chat_id.0)]
        } else {
            participants
        };
        let chat = Chat {
            id: msg.chat_id.clone(),
            participants,
            display_name: None,
            is_group: msg.chat_id.is_group(),
            last_message_at: Some(msg.sent_at),
            unread_count: 0,
            background: None,
            icon_path: None,
            pinned: false,
            pin_order: None,
        };
        let (stored, chat) = self
            .store()?
            .call(move |s| {
                s.upsert_chat(&chat)?;
                let (m, _new) = s.insert_message(&msg, &attachments)?;
                let chat = s.get_chat(&m.chat_id)?;
                Ok((m, chat))
            })
            .await?;
        let mut updates = vec![StoreUpdate::NewMessage(stored)];
        updates.extend(chat.map(StoreUpdate::ChatUpdated));
        self.publish(updates).await;
        Ok(())
    }

    /// The row for a message we just sent; the sender is the handle the
    /// engine actually sent from.
    fn outgoing(&self, sent: &SentMessage, parts: Vec<MessagePart>, reply_to: Option<MessageId>) -> Message {
        Message {
            id: sent.id.clone(),
            chat_id: sent.chat_id.clone(),
            sender: sent.sender.clone(),
            sent_at: sent.sent_at,
            text: crate::store::text_of(&parts),
            parts,
            subject: None,
            reply_to,
            is_from_me: true,
            delivery: DeliveryState::Sent,
            edited: false,
            unsent: false,
            reactions: vec![],
            effect: None,
            verified: true,
            service: sent.service,
        }
    }

    pub async fn send_text(self: &Arc<Self>, target: ChatTarget, text: &str, reply_to: Option<MessageId>) -> OpResult<SentMessage> {
        self.send_text_as(target, text, reply_to, None).await
    }

    /// `service`: `None` = pick ([`Hub::choose_service`]).
    pub async fn send_text_as(self: &Arc<Self>, target: ChatTarget, text: &str, reply_to: Option<MessageId>, service: Option<Service>) -> OpResult<SentMessage> {
        if text.is_empty() {
            return Err(OpError::InvalidInput("empty message".into()));
        }
        let service = self.choose_service(&target, service).await;
        // Text messages carry no reply reference.
        let reply_to = reply_to.filter(|_| service == Service::Imessage);
        let r = self.engine.send_text_via(&target, text, reply_to.as_ref(), service).await;
        let sent = match r {
            Err(EngineError::ChatNotFound(_)) if self.reseed(&target).await => self.engine.send_text_via(&target, text, reply_to.as_ref(), service).await?,
            r => r?,
        };
        let msg = self.outgoing(&sent, vec![MessagePart::Text { text: text.to_string() }], reply_to);
        self.record_outgoing(&target, msg, vec![]).await?;
        Ok(sent)
    }

    pub async fn send_file(self: &Arc<Self>, target: ChatTarget, path: &str, mime: &str) -> OpResult<SentMessage> {
        self.send_file_as(target, path, mime, None).await
    }

    /// `service`: `None` = pick ([`Hub::choose_service`]); `Sms` = MMS.
    pub async fn send_file_as(self: &Arc<Self>, target: ChatTarget, path: &str, mime: &str, service: Option<Service>) -> OpResult<SentMessage> {
        let p = PathBuf::from(path);
        if !p.is_absolute() {
            return Err(OpError::InvalidInput(format!("{path:?} is not an absolute path")));
        }
        let meta = tokio::fs::metadata(&p).await.map_err(|e| OpError::InvalidInput(format!("{path}: {e}")))?;
        if !meta.is_file() {
            return Err(OpError::InvalidInput(format!("{path} is not a regular file")));
        }
        let mime = if mime.trim().is_empty() { "application/octet-stream" } else { mime.trim() };
        let service = self.choose_service(&target, service).await;
        let r = self.engine.send_attachment_via(&target, &p, mime, service).await;
        let sent = match r {
            Err(EngineError::ChatNotFound(_)) if self.reseed(&target).await => self.engine.send_attachment_via(&target, &p, mime, service).await?,
            r => r?,
        };
        let filename = safe_component(&p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), "attachment");
        let id = AttachmentId::for_part(&sent.id, 0);
        // Keep our own copy: the caller may delete or move the original, and
        // an outgoing attachment has no download locator to fall back on.
        let local_path = match self.keep_copy(&id, &filename, &p).await {
            Ok(copy) => copy.to_string_lossy().into_owned(),
            Err(e) => {
                log::warn!("sent attachment {id}: cannot keep a copy ({e}); referring to the original file");
                path.to_string()
            }
        };
        let att = Attachment {
            id,
            message_id: sent.id.clone(),
            mime: mime.to_string(),
            filename: filename.clone(),
            size: meta.len(),
            local_path: Some(local_path),
            mmcs: String::new(),
        };
        let part = MessagePart::Attachment { attachment_id: att.id.clone(), mime: att.mime.clone(), filename, size: att.size };
        let msg = self.outgoing(&sent, vec![part], None);
        self.record_outgoing(&target, msg, vec![att]).await?;
        Ok(sent)
    }

    /// Copy `src` to `state_dir/attachments/<id>/<filename>` (file 0600,
    /// dirs 0700; same layout as downloads) and return the copy's path.
    async fn keep_copy(&self, id: &AttachmentId, filename: &str, src: &Path) -> std::io::Result<PathBuf> {
        let dir = self.attachments_dir().join(safe_component(&id.0, "attachment"));
        let dest = dir.join(safe_component(filename, "attachment"));
        let src = src.to_path_buf();
        tokio::task::spawn_blocking(move || {
            crate::state::ensure_private_dir(&dir)?;
            copy_private(&src, &dest)?;
            Ok(dest)
        })
        .await
        .map_err(std::io::Error::other)?
    }

    async fn stored_message(&self, id: &MessageId) -> OpResult<Option<Message>> {
        let k = id.clone();
        Ok(self.store()?.call(move |s| s.get_message(&k)).await?)
    }

    /// Own messages are always stored before `SendText` returns, so an unknown
    /// id is never ours; refusing it also keeps the store's `sender: None`
    /// (local, pre-authorised) edit/unsend path from ever being held for a row
    /// that later turns out to be a peer's message.
    async fn require_own(&self, id: &MessageId, what: &str) -> OpResult<()> {
        match self.stored_message(id).await? {
            Some(m) if m.is_from_me => Ok(()),
            Some(_) => Err(OpError::InvalidInput(format!("can only {what} your own messages"))),
            None => Err(OpError::NotFound(format!("message {} not found", id.as_str()))),
        }
    }

    pub async fn react(self: &Arc<Self>, chat_id: &str, message_id: &str, kind: &str, remove: bool) -> OpResult<()> {
        let kind = parse_reaction_kind(kind)?;
        let target = Self::target(chat_id, &[])?;
        let mid = MessageId::new(message_id);
        self.refuse_sms(&mid, "a tapback").await?;
        let r = self.engine.react(&target, &mid, kind.clone(), remove).await;
        let sent = match r {
            Err(EngineError::ChatNotFound(_)) if self.reseed(&target).await => self.engine.react(&target, &mid, kind.clone(), remove).await?,
            r => r?,
        };
        let reaction = Reaction { kind, sender: sent.sender.clone(), added: !remove };
        let at = sent.sent_at;
        let updated = self.store()?.call(move |s| s.apply_reaction(&mid, &reaction, at)).await?;
        self.publish(updated.map(StoreUpdate::MessageUpdated).into_iter().collect()).await;
        Ok(())
    }

    pub async fn edit(self: &Arc<Self>, chat_id: &str, message_id: &str, text: &str) -> OpResult<()> {
        if text.is_empty() {
            return Err(OpError::InvalidInput("empty message".into()));
        }
        let target = Self::target(chat_id, &[])?;
        let mid = MessageId::new(message_id);
        self.require_own(&mid, "edit").await?;
        self.refuse_sms(&mid, "editing").await?;
        let r = self.engine.edit(&target, &mid, text).await;
        let sent = match r {
            Err(EngineError::ChatNotFound(_)) if self.reseed(&target).await => self.engine.edit(&target, &mid, text).await?,
            r => r?,
        };
        let parts = vec![MessagePart::Text { text: text.to_string() }];
        let at = sent.sent_at;
        let updated = self.store()?.call(move |s| s.apply_edit(&mid, 0, &parts, None, true, at)).await?;
        self.publish(updated.map(StoreUpdate::MessageUpdated).into_iter().collect()).await;
        Ok(())
    }

    pub async fn unsend(self: &Arc<Self>, chat_id: &str, message_id: &str) -> OpResult<()> {
        let target = Self::target(chat_id, &[])?;
        let mid = MessageId::new(message_id);
        self.require_own(&mid, "unsend").await?;
        self.refuse_sms(&mid, "unsend").await?;
        let r = self.engine.unsend(&target, &mid).await;
        let sent = match r {
            Err(EngineError::ChatNotFound(_)) if self.reseed(&target).await => self.engine.unsend(&target, &mid).await?,
            r => r?,
        };
        let at = sent.sent_at;
        let updated = self.store()?.call(move |s| s.unsend_updates(&mid, 0, None, true, at)).await?;
        self.publish(updated).await;
        Ok(())
    }

    pub async fn set_typing(&self, chat_id: &str, typing: bool) -> OpResult<()> {
        let target = Self::target(chat_id, &[])?;
        if self.chat_is_sms(&ChatId(chat_id.trim().to_string())).await {
            return Err(EngineError::Unsupported("typing indicators are not available for text messages (SMS)".into()).into());
        }
        match self.engine.set_typing(&target, typing).await {
            Err(EngineError::ChatNotFound(_)) if self.reseed(&target).await => Ok(self.engine.set_typing(&target, typing).await?),
            r => Ok(r?),
        }
    }

    /// Local read marker first (always), then the read receipt to the
    /// sender (best effort: a failure is logged, not returned, because the
    /// local state already changed). Empty `up_to` = everything; the receipt
    /// then goes for the newest incoming message. Never for a message from
    /// one of our own handles, `other_handles` included (rows stored before
    /// an address was added there still say `is_from_me = false`).
    pub async fn mark_read(self: &Arc<Self>, chat_id: &str, up_to: &str) -> OpResult<()> {
        let chat = ChatId(chat_id.to_string());
        let up_to = (!up_to.trim().is_empty()).then(|| MessageId::new(up_to.trim()));
        let (c, u) = (chat.clone(), up_to.clone());
        let reg = self.engine.registration_state().await;
        let ours: HashSet<String> = reg.handles.iter().map(|h| h.uri.clone()).chain(reg.other_handles.iter().cloned()).map(|h| Handle::normalize(&h)).collect();
        let from_others = move |m: &Message| !m.is_from_me && !ours.contains(&Handle::normalize(&m.sender.uri));
        let (updated, receipt_for) = self
            .store()?
            .call(move |s| {
                let updated = s.mark_read(&c, u.as_ref())?;
                let receipt_for = match u {
                    Some(u) => match s.get_message(&u)? {
                        // ours, or a text message (no read receipts for SMS)
                        Some(m) if !from_others(&m) || m.service == Service::Sms => None,
                        _ => Some(u),
                    },
                    None => s.list_messages(&c, None, crate::store::DEFAULT_PAGE)?.messages.into_iter().rev().find(|m| from_others(m)).filter(|m| m.service == Service::Imessage).map(|m| m.id),
                };
                Ok((updated, receipt_for))
            })
            .await?;
        self.publish(vec![StoreUpdate::ChatUpdated(updated)]).await;
        if let Some(id) = receipt_for {
            let target = ChatTarget::Chat(chat);
            let r = match self.engine.mark_read(&target, &id).await {
                Err(EngineError::ChatNotFound(_)) if self.reseed(&target).await => self.engine.mark_read(&target, &id).await,
                r => r,
            };
            if let Err(e) = r {
                log::warn!("read receipt for {id} not sent: {e}");
            }
        }
        Ok(())
    }

    pub async fn lookup_handles(&self, handles: Vec<String>) -> OpResult<Vec<Reachability>> {
        let answers = self.engine.lookup_handles(handles).await?;
        let now = Instant::now();
        let mut cache = self.reachability.lock().unwrap_or_else(|p| p.into_inner());
        for (h, ok) in &answers {
            cache.insert(h.clone(), (*ok, now));
        }
        Ok(answers.into_iter().map(|(handle, reachable)| Reachability { handle, reachable }).collect())
    }

    /// Whether the chat's newest message is a text message (then it is an
    /// SMS conversation for now: no typing indicators, a green composer).
    async fn chat_is_sms(&self, chat: &ChatId) -> bool {
        if chat.group_guid().is_some() {
            return false;
        }
        let Ok(store) = self.store() else { return false };
        let c = chat.clone();
        store.call(move |s| s.chat_service(&c)).await.ok().flatten() == Some(Service::Sms)
    }

    /// Path of the downloaded file, downloading it to
    /// `state_dir/attachments/<id>/<filename>` (0600, dir 0700) if needed.
    /// Concurrent calls for the same id download once (the others wait and
    /// then find the file).
    pub async fn download_attachment(&self, id: &str) -> OpResult<String> {
        let aid = AttachmentId(id.to_string());
        let guard = self.downloads.lock().unwrap_or_else(|p| p.into_inner()).entry(aid.clone()).or_default().clone();
        let r = {
            let _held = guard.lock().await;
            self.download_attachment_locked(&aid).await
        };
        // Forget the guard once nobody else holds or waits on it.
        let mut map = self.downloads.lock().unwrap_or_else(|p| p.into_inner());
        drop(guard);
        if map.get(&aid).is_some_and(|g| Arc::strong_count(g) == 1) {
            map.remove(&aid);
        }
        r
    }

    async fn download_attachment_locked(&self, aid: &AttachmentId) -> OpResult<String> {
        let id = aid.0.as_str();
        let aid = aid.clone();
        let k = aid.clone();
        let att = self.store()?.call(move |s| s.get_attachment(&k)).await?.ok_or_else(|| OpError::NotFound(format!("attachment {id}")))?;
        if let Some(p) = &att.local_path
            && tokio::fs::metadata(p).await.is_ok_and(|m| m.is_file())
        {
            return Ok(p.clone());
        }
        if att.mmcs.is_empty() {
            return Err(OpError::NotFound(format!("attachment {id} has no download locator")));
        }
        let dir = self.attachments_dir().join(safe_component(&att.id.0, "attachment"));
        let dest = dir.join(safe_component(&att.filename, "attachment"));
        let d = dir.clone();
        tokio::task::spawn_blocking(move || crate::state::ensure_private_dir(&d))
            .await
            .map_err(|e| OpError::Store(StoreError::Task(e.to_string())))?
            .map_err(|source| OpError::Store(StoreError::Io { path: dir.clone(), source }))?;
        self.engine.download_attachment(&att.mmcs, &dest, MAX_ATTACHMENT_BYTES).await?;
        let path = dest.to_string_lossy().into_owned();
        let p = path.clone();
        self.store()?.call(move |s| s.set_attachment_path(&aid, Some(&p))).await?;
        Ok(path)
    }
}

mod import;
mod profiles;
pub use import::{MAX_IMPORT_MESSAGES, MAX_IMPORT_REACTIONS};
pub use profiles::{CHAT_ICONS_DIR, PROFILE_FETCH_CONCURRENCY, PROFILE_MAX_AGE, PROFILES_DIR, image_ext};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reaction_kinds() {
        assert_eq!(parse_reaction_kind("Love").unwrap(), ReactionKind::Love);
        assert_eq!(parse_reaction_kind("question").unwrap(), ReactionKind::Question);
        assert_eq!(parse_reaction_kind("emoji:🔥").unwrap(), ReactionKind::Emoji("🔥".into()));
        assert_eq!(parse_reaction_kind("🎉").unwrap(), ReactionKind::Emoji("🎉".into()));
        assert!(parse_reaction_kind("bogus").is_err());
        assert!(parse_reaction_kind("").is_err());
    }

    #[test]
    fn safe_components() {
        assert_eq!(safe_component("../../etc/passwd", "x"), "passwd");
        assert_eq!(safe_component(".hidden", "x"), "hidden");
        assert_eq!(safe_component("..", "x"), "x");
        assert_eq!(safe_component("", "x"), "x");
        assert_eq!(safe_component("A1:0", "x"), "A1_0");
        assert_eq!(safe_component("a\nb.png", "x"), "a_b.png");
        // byte-bounded, cut on a char boundary
        let long = "é".repeat(300) + ".png";
        let c = safe_component(&long, "x");
        assert!(c.len() <= MAX_COMPONENT_BYTES, "{} bytes", c.len());
        assert!(c.chars().all(|ch| ch == 'é'));
        assert_eq!(safe_component(&"a".repeat(1000), "x").len(), MAX_COMPONENT_BYTES);
    }

    /// B1: a panicking background fetch does not leave its chat claimed.
    #[tokio::test]
    async fn in_flight_released_on_panic() {
        let set: Arc<Mutex<HashSet<ChatId>>> = Arc::default();
        let id = ChatId::direct("mailto:bob@example.com");
        let g = InFlight::claim(&set, id.clone()).unwrap();
        assert!(InFlight::claim(&set, id.clone()).is_none(), "already in flight");
        let r = tokio::spawn(async move {
            let _g = g;
            panic!("fetch blew up");
        })
        .await;
        assert!(r.unwrap_err().is_panic());
        assert!(set.lock().unwrap().is_empty());
        let g = InFlight::claim(&set, id).unwrap();
        drop(g);
        assert!(set.lock().unwrap().is_empty());
    }

    /// S4: copying never blocks on (or copies) a FIFO.
    #[test]
    fn copy_private_refuses_non_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let e = copy_private(&fifo, &dir.path().join("out")).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
        assert!(copy_private(dir.path(), &dir.path().join("out")).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no temp file left behind");
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        copy_private(&dir.path().join("f"), &dir.path().join("out")).unwrap();
        assert_eq!(std::fs::read(dir.path().join("out")).unwrap(), b"x");
    }

    #[test]
    fn state_names() {
        assert_eq!(connection_state_name(&ConnectionState::Error("x".into())), "error");
        assert_eq!(connection_state_name(&ConnectionState::NeedsTwoFactor), "needs_two_factor");
    }
}
