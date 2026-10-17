//! The daemon shell around an [`Engine`]: engine events -> SQLite store ->
//! signals (in-process broadcast + D-Bus), in that order, plus the D-Bus
//! server. Construct with [`Daemon::new`] (real rustpush engine, store and
//! session bus) or [`Daemon::with_engine`] + [`Daemon::with_store`] /
//! [`Daemon::with_dbus`] (tests with `MockEngine`).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use crate::config::DaemonConfig;
use crate::dbus::BusChoice;
use crate::engine::{Engine, EngineError, EngineEvent, RustpushEngine};
use crate::hub::{DEFAULT_TYPING_TIMEOUT, DaemonSignal, Hub};
use crate::keyring;
use crate::store::{Store, StoreHandle};
use imessage_common::model::Service;

pub struct Daemon {
    pub cfg: DaemonConfig,
    engine: Arc<dyn Engine>,
    store: Option<StoreHandle>,
    bus: Option<BusChoice>,
    typing_timeout: Duration,
}

impl Daemon {
    /// Real daemon: obtain the state key, open the message store, start the
    /// rustpush engine. D-Bus is off until [`Daemon::with_dbus`].
    pub async fn new(cfg: DaemonConfig) -> anyhow::Result<Daemon> {
        crate::state::ensure_private_dir(&cfg.state_dir)?;
        let provider = keyring::provider_for(cfg.key_provider, &cfg.state_dir);
        log::info!("state key provider: {}", provider.describe());
        let key = provider.get_or_create().await.map_err(|e| {
            // The provider could not hand out a key at all: nothing is wrong
            // with state.enc, do NOT tell the user to move it aside.
            anyhow::anyhow!(
                "state key provider unavailable ({}): {e}. {} was not touched; fix the key provider and restart.",
                provider.describe(),
                cfg.state_path().display()
            )
        })?;
        let dir = cfg.state_dir.clone();
        let store_key = key.clone();
        let store = tokio::task::spawn_blocking(move || Store::open(&dir, store_key))
            .await?
            .map_err(|e| anyhow::anyhow!("cannot open the message store {}: {e}", cfg.store_path().display()))?;
        let engine = RustpushEngine::start(cfg.clone(), key).await.map_err(|e| match e {
            EngineError::StateKeyMismatch(_) => anyhow::anyhow!(
                "{} cannot be decrypted with the key from {} ({e}): wrong key (provider switched, keyring item \
                 replaced) or a damaged file. Restore the original key; only if it is truly lost, move the file \
                 aside and log in again.",
                cfg.state_path().display(),
                provider.describe()
            ),
            other => anyhow::anyhow!("cannot load {}: {other}", cfg.state_path().display()),
        })?;
        Ok(Daemon { cfg, engine: Arc::new(engine), store: Some(StoreHandle::new(store)), bus: None, typing_timeout: DEFAULT_TYPING_TIMEOUT })
    }

    /// Any engine, no store, no D-Bus (events are only logged).
    pub fn with_engine(cfg: DaemonConfig, engine: Arc<dyn Engine>) -> Daemon {
        Daemon { cfg, engine, store: None, bus: None, typing_timeout: DEFAULT_TYPING_TIMEOUT }
    }

    pub fn with_store(mut self, store: StoreHandle) -> Daemon {
        self.store = Some(store);
        self
    }

    /// Serve the D-Bus API (requires a store).
    pub fn with_dbus(mut self, bus: BusChoice) -> Daemon {
        self.bus = Some(bus);
        self
    }

    pub fn with_typing_timeout(mut self, d: Duration) -> Daemon {
        self.typing_timeout = d;
        self
    }

    pub fn engine(&self) -> Arc<dyn Engine> {
        self.engine.clone()
    }

    pub fn events(&self) -> broadcast::Receiver<EngineEvent> {
        self.engine.events()
    }

    /// Start the event pump (and D-Bus). Engine events are subscribed to
    /// before anything else so nothing is missed while D-Bus comes up.
    pub async fn start(self) -> anyhow::Result<RunningDaemon> {
        let rx = self.events();
        log::info!("connection state: {:?}", self.engine.connection_state().await);
        if self.bus.is_some() && self.store.is_none() {
            anyhow::bail!("the D-Bus API needs the message store");
        }
        let hub = Hub::new(self.engine.clone(), self.store.clone(), self.cfg.state_dir.clone(), self.typing_timeout);
        hub.delete_empty_groups().await;
        hub.seed_engine_chats().await;
        let conn = match &self.bus {
            Some(bus) => match crate::dbus::serve(hub.clone(), bus).await {
                Ok(c) => Some(c),
                Err(e) => {
                    // Leave everything persisted before failing startup.
                    self.engine.shutdown().await;
                    if let Some(store) = &self.store {
                        let _ = store.call(|s| s.close()).await;
                    }
                    return Err(e);
                }
            },
            None => None,
        };
        let (stop_tx, stop_rx) = watch::channel(false);
        let pump = tokio::spawn(pump(hub.clone(), rx, stop_rx));
        Ok(RunningDaemon { hub, engine: self.engine, store: self.store, conn, pump, stop: stop_tx })
    }

    /// Run until `shutdown` resolves, then shut down in order.
    pub async fn run_until(self, shutdown: impl Future<Output = ()>) -> anyhow::Result<()> {
        let running = self.start().await?;
        shutdown.await;
        running.shutdown().await
    }
}

pub struct RunningDaemon {
    hub: Arc<Hub>,
    engine: Arc<dyn Engine>,
    store: Option<StoreHandle>,
    conn: Option<zbus::Connection>,
    pump: JoinHandle<()>,
    stop: watch::Sender<bool>,
}

impl RunningDaemon {
    pub fn hub(&self) -> Arc<Hub> {
        self.hub.clone()
    }

    pub fn signals(&self) -> broadcast::Receiver<DaemonSignal> {
        self.hub.signals()
    }

    pub fn store(&self) -> Option<StoreHandle> {
        self.store.clone()
    }

    /// The bus connection (tests: its unique name / address).
    pub fn dbus(&self) -> Option<&zbus::Connection> {
        self.conn.as_ref()
    }

    /// Shutdown order: engine (persists its state; may emit final events) ->
    /// drain and stop the event pump (so those events still reach the store
    /// and signals) -> release the D-Bus name -> checkpoint and close the
    /// store.
    pub async fn shutdown(self) -> anyhow::Result<()> {
        log::info!("shutting down");
        self.engine.shutdown().await;
        let _ = self.stop.send(true);
        if tokio::time::timeout(Duration::from_secs(5), self.pump).await.is_err() {
            log::warn!("event pump did not stop in time");
        }
        self.hub.stop();
        if let Some(conn) = self.conn {
            crate::dbus::stop(conn).await;
        }
        if let Some(store) = self.store {
            match store.call(|s| s.close()).await {
                Ok(()) => log::info!("message store closed"),
                Err(e) => log::error!("closing the message store: {e}"),
            }
        }
        Ok(())
    }
}

/// engine events -> store -> signals, strictly in order.
async fn pump(hub: Arc<Hub>, mut rx: broadcast::Receiver<EngineEvent>, mut stop: watch::Receiver<bool>) {
    loop {
        let ev = tokio::select! {
            biased;
            r = rx.recv() => Some(r),
            _ = stop.wait_for(|s| *s) => None,
        };
        let Some(ev) = ev else {
            // Drain what the engine emitted up to (and during) shutdown.
            loop {
                match rx.try_recv() {
                    Ok(ev) => handle_event(&hub, ev).await,
                    Err(broadcast::error::TryRecvError::Lagged(n)) => {
                        log::warn!("event pump lagged during shutdown; {n} engine event(s) lost");
                        continue;
                    }
                    Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => break,
                }
            }
            return;
        };
        match ev {
            Ok(ev) => handle_event(&hub, ev).await,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                log::warn!("event pump lagged; {n} engine event(s) lost");
                hub.emit(DaemonSignal::BackgroundError(format!("{n} engine event(s) were dropped; clients should resync"))).await;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn handle_event(hub: &Arc<Hub>, ev: EngineEvent) {
    log_event(&ev);
    match &ev {
        EngineEvent::Typing(t) => return hub.on_typing(t.chat_id.clone(), t.sender.clone(), t.typing).await,
        EngineEvent::ConnectionChanged(s) => return hub.emit(DaemonSignal::ConnectionStateChanged(s.clone())).await,
        EngineEvent::RegistrationChanged(r) => {
            if r.is_registered() {
                // The engine now knows our handles: re-teach it stored groups.
                hub.seed_engine_chats().await;
            }
            return hub.emit(DaemonSignal::RegisteredChanged(r.is_registered())).await;
        }
        EngineEvent::TwoFactorRequired => return hub.emit(DaemonSignal::TwoFactorRequired).await,
        EngineEvent::Error(e) => return hub.emit(DaemonSignal::BackgroundError(e.clone())).await,
        EngineEvent::SmsForwardingChanged(s) => return hub.emit(DaemonSignal::SmsForwardingChanged(s.clone())).await,
        _ => {}
    }
    if let EngineEvent::ProfileShared { handle, share, at } = ev {
        return hub.on_profile_shared(handle, share, at).await;
    }
    let Ok(store) = hub.store() else { return };
    let fetch_background = match &ev {
        EngineEvent::ChatBackground { chat_id, background: Some(_), .. } => Some(chat_id.clone()),
        _ => None,
    };
    let fetch_icon = match &ev {
        EngineEvent::ChatIcon { chat_id, icon: Some(_), .. } => Some(chat_id.clone()),
        _ => None,
    };
    match store.call(move |s| s.apply_event(&ev)).await {
        Ok(updates) => {
            // Only a background the store took (not a redelivery, an older
            // change or an oversized one) is worth a download.
            let fetch_background = fetch_background.filter(|_| !updates.is_empty());
            let fetch_icon = fetch_icon.filter(|_| !updates.is_empty());
            hub.publish(updates).await;
            if let Some(chat_id) = fetch_background {
                hub.spawn_background_fetch(chat_id);
            }
            if let Some(chat_id) = fetch_icon {
                hub.spawn_icon_fetch(chat_id);
            }
        }
        Err(e) => {
            log::error!("storing engine event failed: {e}");
            hub.emit(DaemonSignal::BackgroundError(format!("message store: {e}"))).await;
        }
    }
}

/// Log an event without message contents. (This keeps bodies out of *our*
/// log lines; rustpush itself logs them at info/debug, which the default
/// filter suppresses. See `config::DEFAULT_LOG_LEVEL`.)
fn log_event(ev: &EngineEvent) {
    match ev {
        EngineEvent::MessageReceived { message, .. } => {
            log::info!("message {} in {} ({} part(s){})", message.id, message.chat_id, message.parts.len(), if message.service == Service::Sms { ", sms" } else { "" })
        }
        EngineEvent::MessageSent { id, delivery, .. } => log::info!("sent {id}: {delivery:?}"),
        EngineEvent::ConnectionChanged(s) => log::info!("connection: {s:?}"),
        EngineEvent::RegistrationChanged(r) => log::info!("registration: {} handle(s), renews at {:?}", r.handles.len(), r.expires_at),
        EngineEvent::TwoFactorRequired => log::info!("two-factor code required"),
        EngineEvent::SmsForwardingChanged(s) => log::info!("sms forwarding: {}{}", s.state.as_str(), s.detail.as_deref().map(|d| format!(" ({d})")).unwrap_or_default()),
        EngineEvent::Error(e) => log::warn!("engine error: {e}"),
        other => log::debug!("event: {}", event_name(other)),
    }
}

fn event_name(ev: &EngineEvent) -> &'static str {
    match ev {
        EngineEvent::MessageReceived { .. } => "message_received",
        EngineEvent::MessageSent { .. } => "message_sent",
        EngineEvent::Delivered(_) => "delivered",
        EngineEvent::Read(_) => "read",
        EngineEvent::Typing(_) => "typing",
        EngineEvent::Reaction { .. } => "reaction",
        EngineEvent::Edited { .. } => "edited",
        EngineEvent::Unsent { .. } => "unsent",
        EngineEvent::ChatRenamed { .. } => "chat_renamed",
        EngineEvent::ParticipantsChanged { .. } => "participants_changed",
        EngineEvent::ChatBackground { .. } => "chat_background",
        EngineEvent::ProfileShared { .. } => "profile_shared",
        EngineEvent::ChatIcon { .. } => "chat_icon",
        EngineEvent::ConnectionChanged(_) => "connection_changed",
        EngineEvent::RegistrationChanged(_) => "registration_changed",
        EngineEvent::TwoFactorRequired => "two_factor_required",
        EngineEvent::SmsForwardingChanged(_) => "sms_forwarding_changed",
        EngineEvent::Error(_) => "error",
    }
}

/// Resolves on SIGTERM or SIGINT.
pub async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => log::info!("SIGTERM"),
        _ = int.recv() => log::info!("SIGINT"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::mock::{MockCall, MockEngine};

    #[tokio::test]
    async fn daemon_runs_mock_engine_and_shuts_down() {
        let mock = Arc::new(MockEngine::new());
        let d = Daemon::with_engine(DaemonConfig::new("/nonexistent".into()), mock.clone());
        let mut rx = d.events();
        mock.inject(EngineEvent::TwoFactorRequired);
        assert_eq!(rx.recv().await.unwrap(), EngineEvent::TwoFactorRequired);
        d.run_until(async {}).await.unwrap();
        assert_eq!(mock.calls(), vec![MockCall::Shutdown]);
    }
}
