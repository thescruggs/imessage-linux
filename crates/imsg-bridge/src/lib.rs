//! `imsg-bridge`: `imessaged` over the network (docs/BRIDGE.md).
//!
//! An ordinary D-Bus client of `org.imessage.Daemon1` that re-exposes a
//! messaging subset of it as HTTP + one WebSocket event stream on the
//! host's Tailscale addresses, behind tailnet identity and per-device
//! bearer tokens. The library is the whole service; the binary only parses
//! the command line. Tests run [`router`] in-process.

pub mod api;
pub mod auth;
pub mod config;
pub mod daemon;
pub mod error;
pub mod events;
pub mod files;
pub mod serve;
pub mod tailscale;
pub mod tokens;

use std::sync::{Arc, Mutex};

use anyhow::Context;
use axum::body::Bytes;
use imessage_common::platform::fs::{create_private_dir_all, set_mode};
use imsg_client::Bus;

pub use api::router;
pub use config::{Config, Paths};

/// This bridge's version (`GET /v1/version`, `hello`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Everything a request handler needs.
pub struct Bridge {
    pub cfg: Config,
    pub paths: Paths,
    pub daemon: Arc<daemon::DaemonLink>,
    pub auth: auth::Auth,
    /// `GET /v1/contacts`: (ETag, body) of the last answer.
    contacts: Mutex<Option<(String, Bytes)>>,
}

impl Bridge {
    /// Prepares the state dir (0700) and an empty uploads dir (0700; files
    /// left by a crash are removed). `policy` is `None` without
    /// `require_whois`.
    pub fn new(cfg: Config, paths: Paths, bus: Bus, policy: Option<auth::PeerPolicy>) -> anyhow::Result<Arc<Bridge>> {
        create_private_dir_all(&paths.state_dir).with_context(|| format!("cannot create {}", paths.state_dir.display()))?;
        let uploads = paths.uploads_dir();
        if uploads.exists() {
            std::fs::remove_dir_all(&uploads).with_context(|| format!("cannot clear {}", uploads.display()))?;
        }
        create_private_dir_all(&uploads).with_context(|| format!("cannot create {}", uploads.display()))?;
        set_mode(&uploads, 0o700)?;
        let auth = auth::Auth::new(tokens::TokenStore::new(paths.tokens_file()), policy, paths.access_log());
        Ok(Arc::new(Bridge { cfg, paths, daemon: daemon::DaemonLink::new(bus), auth, contacts: Mutex::new(None) }))
    }

    /// Start the shared D-Bus signal subscription (runs until aborted).
    pub fn start_events(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(self.daemon.clone().signal_loop())
    }
}
