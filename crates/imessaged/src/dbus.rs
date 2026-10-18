//! D-Bus API: `org.imessage.Daemon1` at `/org/imessage/Daemon1` on the
//! session bus (see docs/DBUS.md for the full contract).
//!
//! Thin layer: every method forwards to a [`Hub`] operation, payloads are
//! JSON strings of `imessage_common::model` types, errors are
//! `org.imessage.Daemon1.Error.<Variant>`. Passwords and 2FA codes are only
//! passed through to the engine; nothing here logs arguments (zbus itself
//! logs message headers and body *signatures* only, at trace level).

use std::sync::Arc;

use serde::Serialize;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::{connection, interface};

use crate::engine::{EngineError, LoginOutcome};
use crate::hub::{DaemonSignal, Hub, OpError, connection_state_name};

pub const BUS_NAME: &str = "org.imessage.Daemon1";
pub const OBJECT_PATH: &str = "/org/imessage/Daemon1";
pub const INTERFACE: &str = "org.imessage.Daemon1";

/// Which bus to serve on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusChoice {
    /// `DBUS_SESSION_BUS_ADDRESS` / the user's session bus.
    Session,
    /// An explicit bus address (tests: a private `dbus-daemon`).
    Address(String),
}

/// D-Bus error names: `org.imessage.Daemon1.Error.<Variant>`.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.imessage.Daemon1.Error")]
pub enum DaemonError {
    #[zbus(error)]
    ZBus(zbus::Error),
    NotLoggedIn(String),
    NoHardwareConfig(String),
    NoPendingTwoFactor(String),
    BadTwoFactorCode(String),
    Login(String),
    Transient(String),
    InvalidHandle(String),
    ChatNotFound(String),
    InvalidInput(String),
    Unsupported(String),
    State(String),
    StateKeyMismatch(String),
    Protocol(String),
    NotFound(String),
    Store(String),
}

impl From<OpError> for DaemonError {
    fn from(e: OpError) -> Self {
        let msg = e.to_string();
        match e {
            OpError::Engine(e) => match e {
                EngineError::NotLoggedIn => DaemonError::NotLoggedIn(msg),
                EngineError::NoHardwareConfig => DaemonError::NoHardwareConfig(msg),
                EngineError::NoPendingTwoFactor => DaemonError::NoPendingTwoFactor(msg),
                EngineError::BadTwoFactorCode => DaemonError::BadTwoFactorCode(msg),
                EngineError::Login(_) => DaemonError::Login(msg),
                EngineError::Transient(_) | EngineError::Anisette(_) => DaemonError::Transient(msg),
                EngineError::InvalidHandle(_) => DaemonError::InvalidHandle(msg),
                EngineError::ChatNotFound(_) => DaemonError::ChatNotFound(msg),
                EngineError::InvalidInput(_) => DaemonError::InvalidInput(msg),
                EngineError::Unsupported(_) => DaemonError::Unsupported(msg),
                EngineError::State(_) => DaemonError::State(msg),
                EngineError::StateKeyMismatch(_) => DaemonError::StateKeyMismatch(msg),
                EngineError::Protocol(_) => DaemonError::Protocol(msg),
            },
            OpError::Store(_) => DaemonError::Store(msg),
            OpError::NotFound(_) => DaemonError::NotFound(msg),
            OpError::InvalidInput(_) => DaemonError::InvalidInput(msg),
        }
    }
}

type Result<T> = std::result::Result<T, DaemonError>;

fn to_json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("model types serialize")
}

fn parse_json_strings(what: &str, s: &str) -> Result<Vec<String>> {
    if s.trim().is_empty() {
        return Ok(vec![]);
    }
    serde_json::from_str(s).map_err(|e| DaemonError::InvalidInput(format!("{what}: expected a JSON array of strings ({e})")))
}

/// `""` / `"auto"` = let the daemon choose.
fn parse_service(s: &str) -> Result<Option<imessage_common::model::Service>> {
    match s.trim() {
        "" | "auto" => Ok(None),
        other => imessage_common::model::Service::parse(other)
            .map(Some)
            .ok_or_else(|| DaemonError::InvalidInput(format!("service: expected \"auto\", \"imessage\" or \"sms\", not {other:?}"))),
    }
}

fn outcome(o: LoginOutcome) -> String {
    match o {
        LoginOutcome::Ok => "ok",
        LoginOutcome::TwoFactorRequired => "two_factor_required",
    }
    .to_string()
}

/// The exported object.
pub struct Daemon1 {
    hub: Arc<Hub>,
}

#[interface(name = "org.imessage.Daemon1")]
impl Daemon1 {
    /// -> "ok" | "two_factor_required"
    async fn login(&self, apple_id: &str, password: &str) -> Result<String> {
        Ok(outcome(self.hub.login(apple_id, password).await?))
    }

    /// -> "ok" | "two_factor_required" (Apple wants another code)
    async fn submit_two_factor(&self, code: &str) -> Result<String> {
        Ok(outcome(self.hub.submit_two_factor(code).await?))
    }

    async fn logout(&self) -> Result<()> {
        Ok(self.hub.logout().await?)
    }

    async fn get_status(&self) -> String {
        to_json(&self.hub.status().await)
    }

    async fn list_chats(&self, limit: u32, offset: u32) -> Result<String> {
        Ok(to_json(&self.hub.list_chats(limit, offset).await?))
    }

    async fn get_chat(&self, chat_id: &str) -> Result<String> {
        Ok(to_json(&self.hub.get_chat(chat_id).await?))
    }

    async fn list_messages(&self, chat_id: &str, before_cursor: &str, limit: u32) -> Result<String> {
        Ok(to_json(&self.hub.list_messages(chat_id, before_cursor, limit).await?))
    }

    async fn search(&self, query: &str, limit: u32) -> Result<String> {
        Ok(to_json(&self.hub.search(query, limit).await?))
    }

    async fn send_text(&self, chat_id_or_empty: &str, participants_json: &str, text: &str, reply_to: &str) -> Result<String> {
        let participants = parse_json_strings("participants_json", participants_json)?;
        let target = Hub::target(chat_id_or_empty, &participants)?;
        let reply_to = (!reply_to.trim().is_empty()).then(|| imessage_common::model::MessageId::new(reply_to.trim()));
        Ok(to_json(&self.hub.send_text(target, text, reply_to).await?))
    }

    async fn send_file(&self, chat_id_or_empty: &str, participants_json: &str, path: &str, mime: &str) -> Result<String> {
        let participants = parse_json_strings("participants_json", participants_json)?;
        let target = Hub::target(chat_id_or_empty, &participants)?;
        Ok(to_json(&self.hub.send_file(target, path, mime).await?))
    }

    /// `SendText` over a chosen service: `service` = `""` / `"auto"` (as
    /// SendText: SMS when Text Message Forwarding is active and the
    /// recipient is not on iMessage), `"imessage"` or `"sms"`.
    async fn send_text_as(&self, chat_id_or_empty: &str, participants_json: &str, text: &str, reply_to: &str, service: &str) -> Result<String> {
        let participants = parse_json_strings("participants_json", participants_json)?;
        let target = Hub::target(chat_id_or_empty, &participants)?;
        let reply_to = (!reply_to.trim().is_empty()).then(|| imessage_common::model::MessageId::new(reply_to.trim()));
        Ok(to_json(&self.hub.send_text_as(target, text, reply_to, parse_service(service)?).await?))
    }

    /// `SendFile` over a chosen service (see SendTextAs; `"sms"` = MMS).
    async fn send_file_as(&self, chat_id_or_empty: &str, participants_json: &str, path: &str, mime: &str, service: &str) -> Result<String> {
        let participants = parse_json_strings("participants_json", participants_json)?;
        let target = Hub::target(chat_id_or_empty, &participants)?;
        Ok(to_json(&self.hub.send_file_as(target, path, mime, parse_service(service)?).await?))
    }

    /// Ask the iPhone to relay text messages here (`true`) or stop.
    /// -> SmsForwarding JSON.
    async fn enable_sms_forwarding(&self, enable: bool) -> Result<String> {
        Ok(to_json(&self.hub.enable_sms_forwarding(enable).await?))
    }

    /// -> SmsForwarding JSON (also in GetStatus).
    async fn get_sms_forwarding(&self) -> String {
        to_json(&self.hub.sms_forwarding().await)
    }

    /// Register with iMessage again now (picks up handles the account
    /// gained, e.g. the iPhone's number). -> RegistrationState JSON (as
    /// GetStatus.registration). Slow: 120 s timeout.
    async fn reregister(&self) -> Result<String> {
        Ok(to_json(&self.hub.reregister().await?))
    }

    async fn react(&self, chat_id: &str, message_id: &str, kind: &str, remove: bool) -> Result<()> {
        Ok(self.hub.react(chat_id, message_id, kind, remove).await?)
    }

    async fn edit(&self, chat_id: &str, message_id: &str, text: &str) -> Result<()> {
        Ok(self.hub.edit(chat_id, message_id, text).await?)
    }

    async fn unsend(&self, chat_id: &str, message_id: &str) -> Result<()> {
        Ok(self.hub.unsend(chat_id, message_id).await?)
    }

    async fn set_typing(&self, chat_id: &str, typing: bool) -> Result<()> {
        Ok(self.hub.set_typing(chat_id, typing).await?)
    }

    async fn mark_read(&self, chat_id: &str, up_to_message_id: &str) -> Result<()> {
        Ok(self.hub.mark_read(chat_id, up_to_message_id).await?)
    }

    async fn lookup_handles(&self, handles_json: &str) -> Result<String> {
        let handles = parse_json_strings("handles_json", handles_json)?;
        Ok(to_json(&self.hub.lookup_handles(handles).await?))
    }

    async fn download_attachment(&self, attachment_id: &str) -> Result<String> {
        Ok(self.hub.download_attachment(attachment_id).await?)
    }

    /// A contact's shared name and photo -> HandleProfile JSON, or `null`.
    async fn get_handle_profile(&self, handle: &str) -> Result<String> {
        Ok(to_json(&self.hub.get_handle_profile(handle).await?))
    }

    /// Set a chat's background locally from a file (not sent to anyone).
    /// -> the updated Chat JSON.
    async fn import_chat_background(&self, chat_id: &str, path: &str) -> Result<String> {
        Ok(to_json(&self.hub.import_chat_background(chat_id, path).await?))
    }

    /// One-time history import (docs/DBUS.md): `batch_json` is an
    /// `ImportBatch`, the result an `ImportResult`.
    async fn import_messages(&self, batch_json: &str) -> Result<String> {
        let batch = serde_json::from_str(batch_json).map_err(|e| DaemonError::InvalidInput(format!("batch_json: {e}")))?;
        Ok(to_json(&self.hub.import_messages(batch).await?))
    }

    /// Maintenance (docs/DBUS.md): remove chats whose id is not a valid
    /// `mailto:` / `tel:` / `group:` id with their messages, reactions,
    /// attachments and files. `dry_run` only reports. -> summary JSON.
    async fn purge_invalid_chats(&self, dry_run: bool) -> Result<String> {
        Ok(to_json(&self.hub.purge_invalid_chats(dry_run).await?))
    }

    fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    /// Snake-case state name; details via GetStatus.
    #[zbus(property)]
    async fn connection_state(&self) -> String {
        connection_state_name(&self.hub.engine().connection_state().await).to_string()
    }

    #[zbus(property)]
    async fn registered(&self) -> bool {
        self.hub.engine().registration_state().await.is_registered()
    }

    #[zbus(signal)]
    async fn message_received(emitter: &SignalEmitter<'_>, chat_id: &str, message_json: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn message_updated(emitter: &SignalEmitter<'_>, chat_id: &str, message_json: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn chat_updated(emitter: &SignalEmitter<'_>, chat_json: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn typing(emitter: &SignalEmitter<'_>, chat_id: &str, handle: &str, typing: bool) -> zbus::Result<()>;

    #[zbus(signal, name = "ConnectionStateChanged")]
    async fn connection_state_signal(emitter: &SignalEmitter<'_>, state: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn two_factor_required(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    /// Background problems not tied to a method call (registration
    /// rejected, state save failed, engine events dropped -> resync).
    #[zbus(signal)]
    async fn background_error(emitter: &SignalEmitter<'_>, message: &str) -> zbus::Result<()>;

    /// A contact's shared name / photo changed; `profile_json` is the
    /// HandleProfile JSON or `null` (they stopped sharing).
    #[zbus(signal)]
    async fn handle_profile_updated(emitter: &SignalEmitter<'_>, handle: &str, profile_json: &str) -> zbus::Result<()>;

    /// Text Message Forwarding changed; `state_json` is SmsForwarding JSON.
    #[zbus(signal)]
    async fn sms_forwarding_changed(emitter: &SignalEmitter<'_>, state_json: &str) -> zbus::Result<()>;
}

/// Emits [`DaemonSignal`]s on the bus.
#[derive(Clone)]
pub(crate) struct Emitter {
    iface: InterfaceRef<Daemon1>,
}

impl Emitter {
    pub(crate) async fn emit(&self, sig: &DaemonSignal) {
        let e = self.iface.signal_emitter();
        let r = match sig {
            DaemonSignal::MessageReceived { chat_id, message } => Daemon1::message_received(e, chat_id.as_str(), &to_json(message)).await,
            DaemonSignal::MessageUpdated { chat_id, message } => Daemon1::message_updated(e, chat_id.as_str(), &to_json(message)).await,
            DaemonSignal::ChatUpdated(c) => Daemon1::chat_updated(e, &to_json(c)).await,
            DaemonSignal::Typing { chat_id, handle, typing } => Daemon1::typing(e, chat_id.as_str(), &handle.uri, *typing).await,
            DaemonSignal::ConnectionStateChanged(s) => {
                let r = Daemon1::connection_state_signal(e, connection_state_name(s)).await;
                let _ = self.iface.get().await.connection_state_changed(e).await;
                r
            }
            DaemonSignal::RegisteredChanged(_) => self.iface.get().await.registered_changed(e).await,
            DaemonSignal::TwoFactorRequired => Daemon1::two_factor_required(e).await,
            DaemonSignal::BackgroundError(m) => Daemon1::background_error(e, m).await,
            DaemonSignal::HandleProfileUpdated { handle, profile } => Daemon1::handle_profile_updated(e, handle, &to_json(profile)).await,
            DaemonSignal::SmsForwardingChanged(s) => Daemon1::sms_forwarding_changed(e, &to_json(s)).await,
        };
        if let Err(err) = r {
            log::debug!("D-Bus signal not sent: {err}");
        }
    }
}

/// Connect, export the object, claim the well-known name (fails if another
/// imessaged owns it) and attach the emitter to `hub`.
pub async fn serve(hub: Arc<Hub>, bus: &BusChoice) -> anyhow::Result<zbus::Connection> {
    let which = if *bus == BusChoice::Session { "session bus" } else { "bus" };
    let builder = match bus {
        BusChoice::Session => connection::Builder::session(),
        BusChoice::Address(a) => connection::Builder::address(a.as_str()),
    }
    .map_err(|e| anyhow::anyhow!("cannot connect to the D-Bus {which} (use --no-dbus to run without it): {e}"))?;
    let conn = builder
        .serve_at(OBJECT_PATH, Daemon1 { hub: hub.clone() })?
        .build()
        .await
        .map_err(|e| anyhow::anyhow!("cannot connect to the D-Bus {which}: {e}"))?;
    // DoNotQueue: a second imessaged must fail, not wait in line.
    use zbus::fdo::{RequestNameFlags, RequestNameReply};
    match conn.request_name_with_flags(BUS_NAME, RequestNameFlags::DoNotQueue.into()).await {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {}
        Ok(_) | Err(zbus::Error::NameTaken) => anyhow::bail!("{BUS_NAME} is already owned on the {which}; is another imessaged running?"),
        Err(e) => anyhow::bail!("cannot claim {BUS_NAME} on the {which}: {e}"),
    }
    let iface = conn.object_server().interface::<_, Daemon1>(OBJECT_PATH).await?;
    hub.set_bus(Some(Emitter { iface }));
    log::info!("D-Bus: {BUS_NAME} at {OBJECT_PATH}");
    Ok(conn)
}

/// How long shutdown waits to unexport the object. `remove` waits for the
/// interface's lock, which an in-flight method (a `Login` stuck on Apple)
/// can hold for a long time.
const UNEXPORT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Release the name and close the connection.
pub async fn stop(conn: zbus::Connection) {
    let _ = conn.release_name(BUS_NAME).await;
    match tokio::time::timeout(UNEXPORT_TIMEOUT, conn.object_server().remove::<Daemon1, _>(OBJECT_PATH)).await {
        Ok(_) => {}
        Err(_) => log::warn!("D-Bus object not unexported within {}s (a method call is still running); closing anyway", UNEXPORT_TIMEOUT.as_secs()),
    }
    if let Err(e) = conn.close().await {
        log::debug!("closing D-Bus connection: {e}");
    }
}
