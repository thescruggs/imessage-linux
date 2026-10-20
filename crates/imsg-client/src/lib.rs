//! D-Bus client for `org.imessage.Daemon1` (see docs/DBUS.md), shared by
//! the `imsg` command-line client and `imsg-tui`.
//!
//! [`Daemon1Proxy`] mirrors the interface 1:1 (methods, properties and
//! signals of docs/DBUS.md). [`Client`] wraps it: every call returns both the raw JSON
//! string (for `--json`, printed unchanged) and the parsed model type, and
//! every error becomes a [`CliError`] with a readable message and an exit
//! code (see [`exit`]).

use std::fmt;
use std::time::Duration;

use imessage_common::model::{Chat, ConnectionState, Message, RegistrationState};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use zbus::DBusError as _;
use zbus::proxy;

pub const BUS_NAME: &str = "org.imessage.Daemon1";
const ERROR_PREFIX: &str = "org.imessage.Daemon1.Error.";

/// Hint for a daemon that is not on the bus.
#[cfg(not(windows))]
const NOT_RUNNING: &str = "imessaged is not running (start it with `systemctl --user start imessaged` or run `imessaged`)";
/// No service manager integration on Windows yet (Task Scheduler: phase W3).
#[cfg(windows)]
const NOT_RUNNING: &str = "imessaged is not running (start it by running `imessaged`)";

/// Process exit codes (documented in docs/CLI.md).
pub mod exit {
    pub const OK: u8 = 0;
    /// Any other error.
    pub const ERROR: u8 = 1;
    /// Bad command line / invalid argument (also the daemon's
    /// `InvalidInput` / `InvalidHandle`). clap uses 2 as well.
    pub const USAGE: u8 = 2;
    /// No account: `NotLoggedIn` / `NoHardwareConfig`.
    pub const NOT_LOGGED_IN: u8 = 3;
    /// A two-factor code is needed (or the codes given were wrong).
    pub const TWO_FACTOR: u8 = 4;
    /// The daemon is not running / not reachable on the bus.
    pub const NO_DAEMON: u8 = 5;
    /// No such chat / message / attachment.
    pub const NOT_FOUND: u8 = 6;
    /// Temporary failure (network, anisette, Apple); retry later.
    pub const TRANSIENT: u8 = 7;
}

/// An error with the exit code it maps to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    pub code: u8,
    pub message: String,
    /// `org.imessage.Daemon1.Error.<name>` short name, if the daemon sent one.
    pub daemon_error: Option<String>,
}

impl CliError {
    pub fn new(code: u8, message: impl Into<String>) -> CliError {
        CliError { code, message: message.into(), daemon_error: None }
    }
    pub fn usage(message: impl Into<String>) -> CliError {
        CliError::new(exit::USAGE, message)
    }
    pub fn is_daemon(&self, name: &str) -> bool {
        self.daemon_error.as_deref() == Some(name)
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

pub type Result<T> = std::result::Result<T, CliError>;

/// Map a D-Bus error name + message to a [`CliError`]. Pure, for tests.
pub fn map_error_name(name: &str, msg: Option<&str>) -> CliError {
    let detail = msg.unwrap_or("").trim();
    let with = |code: u8, text: &str| CliError { code, message: combine(text, detail), daemon_error: None };
    if let Some(short) = name.strip_prefix(ERROR_PREFIX) {
        let mut e = match short {
            "NotLoggedIn" => with(exit::NOT_LOGGED_IN, "not logged in (run `imsg login`)"),
            "NoHardwareConfig" => with(exit::NOT_LOGGED_IN, "no Mac hardware config imported (run `imsg hwconfig import <file>`)"),
            "NoPendingTwoFactor" => with(exit::TWO_FACTOR, "no login is waiting for a two-factor code (run `imsg login`)"),
            "BadTwoFactorCode" => with(exit::TWO_FACTOR, "wrong two-factor code"),
            "Login" => with(exit::ERROR, "login failed"),
            "Transient" => with(exit::TRANSIENT, "temporary failure, try again later"),
            "InvalidHandle" => with(exit::USAGE, "invalid handle (expected an email address or phone number)"),
            "ChatNotFound" => with(exit::NOT_FOUND, "unknown chat"),
            "InvalidInput" => with(exit::USAGE, "invalid input"),
            "Unsupported" => with(exit::ERROR, "not supported"),
            "State" | "StateKeyMismatch" => with(exit::ERROR, "daemon state problem (see docs/DAEMON.md, recovery)"),
            "Protocol" => with(exit::ERROR, "iMessage protocol error"),
            "NotFound" => with(exit::NOT_FOUND, "not found"),
            "Store" => with(exit::ERROR, "daemon storage error"),
            _ => with(exit::ERROR, &format!("daemon error {short}")),
        };
        e.daemon_error = Some(short.to_string());
        return e;
    }
    match name {
        "org.freedesktop.DBus.Error.ServiceUnknown" | "org.freedesktop.DBus.Error.NameHasNoOwner" => with(exit::NO_DAEMON, NOT_RUNNING),
        "org.freedesktop.DBus.Error.Spawn.ChildExited"
        | "org.freedesktop.DBus.Error.Spawn.Failed"
        | "org.freedesktop.DBus.Error.Spawn.ExecFailed"
        | "org.freedesktop.DBus.Error.Spawn.FailedToSetup" => {
            with(exit::NO_DAEMON, "imessaged could not be started by D-Bus activation (check `journalctl --user -u imessaged`)")
        }
        "org.freedesktop.DBus.Error.NoReply" | "org.freedesktop.DBus.Error.Timeout" | "org.freedesktop.DBus.Error.TimedOut" => {
            with(exit::NO_DAEMON, "imessaged did not reply in time (it may still be starting; retry)")
        }
        "org.freedesktop.DBus.Error.UnknownMethod" | "org.freedesktop.DBus.Error.UnknownInterface" | "org.freedesktop.DBus.Error.UnknownObject" => {
            with(exit::ERROR, "imessaged does not support this call (version mismatch?)")
        }
        other => with(exit::ERROR, &format!("D-Bus error {other}")),
    }
}

/// `text` is `label (hint)`; avoid repeating the label when the daemon's
/// detail already starts with it.
fn combine(text: &str, detail: &str) -> String {
    if detail.is_empty() {
        return text.to_string();
    }
    let (label, hint) = match text.find(" (") {
        Some(i) => (&text[..i], &text[i..]),
        None => (text, ""),
    };
    if detail.to_lowercase().starts_with(&label.to_lowercase()) { format!("{detail}{hint}") } else { format!("{text}: {detail}") }
}

/// Map any zbus error.
pub fn map_zbus(e: zbus::Error) -> CliError {
    match e {
        zbus::Error::MethodError(name, msg, _) => map_error_name(name.as_str(), msg.as_deref()),
        zbus::Error::FDO(fdo) => {
            let name = fdo.name().to_string();
            let text = fdo.description().map(str::to_string);
            map_error_name(&name, text.as_deref())
        }
        zbus::Error::InputOutput(io) => CliError::new(exit::NO_DAEMON, format!("cannot reach the D-Bus bus: {io}")),
        zbus::Error::Address(a) => CliError::new(exit::NO_DAEMON, format!("bad D-Bus address: {a}")),
        zbus::Error::Handshake(h) => CliError::new(exit::NO_DAEMON, format!("D-Bus handshake failed: {h}")),
        other => CliError::new(exit::ERROR, format!("D-Bus: {other}")),
    }
}

#[proxy(
    interface = "org.imessage.Daemon1",
    default_service = "org.imessage.Daemon1",
    default_path = "/org/imessage/Daemon1",
    gen_blocking = false
)]
pub trait Daemon1 {
    fn login(&self, apple_id: &str, password: &str) -> zbus::Result<String>;
    fn submit_two_factor(&self, code: &str) -> zbus::Result<String>;
    fn logout(&self) -> zbus::Result<()>;
    fn get_status(&self) -> zbus::Result<String>;
    fn list_chats(&self, limit: u32, offset: u32) -> zbus::Result<String>;
    fn get_chat(&self, chat_id: &str) -> zbus::Result<String>;
    fn list_messages(&self, chat_id: &str, before_cursor: &str, limit: u32) -> zbus::Result<String>;
    fn search(&self, query: &str, limit: u32) -> zbus::Result<String>;
    fn send_text(&self, chat_id_or_empty: &str, participants_json: &str, text: &str, reply_to: &str) -> zbus::Result<String>;
    fn send_file(&self, chat_id_or_empty: &str, participants_json: &str, path: &str, mime: &str) -> zbus::Result<String>;
    fn send_text_as(&self, chat_id_or_empty: &str, participants_json: &str, text: &str, reply_to: &str, service: &str) -> zbus::Result<String>;
    fn send_file_as(&self, chat_id_or_empty: &str, participants_json: &str, path: &str, mime: &str, service: &str) -> zbus::Result<String>;
    fn enable_sms_forwarding(&self, enable: bool) -> zbus::Result<String>;
    fn get_sms_forwarding(&self) -> zbus::Result<String>;
    fn reregister(&self) -> zbus::Result<String>;
    fn react(&self, chat_id: &str, message_id: &str, kind: &str, remove: bool) -> zbus::Result<()>;
    fn edit(&self, chat_id: &str, message_id: &str, text: &str) -> zbus::Result<()>;
    fn unsend(&self, chat_id: &str, message_id: &str) -> zbus::Result<()>;
    fn set_typing(&self, chat_id: &str, typing: bool) -> zbus::Result<()>;
    fn mark_read(&self, chat_id: &str, up_to_message_id: &str) -> zbus::Result<()>;
    fn lookup_handles(&self, handles_json: &str) -> zbus::Result<String>;
    fn download_attachment(&self, attachment_id: &str) -> zbus::Result<String>;
    fn import_chat_background(&self, chat_id: &str, path: &str) -> zbus::Result<String>;
    fn set_chat_pinned(&self, chat_id: &str, pinned: bool, order: i32) -> zbus::Result<String>;
    fn reorder_pins(&self, chat_ids_json: &str) -> zbus::Result<String>;
    fn import_messages(&self, batch_json: &str) -> zbus::Result<String>;
    fn purge_invalid_chats(&self, dry_run: bool) -> zbus::Result<String>;
    fn get_handle_profile(&self, handle: &str) -> zbus::Result<String>;
    fn version(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn connection_state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn registered(&self) -> zbus::Result<bool>;

    #[zbus(signal)]
    fn message_received(&self, chat_id: String, message_json: String) -> zbus::Result<()>;
    #[zbus(signal)]
    fn message_updated(&self, chat_id: String, message_json: String) -> zbus::Result<()>;
    #[zbus(signal)]
    fn chat_updated(&self, chat_json: String) -> zbus::Result<()>;
    #[zbus(signal)]
    fn typing(&self, chat_id: String, handle: String, typing: bool) -> zbus::Result<()>;
    /// `ConnectionStateChanged` (named `_signal` so it does not clash with
    /// the `ConnectionState` property's change stream).
    #[zbus(signal, name = "ConnectionStateChanged")]
    fn connection_state_signal(&self, state: String) -> zbus::Result<()>;
    #[zbus(signal)]
    fn two_factor_required(&self) -> zbus::Result<()>;
    #[zbus(signal)]
    fn background_error(&self, message: String) -> zbus::Result<()>;
    #[zbus(signal)]
    fn handle_profile_updated(&self, handle: String, profile_json: String) -> zbus::Result<()>;
    #[zbus(signal)]
    fn sms_forwarding_changed(&self, state_json: String) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------
// JSON payloads that have no model type
// ---------------------------------------------------------------------------

/// `GetStatus`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Status {
    pub version: String,
    pub connection: ConnectionState,
    pub registered: bool,
    pub registration: RegistrationState,
    pub unread: Option<u32>,
    /// Older daemons do not report it.
    #[serde(default)]
    pub profiles: Option<imessage_common::model::ProfilesStatus>,
    /// Text Message Forwarding; older daemons do not report it.
    #[serde(default)]
    pub sms: Option<imessage_common::model::SmsForwarding>,
    /// The daemon's `mac_host` setting (`imsg sync-mac`); older daemons
    /// do not report it.
    #[serde(default)]
    pub mac_host: Option<String>,
}

/// `ListMessages`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MessagePage {
    pub messages: Vec<Message>,
    pub next_cursor: Option<String>,
}

/// `SendText` / `SendFile`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SentMessage {
    pub id: String,
    pub chat_id: String,
    pub sent_at: u64,
    pub sender: imessage_common::model::Handle,
    #[serde(default)]
    pub service: imessage_common::model::Service,
}

/// `LookupHandles` element.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Reachability {
    pub handle: String,
    pub reachable: bool,
}

/// `Login` / `SubmitTwoFactor` outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginOutcome {
    Ok,
    TwoFactorRequired,
}

impl LoginOutcome {
    pub fn parse(s: &str) -> Result<LoginOutcome> {
        match s {
            "ok" => Ok(LoginOutcome::Ok),
            "two_factor_required" => Ok(LoginOutcome::TwoFactorRequired),
            other => Err(CliError::new(exit::ERROR, format!("unexpected login result {other:?} from imessaged"))),
        }
    }
}

/// A daemon reply: the exact JSON text plus its parsed form.
#[derive(Debug, Clone)]
pub struct Json<T> {
    pub raw: String,
    pub value: T,
}

pub fn parse<T: DeserializeOwned>(what: &str, raw: String) -> Result<Json<T>> {
    match serde_json::from_str(&raw) {
        Ok(value) => Ok(Json { raw, value }),
        Err(e) => Err(CliError::new(exit::ERROR, format!("imessaged sent malformed {what} JSON ({e})"))),
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Which bus to talk to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bus {
    Session,
    Address(String),
}

pub struct Client {
    pub conn: zbus::Connection,
    pub proxy: Daemon1Proxy<'static>,
}

/// Timeout for connecting to the bus itself.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Login, SubmitTwoFactor and SendFile do slow work in the daemon (the
/// anisette command alone may take 30 s, then GSA / IDS round trips, or an
/// upload). Same value as kmessages' `LongTimeoutMs`.
pub const LONG_CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Await a slow daemon call for at most [`LONG_CALL_TIMEOUT`].
async fn long_call<T>(what: &str, fut: impl std::future::Future<Output = zbus::Result<T>>) -> Result<T> {
    long_call_within(LONG_CALL_TIMEOUT, what, fut).await
}

async fn long_call_within<T>(limit: Duration, what: &str, fut: impl std::future::Future<Output = zbus::Result<T>>) -> Result<T> {
    match tokio::time::timeout(limit, fut).await {
        Ok(r) => r.map_err(map_zbus),
        Err(_) => Err(CliError::new(
            exit::TRANSIENT,
            format!("imessaged did not finish {what} within {} s; it may still be working, check `imsg status`", limit.as_secs()),
        )),
    }
}

impl Client {
    pub async fn connect(bus: &Bus) -> Result<Client> {
        let builder = match bus {
            Bus::Session => zbus::connection::Builder::session(),
            Bus::Address(a) => zbus::connection::Builder::address(a.as_str()),
        }
        .map_err(|e| match bus {
            Bus::Session => CliError::new(exit::NO_DAEMON, format!("no D-Bus session bus ({e}); is DBUS_SESSION_BUS_ADDRESS set?")),
            Bus::Address(a) => CliError::new(exit::NO_DAEMON, format!("bad bus address {a:?}: {e}")),
        })?;
        let conn = match tokio::time::timeout(CONNECT_TIMEOUT, builder.build()).await {
            Ok(r) => r.map_err(|e| {
                let mut ce = map_zbus(e);
                ce.code = exit::NO_DAEMON;
                ce.message = format!("cannot connect to the D-Bus bus: {}", ce.message);
                ce
            })?,
            Err(_) => return Err(CliError::new(exit::NO_DAEMON, "timed out connecting to the D-Bus bus")),
        };
        let proxy = Daemon1Proxy::builder(&conn)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .map_err(map_zbus)?;
        Ok(Client { conn, proxy })
    }

    /// Does anyone own `org.imessage.Daemon1` right now (no activation)?
    pub async fn daemon_present(&self) -> bool {
        let Ok(dbus) = zbus::fdo::DBusProxy::new(&self.conn).await else { return false };
        let Ok(name) = zbus::names::BusName::try_from(BUS_NAME) else { return false };
        dbus.name_has_owner(name).await.unwrap_or(false)
    }

    pub async fn login(&self, apple_id: &str, password: &str) -> Result<LoginOutcome> {
        LoginOutcome::parse(&long_call("the login", self.proxy.login(apple_id, password)).await?)
    }

    pub async fn submit_two_factor(&self, code: &str) -> Result<LoginOutcome> {
        LoginOutcome::parse(&long_call("checking the code", self.proxy.submit_two_factor(code)).await?)
    }

    pub async fn logout(&self) -> Result<()> {
        self.proxy.logout().await.map_err(map_zbus)
    }

    pub async fn status(&self) -> Result<Json<Status>> {
        parse("status", self.proxy.get_status().await.map_err(map_zbus)?)
    }

    pub async fn list_chats(&self, limit: u32, offset: u32) -> Result<Json<Vec<Chat>>> {
        parse("chat list", self.proxy.list_chats(limit, offset).await.map_err(map_zbus)?)
    }

    /// All chats (pages of 500).
    pub async fn all_chats(&self) -> Result<Vec<Chat>> {
        let mut out = Vec::new();
        loop {
            let page = self.list_chats(500, out.len() as u32).await?.value;
            let n = page.len();
            out.extend(page);
            if n < 500 {
                return Ok(out);
            }
        }
    }

    /// A contact's shared name and photo (`None`: nothing shared).
    pub async fn get_handle_profile(&self, handle: &str) -> Result<Json<Option<imessage_common::model::HandleProfile>>> {
        parse("profile", self.proxy.get_handle_profile(handle).await.map_err(map_zbus)?)
    }

    /// `Ok(None)` for `NotFound`.
    pub async fn get_chat(&self, chat_id: &str) -> Result<Option<Json<Chat>>> {
        match self.proxy.get_chat(chat_id).await.map_err(map_zbus) {
            Ok(raw) => parse("chat", raw).map(Some),
            Err(e) if e.is_daemon("NotFound") => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn list_messages(&self, chat_id: &str, before: &str, limit: u32) -> Result<Json<MessagePage>> {
        parse("message page", self.proxy.list_messages(chat_id, before, limit).await.map_err(map_zbus)?)
    }

    pub async fn search(&self, query: &str, limit: u32) -> Result<Json<Vec<Message>>> {
        parse("search result", self.proxy.search(query, limit).await.map_err(map_zbus)?)
    }

    pub async fn send_text(&self, target: &Target, text: &str, reply_to: &str) -> Result<Json<SentMessage>> {
        let (chat, participants) = target.args();
        parse("sent message", self.proxy.send_text(&chat, &participants, text, reply_to).await.map_err(map_zbus)?)
    }

    pub async fn send_file(&self, target: &Target, path: &str, mime: &str) -> Result<Json<SentMessage>> {
        let (chat, participants) = target.args();
        parse("sent message", long_call("sending the file", self.proxy.send_file(&chat, &participants, path, mime)).await?)
    }

    /// `SendTextAs`: `service` = `imessage` / `sms`.
    pub async fn send_text_as(&self, target: &Target, text: &str, reply_to: &str, service: &str) -> Result<Json<SentMessage>> {
        let (chat, participants) = target.args();
        parse("sent message", self.proxy.send_text_as(&chat, &participants, text, reply_to, service).await.map_err(map_zbus)?)
    }

    pub async fn send_file_as(&self, target: &Target, path: &str, mime: &str, service: &str) -> Result<Json<SentMessage>> {
        let (chat, participants) = target.args();
        parse("sent message", long_call("sending the file", self.proxy.send_file_as(&chat, &participants, path, mime, service)).await?)
    }

    pub async fn enable_sms_forwarding(&self, enable: bool) -> Result<Json<imessage_common::model::SmsForwarding>> {
        parse("sms forwarding", long_call("asking the iPhone", self.proxy.enable_sms_forwarding(enable)).await?)
    }

    /// `Reregister`: register with iMessage again now.
    pub async fn reregister(&self) -> Result<Json<RegistrationState>> {
        parse("registration", long_call("registering", self.proxy.reregister()).await?)
    }

    pub async fn sms_forwarding(&self) -> Result<Json<imessage_common::model::SmsForwarding>> {
        parse("sms forwarding", self.proxy.get_sms_forwarding().await.map_err(map_zbus)?)
    }

    pub async fn react(&self, chat_id: &str, message_id: &str, kind: &str, remove: bool) -> Result<()> {
        self.proxy.react(chat_id, message_id, kind, remove).await.map_err(map_zbus)
    }

    pub async fn edit(&self, chat_id: &str, message_id: &str, text: &str) -> Result<()> {
        self.proxy.edit(chat_id, message_id, text).await.map_err(map_zbus)
    }

    pub async fn unsend(&self, chat_id: &str, message_id: &str) -> Result<()> {
        self.proxy.unsend(chat_id, message_id).await.map_err(map_zbus)
    }

    pub async fn set_typing(&self, chat_id: &str, typing: bool) -> Result<()> {
        self.proxy.set_typing(chat_id, typing).await.map_err(map_zbus)
    }

    pub async fn mark_read(&self, chat_id: &str, up_to: &str) -> Result<()> {
        self.proxy.mark_read(chat_id, up_to).await.map_err(map_zbus)
    }

    pub async fn lookup_handles(&self, handles: &[String]) -> Result<Json<Vec<Reachability>>> {
        let arg = serde_json::to_string(handles).expect("strings serialize");
        parse("lookup result", self.proxy.lookup_handles(&arg).await.map_err(map_zbus)?)
    }

    pub async fn download_attachment(&self, id: &str) -> Result<String> {
        self.proxy.download_attachment(id).await.map_err(map_zbus)
    }

    pub async fn import_chat_background(&self, chat_id: &str, path: &str) -> Result<Json<Chat>> {
        parse("chat", self.proxy.import_chat_background(chat_id, path).await.map_err(map_zbus)?)
    }

    /// `SetChatPinned`: `order` = position among the pins (0 = first);
    /// `None` = last for a new pin, unchanged for a pinned one.
    pub async fn set_chat_pinned(&self, chat_id: &str, pinned: bool, order: Option<u32>) -> Result<Json<Chat>> {
        let order = order.and_then(|o| i32::try_from(o).ok()).unwrap_or(-1);
        parse("chat", self.proxy.set_chat_pinned(chat_id, pinned, order).await.map_err(map_zbus)?)
    }

    /// `ReorderPins`: these chats first, in order; returns all pinned chats.
    pub async fn reorder_pins(&self, chat_ids: &[String]) -> Result<Json<Vec<Chat>>> {
        let arg = serde_json::to_string(chat_ids).expect("strings serialize");
        parse("chat list", self.proxy.reorder_pins(&arg).await.map_err(map_zbus)?)
    }

    /// One `ImportMessages` batch (copies attachment files: slow).
    pub async fn import_messages(&self, batch_json: &str) -> Result<imessage_common::import::ImportResult> {
        parse("import result", long_call("the import batch", self.proxy.import_messages(batch_json)).await?).map(|j| j.value)
    }

    /// `PurgeInvalidChats`: the daemon's summary (see docs/DBUS.md).
    pub async fn purge_invalid_chats(&self, dry_run: bool) -> Result<Json<serde_json::Value>> {
        parse("purge summary", long_call("the purge", self.proxy.purge_invalid_chats(dry_run)).await?)
    }

    pub async fn version(&self) -> Result<String> {
        self.proxy.version().await.map_err(map_zbus)
    }

    pub async fn connection_state(&self) -> Result<String> {
        self.proxy.connection_state().await.map_err(map_zbus)
    }
}

/// Where a send goes: an existing chat, or a set of handles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Chat(String),
    Participants(Vec<String>),
}

impl Target {
    /// (`chat_id_or_empty`, `participants_json`) method arguments.
    pub fn args(&self) -> (String, String) {
        match self {
            Target::Chat(id) => (id.clone(), String::new()),
            Target::Participants(p) => (String::new(), serde_json::to_string(p).expect("strings serialize")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn long_calls_time_out_with_a_hint() {
        assert_eq!(LONG_CALL_TIMEOUT, Duration::from_secs(120));
        let slow = async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            zbus::Result::Ok(())
        };
        let e = long_call_within(Duration::from_millis(10), "the login", slow).await.unwrap_err();
        assert_eq!(e.code, exit::TRANSIENT);
        assert!(e.message.contains("the login") && e.message.contains("imsg status"), "{e}");
        assert_eq!(long_call_within(Duration::from_secs(1), "x", async { zbus::Result::Ok(7) }).await.unwrap(), 7);
    }

    #[test]
    fn error_names_map_to_exit_codes() {
        let cases = [
            ("org.imessage.Daemon1.Error.NotLoggedIn", exit::NOT_LOGGED_IN),
            ("org.imessage.Daemon1.Error.NoHardwareConfig", exit::NOT_LOGGED_IN),
            ("org.imessage.Daemon1.Error.BadTwoFactorCode", exit::TWO_FACTOR),
            ("org.imessage.Daemon1.Error.NoPendingTwoFactor", exit::TWO_FACTOR),
            ("org.imessage.Daemon1.Error.InvalidHandle", exit::USAGE),
            ("org.imessage.Daemon1.Error.InvalidInput", exit::USAGE),
            ("org.imessage.Daemon1.Error.NotFound", exit::NOT_FOUND),
            ("org.imessage.Daemon1.Error.ChatNotFound", exit::NOT_FOUND),
            ("org.imessage.Daemon1.Error.Transient", exit::TRANSIENT),
            ("org.imessage.Daemon1.Error.Login", exit::ERROR),
            ("org.imessage.Daemon1.Error.Protocol", exit::ERROR),
            ("org.imessage.Daemon1.Error.Store", exit::ERROR),
            ("org.imessage.Daemon1.Error.SomethingNew", exit::ERROR),
            ("org.freedesktop.DBus.Error.ServiceUnknown", exit::NO_DAEMON),
            ("org.freedesktop.DBus.Error.NameHasNoOwner", exit::NO_DAEMON),
            ("org.freedesktop.DBus.Error.NoReply", exit::NO_DAEMON),
            ("org.freedesktop.DBus.Error.Failed", exit::ERROR),
        ];
        for (name, code) in cases {
            assert_eq!(map_error_name(name, Some("x")).code, code, "{name}");
        }
    }

    #[test]
    fn error_messages_are_readable() {
        let e = map_error_name("org.imessage.Daemon1.Error.NotFound", Some("not found: chat nobody"));
        assert_eq!(e.message, "not found: chat nobody");
        assert!(e.is_daemon("NotFound"));
        let e = map_error_name("org.imessage.Daemon1.Error.NotLoggedIn", Some("not logged in"));
        assert_eq!(e.message, "not logged in (run `imsg login`)");
        let e = map_error_name("org.imessage.Daemon1.Error.Login", Some("Apple said no"));
        assert_eq!(e.message, "login failed: Apple said no");
        let e = map_error_name("org.imessage.Daemon1.Error.NotLoggedIn", None);
        assert!(e.message.contains("imsg login"), "{}", e.message);
        let e = map_error_name("org.freedesktop.DBus.Error.ServiceUnknown", Some("The name is not activatable"));
        assert!(e.message.contains("not running"));
        assert_eq!(e.daemon_error, None);
    }

    #[test]
    fn zbus_errors_map() {
        let io = zbus::Error::InputOutput(std::sync::Arc::new(std::io::Error::other("refused")));
        assert_eq!(map_zbus(io).code, exit::NO_DAEMON);
        assert_eq!(map_zbus(zbus::Error::Unsupported).code, exit::ERROR);
    }

    #[test]
    fn target_args() {
        assert_eq!(Target::Chat("group:X".into()).args(), ("group:X".into(), String::new()));
        assert_eq!(Target::Participants(vec!["a@b.c".into(), "tel:+1".into()]).args(), (String::new(), r#"["a@b.c","tel:+1"]"#.into()));
    }

    #[test]
    fn payloads_parse() {
        let st: Status = parse(
            "s",
            r#"{"version":"0.1.0","connection":{"state":"error","detail":"x"},"registered":false,"registration":{"handles":[],"registered_at":null,"expires_at":null},"unread":null}"#.into(),
        )
        .unwrap()
        .value;
        assert_eq!(st.connection, ConnectionState::Error("x".into()));
        assert_eq!(st.unread, None);
        let p: MessagePage = parse("p", r#"{"messages":[],"next_cursor":null}"#.into()).unwrap().value;
        assert!(p.messages.is_empty() && p.next_cursor.is_none());
        assert_eq!(parse::<Status>("status", "nope".into()).unwrap_err().code, exit::ERROR);
        assert_eq!(LoginOutcome::parse("two_factor_required").unwrap(), LoginOutcome::TwoFactorRequired);
        assert!(LoginOutcome::parse("maybe").is_err());
    }
}
