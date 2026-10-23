//! Daemon reply payloads that have no model type (docs/DBUS.md, JSON
//! payloads). Shared by the D-Bus client (`imsg-client`), the network
//! bridge (`imsg-bridge`) and the portable client (`imsg-remote`), so
//! clients that do not speak D-Bus need not pull in zbus.

use serde::{Deserialize, Serialize};

use crate::model::{ConnectionState, Handle, Message, ProfilesStatus, RegistrationState, Service, SmsForwarding};

/// `GetStatus`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub version: String,
    pub connection: ConnectionState,
    pub registered: bool,
    pub registration: RegistrationState,
    pub unread: Option<u32>,
    /// Older daemons do not report it.
    #[serde(default)]
    pub profiles: Option<ProfilesStatus>,
    /// Text Message Forwarding; older daemons do not report it.
    #[serde(default)]
    pub sms: Option<SmsForwarding>,
    /// The daemon's `mac_host` setting (`imsg sync-mac`); older daemons
    /// do not report it.
    #[serde(default)]
    pub mac_host: Option<String>,
}

/// `ListMessages`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessagePage {
    pub messages: Vec<Message>,
    pub next_cursor: Option<String>,
}

/// `SendText` / `SendFile`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SentMessage {
    pub id: String,
    pub chat_id: String,
    pub sent_at: u64,
    pub sender: Handle,
    #[serde(default)]
    pub service: Service,
}

/// `LookupHandles` element.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reachability {
    pub handle: String,
    pub reachable: bool,
}
