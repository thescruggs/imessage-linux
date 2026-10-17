use std::path::PathBuf;

use clap::Parser;
use imessaged::config::{self, DaemonConfig, KeyProviderKind, Overrides};
use imessaged::daemon::{Daemon, shutdown_signal};
use imessaged::dbus::BusChoice;

#[derive(Parser, Debug)]
#[command(name = "imessaged", version, about = "iMessage daemon for Linux")]
struct Args {
    /// State directory (keys, registration, message store).
    /// Default: $XDG_STATE_HOME/imessaged or ~/.local/state/imessaged
    #[arg(long, env = "IMESSAGED_STATE_DIR")]
    state_dir: Option<PathBuf>,

    /// Remote anisette v3 server. Default: https://ani.sidestore.io
    #[arg(long, env = "IMESSAGED_ANISETTE_URL")]
    anisette_url: Option<String>,

    /// Command printing native Mac anisette headers as JSON, e.g.
    /// "ssh -F ~/.ssh/config -o BatchMode=yes -o LogLevel=ERROR mac.local
    /// .local/bin/imessage-anisette" (see tools/mac-anisette-install.sh;
    /// -F skips the system ssh config the sandbox cannot read, LogLevel
    /// keeps banners out of the journal). Replaces the remote server.
    #[arg(long, env = "IMESSAGED_ANISETTE_COMMAND")]
    anisette_command: Option<String>,

    /// Where the state-encryption key lives: auto | secret-service | file.
    #[arg(long, env = "IMESSAGED_KEY_PROVIDER")]
    key_provider: Option<KeyProviderKind>,

    /// Log filter (env_logger syntax). Replaces the default
    /// `info,rustpush=warn,icloud_auth=warn,omnisette=warn,keystore=warn,zbus=warn,tracing=warn`
    /// entirely. RUST_LOG, if set, wins. WARNING: rustpush / icloud_auth /
    /// omnisette / keystore at info or debug log message bodies, push tokens
    /// and GSA tokens.
    #[arg(long, env = "IMESSAGED_LOG")]
    log_level: Option<String>,

    /// Keep the Apple ID SRP password hash in state.enc for silent
    /// re-login (default true).
    #[arg(long, env = "IMESSAGED_STORE_CREDENTIALS")]
    store_credentials: Option<bool>,

    /// Registered handle to send new messages from (email address or phone
    /// number). Default: the first registered handle.
    #[arg(long, env = "IMESSAGED_SENDER_HANDLE")]
    sender_handle: Option<String>,

    /// Another address of yours this device is not registered for (e.g.
    /// your iPhone's phone number); repeatable. Messages from it count as
    /// your own; nothing is sent from it. Env: comma-separated.
    #[arg(long = "other-handle", env = "IMESSAGED_OTHER_HANDLES", value_delimiter = ',')]
    other_handles: Vec<String>,

    /// Do not serve the D-Bus API (headless / tests). Messages are still
    /// received and stored.
    #[arg(long)]
    no_dbus: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let state_dir = args.state_dir.unwrap_or_else(config::default_state_dir);
    let cfg = DaemonConfig::load(
        state_dir,
        Overrides {
            anisette_url: args.anisette_url,
            anisette_command: args.anisette_command,
            key_provider: args.key_provider,
            log_level: args.log_level,
            store_credentials: args.store_credentials,
            sender_handle: args.sender_handle,
            other_handles: (!args.other_handles.is_empty()).then_some(args.other_handles),
        },
    )?;

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&cfg.log_level)).init();
    log::info!("imessaged {} starting", env!("CARGO_PKG_VERSION"));
    log::info!("state dir: {}", cfg.state_dir.display());
    match &cfg.anisette_command {
        Some(c) => log::info!("anisette: command {c:?}"),
        None => log::info!("anisette: remote v3 server {}", cfg.anisette_url),
    }
    if let Some(h) = &cfg.sender_handle {
        log::info!("sender handle: {h} (if registered)");
    }
    if !cfg.other_handles.is_empty() {
        log::info!("other handles (also mine): {}", cfg.other_handles.join(", "));
    }
    // M-C: judge the filter env_logger actually uses (RUST_LOG wins).
    let effective = config::effective_log_filter(std::env::var("RUST_LOG").ok(), &cfg.log_level);
    if !config::filter_quiets_protocol_crates(&effective) {
        log::warn!(
            "log filter {effective:?} does not keep {} at warn; message bodies and tokens may be logged",
            config::QUIET_CRATES.join("/")
        );
    }

    let mut daemon = Daemon::new(cfg).await?;
    if !args.no_dbus {
        daemon = daemon.with_dbus(BusChoice::Session);
    }
    daemon.run_until(shutdown_signal()).await
}
