//! [`Engine`] backed by OpenBubbles rustpush.
//!
//! Lifecycle (function names are rustpush's):
//!
//! 1. **Startup** ([`RustpushEngine::start`]): decrypt `state.enc`, install the
//!    process-global software keystore (`keystore::init_keystore`) with the
//!    saved key material, load `hwconfig.plist` as `rustpush::macos::MacOSConfig`.
//!    With saved IDS users: `APSConnectionResource::new(config, Some(push))`
//!    then `IMClient::new(...)` and the receive loop. Without: `NeedsLogin`.
//! 2. **Login** ([`Engine::login`]): `AppleAccount::new_with_anisette` (the
//!    anisette source from [`super::anisette`]: `anisette_command`, else the
//!    remote v3 server at `anisette_url`) + `login_email_pass(user, sha256(pw))`.
//!    `NeedsDevice2FA` -> `send_2fa_to_devices`; `NeedsSMS2FA` ->
//!    `get_auth_extras` + `send_sms_2fa_to_devices`; then the session is
//!    parked and `TwoFactorRequired` returned. [`Engine::submit_two_factor`]
//!    calls `verify_2fa` / `verify_sms_2fa`.
//! 3. **Registration**: `login_apple_delegates(.., [LoginDelegate::IDS])` ->
//!    `authenticate_apple` -> `register(config, aps_state, [MADRID], users,
//!    identity)` -> `IMClient::new`.
//! 4. **Steady state**: rustpush's `ResourceManager`s do the work. The APS
//!    connection reconnects itself (exponential backoff capped at 30 s, 60 s
//!    keep-alive pings). The IDS identity re-registers itself on schedule
//!    (`calculate_rereg_time_s`: heartbeat interval, or 5 min before the
//!    cert expires), on IDS push command 32 ("re-register") / 66 (handles
//!    changed), and on lookup error 6005 — and after each success fires the
//!    `keys_updated` callback, which we persist and surface as
//!    `RegistrationChanged`. If registration fails with 6005 rustpush gives
//!    up for good (`DoNotRetry`); we then attempt a silent GSA re-login with
//!    the saved `PersistAccountData` (retrying network / anisette failures
//!    after 10 s, 60 s, 5 min and then every 15 min until they work) and
//!    rebuild the client, or fall back to `NeedsLogin`/`TwoFactorRequired`
//!    when Apple rejects the credentials or wants a code.
//! 5. **Supervision**: the receive loop and the APS/IDS state monitors run
//!    under [`supervise::supervise`]: a panic inside rustpush's handle path
//!    is logged, emitted as `EngineEvent::Error`, the connection shows
//!    `Connecting` while degraded, and the task is respawned (1 s .. 30 s).
//!
//! Only one `RustpushEngine` may exist per process: rustpush's keystore is
//! a process global.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io::Cursor;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use icloud_auth::{AppleAccount, LoginState, PersistAccountData, VerifyBody};
use imessage_common::model::{Chat, ChatId, ConnectionState, DeliveryState, Handle, MessageId, ProfilesStatus, ReactionKind, RegistrationState};
use omnisette::remote_anisette_v3::RemoteAnisetteProviderV3;
use omnisette::{AnisetteClient, ArcAnisetteClient};
use rustpush::macos::MacOSConfig;
use rustpush::{
    APSConnection, APSConnectionResource, Attachment as RpAttachment, AttachmentType, ConversationData, EditMessage,
    IDSNGMIdentity, IDSUser, IMClient, IndexedMessagePart, LoginDelegate, MADRID_SERVICE, MMCSFile, Message as RpMessage,
    MessageInst, MessagePart as RpMessagePart, MessageParts, MessageType, NormalMessage, OSConfig, ReactMessage,
    ReactMessageType, ResourceState, UnsendMessage, authenticate_apple, login_apple_delegates, register,
};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use zeroize::Zeroizing;

mod profiles;

use super::supervise::{self, Backoff};
use super::{convert, SentMessage};
use super::{ChatTarget, Engine, EngineError, EngineEvent, EngineResult, FetchedProfile, LoginOutcome, ShareRef, EVENT_CHANNEL_CAPACITY};
use crate::config::DaemonConfig;
use crate::keyring::StateKey;
use crate::state::{SavedState, StateError, StateStore};

type Provider = super::anisette::Provider;
type Anisette = ArcAnisetteClient<Provider>;
type Account = AppleAccount<Provider>;

const MADRID: &str = "com.apple.madrid";
/// How long to wait for the first APNs connection before giving up on a
/// login/registration attempt (APNs keeps retrying in the background).
const APS_READY_TIMEOUT: Duration = Duration::from_secs(90);
/// Interactive login waits less: the user is staring at a spinner.
const LOGIN_APS_TIMEOUT: Duration = Duration::from_secs(30);
const DEDUP_WINDOW: usize = 2048;
/// Waits between silent re-login attempts after a *transient* failure
/// (network, anisette source unreachable): 10 s, 60 s, 5 min, then every
/// 15 min for as long as it takes (the saved credentials are still good,
/// e.g. the Mac answering anisette over ssh is asleep for the night).
const RELOGIN_RETRY_DELAYS: [Duration; 4] =
    [Duration::from_secs(10), Duration::from_secs(60), Duration::from_secs(300), Duration::from_secs(900)];
/// `Protocol` failures (rustpush / IDS errors that may or may not be Apple
/// refusing us) get only this many retries before `NeedsLogin`.
const RELOGIN_PROTOCOL_RETRIES: usize = 3;
/// Longest anisette failure reason shown in the connection state.
const ANISETTE_DETAIL_MAX: usize = 160;
/// Consecutive *successful* silent re-logins (each followed by another IDS
/// rejection) before we stop and ask the user (N1).
const MAX_SILENT_RELOGIN_CYCLES: u32 = 5;
/// A silent re-login more than this long after the previous one starts a
/// fresh cycle count.
const SILENT_RELOGIN_CYCLE_RESET: Duration = Duration::from_secs(3600);
/// Shutdown: how long to wait for the IDS ResourceManager loop to finish
/// after `close()` so its last keystore/keys-updated write is captured.
const SHUTDOWN_IDS_WAIT: Duration = Duration::from_secs(2);

fn shutting_down() -> EngineError {
    EngineError::Transient("engine is shutting down".into())
}

/// Pre-allocate at most this much for a download (the rest grows as
/// bytes actually arrive).
const DOWNLOAD_PREALLOC_MAX: usize = 4 << 20;

/// An in-memory download sink that refuses to grow past `max` bytes.
struct CappedWriter {
    buf: Vec<u8>,
    max: u64,
    received: u64,
    exceeded: bool,
}

impl CappedWriter {
    fn new(max: u64, size_hint: usize) -> CappedWriter {
        let cap = size_hint.min(DOWNLOAD_PREALLOC_MAX).min(usize::try_from(max).unwrap_or(usize::MAX));
        CappedWriter { buf: Vec::with_capacity(cap), max, received: 0, exceeded: false }
    }
}

impl std::io::Write for CappedWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.received = self.received.saturating_add(data.len() as u64);
        if self.received > self.max {
            self.exceeded = true;
            return Err(std::io::Error::other("download exceeds the size limit"));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn perr(e: impl std::fmt::Display) -> EngineError {
    EngineError::Protocol(e.to_string())
}

/// Classify a GSA (icloud_auth) error: Apple said no (don't retry without
/// the user) vs. something that may work on retry.
fn gsa_err(e: icloud_auth::Error) -> EngineError {
    use icloud_auth::Error as E;
    match e {
        E::ErrorGettingAnisette(a) => anisette_err(&a),
        E::ReqwestError(_) | E::Parse | E::PlistError(_) | E::SerdeError(_) | E::HappyBirthdayError => {
            EngineError::Transient(e.to_string())
        }
        E::Bad2faCode => EngineError::BadTwoFactorCode,
        E::AuthSrp | E::AuthSrpWithMessage(..) | E::ExtraStep(_) | E::FailedGetting2FAConfig | E::HardwareKeyError | E::BadSpd(_) => {
            EngineError::Login(e.to_string())
        }
    }
}

/// An anisette provider error: [`EngineError::Anisette`], text starting
/// with "anisette" (the command provider's errors already do).
fn anisette_err(a: &omnisette::AnisetteError) -> EngineError {
    let m = a.to_string();
    EngineError::Anisette(if m.starts_with("anisette") { m } else { format!("anisette server failed: {m}") })
}

/// Classify a rustpush error on the login / registration path: anisette
/// and network failures are transient, the rest is `Protocol`.
fn push_err(e: rustpush::PushError) -> EngineError {
    use rustpush::PushError as P;
    match e {
        P::AnisetteError(a) => anisette_err(&a),
        P::RequestError(_) | P::IoError(_) | P::ResourceTimeout | P::NotConnected | P::SendTimedOut => {
            EngineError::Transient(e.to_string())
        }
        _ => EngineError::Protocol(e.to_string()),
    }
}

/// Connection-state detail for an anisette failure (pure; unit tested):
/// `anisette source unreachable: <short reason>`.
fn anisette_detail(msg: &str) -> String {
    let reason = msg
        .strip_prefix(super::anisette::ANISETTE_COMMAND_FAILED)
        .map(|r| r.trim_start_matches([':', ' ']))
        .unwrap_or(msg);
    let reason: String = match reason.char_indices().nth(ANISETTE_DETAIL_MAX) {
        Some((i, _)) => format!("{}...", &reason[..i]),
        None => reason.to_string(),
    };
    format!("anisette source unreachable: {reason}")
}

/// Silent re-login retry policy (pure; unit tested): `Some(wait)` to retry
/// after `failures` consecutive failures ending in `e`, `None` to give up.
/// Network and anisette failures retry forever on [`RELOGIN_RETRY_DELAYS`]
/// (the last delay repeating); `Protocol` errors a few times; credential /
/// 2FA / configuration problems short-circuit.
fn relogin_retry_delay(e: &EngineError, failures: usize) -> Option<Duration> {
    let i = failures.saturating_sub(1);
    match e {
        EngineError::Transient(_) | EngineError::Anisette(_) => {
            Some(RELOGIN_RETRY_DELAYS[i.min(RELOGIN_RETRY_DELAYS.len() - 1)])
        }
        EngineError::Protocol(_) if i < RELOGIN_PROTOCOL_RETRIES => Some(RELOGIN_RETRY_DELAYS[i]),
        _ => None,
    }
}

/// Back-off before silent re-login cycle number `cycle` (0-based count of
/// consecutive successful silent re-logins so far): none for the first,
/// then 30 s, 60 s, 120 s, 240 s; `None` = give up (pure; unit tested).
fn relogin_cycle_delay(cycle: u32) -> Option<Duration> {
    if cycle >= MAX_SILENT_RELOGIN_CYCLES {
        return None;
    }
    Some(if cycle == 0 { Duration::ZERO } else { Duration::from_secs(30 << (cycle - 1)) })
}

/// One silent re-login attempt for [`Inner::silent_relogin_with`].
type ReloginAttempt = for<'a> fn(&'a Arc<Inner>, u64) -> Pin<Box<dyn Future<Output = EngineResult<LoginOutcome>> + Send + 'a>>;

/// Receive-path de-duplication window, shared by every receive loop of the
/// engine so a client replacement cannot re-emit a message (N3).
#[derive(Debug, Default)]
struct Dedup {
    order: VecDeque<String>,
    set: HashSet<String>,
}

impl Dedup {
    /// `true` if `id` was seen before; otherwise remembers it.
    fn check_and_insert(&mut self, id: &str) -> bool {
        if self.set.contains(id) {
            return true;
        }
        self.set.insert(id.to_string());
        self.order.push_back(id.to_string());
        if self.order.len() > DEDUP_WINDOW
            && let Some(old) = self.order.pop_front()
        {
            self.set.remove(&old);
        }
        false
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Connection-state bookkeeping (pure; unit tested)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Idle,
    Busy,
    Ready,
    Retrying(String),
    /// Our own retry loop is waiting on something outside Apple (the
    /// anisette source); the text is the full connection-state detail.
    Stalled(String),
    Fatal(String),
    /// The ResourceManager's loop has ended (see [`phase_of`]).
    Closed,
}

#[derive(Debug, Clone, Default)]
struct Status {
    aps: Phase,
    ids: Phase,
    needs_login: bool,
    two_factor: bool,
    fatal: Option<String>,
    /// Supervised tasks currently dead and waiting to be respawned
    /// (task name -> reason). Non-empty = `Connecting` instead of
    /// `Connected`/`Registering`.
    degraded: BTreeMap<&'static str, String>,
}

fn compute_connection(s: &Status) -> ConnectionState {
    if let Some(e) = &s.fatal {
        return ConnectionState::Error(e.clone());
    }
    if s.two_factor {
        return ConnectionState::NeedsTwoFactor;
    }
    if s.needs_login {
        return ConnectionState::NeedsLogin;
    }
    match &s.aps {
        Phase::Idle | Phase::Closed => ConnectionState::Disconnected,
        Phase::Busy | Phase::Retrying(_) | Phase::Stalled(_) => ConnectionState::Connecting,
        Phase::Fatal(e) => ConnectionState::Error(e.clone()),
        Phase::Ready => match &s.ids {
            Phase::Fatal(_) | Phase::Closed => ConnectionState::NeedsLogin,
            Phase::Retrying(e) => ConnectionState::Error(format!("registration failing, retrying: {e}")),
            Phase::Stalled(d) => ConnectionState::Error(d.clone()),
            _ if !s.degraded.is_empty() => ConnectionState::Connecting,
            Phase::Busy => ConnectionState::Registering,
            Phase::Idle | Phase::Ready => ConnectionState::Connected,
        },
    }
}

/// Map a rustpush `ResourceState`.
///
/// `Closed` is reported once the manager's loop has exited, which happens
/// (a) after `close()` / the last `Arc` dropping — we only do that on
/// shutdown or when replacing the client, and those monitors are stopped
/// first (shutdown signal / `client_gen`), so they never act on it — and
/// (b) immediately after a `DoNotRetry` failure: the loop publishes
/// `Failed{retry_wait: None}` and then `Closed`, and a `watch` receiver may
/// observe only the latter. So for a live IDS identity `Closed` means the
/// same as `Fatal` (registration rejected), and for APNs "disconnected".
fn phase_of(s: &ResourceState) -> Phase {
    match s {
        ResourceState::Generated => Phase::Ready,
        ResourceState::Generating => Phase::Busy,
        ResourceState::Failed(f) if f.retry_wait.is_some() => Phase::Retrying(f.error.to_string()),
        ResourceState::Failed(f) => Phase::Fatal(f.error.to_string()),
        ResourceState::Closed => Phase::Closed,
    }
}

/// Summarize IDS users into the model's registration state.
pub fn registration_from_users(users: &[IDSUser]) -> RegistrationState {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut handles = vec![];
    let mut registered_at: Option<u64> = None;
    let mut expires_at: Option<u64> = None;
    for u in users {
        let Some(reg) = u.registration.get(MADRID) else { continue };
        for h in convert::email_handles(&reg.handles) {
            let h = Handle::new(&h);
            if !handles.contains(&h) {
                handles.push(h);
            }
        }
        if reg.registered_at_s > 0 {
            registered_at = Some(registered_at.map_or(reg.registered_at_s, |r: u64| r.min(reg.registered_at_s)));
        }
        if let Ok(secs) = reg.calculate_rereg_time_s() {
            let at = (now as i64 + secs).max(0) as u64;
            expires_at = Some(expires_at.map_or(at, |e: u64| e.min(at)));
        }
    }
    RegistrationState { handles, registered_at, expires_at, sender: None, other_handles: vec![] }
}

/// The handle new messages go out from: `preferred` (the configured
/// `sender_handle`) if it is one of the registered email handles, else the
/// first registered email handle. Returns the registered spelling.
pub fn pick_sender(registered: &[String], preferred: Option<&str>) -> Option<String> {
    let emails = convert::email_handles(registered);
    if let Some(p) = preferred {
        let p = Handle::normalize(p);
        if let Some(h) = emails.iter().find(|h| Handle::normalize(h) == p) {
            return Some(h.clone());
        }
    }
    emails.into_iter().next()
}

/// Whether a configured `sender_handle` cannot be used with these
/// registered handles (the caller warns; sends fall back to the default).
fn sender_unusable(registered: &[String], preferred: Option<&str>) -> bool {
    match preferred {
        Some(p) if !registered.is_empty() => {
            let p = Handle::normalize(p);
            !convert::email_handles(registered).iter().any(|h| Handle::normalize(h) == p)
        }
        _ => false,
    }
}

/// `reg` with `sender` filled in for the current configuration.
fn with_sender(mut reg: RegistrationState, preferred: Option<&str>) -> RegistrationState {
    let uris: Vec<String> = reg.handles.iter().map(|h| h.uri.clone()).collect();
    reg.sender = pick_sender(&uris, preferred).map(|h| Handle::new(&h));
    reg
}

/// Validate + normalize user-supplied handles.
fn normalize_handles(raw: &[String]) -> EngineResult<Vec<String>> {
    raw.iter().map(|h| Handle::try_normalize(h).ok_or_else(|| EngineError::InvalidHandle(h.clone()))).collect()
}

/// Resolve a send target to (chat id, conversation including us, the
/// handle to send from). Pure.
///
/// `me` is the default handle we send as (see [`pick_sender`]); `mine` is
/// *every* handle registered to the account, used to decide who the
/// "others" are — the receive path (`convert::chat_id_for`) uses all handles
/// too, so both agree on chat ids for accounts with several addresses.
/// `other` is the configured `other_handles`: also "us" when deciding who
/// the others are (the receive path counts them too), never a sender.
///
/// Sender (as Messages.app does it): `me` starts *new* conversations. A
/// remembered conversation keeps going out from the handle of ours it
/// names — for a 1:1, the address the peer wrote to, rewritten to
/// `[peer, that handle]` (falling back to `me` when it names none of
/// ours); for a group, the handle we are a member under, so its
/// membership does not change.
fn resolve_target(
    chats: &HashMap<ChatId, ConversationData>,
    chat: &ChatTarget,
    me: &str,
    mine: &[String],
    other: &[String],
) -> EngineResult<(ChatId, ConversationData, String)> {
    let all_mine: Vec<String> = mine.iter().chain(other).cloned().collect();
    let direct = |other: &str| ConversationData { participants: vec![other.to_string(), me.to_string()], cv_name: None, sender_guid: None, after_guid: None };
    // Note to self: our own handle is the only participant and the sender
    // (rustpush then delivers to our other devices only).
    let own = |h: &str| mine.iter().find(|m| Handle::normalize(m) == Handle::normalize(h)).cloned();
    let note_to_self = |h: String, known: Option<&ConversationData>| -> (ChatId, ConversationData, String) {
        let mut c = known.cloned().unwrap_or(ConversationData { participants: vec![], cv_name: None, sender_guid: None, after_guid: None });
        c.participants = vec![h.clone()];
        (ChatId::direct(&h), c, h)
    };
    // A remembered conversation, made consistent with the sender.
    let known = |id: &ChatId, c: &ConversationData| -> (ChatId, ConversationData, String) {
        if let (false, Some(h)) = (id.is_group(), own(&id.0)) {
            note_to_self(h, Some(c))
        } else if id.is_group() {
            let member = c.participants.iter().find(|p| Handle::normalize(p) == Handle::normalize(me)).cloned().or_else(|| c.participants.iter().find_map(|p| own(p)));
            let sender = member.unwrap_or_else(|| me.to_string());
            (id.clone(), c.clone(), sender)
        } else {
            // Reply from the address the peer wrote to; only two participants,
            // or rustpush would add the sender as a third and make a group.
            let sender = c.participants.iter().find_map(|p| own(p)).unwrap_or_else(|| me.to_string());
            let mut c = c.clone();
            c.participants = vec![id.0.clone(), sender.clone()];
            (id.clone(), c, sender)
        }
    };
    match chat {
        ChatTarget::Participants(p) => {
            let all = normalize_handles(p)?;
            let others = convert::others(&all, &all_mine);
            if others.is_empty() {
                // Only our own handles: a note to self, if it names exactly one.
                let mut selves: Vec<String> = all.iter().filter_map(|h| own(h)).collect();
                selves.dedup();
                return match selves.as_slice() {
                    [] if !all.is_empty() => {
                        Err(EngineError::InvalidInput("those are your other addresses (other_handles); a note to self goes to a registered handle".into()))
                    }
                    [h] => {
                        let id = ChatId::direct(h);
                        Ok(note_to_self(h.clone(), chats.get(&id)))
                    }
                    [] => Err(EngineError::InvalidInput("no recipients".into())),
                    _ => Err(EngineError::InvalidInput("a note to self names one of your handles; a group needs someone else".into())),
                };
            }
            if others.len() == 1 {
                let id = ChatId::direct(&others[0]);
                if let Some(c) = chats.get(&id) {
                    return Ok(known(&id, c));
                }
                return Ok((id, direct(&others[0]), me.to_string()));
            }
            let want: HashSet<&String> = others.iter().collect();
            if let Some((id, c)) = chats
                .iter()
                .find(|(id, c)| id.is_group() && convert::others(&c.participants, &all_mine).iter().collect::<HashSet<_>>() == want)
            {
                return Ok(known(id, c));
            }
            let guid = uuid::Uuid::new_v4().to_string().to_uppercase();
            let mut participants = others.clone();
            participants.push(me.to_string());
            Ok((ChatId::group(&guid), ConversationData { participants, cv_name: None, sender_guid: Some(guid), after_guid: None }, me.to_string()))
        }
        ChatTarget::Chat(id) => {
            if let Some(c) = chats.get(id) {
                return Ok(known(id, c));
            }
            if !id.is_group() {
                let other = Handle::try_normalize(&id.0).ok_or_else(|| EngineError::InvalidHandle(id.0.clone()))?;
                if let Some(h) = own(&other) {
                    return Ok(note_to_self(h, None));
                }
                return Ok((ChatId::direct(&other), direct(&other), me.to_string()));
            }
            Err(EngineError::ChatNotFound(id.clone()))
        }
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

enum TwoFactorKind {
    Device,
    Sms(VerifyBody),
}

/// A GSA session waiting for a 2FA code. Holds the SRP password hash.
struct PendingLogin {
    account: Account,
    username: String,
    hashed_password: Zeroizing<Vec<u8>>,
    kind: TwoFactorKind,
}

impl std::fmt::Debug for PendingLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PendingLogin(<redacted>)")
    }
}

struct Inner {
    cfg: DaemonConfig,
    os_config: Option<Arc<MacOSConfig>>,
    store: StateStore,
    saved: StdMutex<SavedState>,
    keystore_snapshot: Arc<StdMutex<Option<plist::Value>>>,
    events: broadcast::Sender<EngineEvent>,
    status: StdMutex<(Status, ConnectionState)>,
    /// Serializes login / bring-up / relogin.
    flow: Mutex<()>,
    /// Anisette client, keyed by the push token its `LoginClientInfo` was
    /// built with (rebuilt when APNs hands out a new token).
    anisette: StdMutex<Option<(Option<[u8; 32]>, Anisette)>>,
    aps: StdMutex<Option<APSConnection>>,
    client: StdMutex<Option<Arc<IMClient>>>,
    client_gen: AtomicU64,
    pending: Mutex<Option<PendingLogin>>,
    chats: StdMutex<HashMap<ChatId, ConversationData>>,
    save_tx: mpsc::UnboundedSender<()>,
    /// Monotonic save sequence; `written_seq` (held while writing) makes a
    /// slow older write skip itself if a newer snapshot already hit disk.
    save_seq: AtomicU64,
    written_seq: Arc<StdMutex<u64>>,
    save_task: StdMutex<Option<JoinHandle<()>>>,
    tasks: StdMutex<Vec<JoinHandle<()>>>,
    client_tasks: StdMutex<Vec<JoinHandle<()>>>,
    relogin_running: AtomicBool,
    /// Bumped when an interactive login completes or on logout: a silent
    /// re-login started under an older epoch is abandoned (N2).
    login_epoch: AtomicU64,
    /// (consecutive successful silent re-logins, when the last one finished).
    silent_cycles: StdMutex<(u32, Option<Instant>)>,
    /// Message ids already emitted (shared across client generations).
    dedup: StdMutex<Dedup>,
    /// `true` once `shutdown()` started: stops supervisors and retry sleeps.
    shutdown: watch::Sender<bool>,
    /// CloudKit client for shared profiles + its status (`profiles.rs`).
    profiles: StdMutex<profiles::ProfilesSlot>,
    /// Serializes building the CloudKit client.
    profiles_init: Mutex<()>,
}

/// Cheap to clone; all clones share one engine.
#[derive(Clone)]
pub struct RustpushEngine {
    inner: Arc<Inner>,
}

fn lock<T>(m: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn install_keystore(
    saved: Option<&plist::Value>,
    snapshot: Arc<StdMutex<Option<plist::Value>>>,
    save_tx: mpsc::UnboundedSender<()>,
) -> EngineResult<()> {
    use keystore::software::{NoEncryptor, SoftwareKeystore, SoftwareKeystoreState};
    let state: SoftwareKeystoreState = match saved {
        // A present-but-unparseable keystore must not silently become an
        // empty one: the APS/IDS private keys would be gone and the next
        // save would make that permanent.
        Some(v) => plist::from_value(v).map_err(|e| EngineError::State(format!("saved keystore is unreadable: {e}")))?,
        None => SoftwareKeystoreState::default(),
    };
    *lock(&snapshot) = saved.cloned();
    // NoEncryptor: the whole SavedState (including this) is AES-GCM sealed
    // by state.rs, so a second layer would add nothing.
    keystore::init_keystore(SoftwareKeystore {
        state: std::sync::RwLock::new(state),
        update_state: Box::new(move |s| {
            if let Ok(v) = plist::to_value(s) {
                *lock(&snapshot) = Some(v);
            }
            let _ = save_tx.send(());
        }),
        encryptor: NoEncryptor,
    });
    Ok(())
}

fn state_err(e: StateError) -> EngineError {
    match e {
        StateError::Decrypt | StateError::Truncated => EngineError::StateKeyMismatch(e.to_string()),
        other => EngineError::State(other.to_string()),
    }
}

impl RustpushEngine {
    /// Load state and bring the engine up as far as the saved state allows.
    /// Returns an error only if `state.enc` exists but cannot be decrypted
    /// (`StateKeyMismatch`) or parsed (`State`): the caller must not
    /// overwrite it blindly.
    pub async fn start(cfg: DaemonConfig, key: StateKey) -> EngineResult<RustpushEngine> {
        let store = StateStore::new(cfg.state_path(), key);
        let loader = store.clone();
        let mut saved: SavedState = tokio::task::spawn_blocking(move || loader.load())
            .await
            .map_err(|e| EngineError::State(e.to_string()))?
            .map_err(state_err)?
            .unwrap_or_default();
        let drop_credentials = !cfg.store_credentials && saved.account.take().is_some();

        let (save_tx, save_rx) = mpsc::unbounded_channel();
        let keystore_snapshot = Arc::new(StdMutex::new(None));
        install_keystore(saved.keystore.as_ref(), keystore_snapshot.clone(), save_tx.clone())?;

        let hw = cfg.hwconfig_path();
        let os_config = tokio::task::spawn_blocking(move || {
            if !hw.exists() {
                log::warn!("no hwconfig at {}; import one with `imsg hwconfig import <file>`. Staying in NeedsLogin.", hw.display());
                return None;
            }
            match plist::from_file::<_, MacOSConfig>(&hw) {
                Ok(c) => Some(Arc::new(c)),
                Err(e) => {
                    log::error!("hwconfig {} unreadable: {e}", hw.display());
                    None
                }
            }
        })
        .await
        .map_err(|e| EngineError::State(e.to_string()))?;

        let resume = os_config.is_some() && !saved.users.is_empty() && saved.identity.is_some();
        let inner = Arc::new(Inner {
            cfg,
            os_config,
            store,
            saved: StdMutex::new(saved),
            keystore_snapshot,
            events: broadcast::channel(EVENT_CHANNEL_CAPACITY).0,
            status: StdMutex::new((Status { needs_login: !resume, ..Default::default() }, ConnectionState::Disconnected)),
            flow: Mutex::new(()),
            anisette: StdMutex::new(None),
            aps: StdMutex::new(None),
            client: StdMutex::new(None),
            client_gen: AtomicU64::new(0),
            pending: Mutex::new(None),
            chats: StdMutex::new(HashMap::new()),
            save_tx,
            save_seq: AtomicU64::new(0),
            written_seq: Arc::new(StdMutex::new(0)),
            save_task: StdMutex::new(None),
            tasks: StdMutex::new(vec![]),
            client_tasks: StdMutex::new(vec![]),
            relogin_running: AtomicBool::new(false),
            login_epoch: AtomicU64::new(0),
            silent_cycles: StdMutex::new((0, None)),
            dedup: StdMutex::new(Dedup::default()),
            shutdown: watch::channel(false).0,
            profiles: StdMutex::new(profiles::ProfilesSlot::default()),
            profiles_init: Mutex::new(()),
        });
        inner.update_status(|_| {});

        *lock(&inner.save_task) = Some(tokio::spawn(save_loop(Arc::downgrade(&inner), save_rx)));
        if drop_credentials {
            log::info!("store_credentials = false: removing saved Apple ID credentials from {}", inner.store.path().display());
            inner.request_save();
        }

        if resume {
            let i = inner.clone();
            let t = tokio::spawn(async move {
                if let Err(e) = i.resume().await {
                    log::error!("resume failed: {e}");
                    i.emit(EngineEvent::Error(format!("resume failed: {e}")));
                }
            });
            lock(&inner.tasks).push(t);
            inner.spawn_profiles_warmup();
        }
        Ok(RustpushEngine { inner })
    }
}

async fn save_loop(weak: Weak<Inner>, mut rx: mpsc::UnboundedReceiver<()>) {
    while rx.recv().await.is_some() {
        // Coalesce bursts (keystore + users + aps updates arrive together).
        tokio::time::sleep(Duration::from_millis(250)).await;
        while rx.try_recv().is_ok() {}
        let Some(inner) = weak.upgrade() else { break };
        inner.save_now().await;
    }
}

impl Inner {
    fn emit(&self, e: EngineEvent) {
        let _ = self.events.send(e);
    }

    fn request_save(&self) {
        let _ = self.save_tx.send(());
    }

    fn is_shutting_down(&self) -> bool {
        *self.shutdown.borrow()
    }

    fn check_not_shutting_down(&self) -> EngineResult<()> {
        if self.is_shutting_down() { Err(shutting_down()) } else { Ok(()) }
    }

    /// Sleep `d` unless shutdown starts first. `false` = shutting down.
    async fn sleep_unless_shutdown(&self, d: Duration) -> bool {
        let mut rx = self.shutdown.subscribe();
        let stopped = tokio::select! {
            _ = tokio::time::sleep(d) => false,
            _ = rx.wait_for(|v| *v) => true,
        };
        !stopped && !self.is_shutting_down()
    }

    fn update_status(&self, f: impl FnOnce(&mut Status)) {
        let changed = {
            let mut g = lock(&self.status);
            f(&mut g.0);
            let new = compute_connection(&g.0);
            if new != g.1 {
                g.1 = new.clone();
                Some(new)
            } else {
                None
            }
        };
        if let Some(s) = changed {
            log::info!("connection state: {s:?}");
            self.emit(EngineEvent::ConnectionChanged(s));
        }
    }

    fn connection(&self) -> ConnectionState {
        lock(&self.status).1.clone()
    }

    fn os_config(&self) -> EngineResult<Arc<MacOSConfig>> {
        self.os_config.clone().ok_or(EngineError::NoHardwareConfig)
    }

    fn client(&self) -> EngineResult<Arc<IMClient>> {
        lock(&self.client).clone().ok_or(EngineError::NotLoggedIn)
    }

    /// Snapshot the state and write it on the blocking pool (encryption +
    /// fsync). Concurrent saves are ordered by `save_seq`: an older snapshot
    /// never overwrites a newer one.
    async fn save_now(&self) {
        let aps = lock(&self.aps).clone();
        if let Some(aps) = aps {
            let push = aps.state.read().await.clone();
            lock(&self.saved).push = push;
        }
        // M-A: take the snapshot and its sequence number under the `saved`
        // lock, so a snapshot taken earlier can never get a later seq than
        // one taken after it (lock order: saved -> keystore_snapshot).
        let (mut snap, seq) = {
            let saved = lock(&self.saved);
            let mut snap = saved.clone();
            snap.keystore = lock(&self.keystore_snapshot).clone();
            (snap, self.save_seq.fetch_add(1, Ordering::SeqCst) + 1)
        };
        if !self.cfg.store_credentials {
            snap.account = None;
            snap.cloudkit_tokens = None;
        }
        let store = self.store.clone();
        let written = self.written_seq.clone();
        let r = tokio::task::spawn_blocking(move || {
            let mut w = lock(&written);
            if *w > seq {
                return Ok(false);
            }
            store.save(&snap)?;
            *w = seq;
            Ok::<bool, StateError>(true)
        })
        .await;
        match r {
            Ok(Ok(true)) => log::debug!("state saved"),
            Ok(Ok(false)) => log::debug!("state save superseded by a newer one"),
            Ok(Err(e)) => {
                log::error!("saving state to {} failed: {e}", self.store.path().display());
                self.emit(EngineEvent::Error(format!("saving state failed: {e}")));
            }
            Err(e) => log::error!("state save task failed: {e}"),
        }
    }

    /// The anisette client for the current push token. omnisette bakes the
    /// `LoginClientInfo` (which carries the token) in at construction, so a
    /// new token means a new client.
    fn anisette(&self, cfg: &MacOSConfig, push: &rustpush::APSState) -> Anisette {
        let mut g = lock(&self.anisette);
        if let Some((token, a)) = &*g
            && *token == push.token
        {
            return a.clone();
        }
        let dir = self.cfg.anisette_dir();
        let _ = crate::state::ensure_private_dir(&dir);
        let provider = match &self.cfg.anisette_command {
            Some(command) => {
                log::info!("anisette: command {command:?}");
                Provider::Command(super::anisette::CommandAnisetteProvider::new(command.clone()))
            }
            None => {
                log::info!("anisette: remote v3 server {}", self.cfg.anisette_url);
                Provider::Remote(Box::new(RemoteAnisetteProviderV3::new(self.cfg.anisette_url.clone(), cfg.get_gsa_config(push, false), dir)))
            }
        };
        let a: Anisette = Arc::new(Mutex::new(AnisetteClient::new(provider)));
        *g = Some((push.token, a.clone()));
        a
    }

    /// Spawn `make`'s task under [`supervise::supervise`]. `generation`:
    /// stop (no respawn) once `client_gen` moves on.
    fn spawn_supervised<F, Fut>(self: &Arc<Self>, name: &'static str, generation: Option<u64>, mut make: F) -> JoinHandle<()>
    where
        F: FnMut(&Arc<Inner>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        let weak = Arc::downgrade(self);
        let weak_fail = weak.clone();
        let shutdown = self.shutdown.subscribe();
        tokio::spawn(supervise::supervise(
            name,
            move || {
                let inner = weak.upgrade()?;
                if inner.is_shutting_down() || generation.is_some_and(|g| inner.client_gen.load(Ordering::SeqCst) != g) {
                    return None;
                }
                let fut = make(&inner);
                if lock(&inner.status).0.degraded.contains_key(name) {
                    log::info!("{name} restarted");
                    inner.update_status(|s| {
                        s.degraded.remove(name);
                    });
                }
                Some(fut)
            },
            move |reason, delay| {
                log::error!("{reason}; restarting in {}s", delay.as_secs_f32());
                if let Some(inner) = weak_fail.upgrade() {
                    inner.emit(EngineEvent::Error(reason.to_string()));
                    inner.update_status(|s| {
                        s.degraded.insert(name, reason.to_string());
                    });
                }
            },
            shutdown,
            Backoff::standard(),
        ))
    }

    /// Connect APNs (once) and spawn the state monitors.
    async fn ensure_aps(self: &Arc<Self>) -> EngineResult<APSConnection> {
        self.check_not_shutting_down()?;
        if let Some(a) = lock(&self.aps).clone() {
            return Ok(a);
        }
        let config = self.os_config()?;
        let push = lock(&self.saved).push.clone();
        let had_token = push.token.is_some();
        self.update_status(|s| s.aps = Phase::Busy);
        let (aps, err) = APSConnectionResource::new(config as Arc<dyn OSConfig>, Some(push)).await;
        if let Some(e) = err {
            // The ResourceManager retries on its own; report and carry on.
            log::warn!("initial APNs connection failed (will retry): {e}");
        }
        // M-B: shutdown may have started (and already taken `aps`) while we
        // were connecting; do not install a connection nobody will close.
        {
            let mut slot = lock(&self.aps);
            if self.is_shutting_down() {
                drop(slot);
                aps.close();
                return Err(shutting_down());
            }
            *slot = Some(aps.clone());
        }

        let a = aps.clone();
        let monitor = self.spawn_supervised("APNs state monitor", None, move |inner| {
            let weak = Arc::downgrade(inner);
            let mut rx = a.resource_state.subscribe();
            async move {
                loop {
                    let phase = phase_of(&rx.borrow_and_update());
                    let Some(inner) = weak.upgrade() else { break };
                    if inner.is_shutting_down() {
                        break;
                    }
                    if let Phase::Retrying(e) = &phase {
                        log::warn!("APNs: {e}; retrying");
                    }
                    inner.update_status(|s| s.aps = phase);
                    drop(inner);
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
                Ok(())
            }
        });
        let a = aps.clone();
        let persist = self.spawn_supervised("APNs persist monitor", None, move |inner| {
            let weak = Arc::downgrade(inner);
            let mut gen_rx = a.generated_signal.subscribe();
            async move {
                // Every (re)connect may have activated a new push cert/token.
                while let Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) = gen_rx.recv().await {
                    match weak.upgrade() {
                        Some(i) => i.request_save(),
                        None => break,
                    }
                }
                Ok(())
            }
        });
        lock(&self.tasks).extend([monitor, persist]);
        if !had_token {
            self.request_save();
        }
        Ok(aps)
    }

    async fn wait_aps_ready(&self, aps: &APSConnection, timeout: Duration) -> EngineResult<()> {
        let mut rx = aps.resource_state.subscribe();
        let r = tokio::time::timeout(timeout, async { rx.wait_for(|s| matches!(s, ResourceState::Generated)).await.map(|_| ()) }).await;
        match r {
            Ok(Ok(_)) if aps.state.read().await.token.is_some() => Ok(()),
            Ok(Ok(_)) => Err(EngineError::Protocol("APNs connected without a push token".into())),
            Ok(Err(_)) => Err(EngineError::Protocol("APNs connection closed".into())),
            Err(_) => Err(EngineError::Transient(format!("APNs not connected after {}s", timeout.as_secs()))),
        }
    }

    /// Startup with saved users: connect and start the client.
    async fn resume(self: &Arc<Self>) -> EngineResult<()> {
        let _flow = self.flow.lock().await;
        let aps = self.ensure_aps().await?;
        self.wait_aps_ready(&aps, APS_READY_TIMEOUT).await?;
        let (users, identity) = {
            let s = lock(&self.saved);
            (s.users.clone(), s.identity.clone().ok_or(EngineError::NotLoggedIn)?)
        };
        self.emit(EngineEvent::RegistrationChanged(self.registration(&users)));
        self.start_client(aps, users, identity).await
    }

    /// (Re)create the IMClient and its receive loop.
    ///
    /// Boxed with an explicit `Send` bound: start_client -> (6005) ->
    /// silent_relogin -> finish_login -> start_client is a cycle the
    /// compiler cannot prove `Send` through opaque `async fn` types.
    fn start_client(
        self: &Arc<Self>,
        aps: APSConnection,
        users: Vec<IDSUser>,
        identity: IDSNGMIdentity,
    ) -> Pin<Box<dyn Future<Output = EngineResult<()>> + Send + '_>> {
        Box::pin(self.start_client_inner(aps, users, identity))
    }

    async fn start_client_inner(self: &Arc<Self>, aps: APSConnection, users: Vec<IDSUser>, identity: IDSNGMIdentity) -> EngineResult<()> {
        self.check_not_shutting_down()?;
        let config = self.os_config()?;
        let generation = self.client_gen.fetch_add(1, Ordering::SeqCst) + 1;
        for t in lock(&self.client_tasks).drain(..) {
            t.abort();
        }
        *lock(&self.client) = None;

        // Subscribe before creating the client so nothing slips through.
        let first_sub = aps.messages_cont.subscribe();
        let weak = Arc::downgrade(self);
        let keys_updated = Box::new(move |users: Vec<IDSUser>| {
            if let Some(i) = weak.upgrade() {
                i.on_users_updated(users);
            }
        });
        let services: &'static [&'static _] = &[&MADRID_SERVICE];
        let client = Arc::new(
            IMClient::new(aps.clone(), users, identity, services, self.cfg.id_cache_path(), config as Arc<dyn OSConfig>, keys_updated).await,
        );
        // M-B: shutdown may have started during IMClient::new.
        {
            let mut slot = lock(&self.client);
            if self.is_shutting_down() || self.client_gen.load(Ordering::SeqCst) != generation {
                drop(slot);
                client.identity.close();
                return Err(shutting_down());
            }
            *slot = Some(client.clone());
        }

        // Registration state monitor.
        let c = client.clone();
        let monitor = self.spawn_supervised("IDS state monitor", Some(generation), move |inner| {
            let weak = Arc::downgrade(inner);
            let mut rx = c.identity.resource_state.subscribe();
            async move {
                loop {
                    let phase = phase_of(&rx.borrow_and_update());
                    let Some(inner) = weak.upgrade() else { break };
                    if inner.client_gen.load(Ordering::SeqCst) != generation || inner.is_shutting_down() {
                        break; // superseded client / shutting down
                    }
                    let fatal = matches!(phase, Phase::Fatal(_) | Phase::Closed);
                    if let Phase::Retrying(e) | Phase::Fatal(e) = &phase {
                        log::warn!("IDS registration: {e}");
                    }
                    inner.update_status(|s| s.ids = phase.clone());
                    if fatal {
                        let why = match &phase {
                            Phase::Fatal(e) => e.clone(),
                            _ => "identity resource closed".into(),
                        };
                        inner.emit(EngineEvent::Error(format!("registration rejected; re-login required ({why})")));
                        let i = inner.clone();
                        tokio::spawn(async move { i.silent_relogin().await });
                        break;
                    }
                    drop(inner);
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
                Ok(())
            }
        });

        // Receive loop; a respawn re-subscribes to `messages_cont`. APNs
        // messages broadcast while it was dead (backoff window) are lost.
        let mut first_sub = Some(first_sub);
        let (a, c) = (aps.clone(), client.clone());
        let recv = self.spawn_supervised("receive loop", Some(generation), move |inner| {
            let sub = first_sub.take().unwrap_or_else(|| a.messages_cont.subscribe());
            receive_loop(Arc::downgrade(inner), c.clone(), sub)
        });
        lock(&self.client_tasks).extend([monitor, recv]);
        self.update_status(|s| {
            s.needs_login = false;
            s.two_factor = false;
        });
        Ok(())
    }

    fn on_users_updated(&self, users: Vec<IDSUser>) {
        let reg = self.registration(&users);
        {
            let mut s = lock(&self.saved);
            s.users = users;
            s.registration = reg.clone();
        }
        log::info!("IDS registration refreshed: {} handle(s)", reg.handles.len());
        self.emit(EngineEvent::RegistrationChanged(reg));
        self.request_save();
    }

    fn set_account(&self, v: Option<plist::Value>) {
        if !self.cfg.store_credentials {
            return;
        }
        lock(&self.saved).account = v;
        self.request_save();
    }

    fn new_account(self: &Arc<Self>, config: &MacOSConfig, push: &rustpush::APSState, persisted: Option<PersistAccountData>) -> EngineResult<Account> {
        let anisette = self.anisette(config, push);
        let weak = Arc::downgrade(self);
        AppleAccount::new_with_anisette(
            config.get_gsa_config(push, false),
            anisette,
            persisted,
            Box::new(move |p: &PersistAccountData| {
                if let Some(i) = weak.upgrade() {
                    i.set_account(plist::to_value(p).ok());
                }
            }),
        )
        .map_err(gsa_err)
    }

    /// Walk the GSA state machine until logged in or a code is needed.
    /// `interactive`: a user is driving this (vs. silent re-login), so a
    /// failure in the registration half drops straight to `NeedsLogin`.
    async fn drive_login(
        self: &Arc<Self>,
        mut account: Account,
        username: String,
        hashed: Zeroizing<Vec<u8>>,
        mut st: LoginState,
        interactive: bool,
    ) -> EngineResult<LoginOutcome> {
        let mut relogged = false;
        loop {
            match st {
                LoginState::LoggedIn => break,
                LoginState::NeedsExtraStep(step) => {
                    if account.get_pet().is_some() {
                        break;
                    }
                    return Err(EngineError::Login(format!("Apple requires an extra step ({step}); sign in at account.apple.com")));
                }
                LoginState::NeedsDevice2FA => {
                    st = account.send_2fa_to_devices().await.map_err(gsa_err)?;
                }
                LoginState::NeedsSMS2FA => {
                    let extras = account.get_auth_extras().await.map_err(gsa_err)?;
                    let id = extras
                        .trusted_phone_numbers
                        .first()
                        .map(|p| p.id)
                        .ok_or_else(|| EngineError::Login("no trusted phone number on the account".into()))?;
                    st = account.send_sms_2fa_to_devices(id).await.map_err(gsa_err)?;
                }
                LoginState::Needs2FAVerification | LoginState::NeedsSMS2FAVerification(_) => {
                    let kind = match st {
                        LoginState::NeedsSMS2FAVerification(b) => TwoFactorKind::Sms(b),
                        _ => TwoFactorKind::Device,
                    };
                    *self.pending.lock().await = Some(PendingLogin { account, username, hashed_password: hashed, kind });
                    self.update_status(|s| s.two_factor = true);
                    self.emit(EngineEvent::TwoFactorRequired);
                    return Ok(LoginOutcome::TwoFactorRequired);
                }
                LoginState::NeedsLogin => {
                    // After a successful 2FA Apple wants the SRP exchange again.
                    if relogged {
                        return Err(EngineError::Login("Apple rejected the Apple ID or password".into()));
                    }
                    relogged = true;
                    st = account.login_email_pass(&username, &hashed).await.map_err(gsa_err)?;
                }
            }
        }
        self.finish_login(account, interactive).await?;
        Ok(LoginOutcome::Ok)
    }

    /// Logged-in GSA account -> IDS user -> registration -> client.
    async fn finish_login(self: &Arc<Self>, account: Account, interactive: bool) -> EngineResult<()> {
        self.check_not_shutting_down()?;
        let config = self.os_config()?;
        if account.get_pet().is_none() {
            return Err(EngineError::Login("login finished without a PET token".into()));
        }
        if let Some(p) = &account.persisted {
            self.set_account(plist::to_value(p).ok());
        }
        self.update_status(|s| {
            s.two_factor = false;
            s.ids = Phase::Busy;
        });
        let result = async {
            // MobileMe (iCloud) next to IDS: its tokens let CloudKit fetch
            // contacts' shared names and photos. Optional: if Apple refuses
            // it, log in with IDS alone.
            let delegates = match login_apple_delegates(&account, None, config.as_ref(), &[LoginDelegate::IDS, LoginDelegate::MobileMe]).await {
                Ok(d) => d,
                Err(e) => {
                    log::warn!("iCloud (MobileMe) delegate refused ({}); shared names and photos will be unavailable", profiles::classify(&e).1);
                    login_apple_delegates(&account, None, config.as_ref(), &[LoginDelegate::IDS]).await.map_err(push_err)?
                }
            };
            self.reset_profiles(delegates.mobileme.as_ref().and_then(profiles::token_state_with));
            let ids = delegates.ids.ok_or_else(|| EngineError::Login("Apple returned no IDS delegate".into()))?;
            let user = authenticate_apple(ids, config.as_ref()).await.map_err(push_err)?;

            let aps = self.ensure_aps().await?;
            self.wait_aps_ready(&aps, APS_READY_TIMEOUT).await?;
            let identity = {
                let existing = lock(&self.saved).identity.clone();
                match existing {
                    Some(i) => i,
                    None => {
                        let i = IDSNGMIdentity::new().map_err(perr)?;
                        lock(&self.saved).identity = Some(i.clone());
                        i
                    }
                }
            };
            let mut users = vec![user];
            let services: &'static [&'static _] = &[&MADRID_SERVICE];
            let push = aps.state.read().await.clone();
            register(config.as_ref(), &push, services, &mut users, &identity).await.map_err(push_err)?;
            let reg = self.registration(&users);
            if reg.handles.is_empty() {
                log::warn!("registered, but Apple returned no email handles for this account");
            }
            {
                let mut s = lock(&self.saved);
                s.users = users.clone();
                s.registration = reg.clone();
            }
            self.save_now().await;
            self.emit(EngineEvent::RegistrationChanged(reg));
            self.start_client(aps, users, identity).await
        }
        .await;
        match &result {
            Err(_) if interactive => self.update_status(|s| {
                s.ids = Phase::Idle;
                s.needs_login = true;
            }),
            // silent_relogin decides (retry or NeedsLogin).
            Err(_) => {}
            Ok(()) => {
                self.update_status(|s| s.ids = Phase::Ready);
                if interactive {
                    // N2: a pending silent re-login must not replace this client.
                    self.login_epoch.fetch_add(1, Ordering::SeqCst);
                    *lock(&self.silent_cycles) = (0, None);
                }
            }
        }
        result
    }

    /// One silent GSA login with the saved `PersistAccountData`.
    async fn silent_relogin_once(self: &Arc<Self>, epoch: u64) -> EngineResult<LoginOutcome> {
        self.check_not_shutting_down()?;
        let _flow = self.flow.lock().await;
        self.check_not_shutting_down()?;
        if self.login_epoch.load(Ordering::SeqCst) != epoch {
            return Err(EngineError::Login("superseded by an interactive login / logout".into()));
        }
        let config = self.os_config()?;
        let persisted: Option<PersistAccountData> = lock(&self.saved).account.as_ref().and_then(|v| plist::from_value(v).ok());
        let Some(p) = persisted.filter(|p| !p.username.is_empty() && !p.hashed_password.is_empty()) else {
            return Err(EngineError::NotLoggedIn);
        };
        let username = p.username.clone();
        let hashed = Zeroizing::new(p.hashed_password.clone());
        let push = lock(&self.saved).push.clone();
        let mut account = self.new_account(&config, &push, Some(p))?;
        log::info!("attempting silent re-login after registration rejection");
        let st = account.login_email_pass(&username, &hashed).await.map_err(gsa_err)?;
        self.drive_login(account, username, hashed, st, false).await
    }

    /// After IDS error 6005: GSA login with the saved password hash, no user
    /// interaction unless Apple insists on 2FA. Network / anisette failures
    /// are retried indefinitely ([`relogin_retry_delay`]; an unreachable
    /// anisette source shows as `Error("anisette source unreachable: ..")`)
    /// until it works, an interactive login / logout supersedes it, or
    /// shutdown; credential problems, 2FA, or running out of `Protocol`
    /// retries end in `NeedsLogin` / `NeedsTwoFactor`.
    async fn silent_relogin(self: &Arc<Self>) {
        self.silent_relogin_with(|i, epoch| Box::pin(i.silent_relogin_once(epoch))).await
    }

    /// [`Self::silent_relogin`] with the single attempt injectable (tests).
    async fn silent_relogin_with(self: &Arc<Self>, attempt: ReloginAttempt) {
        if self.relogin_running.swap(true, Ordering::SeqCst) {
            return;
        }
        let epoch = self.login_epoch.load(Ordering::SeqCst);
        let superseded = |i: &Self| i.login_epoch.load(Ordering::SeqCst) != epoch;

        // N1: a silent re-login that "succeeds" but is rejected again right
        // away would loop forever. Count consecutive cycles, back off, and
        // hand over to the user after MAX_SILENT_RELOGIN_CYCLES.
        let cycle = {
            let mut g = lock(&self.silent_cycles);
            if g.1.is_some_and(|t| t.elapsed() > SILENT_RELOGIN_CYCLE_RESET) {
                *g = (0, None);
            }
            g.0
        };
        let Some(pause) = relogin_cycle_delay(cycle) else {
            self.relogin_running.store(false, Ordering::SeqCst);
            log::warn!("{cycle} silent re-logins in a row were rejected again; giving up until the user logs in");
            self.emit(EngineEvent::Error("registration keeps being rejected after re-login; please log in again".into()));
            self.update_status(|s| {
                s.ids = Phase::Idle;
                s.needs_login = true;
            });
            return;
        };
        if !pause.is_zero() {
            if !superseded(self) {
                self.update_status(|s| s.ids = Phase::Retrying(format!("re-login in {}s", pause.as_secs())));
            }
            log::warn!("silent re-login #{} in a row; waiting {}s first", cycle + 1, pause.as_secs());
            if !self.sleep_unless_shutdown(pause).await {
                self.relogin_running.store(false, Ordering::SeqCst);
                return;
            }
        }

        let mut failures = 0;
        let r = loop {
            // Superseded first: an interactive login that finished while we
            // slept has already set `ids = Ready`, which must stay.
            if superseded(self) {
                break Err(EngineError::Login("superseded by an interactive login / logout".into()));
            }
            self.update_status(|s| s.ids = Phase::Busy);
            let r = attempt(self, epoch).await;
            let Err(e) = &r else { break r };
            failures += 1;
            match relogin_retry_delay(e, failures) {
                Some(wait) if !superseded(self) => {
                    log::warn!("silent re-login failed ({e}); retrying in {}s (attempt {failures})", wait.as_secs());
                    let phase = match e {
                        EngineError::Anisette(m) => Phase::Stalled(anisette_detail(m)),
                        _ => Phase::Retrying(format!("re-login: {e}")),
                    };
                    self.update_status(|s| s.ids = phase);
                    if !self.sleep_unless_shutdown(wait).await {
                        break r;
                    }
                }
                _ => break r,
            }
        };
        self.relogin_running.store(false, Ordering::SeqCst);
        match r {
            Ok(LoginOutcome::Ok) => {
                let mut g = lock(&self.silent_cycles);
                *g = (g.0 + 1, Some(Instant::now()));
                log::info!("silent re-login succeeded");
            }
            Ok(LoginOutcome::TwoFactorRequired) => log::warn!("re-login needs a 2FA code"),
            Err(_) if self.is_shutting_down() => {}
            Err(_) if superseded(self) => log::info!("silent re-login abandoned: an interactive login or logout happened meanwhile"),
            Err(e) => {
                log::warn!("silent re-login failed after {failures} attempt(s): {e}");
                self.update_status(|s| {
                    s.ids = Phase::Idle;
                    s.needs_login = true;
                });
            }
        }
    }

    /// (all registered handles, the email handle we send as by default)
    async fn my_handles(&self, client: &IMClient) -> EngineResult<(Vec<String>, String)> {
        let all = client.identity.get_handles().await;
        let me = pick_sender(&all, self.cfg.sender_handle.as_deref()).ok_or(EngineError::NotLoggedIn)?;
        Ok((all, me))
    }

    /// `handles` (registered) plus the configured `other_handles` not among
    /// them: everything that counts as "us" when deciding who sent what and
    /// which chat a message belongs to. Never used to pick a sender.
    fn with_other_handles(&self, handles: &[String]) -> Vec<String> {
        let mut all = handles.to_vec();
        for h in &self.cfg.other_handles {
            if !all.iter().any(|m| Handle::normalize(m) == Handle::normalize(h)) {
                all.push(h.clone());
            }
        }
        all
    }

    /// Registration summary for `users` (with the effective sender and the
    /// configured `other_handles`); warns when the configured
    /// `sender_handle` is not among the handles.
    fn registration(&self, users: &[IDSUser]) -> RegistrationState {
        let mut reg = with_sender(registration_from_users(users), self.cfg.sender_handle.as_deref());
        reg.other_handles = self.cfg.other_handles.clone();
        self.warn_unusable_sender(&reg);
        reg
    }

    fn warn_unusable_sender(&self, reg: &RegistrationState) {
        let uris: Vec<String> = reg.handles.iter().map(|h| h.uri.clone()).collect();
        if sender_unusable(&uris, self.cfg.sender_handle.as_deref()) {
            log::warn!(
                "sender_handle {} is not registered on this account (registered: {}); sending from {}",
                self.cfg.sender_handle.as_deref().unwrap_or(""),
                uris.join(", "),
                reg.sender.as_ref().map_or("?", |h| h.uri.as_str()),
            );
        }
    }

    /// The saved registration with `sender` for the current configuration
    /// (a saved state may predate the setting or a config change).
    fn registration_view(&self) -> RegistrationState {
        let mut reg = with_sender(lock(&self.saved).registration.clone(), self.cfg.sender_handle.as_deref());
        reg.other_handles = self.cfg.other_handles.clone();
        reg
    }

    fn remember_conversation(&self, id: ChatId, conv: ConversationData) {
        lock(&self.chats).insert(id, conv);
    }

    /// Send any rustpush message to a chat. `report` = emit MessageSent.
    async fn send(&self, chat: &ChatTarget, message: RpMessage, id_override: Option<&MessageId>, report: bool) -> EngineResult<SentMessage> {
        let client = self.client()?;
        let (mine, me) = self.my_handles(&client).await?;
        let (chat_id, conv, sender) = resolve_target(&lock(&self.chats), chat, &me, &mine, &self.cfg.other_handles)?;
        let mut inst = MessageInst::new(conv, &sender, message);
        if let Some(id) = id_override {
            inst.id = id.0.clone();
        }
        let id = MessageId::new(&inst.id);
        match client.send(&mut inst).await {
            Ok(job) => {
                // prepare_send stamps sent_timestamp; fall back to our clock.
                let sent_at = if inst.sent_timestamp > 0 { inst.sent_timestamp } else { now_ms() };
                if let Some(conv) = inst.conversation.clone() {
                    self.remember_conversation(chat_id.clone(), conv);
                }
                if report {
                    self.emit(EngineEvent::MessageSent { id: id.clone(), chat_id: Some(chat_id.clone()), delivery: DeliveryState::Sent, error: None, at: sent_at });
                    if let Some(handle) = job.handle {
                        let events = self.events.clone();
                        let (id2, chat2) = (id.clone(), chat_id.clone());
                        tokio::spawn(async move {
                            if let Ok(Err(e)) = handle.await {
                                let _ = events.send(EngineEvent::MessageSent {
                                    id: id2,
                                    chat_id: Some(chat2),
                                    delivery: DeliveryState::Failed,
                                    error: Some(e.to_string()),
                                    at: now_ms(),
                                });
                            }
                        });
                    }
                }
                Ok(SentMessage { id, chat_id, sent_at, sender: Handle::new(&sender) })
            }
            Err(e) => {
                if report {
                    self.emit(EngineEvent::MessageSent { id: id.clone(), chat_id: Some(chat_id), delivery: DeliveryState::Failed, error: Some(e.to_string()), at: now_ms() });
                }
                Err(perr(e))
            }
        }
    }
}

/// APS -> IMClient::handle -> convert -> events, plus delivery receipts.
/// Runs under the supervisor; `Ok(())` = channel closed / engine gone.
async fn receive_loop(weak: Weak<Inner>, client: Arc<IMClient>, mut sub: broadcast::Receiver<rustpush::APSMessage>) -> Result<(), String> {
    loop {
        let msg = match sub.recv().await {
            Ok(m) => m,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                log::warn!("receive loop lagged; {n} APNs messages dropped");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        };
        let inst = match client.handle(msg).await {
            Ok(Some(i)) => i,
            Ok(None) => continue,
            Err(e) => {
                log::warn!("failed to decode incoming message: {e}");
                continue;
            }
        };
        let Some(inner) = weak.upgrade() else { return Ok(()) };

        // Redeliveries (APNs resends until acked) are detected here but
        // still acknowledged below, so the sender's "Delivered" completes.
        let duplicate = inst.has_payload() && lock(&inner.dedup).check_and_insert(&inst.id);
        // Registered handles (we can send from them) and, for deciding what
        // is ours, those plus `other_handles` (e.g. the user's iPhone number).
        let my_handles = client.identity.get_handles().await;
        let all_mine = inner.with_other_handles(&my_handles);

        // Delivery acknowledgement: certified delivery when the sender asked
        // for it (current rustpush), else the legacy "Delivered" message.
        if let Some(ctx) = &inst.certified_context {
            if let Err(e) = client.identity.certify_delivery(MADRID, ctx, false).await {
                log::debug!("certify_delivery failed: {e}");
            }
        } else if convert::wants_delivered_receipt(&inst, &all_mine)
            && let Some(conv) = inst.conversation.clone()
            && let Some(me) = conv
                .participants
                .iter()
                .find(|p| my_handles.contains(p))
                .cloned()
                .or_else(|| pick_sender(&my_handles, inner.cfg.sender_handle.as_deref()))
        {
            let mut receipt = MessageInst::new(conv, &me, RpMessage::Delivered);
            receipt.id = inst.id.clone();
            receipt.target = inst.target.clone();
            if let Err(e) = client.send(&mut receipt).await {
                log::debug!("delivered receipt failed: {e}");
            }
        }
        if duplicate {
            log::debug!("dropping redelivered message {}", inst.id);
            continue;
        }

        if inst.verification_failed {
            // Never log the body; id + claimed sender only.
            log::warn!(
                "message {} claims sender {} but the sender key could not be verified; delivering it flagged unverified",
                inst.id,
                inst.sender.as_deref().unwrap_or("?")
            );
        }

        // Track conversations so replies / ChatTarget::Chat resolve. A
        // membership change is tracked under the group the old and new
        // members form (and dropped outside one), so it never rewrites a
        // 1:1 chat's participants.
        let tracked = inst.conversation.as_ref().and_then(|conv| {
            let id = match &inst.message {
                RpMessage::ChangeParticipants(c) => convert::membership_chat_id(conv, &c.new_participants, inst.sender.as_deref(), &all_mine)?,
                _ => convert::chat_id_for(conv, inst.sender.as_deref(), &all_mine),
            };
            Some((conv, id))
        });
        if let Some((conv, id)) = tracked {
            let mut chats = lock(&inner.chats);
            let entry = chats.entry(id).or_insert_with(|| conv.clone());
            match &inst.message {
                RpMessage::RenameMessage(r) => entry.cv_name = Some(r.new_name.clone()).filter(|n| !n.is_empty()),
                RpMessage::ChangeParticipants(c) => entry.participants = c.new_participants.clone(),
                RpMessage::Message(_) => {
                    entry.participants = conv.participants.clone();
                    if conv.sender_guid.is_some() {
                        entry.sender_guid = conv.sender_guid.clone();
                    }
                    if conv.cv_name.is_some() {
                        entry.cv_name = conv.cv_name.clone();
                    }
                }
                _ => {}
            }
        }

        for ev in convert::convert_incoming(&inst, &all_mine).events {
            inner.emit(ev);
        }
    }
}

#[async_trait]
impl Engine for RustpushEngine {
    async fn login(&self, apple_id: &str, password: &str) -> EngineResult<LoginOutcome> {
        let inner = &self.inner;
        let config = inner.os_config()?;
        let _flow = inner.flow.lock().await;
        *inner.pending.lock().await = None;
        let username = apple_id.trim().to_string();
        if username.is_empty() || password.is_empty() {
            return Err(EngineError::InvalidInput("Apple ID and password are required".into()));
        }
        let hashed = Zeroizing::new(Sha256::digest(password.as_bytes()).to_vec());
        // APNs first: the GSA client info carries the push token, and a
        // login without one would register a device Apple cannot reach.
        let aps = inner.ensure_aps().await?;
        inner.wait_aps_ready(&aps, LOGIN_APS_TIMEOUT).await?;
        let push = aps.state.read().await.clone();
        let mut account = inner.new_account(&config, &push, None)?;
        log::info!("logging in to Apple ID");
        let st = account.login_email_pass(&username, &hashed).await.map_err(gsa_err)?;
        inner.drive_login(account, username, hashed, st, true).await
    }

    async fn submit_two_factor(&self, code: &str) -> EngineResult<LoginOutcome> {
        let inner = &self.inner;
        let _flow = inner.flow.lock().await;
        let Some(mut p) = inner.pending.lock().await.take() else {
            return Err(EngineError::NoPendingTwoFactor);
        };
        let code = code.trim().to_string();
        let r = match std::mem::replace(&mut p.kind, TwoFactorKind::Device) {
            TwoFactorKind::Device => p.account.verify_2fa(code).await,
            TwoFactorKind::Sms(body) => {
                let keep = body.clone();
                p.kind = TwoFactorKind::Sms(keep);
                p.account.verify_sms_2fa(code, body).await
            }
        };
        match r.map_err(gsa_err) {
            Ok(st) => {
                let PendingLogin { account, username, hashed_password, .. } = p;
                // Ok(TwoFactorRequired): Apple accepted the code but wants
                // another (a new session is parked); the caller asks again.
                inner.drive_login(account, username, hashed_password, st, true).await
            }
            Err(e @ (EngineError::BadTwoFactorCode | EngineError::Transient(_))) => {
                // Keep the session so the user can retry.
                *inner.pending.lock().await = Some(p);
                Err(e)
            }
            Err(e) => {
                inner.update_status(|s| {
                    s.two_factor = false;
                    s.needs_login = true;
                });
                Err(e)
            }
        }
    }

    async fn logout(&self) -> EngineResult<()> {
        let inner = &self.inner;
        // Abandon a pending / sleeping silent re-login first (N2).
        inner.login_epoch.fetch_add(1, Ordering::SeqCst);
        let _flow = inner.flow.lock().await;
        *inner.pending.lock().await = None;
        inner.client_gen.fetch_add(1, Ordering::SeqCst);
        for t in lock(&inner.client_tasks).drain(..) {
            t.abort();
        }
        if let Some(c) = lock(&inner.client).take() {
            c.identity.close();
        }
        {
            let mut s = lock(&inner.saved);
            s.users.clear();
            s.identity = None;
            // Best effort: overwrite the saved account blob (SRP password
            // hash, tokens) before dropping it. Other copies (rustpush's own
            // `AppleAccount`, serializer buffers, freed allocations) are out
            // of our reach.
            if let Some(mut v) = s.account.take() {
                scrub_plist(&mut v);
            }
            if let Some(mut v) = s.cloudkit_tokens.take() {
                scrub_plist(&mut v);
            }
            s.registration = RegistrationState::default();
        }
        *lock(&inner.silent_cycles) = (0, None);
        *lock(&inner.profiles) = profiles::ProfilesSlot::default();
        lock(&inner.chats).clear();
        inner.update_status(|s| {
            s.ids = Phase::Idle;
            s.two_factor = false;
            s.needs_login = true;
        });
        inner.emit(EngineEvent::RegistrationChanged(RegistrationState::default()));
        inner.save_now().await;
        log::info!("logged out");
        Ok(())
    }

    async fn register(&self) -> EngineResult<RegistrationState> {
        let inner = &self.inner;
        if let Ok(client) = inner.client() {
            inner.update_status(|s| s.ids = Phase::Busy);
            // Requests a re-registration; rustpush ignores the request if the
            // identity was (re)generated less than 15 s ago (refresh_option's
            // MAX_RESOURCE_REGEN window) and returns Ok without doing anything.
            client.identity.refresh_now().await.map_err(perr)?;
            let users = client.identity.users.read().await.clone();
            inner.on_users_updated(users);
            return Ok(inner.registration_view());
        }
        let has_users = !lock(&inner.saved).users.is_empty();
        if !has_users {
            return Err(EngineError::NotLoggedIn);
        }
        inner.resume().await?;
        Ok(inner.registration_view())
    }

    async fn registration_state(&self) -> RegistrationState {
        self.inner.registration_view()
    }

    async fn connection_state(&self) -> ConnectionState {
        self.inner.connection()
    }

    async fn handles(&self) -> Vec<Handle> {
        lock(&self.inner.saved).registration.handles.clone()
    }

    async fn send_text(&self, chat: &ChatTarget, text: &str, reply_to: Option<&MessageId>) -> EngineResult<SentMessage> {
        if text.is_empty() {
            return Err(EngineError::InvalidInput("empty message".into()));
        }
        let mut n = NormalMessage::new(text.to_string(), MessageType::IMessage);
        if let Some(r) = reply_to {
            n.reply_guid = Some(r.0.clone());
            // "<part>:<start>:<length>" of the quoted message; whole first part.
            n.reply_part = Some("0:0:0".into());
        }
        self.inner.send(chat, RpMessage::Message(n), None, true).await
    }

    async fn send_attachment(&self, chat: &ChatTarget, path: &Path, mime: &str) -> EngineResult<SentMessage> {
        let inner = &self.inner;
        let aps = lock(&inner.aps).clone().ok_or(EngineError::NotLoggedIn)?;
        inner.client()?;
        // Read once, off the runtime; rustpush's MMCS code reads synchronously.
        let bytes = tokio::fs::read(path).await.map_err(|e| EngineError::InvalidInput(format!("{}: {e}", path.display())))?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "attachment".into());
        let prepared = MMCSFile::prepare_put(Cursor::new(&bytes[..])).await.map_err(perr)?;
        let attachment = RpAttachment::new_mmcs(&aps, &prepared, Cursor::new(&bytes[..]), mime, convert::uti_for_mime(mime), &name, |_, _| {})
            .await
            .map_err(perr)?;
        let mut n = NormalMessage::new(String::new(), MessageType::IMessage);
        n.parts = MessageParts(vec![IndexedMessagePart { part: RpMessagePart::Attachment(attachment), idx: None, ext: None }]);
        inner.send(chat, RpMessage::Message(n), None, true).await
    }

    async fn react(&self, chat: &ChatTarget, message_id: &MessageId, kind: ReactionKind, remove: bool) -> EngineResult<SentMessage> {
        let m = RpMessage::React(ReactMessage {
            to_uuid: message_id.0.clone(),
            to_part: Some(0),
            reaction: ReactMessageType::React { reaction: convert::reaction_to_rustpush(&kind), enable: !remove },
            to_text: String::new(),
            embedded_profile: None,
        });
        self.inner.send(chat, m, None, false).await
    }

    async fn edit(&self, chat: &ChatTarget, message_id: &MessageId, new_text: &str) -> EngineResult<SentMessage> {
        let m = RpMessage::Edit(EditMessage {
            tuuid: message_id.0.clone(),
            edit_part: 0,
            new_parts: NormalMessage::new(new_text.to_string(), MessageType::IMessage).parts,
        });
        self.inner.send(chat, m, None, false).await
    }

    async fn unsend(&self, chat: &ChatTarget, message_id: &MessageId) -> EngineResult<SentMessage> {
        let m = RpMessage::Unsend(UnsendMessage { tuuid: message_id.0.clone(), edit_part: 0 });
        self.inner.send(chat, m, None, false).await
    }

    async fn set_typing(&self, chat: &ChatTarget, typing: bool) -> EngineResult<()> {
        self.inner.send(chat, RpMessage::Typing(typing, None), None, false).await.map(|_| ())
    }

    async fn mark_read(&self, chat: &ChatTarget, up_to: &MessageId) -> EngineResult<()> {
        self.inner.send(chat, RpMessage::Read, Some(up_to), false).await.map(|_| ())
    }

    async fn lookup_handles(&self, handles: Vec<String>) -> EngineResult<Vec<(String, bool)>> {
        let targets = normalize_handles(&handles)?;
        let client = self.inner.client()?;
        let (_, me) = self.inner.my_handles(&client).await?;
        let valid = client.identity.validate_targets(&targets, MADRID, &me).await.map_err(perr)?;
        Ok(targets
            .into_iter()
            .map(|t| {
                let ok = valid.contains(&t);
                (t, ok)
            })
            .collect())
    }

    async fn download_attachment(&self, locator: &str, dest: &Path, max_bytes: u64) -> EngineResult<()> {
        let att = convert::attachment_from_locator(locator).map_err(|e| EngineError::InvalidInput(format!("attachment locator: {e}")))?;
        let too_large = |n: u64| EngineError::InvalidInput(format!("attachment too large ({n} bytes, limit {max_bytes})"));
        let bytes = match &att.a_type {
            AttachmentType::Inline(data) if data.len() as u64 > max_bytes => return Err(too_large(data.len() as u64)),
            AttachmentType::Inline(data) => data.clone(),
            AttachmentType::MMCS(_) => {
                // The announced size is the sender's claim: refuse an
                // oversized one up front, and never pre-allocate more than
                // a little of it.
                if att.get_size() as u64 > max_bytes {
                    return Err(too_large(att.get_size() as u64));
                }
                let aps = lock(&self.inner.aps).clone().ok_or(EngineError::NotLoggedIn)?;
                let mut w = CappedWriter::new(max_bytes, att.get_size());
                let r = att.get_attachment(&aps, &mut w, |_, _| {}).await;
                if w.exceeded {
                    return Err(too_large(w.received));
                }
                r.map_err(perr)?;
                w.buf
            }
        };
        let dest = dest.to_path_buf();
        tokio::task::spawn_blocking(move || {
            crate::state::atomic_write_in_place(&dest, &bytes).map_err(|e| EngineError::InvalidInput(format!("cannot write {}: {e}", dest.display())))
        })
        .await
        .map_err(|e| EngineError::Protocol(e.to_string()))?
    }

    async fn fetch_profile(&self, share: &ShareRef) -> EngineResult<FetchedProfile> {
        self.inner.fetch_profile(share).await
    }

    async fn profiles_status(&self) -> ProfilesStatus {
        self.inner.profiles_status_now()
    }

    async fn upsert_chat(&self, chat: &Chat) {
        let me = self.inner.registration_view().sender.map(|h| h.uri);
        let mut participants: Vec<String> = chat.participants.iter().map(|h| h.uri.clone()).collect();
        if let Some(me) = me {
            participants.push(me);
        }
        let conv = ConversationData {
            participants,
            cv_name: chat.display_name.clone(),
            sender_guid: chat.id.group_guid().map(String::from),
            after_guid: None,
        };
        self.inner.remember_conversation(chat.id.clone(), conv);
    }

    fn events(&self) -> broadcast::Receiver<EngineEvent> {
        self.inner.events.subscribe()
    }

    /// Order matters (every step can still produce state to persist):
    /// stop supervisors/tasks -> close + drop the client -> close APNs ->
    /// wait briefly for the IDS ResourceManager loop to end -> stop the
    /// save loop -> one final `save_now()` after the last possible mutation.
    async fn shutdown(&self) {
        let inner = &self.inner;
        let _ = inner.shutdown.send(true);
        inner.client_gen.fetch_add(1, Ordering::SeqCst);
        for t in lock(&inner.client_tasks).drain(..) {
            t.abort();
        }
        for t in lock(&inner.tasks).drain(..) {
            t.abort();
        }
        let client = lock(&inner.client).take();
        let ids_state = client.as_ref().map(|c| {
            let rx = c.identity.resource_state.subscribe();
            c.identity.close();
            rx
        });
        drop(client);
        let aps = lock(&inner.aps).take();
        if let Some(aps) = aps {
            aps.close();
            let push = aps.state.read().await.clone();
            lock(&inner.saved).push = push;
        }
        if let Some(mut rx) = ids_state {
            // Err = the loop already exited and dropped its sender: also done.
            let _ = tokio::time::timeout(SHUTDOWN_IDS_WAIT, rx.wait_for(|s| matches!(s, ResourceState::Closed))).await;
        }
        // No more periodic saves; an in-flight one is ordered by save_seq.
        if let Some(t) = lock(&inner.save_task).take() {
            t.abort();
        }
        inner.save_now().await;
        inner.update_status(|s| s.aps = Phase::Idle);
    }
}

/// Overwrite every string / data leaf of a plist value with zeros
/// (best-effort secret hygiene before the value is dropped).
fn scrub_plist(v: &mut plist::Value) {
    use zeroize::Zeroize;
    match v {
        plist::Value::Array(a) => a.iter_mut().for_each(scrub_plist),
        plist::Value::Dictionary(d) => d.values_mut().for_each(scrub_plist),
        plist::Value::Data(b) => b.zeroize(),
        plist::Value::String(s) => s.zeroize(),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn scrub_plist_zeroes_secrets() {
        let mut d = plist::Dictionary::new();
        d.insert("hash".into(), plist::Value::Data(vec![1, 2, 3]));
        d.insert("token".into(), plist::Value::Array(vec![plist::Value::String("secret".into())]));
        let mut v = plist::Value::Dictionary(d);
        scrub_plist(&mut v);
        let d = v.as_dictionary().unwrap();
        assert!(d["hash"].as_data().unwrap().iter().all(|b| *b == 0));
        assert!(d["token"].as_array().unwrap()[0].as_string().unwrap().is_empty());
    }

    use super::*;

    #[test]
    fn connection_state_mapping() {
        let mut s = Status { needs_login: true, ..Default::default() };
        assert_eq!(compute_connection(&s), ConnectionState::NeedsLogin);
        s.two_factor = true;
        assert_eq!(compute_connection(&s), ConnectionState::NeedsTwoFactor);
        s = Status::default();
        assert_eq!(compute_connection(&s), ConnectionState::Disconnected);
        s.aps = Phase::Busy;
        assert_eq!(compute_connection(&s), ConnectionState::Connecting);
        s.aps = Phase::Retrying("dns".into());
        assert_eq!(compute_connection(&s), ConnectionState::Connecting);
        s.aps = Phase::Ready;
        assert_eq!(compute_connection(&s), ConnectionState::Connected);
        s.degraded.insert("receive loop", "panicked".into());
        assert_eq!(compute_connection(&s), ConnectionState::Connecting, "dead receive loop is not Connected");
        s.degraded.clear();
        s.ids = Phase::Busy;
        assert_eq!(compute_connection(&s), ConnectionState::Registering);
        s.ids = Phase::Retrying("6001".into());
        assert!(matches!(compute_connection(&s), ConnectionState::Error(e) if e.contains("6001")));
        s.ids = Phase::Fatal("6005".into());
        assert_eq!(compute_connection(&s), ConnectionState::NeedsLogin);
        s.ids = Phase::Closed;
        assert_eq!(compute_connection(&s), ConnectionState::NeedsLogin);
        s.aps = Phase::Closed;
        assert_eq!(compute_connection(&s), ConnectionState::Disconnected);
        s.fatal = Some("bad state".into());
        assert_eq!(compute_connection(&s), ConnectionState::Error("bad state".into()));
        assert_eq!(phase_of(&ResourceState::Closed), Phase::Closed);
    }

    #[test]
    fn relogin_retry_policy() {
        let net = EngineError::Transient("dns".into());
        assert_eq!(relogin_retry_delay(&net, 1), Some(Duration::from_secs(10)));
        assert_eq!(relogin_retry_delay(&net, 2), Some(Duration::from_secs(60)));
        assert_eq!(relogin_retry_delay(&net, 3), Some(Duration::from_secs(300)));
        // network / anisette: forever, every 15 min after the first three
        for n in [4, 5, 50, 10_000] {
            assert_eq!(relogin_retry_delay(&net, n), Some(Duration::from_secs(900)), "{n}");
        }
        let ani = EngineError::Anisette("anisette command failed: exit status: 255: ssh: connect to host mac port 22: timed out".into());
        assert_eq!(relogin_retry_delay(&ani, 1), Some(Duration::from_secs(10)));
        assert_eq!(relogin_retry_delay(&ani, 100), Some(Duration::from_secs(900)));
        // Protocol errors: three retries, then NeedsLogin
        let proto = EngineError::Protocol("6001".into());
        assert_eq!(relogin_retry_delay(&proto, 1), Some(Duration::from_secs(10)));
        assert_eq!(relogin_retry_delay(&proto, 3), Some(Duration::from_secs(300)));
        assert_eq!(relogin_retry_delay(&proto, 4), None);
        // genuine auth problems short-circuit
        for e in [EngineError::Login("bad password".into()), EngineError::BadTwoFactorCode, EngineError::NotLoggedIn, EngineError::NoHardwareConfig] {
            assert_eq!(relogin_retry_delay(&e, 1), None, "{e}");
        }
        assert!(matches!(gsa_err(icloud_auth::Error::AuthSrp), EngineError::Login(_)));
        assert!(matches!(gsa_err(icloud_auth::Error::Parse), EngineError::Transient(_)));
        assert!(matches!(gsa_err(icloud_auth::Error::Bad2faCode), EngineError::BadTwoFactorCode));
        // anisette failures are their own (transient) kind, whichever path
        let cmd = || omnisette::AnisetteError::from(anyhow::anyhow!("anisette command failed: no answer within 30 s"));
        let e = gsa_err(icloud_auth::Error::ErrorGettingAnisette(cmd()));
        assert!(matches!(&e, EngineError::Anisette(m) if m == "anisette command failed: no answer within 30 s"), "{e:?}");
        assert!(matches!(push_err(rustpush::PushError::AnisetteError(cmd())), EngineError::Anisette(_)));
        let e = gsa_err(icloud_auth::Error::ErrorGettingAnisette(omnisette::AnisetteError::Misc));
        assert!(matches!(&e, EngineError::Anisette(m) if m.starts_with("anisette server failed")), "{e:?}");
        assert!(matches!(push_err(rustpush::PushError::ResourceTimeout), EngineError::Transient(_)));
        assert!(matches!(push_err(rustpush::PushError::BadMsg), EngineError::Protocol(_)));
    }

    #[test]
    fn anisette_failure_is_shown_as_error_not_needs_login() {
        let d = anisette_detail("anisette command failed: exit status: 255: ssh: connect to host mac port 22: Operation timed out");
        assert_eq!(d, "anisette source unreachable: exit status: 255: ssh: connect to host mac port 22: Operation timed out");
        assert_eq!(anisette_detail("anisette server failed: Misc"), "anisette source unreachable: anisette server failed: Misc");
        let long = anisette_detail(&format!("anisette command failed: {}", "x".repeat(500)));
        assert!(long.len() < 220 && long.ends_with("..."), "{long}");

        let mut s = Status { aps: Phase::Ready, ids: Phase::Stalled(d.clone()), ..Default::default() };
        assert_eq!(compute_connection(&s), ConnectionState::Error(d));
        // a successful retry clears it
        s.ids = Phase::Ready;
        assert_eq!(compute_connection(&s), ConnectionState::Connected);
    }

    /// N1: consecutive silent re-login cycles back off and stop after 5.
    #[test]
    fn relogin_cycle_policy() {
        assert_eq!(relogin_cycle_delay(0), Some(Duration::ZERO));
        assert_eq!(relogin_cycle_delay(1), Some(Duration::from_secs(30)));
        assert_eq!(relogin_cycle_delay(2), Some(Duration::from_secs(60)));
        assert_eq!(relogin_cycle_delay(4), Some(Duration::from_secs(240)));
        assert_eq!(relogin_cycle_delay(5), None);
        assert_eq!(relogin_cycle_delay(50), None);
    }

    /// N3: one window shared by all receive loops; bounded.
    #[test]
    fn dedup_window() {
        let mut d = Dedup::default();
        assert!(!d.check_and_insert("a"));
        assert!(d.check_and_insert("a"));
        for i in 0..DEDUP_WINDOW {
            d.check_and_insert(&i.to_string());
        }
        assert!(!d.check_and_insert("a"), "evicted after the window");
        assert_eq!(d.order.len(), DEDUP_WINDOW);
        assert_eq!(d.set.len(), DEDUP_WINDOW);
    }

    /// m3 / n6: the send path decides "others" with *all* our handles, like
    /// the receive path, and rejects garbage handles.
    #[test]
    fn resolve_uses_all_handles_and_validates() {
        let me = "mailto:me@example.com";
        let mine = vec![me.to_string(), "mailto:me2@example.com".to_string()];
        let chats = HashMap::new();
        let (id, conv, from) = resolve_target(&chats, &ChatTarget::Participants(vec!["bob@x.com".into(), "ME2@example.com".into()]), me, &mine, &[]).unwrap();
        assert_eq!(id, ChatId::direct("bob@x.com"), "my second handle is not a recipient");
        assert_eq!(id, convert::chat_id_for(&conv, Some(me), &mine), "send and receive agree");
        assert_eq!(from, me);

        let (gid, gconv, _) = resolve_target(&chats, &ChatTarget::Participants(vec!["bob@x.com".into(), "carol@x.com".into()]), me, &mine, &[]).unwrap();
        assert!(gid.group_guid().is_some());
        assert_eq!(gid, convert::chat_id_for(&gconv, Some(me), &mine));
        let mut chats = HashMap::new();
        // the receive path saw this group with our *second* handle in it
        chats.insert(gid.clone(), ConversationData { participants: vec!["mailto:bob@x.com".into(), "mailto:carol@x.com".into(), "mailto:me2@example.com".into()], ..gconv });
        let (again, _, from) = resolve_target(&chats, &ChatTarget::Participants(vec!["carol@x.com".into(), "bob@x.com".into()]), me, &mine, &[]).unwrap();
        assert_eq!(again, gid, "existing group found despite different own handle");
        assert_eq!(from, "mailto:me2@example.com", "a group keeps the handle we are a member under");

        for bad in [vec!["bob".to_string()], vec!["".to_string()], vec!["bob@x.com".into(), "tel:".into()]] {
            assert!(matches!(resolve_target(&chats, &ChatTarget::Participants(bad), me, &mine, &[]), Err(EngineError::InvalidHandle(_))));
        }
        assert!(matches!(resolve_target(&chats, &ChatTarget::Chat(ChatId("bob".into())), me, &mine, &[]), Err(EngineError::InvalidHandle(_))));
        assert!(matches!(resolve_target(&chats, &ChatTarget::Chat(ChatId::group("nope")), me, &mine, &[]), Err(EngineError::ChatNotFound(_))));
        assert!(matches!(resolve_target(&chats, &ChatTarget::Participants(vec![me.into(), "me2@example.com".into()]), me, &mine, &[]), Err(EngineError::InvalidInput(_))));
        assert!(matches!(normalize_handles(&["x@y.z".into(), "nope".into()]), Err(EngineError::InvalidHandle(h)) if h == "nope"));
    }

    /// Our imported hwconfig.plist must deserialize straight into rustpush's
    /// `MacOSConfig` (the daemon loads it that way).
    #[test]
    fn hwconfig_fixture_is_a_rustpush_macos_config() {
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../imessage-common/tests/data/hwconfig-fake.plist");
        let c: MacOSConfig = plist::from_file(p).unwrap();
        let ours = imessage_common::hwconfig::MacOSConfig::from_path(p).unwrap();
        assert_eq!(c.inner.product_name, ours.inner.product_name);
        assert_eq!(c.version, ours.version);
        assert_eq!(c.protocol_version, ours.protocol_version);
    }

    #[test]
    fn registration_summary_of_nothing() {
        assert_eq!(registration_from_users(&[]), RegistrationState::default());
    }

    /// Note to self: a target naming only one of our handles is the 1:1
    /// chat with that handle, sent from it, with just that participant;
    /// the receive path files our other devices' notes in the same chat.
    #[test]
    fn note_to_self_target() {
        let me = "mailto:me@example.com";
        let me2 = "mailto:me2@example.com";
        let mine = vec![me.to_string(), me2.to_string()];
        let mut chats = HashMap::new();
        for target in [ChatTarget::Participants(vec!["ME2@example.com".into()]), ChatTarget::Participants(vec![me2.into(), me2.into()]), ChatTarget::Chat(ChatId::direct(me2))] {
            let (id, conv, from) = resolve_target(&chats, &target, me, &mine, &[]).unwrap();
            assert_eq!(id, ChatId::direct(me2), "{target:?}");
            assert_eq!(from, me2, "sent from the handle we write to");
            assert_eq!(conv.participants, vec![me2.to_string()]);
            assert_eq!(convert::chat_id_for(&conv, Some(&from), &mine), id, "send and receive agree");
        }
        // remembered from the receive path (e.g. a note from the Mac)
        chats.insert(ChatId::direct(me2), ConversationData { participants: vec![], cv_name: None, sender_guid: Some("S".into()), after_guid: None });
        let (id, conv, from) = resolve_target(&chats, &ChatTarget::Chat(ChatId::direct(me2)), me, &mine, &[]).unwrap();
        assert_eq!((id, from.as_str()), (ChatId::direct(me2), me2));
        assert_eq!((conv.participants, conv.sender_guid.as_deref()), (vec![me2.to_string()], Some("S")));
        // groups still need someone else
        assert!(matches!(resolve_target(&chats, &ChatTarget::Participants(vec![me.into(), me2.into()]), me, &mine, &[]), Err(EngineError::InvalidInput(_))));
        assert!(matches!(resolve_target(&chats, &ChatTarget::Participants(vec![]), me, &mine, &[]), Err(EngineError::InvalidInput(_))));
        let (gid, gconv, _) = resolve_target(&chats, &ChatTarget::Participants(vec![me2.into(), "bob@x.com".into()]), me, &mine, &[]).unwrap();
        assert_eq!(gid, ChatId::direct("bob@x.com"), "own handle + one other = 1:1 with the other");
        assert_eq!(gconv.participants.len(), 2);
    }

    /// sender_handle: used when registered, else the first email handle.
    #[test]
    fn sender_choice_and_fallback() {
        let reg = vec!["mailto:me@icloud.example".to_string(), "tel:+15551234567".to_string(), "mailto:me@example.com".to_string()];
        assert_eq!(pick_sender(&reg, None).as_deref(), Some("mailto:me@icloud.example"));
        assert_eq!(pick_sender(&reg, Some("mailto:me@example.com")).as_deref(), Some("mailto:me@example.com"));
        assert_eq!(pick_sender(&reg, Some("Me@Example.com")).as_deref(), Some("mailto:me@example.com"));
        // not registered (or a phone number, which Mac-class registration
        // cannot send from) -> default, and flagged for a warning
        assert_eq!(pick_sender(&reg, Some("mailto:other@example.com")).as_deref(), Some("mailto:me@icloud.example"));
        assert_eq!(pick_sender(&reg, Some("tel:+15551234567")).as_deref(), Some("mailto:me@icloud.example"));
        assert!(sender_unusable(&reg, Some("mailto:other@example.com")));
        assert!(!sender_unusable(&reg, Some("mailto:me@example.com")));
        assert!(!sender_unusable(&reg, None));
        assert!(!sender_unusable(&[], Some("mailto:x@y.z")), "nothing registered yet: no warning");
        assert_eq!(pick_sender(&[], Some("mailto:x@y.z")), None);

        let state = RegistrationState { handles: reg.iter().map(|h| Handle::new(h)).collect(), registered_at: Some(1), expires_at: None, sender: None, other_handles: vec![] };
        assert_eq!(with_sender(state.clone(), Some("mailto:me@example.com")).sender, Some(Handle::new("mailto:me@example.com")));
        assert_eq!(with_sender(state.clone(), Some("mailto:nope@x.com")).sender, Some(Handle::new("mailto:me@icloud.example")));
        let json = serde_json::to_value(with_sender(state, None)).unwrap();
        assert_eq!(json["sender"]["uri"], "mailto:me@icloud.example");

        // the configured sender starts new 1:1 chats; a remembered 1:1 replies
        // from the address the peer wrote to (like Messages.app)
        let me = "mailto:me@example.com";
        let icloud = "mailto:me@icloud.example";
        let mut chats = HashMap::new();
        let bob = ChatId::direct("bob@x.com");
        chats.insert(bob.clone(), ConversationData { participants: vec!["mailto:bob@x.com".into(), "mailto:Me@iCloud.example".into()], cv_name: None, sender_guid: Some("G".into()), after_guid: None });
        for target in [ChatTarget::Chat(bob.clone()), ChatTarget::Participants(vec!["bob@x.com".into()])] {
            let (id, conv, from) = resolve_target(&chats, &target, me, &reg, &[]).unwrap();
            assert_eq!((id, from.as_str()), (bob.clone(), icloud), "{target:?}");
            assert_eq!(conv.participants, vec!["mailto:bob@x.com".to_string(), icloud.to_string()], "still 1:1, not a group of three");
            assert_eq!(conv.sender_guid.as_deref(), Some("G"));
        }
        // remembered without any of our handles: the configured sender
        chats.insert(bob.clone(), ConversationData { participants: vec!["mailto:bob@x.com".into()], cv_name: None, sender_guid: None, after_guid: None });
        let (_, conv, from) = resolve_target(&chats, &ChatTarget::Chat(bob.clone()), me, &reg, &[]).unwrap();
        assert_eq!(from, me);
        assert_eq!(conv.participants, vec!["mailto:bob@x.com".to_string(), me.to_string()]);
        // new conversations (nothing remembered, or a bare chat id)
        let (_, _, from) = resolve_target(&HashMap::new(), &ChatTarget::Participants(vec!["bob@x.com".into()]), me, &reg, &[]).unwrap();
        assert_eq!(from, me);
        let (_, _, from) = resolve_target(&HashMap::new(), &ChatTarget::Chat(bob.clone()), me, &reg, &[]).unwrap();
        assert_eq!(from, me);
        // a group we are in under another handle (spelled differently):
        // that handle, matched normalized
        let g = ChatId::group("G2");
        chats.insert(g.clone(), ConversationData { participants: vec!["mailto:bob@x.com".into(), "mailto:carol@x.com".into(), "mailto:Me@iCloud.example".into()], cv_name: None, sender_guid: Some("G2".into()), after_guid: None });
        let (_, _, from) = resolve_target(&chats, &ChatTarget::Chat(g), me, &reg, &[]).unwrap();
        assert_eq!(from, icloud);

        // the peer writing to two of our addresses (with a gid the Mac reuses
        // across 1:1 chats) is the 1:1 on receive, and replies go out 1:1
        let three = ConversationData {
            participants: vec!["mailto:bob@x.com".into(), me.into(), icloud.into()],
            cv_name: None,
            sender_guid: Some("B5E489CA-0000".into()),
            after_guid: None,
        };
        let id = convert::chat_id_for(&three, Some("mailto:bob@x.com"), &reg);
        assert_eq!(id, bob);
        let mut chats = HashMap::new();
        chats.insert(id, three);
        for target in [ChatTarget::Chat(bob.clone()), ChatTarget::Participants(vec!["bob@x.com".into()])] {
            let (id, conv, from) = resolve_target(&chats, &target, me, &reg, &[]).unwrap();
            assert_eq!((id, from.as_str()), (bob.clone(), me), "{target:?}");
            assert_eq!(conv.participants, vec!["mailto:bob@x.com".to_string(), me.to_string()], "two participants: rustpush sends it 1:1");
        }

        // other_handles count as us for "who are the others" (matching the
        // receive path) but are never sent from
        let phone = ["tel:+12055550100".to_string()];
        let (id, conv, from) = resolve_target(&HashMap::new(), &ChatTarget::Participants(vec!["bob@x.com".into(), "+1 205 555 0100".into()]), me, &reg, &phone).unwrap();
        assert_eq!((id, from.as_str()), (bob.clone(), me));
        assert_eq!(conv.participants, vec!["mailto:bob@x.com".to_string(), me.to_string()]);
        let mut chats = HashMap::new();
        chats.insert(bob.clone(), ConversationData { participants: vec!["mailto:bob@x.com".into(), phone[0].clone()], cv_name: None, sender_guid: None, after_guid: None });
        let (_, conv, from) = resolve_target(&chats, &ChatTarget::Chat(bob.clone()), me, &reg, &phone).unwrap();
        assert_eq!(from, me, "a chat the phone started replies from the configured sender");
        assert_eq!(conv.participants, vec!["mailto:bob@x.com".to_string(), me.to_string()]);
        assert!(matches!(resolve_target(&chats, &ChatTarget::Participants(phone.to_vec()), me, &reg, &phone), Err(EngineError::InvalidInput(_))));
    }

    /// No hwconfig, no state: the engine starts, reports NeedsLogin, refuses
    /// work with clear errors, and touches no network. (Installs the global
    /// keystore, which is fine: tests never use it.)
    #[tokio::test]
    async fn starts_without_hwconfig() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = DaemonConfig::new(dir.path().to_path_buf());
        let e = RustpushEngine::start(cfg, StateKey::from_bytes([1; 32])).await.unwrap();
        assert_eq!(e.connection_state().await, ConnectionState::NeedsLogin);
        assert!(matches!(e.login("a@b.c", "pw").await, Err(EngineError::NoHardwareConfig)));
        assert!(matches!(e.send_text(&ChatTarget::Participants(vec!["x@y.z".into()]), "hi", None).await, Err(EngineError::NotLoggedIn)));
        assert!(matches!(e.submit_two_factor("123456").await, Err(EngineError::NoPendingTwoFactor)));
        assert!(matches!(e.register().await, Err(EngineError::NotLoggedIn)));
        assert!(matches!(e.lookup_handles(vec!["bob".into()]).await, Err(EngineError::InvalidHandle(_))));
        assert!(matches!(e.download_attachment("{}", &dir.path().join("x"), 10).await, Err(EngineError::InvalidInput(_))));
        e.shutdown().await;
        assert!(dir.path().join(crate::state::STATE_FILE).exists(), "shutdown persists state");

        // Restart with the wrong key refuses rather than clobbering.
        let cfg = DaemonConfig::new(dir.path().to_path_buf());
        assert!(matches!(RustpushEngine::start(cfg, StateKey::from_bytes([2; 32])).await, Err(EngineError::StateKeyMismatch(_))));
    }

    /// An interactive login that succeeds while the silent re-login loop
    /// sleeps after a failure supersedes it, and the loop must leave the
    /// interactive login's `ids = Ready` alone (not "registering" forever).
    #[tokio::test(start_paused = true)]
    async fn superseded_relogin_keeps_ready_after_sleep() {
        fn attempt(i: &Arc<Inner>, _epoch: u64) -> Pin<Box<dyn Future<Output = EngineResult<LoginOutcome>> + Send + '_>> {
            let i2 = i.clone();
            Box::pin(async move {
                // the interactive login completes during the retry sleep
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    i2.update_status(|s| s.ids = Phase::Ready);
                    i2.login_epoch.fetch_add(1, Ordering::SeqCst);
                });
                Err(EngineError::Anisette("mac asleep".into()))
            })
        }
        let dir = tempfile::tempdir().unwrap();
        let cfg = DaemonConfig::new(dir.path().to_path_buf());
        let e = RustpushEngine::start(cfg, StateKey::from_bytes([3; 32])).await.unwrap();
        e.inner.silent_relogin_with(attempt).await;
        assert_eq!(lock(&e.inner.status).0.ids, Phase::Ready);
        assert!(!e.inner.relogin_running.load(Ordering::SeqCst));
        e.shutdown().await;
    }

    /// m2: a keystore blob that does not parse must fail startup, not turn
    /// into an empty keystore that the next save would make permanent.
    #[test]
    fn unparseable_keystore_is_an_error() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let bogus = plist::Value::String("not a keystore".into());
        let r = install_keystore(Some(&bogus), Arc::new(StdMutex::new(None)), tx);
        assert!(matches!(r, Err(EngineError::State(m)) if m.contains("keystore")));
    }

    /// The MMCS download sink stops at the cap and never pre-allocates the
    /// sender's claimed size.
    #[test]
    fn capped_writer() {
        use std::io::Write;
        let mut w = CappedWriter::new(super::super::MAX_ATTACHMENT_BYTES, 1 << 40);
        assert!(w.buf.capacity() <= DOWNLOAD_PREALLOC_MAX);
        let mut w2 = CappedWriter::new(5, 1 << 40);
        assert!(w2.buf.capacity() <= 5);
        w2.write_all(b"abc").unwrap();
        assert!(w2.write_all(b"def").is_err());
        assert!(w2.exceeded);
        assert_eq!((w2.buf.as_slice(), w2.received), (&b"abc"[..], 6));
        w.write_all(b"x").unwrap();
        assert!(!w.exceeded);
    }

    /// Inline attachments need no network: download writes the bytes
    /// atomically to `dest`.
    #[tokio::test]
    async fn download_inline_attachment() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = DaemonConfig::new(dir.path().join("state"));
        let e = RustpushEngine::start(cfg, StateKey::from_bytes([5; 32])).await.unwrap();
        let att = RpAttachment {
            a_type: AttachmentType::Inline(b"inline bytes".to_vec()),
            part: 0,
            uti_type: "public.plain-text".into(),
            mime: "text/plain".into(),
            name: "a.txt".into(),
            iris: false,
        };
        let dest = dir.path().join("a.txt");
        e.download_attachment(&convert::attachment_locator(&att), &dest, 12).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"inline bytes");
        // over the cap: refused, nothing written
        let small = dir.path().join("b.txt");
        let r = e.download_attachment(&convert::attachment_locator(&att), &small, 11).await;
        assert!(matches!(&r, Err(EngineError::InvalidInput(m)) if m.contains("too large")), "{r:?}");
        assert!(!small.exists());
        // MMCS attachments need APNs
        let mmcs = RpAttachment { a_type: AttachmentType::MMCS(MMCSFile { signature: vec![1], object: "o".into(), url: "https://x".into(), key: vec![0; 32], size: 1 }), ..att };
        assert!(matches!(e.download_attachment(&convert::attachment_locator(&mmcs), &dest, 10).await, Err(EngineError::NotLoggedIn)));
        // a peer announcing a huge MMCS file is refused before any allocation
        let huge = RpAttachment { a_type: AttachmentType::MMCS(MMCSFile { signature: vec![1], object: "o".into(), url: "https://x".into(), key: vec![0; 32], size: 1 << 40 }), ..mmcs };
        let r = e.download_attachment(&convert::attachment_locator(&huge), &dest, super::super::MAX_ATTACHMENT_BYTES).await;
        assert!(matches!(&r, Err(EngineError::InvalidInput(m)) if m.contains("too large")), "{r:?}");
        e.shutdown().await;
    }

    /// n7: with store_credentials = false the account blob never reaches
    /// disk, and an existing one is removed at startup.
    #[tokio::test]
    async fn store_credentials_false_drops_account() {
        let dir = tempfile::tempdir().unwrap();
        let key = StateKey::from_bytes([7; 32]);
        let store = StateStore::new(dir.path().join(crate::state::STATE_FILE), key.clone());
        let s = SavedState { account: Some(plist::Value::String("srp-hash".into())), ..Default::default() };
        store.save(&s).unwrap();
        let mut cfg = DaemonConfig::new(dir.path().to_path_buf());
        cfg.store_credentials = false;
        let e = RustpushEngine::start(cfg, key).await.unwrap();
        e.shutdown().await;
        let back: SavedState = store.load().unwrap().unwrap();
        assert!(back.account.is_none());
    }
}
