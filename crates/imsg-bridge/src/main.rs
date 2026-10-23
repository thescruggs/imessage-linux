//! `imsg-bridge`: serve imessaged over the network, and manage the
//! per-device tokens (docs/BRIDGE.md).

use std::net::IpAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Context;
use clap::{Parser, Subcommand};
use imsg_bridge::auth::utc_timestamp;
use imsg_bridge::config::{Config, Listen, Paths};
use imsg_bridge::tailscale::LocalApi;
use imsg_bridge::tokens::{TokenStore, pairing_string};
use imsg_client::Bus;

#[derive(Parser, Debug)]
#[command(name = "imsg-bridge", version, about = "imessaged over HTTP + WebSocket on the Tailscale interface")]
struct Args {
    /// Configuration file (default: $XDG_CONFIG_HOME/imsg-bridge/config.toml).
    #[arg(long, global = true, env = "IMSG_BRIDGE_CONFIG", value_name = "FILE")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the bridge (the default).
    Serve {
        /// D-Bus address to use instead of the session bus (tests).
        #[arg(long, env = "IMSG_BUS_ADDRESS", value_name = "ADDR")]
        bus_address: Option<String>,
    },
    /// Manage client tokens.
    #[command(subcommand)]
    Token(TokenCmd),
}

#[derive(Subcommand, Debug)]
enum TokenCmd {
    /// Create a token for a device and print its pairing string (once).
    New { name: String },
    /// List tokens (names and use, never the secrets).
    List,
    /// Delete a token; the device is refused from its next request on.
    Revoke { name: String },
}

#[tokio::main]
async fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,zbus=warn,tracing=warn")).format_timestamp(None).init();
    let args = Args::parse();
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("imsg-bridge: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> anyhow::Result<()> {
    let paths = Paths::from_env(args.config.clone());
    let cfg = Config::load(&paths.config_file)?;
    match args.cmd.unwrap_or(Cmd::Serve { bus_address: None }) {
        Cmd::Serve { bus_address } => {
            let bus = match bus_address {
                Some(a) => Bus::Address(a),
                None => Bus::for_state_dir(&cfg.daemon_state_dir).map_err(|e| anyhow::anyhow!(e.message))?,
            };
            imsg_bridge::serve::run(cfg, paths, bus).await
        }
        Cmd::Token(t) => token(t, &cfg, &paths).await,
    }
}

/// The host a client should use: the first Tailscale IPv4, else the
/// first explicit listen address.
async fn pairing_host(cfg: &Config) -> Option<IpAddr> {
    if cfg.listen.contains(&Listen::Tailscale) {
        let api = LocalApi::default();
        if let Ok(Ok(ips)) = tokio::time::timeout(Duration::from_secs(5), imsg_bridge::serve::tailscale_ips(&api)).await {
            return ips.iter().find(|i| i.is_ipv4()).or(ips.first()).copied();
        }
    }
    cfg.listen.iter().find_map(|l| match l {
        Listen::Addr(a) => Some(a.ip()),
        Listen::Tailscale => None,
    })
}

async fn token(cmd: TokenCmd, cfg: &Config, paths: &Paths) -> anyhow::Result<()> {
    let store = TokenStore::new(paths.tokens_file());
    match cmd {
        TokenCmd::New { name } => {
            let token = store.create(&name)?;
            let (host, port) = match pairing_host(cfg).await {
                Some(ip) => (ip.to_string(), cfg.port),
                None => {
                    eprintln!("imsg-bridge: Tailscale is not up; replace <host> with this host's Tailscale IPv4");
                    ("<host>".to_string(), cfg.port)
                }
            };
            // An explicit listen address carries its own port.
            let port = cfg
                .listen
                .iter()
                .find_map(|l| match l {
                    Listen::Addr(a) if a.ip().to_string() == host => Some(a.port()),
                    _ => None,
                })
                .unwrap_or(port);
            println!("{}", pairing_string(&host, port, &token));
            eprintln!("Token {name:?} created. This is the only time it is shown; paste it into the client now.");
        }
        TokenCmd::List => {
            let tokens = store.load()?;
            if tokens.is_empty() {
                println!("no tokens (create one with `imsg-bridge token new <name>`)");
                return Ok(());
            }
            let at = |s: u64| utc_timestamp(UNIX_EPOCH + Duration::from_secs(s));
            let width = tokens.iter().map(|t| t.name.len()).max().unwrap_or(4).max(4);
            println!("{:width$}  {:20}  {:20}  LAST_PEER", "NAME", "CREATED", "LAST_USED");
            for t in tokens {
                let used = t.last_used.map(at).unwrap_or_else(|| "never".into());
                println!("{:width$}  {:20}  {:20}  {}", t.name, at(t.created), used, t.last_peer.as_deref().unwrap_or("-"));
            }
        }
        TokenCmd::Revoke { name } => {
            if !store.revoke(&name).context("cannot revoke")? {
                anyhow::bail!("no token named {name:?} (see `imsg-bridge token list`)");
            }
            println!("Token {name:?} revoked; the bridge refuses it from the next request on.");
        }
    }
    Ok(())
}
