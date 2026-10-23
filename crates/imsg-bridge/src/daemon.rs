//! The bridge's own link to `imessaged`: one D-Bus connection for all HTTP
//! requests, and one signal subscription whose events are turned into
//! WebSocket frames and fanned out to every socket through a broadcast
//! channel (docs/BRIDGE.md, Events).
//!
//! The loop mirrors imsg-tui's backend: when the signal stream ends (bus or
//! peer connection gone) it reconnects with backoff. On the session bus it
//! also follows `org.imessage.Daemon1`'s owner, so a daemon restart shows
//! up as `daemon` `lost` / `connected` even though the bus connection
//! itself survived.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::ws::Utf8Bytes;
use futures_util::StreamExt;
use imsg_client::{Bus, Client, Result};
use serde_json::{Value, json};
use tokio::sync::broadcast;

/// Frames buffered per WebSocket before a slow one counts as lagged.
const EVENT_BUFFER: usize = 1024;

pub struct DaemonLink {
    bus: Bus,
    client: tokio::sync::Mutex<Option<Arc<Client>>>,
    events: broadcast::Sender<Utf8Bytes>,
    connected: AtomicBool,
}

impl DaemonLink {
    pub fn new(bus: Bus) -> Arc<DaemonLink> {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        Arc::new(DaemonLink { bus, client: tokio::sync::Mutex::new(None), events, connected: AtomicBool::new(false) })
    }

    /// The shared connection, made on first use and replaced once closed.
    pub async fn client(&self) -> Result<Arc<Client>> {
        let mut guard = self.client.lock().await;
        if let Some(c) = guard.as_ref().filter(|c| !c.conn.is_closed()) {
            return Ok(c.clone());
        }
        let c = Arc::new(Client::connect(&self.bus).await?);
        *guard = Some(c.clone());
        Ok(c)
    }

    async fn reconnect(&self) -> Result<Arc<Client>> {
        *self.client.lock().await = None;
        self.client().await
    }

    /// A receiver for every frame sent from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Utf8Bytes> {
        self.events.subscribe()
    }

    /// Subscribed to the daemon's signals and the daemon is there.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    fn send(&self, frame: Value) {
        // No receivers (no socket open) is fine.
        let _ = self.events.send(frame.to_string().into());
    }

    /// Record the daemon's presence; a change is announced as a `daemon` frame.
    fn set_connected(&self, up: bool, detail: Option<String>) {
        if self.connected.swap(up, Ordering::SeqCst) == up {
            return;
        }
        if up {
            log::info!("imessaged connected");
            self.send(json!({"type": "daemon", "state": "connected"}));
        } else {
            let detail = detail.unwrap_or_else(|| "imessaged is not reachable".into());
            log::warn!("imessaged lost: {detail}");
            self.send(json!({"type": "daemon", "state": "lost", "detail": detail}));
        }
    }

    /// Forward the daemon's signals forever, reconnecting with backoff.
    pub async fn signal_loop(self: Arc<Self>) {
        let mut delay = 1;
        let mut fresh = false;
        loop {
            let client = if fresh { self.reconnect().await } else { self.client().await };
            fresh = true;
            match client {
                Ok(client) => {
                    delay = 1;
                    let e = self.run_signals(&client).await;
                    self.set_connected(false, Some(e));
                }
                Err(e) => self.set_connected(false, Some(e.message)),
            }
            tokio::time::sleep(Duration::from_secs(delay)).await;
            delay = (delay * 2).min(30);
        }
    }

    /// Returns why the subscription ended.
    async fn run_signals(&self, client: &Client) -> String {
        let p = client.proxy.inner();
        let err = |e| imsg_client::map_zbus(e).message;
        // One stream for all nine signals keeps the daemon's emission order.
        let mut signals = match p.receive_all_signals().await {
            Ok(s) => s,
            Err(e) => return err(e),
        };
        let mut owner = if client.is_peer() {
            None
        } else {
            match p.receive_owner_changed().await {
                Ok(s) => Some(s),
                Err(e) => return err(e),
            }
        };
        if client.daemon_present().await {
            self.set_connected(true, None);
        } else {
            self.set_connected(false, Some("imessaged is not on the bus".into()));
        }
        loop {
            let next_owner = async {
                match owner.as_mut() {
                    Some(s) => s.next().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                m = signals.next() => match m {
                    Some(m) => {
                        if let Some(frame) = frame_for(&m) {
                            self.send(frame);
                        }
                    }
                    None => return "lost the connection to imessaged".into(),
                },
                o = next_owner => match o {
                    Some(Some(_)) => self.set_connected(true, None),
                    Some(None) => self.set_connected(false, Some("imessaged left the bus".into())),
                    None => return "lost the connection to imessaged".into(),
                },
            }
        }
    }
}

/// A signal's JSON-string argument as embedded JSON.
fn embed(what: &str, json: &str) -> Option<Value> {
    match serde_json::from_str(json) {
        Ok(v) => Some(v),
        Err(e) => {
            log::warn!("imessaged sent malformed {what} JSON in a signal: {e}");
            None
        }
    }
}

/// The WebSocket frame for one daemon signal (docs/BRIDGE.md, Events);
/// `None` for anything else or a malformed one.
pub fn frame_for(m: &zbus::Message) -> Option<Value> {
    let header = m.header();
    let member = header.member()?.as_str().to_string();
    let body = m.body();
    let frame = match member.as_str() {
        "MessageReceived" | "MessageUpdated" => {
            let (chat_id, message): (String, String) = body.deserialize().ok()?;
            let ty = if member == "MessageReceived" { "message_received" } else { "message_updated" };
            json!({"type": ty, "chat_id": chat_id, "message": embed("message", &message)?})
        }
        "ChatUpdated" => json!({"type": "chat_updated", "chat": embed("chat", &body.deserialize::<String>().ok()?)?}),
        "Typing" => {
            let (chat_id, handle, typing): (String, String, bool) = body.deserialize().ok()?;
            json!({"type": "typing", "chat_id": chat_id, "handle": handle, "typing": typing})
        }
        "ConnectionStateChanged" => json!({"type": "connection_state", "state": body.deserialize::<String>().ok()?}),
        "TwoFactorRequired" => json!({"type": "two_factor_required"}),
        "BackgroundError" => json!({"type": "background_error", "message": body.deserialize::<String>().ok()?}),
        "HandleProfileUpdated" => {
            let (handle, profile): (String, String) = body.deserialize().ok()?;
            json!({"type": "handle_profile_updated", "handle": handle, "profile": embed("profile", &profile)?})
        }
        "SmsForwardingChanged" => json!({"type": "sms_forwarding_changed", "state": embed("sms forwarding", &body.deserialize::<String>().ok()?)?}),
        _ => return None,
    };
    Some(frame)
}
