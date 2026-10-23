//! `imsg-bridge serve`: resolve the listen addresses, bind them and run
//! the router until SIGINT / SIGTERM.
//!
//! `"tailscale"` resolves through tailscaled's LocalAPI; while Tailscale is
//! not up (or an address cannot be bound yet) the bridge retries every
//! 5 s. Once listening it re-checks the addresses every 60 s and exits
//! with an error if they changed, so systemd (`Restart=always`) starts it
//! again on the new ones.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::bail;
use imsg_client::Bus;
use tokio::net::TcpListener;

use crate::auth::PeerPolicy;
use crate::config::{Config, Listen, Paths};
use crate::tailscale::LocalApi;
use crate::{Bridge, router};

const RETRY: Duration = Duration::from_secs(5);
const RECHECK: Duration = Duration::from_secs(60);

/// This host's Tailscale addresses, or an error while Tailscale is not up.
pub async fn tailscale_ips(api: &LocalApi) -> anyhow::Result<Vec<IpAddr>> {
    let st = api.status().await?;
    if !st.running || st.ips.is_empty() {
        bail!("Tailscale is not running (no Tailscale address yet)");
    }
    Ok(st.ips)
}

/// Every address to listen on.
async fn addresses(cfg: &Config, api: &LocalApi) -> anyhow::Result<Vec<SocketAddr>> {
    let mut out = Vec::new();
    for l in &cfg.listen {
        match l {
            Listen::Tailscale => out.extend(tailscale_ips(api).await?.into_iter().map(|ip| SocketAddr::new(ip, cfg.port))),
            Listen::Addr(a) => out.push(*a),
        }
    }
    out.sort();
    out.dedup();
    // Belt and braces: config parsing already refuses wildcards.
    if out.iter().any(|a| a.ip().is_unspecified()) {
        bail!("refusing to listen on all interfaces");
    }
    Ok(out)
}

/// Bind all of `addrs`, or none.
async fn bind_all(addrs: &[SocketAddr]) -> anyhow::Result<Vec<TcpListener>> {
    let mut out = Vec::new();
    for a in addrs {
        match TcpListener::bind(a).await {
            Ok(l) => out.push(l),
            Err(e) => bail!("cannot listen on {a}: {e}"),
        }
    }
    Ok(out)
}

/// Retry `f` every 5 s until it succeeds, logging each new failure once.
async fn retry<T, F, Fut>(mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let mut last = String::new();
    loop {
        match f().await {
            Ok(v) => return v,
            Err(e) => {
                let msg = format!("{e:#}");
                if msg != last {
                    log::warn!("{msg}; retrying every {} s", RETRY.as_secs());
                    last = msg;
                }
            }
        }
        tokio::time::sleep(RETRY).await;
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

pub async fn run(cfg: Config, paths: Paths, bus: Bus) -> anyhow::Result<()> {
    let api = LocalApi::default();
    let policy = if cfg.require_whois {
        let mut logins = cfg.allowed_logins.clone();
        if logins.is_empty() {
            // Default: the tailnet user this host belongs to.
            let own = retry(|| async {
                let st = api.status().await?;
                st.login.ok_or_else(|| anyhow::anyhow!("tailscaled does not report this node's user yet"))
            })
            .await;
            log::info!("allowed_logins not set: allowing this host's tailnet user {own}");
            logins.push(own);
        }
        Some(PeerPolicy { localapi: api.clone(), allowed_logins: logins, allowed_tags: cfg.allowed_tags.clone() })
    } else {
        log::warn!("require_whois = false: any client with a valid token is accepted (use only on loopback)");
        None
    };
    let bridge = Bridge::new(cfg.clone(), paths, bus, policy)?;
    let _events = bridge.start_events();
    let app = router(bridge).into_make_service_with_connect_info::<SocketAddr>();

    let (addrs, listeners) = retry(|| async {
        let addrs = addresses(&cfg, &api).await?;
        let listeners = bind_all(&addrs).await?;
        Ok((addrs, listeners))
    })
    .await;
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    for (addr, l) in addrs.iter().zip(listeners) {
        log::info!("listening on http://{addr}/v1");
        let (app, done) = (app.clone(), done_tx.clone());
        let addr = *addr;
        tokio::spawn(async move {
            let r = axum::serve(l, app).await;
            let _ = done.send(format!("server on {addr} stopped: {r:?}"));
        });
    }

    let uses_tailscale = cfg.listen.contains(&Listen::Tailscale);
    let watch = async {
        if !uses_tailscale {
            return std::future::pending::<anyhow::Result<()>>().await;
        }
        loop {
            tokio::time::sleep(RECHECK).await;
            // Only a definite change counts; tailscaled briefly down does not.
            if let Ok(now) = addresses(&cfg, &api).await
                && now != addrs
            {
                bail!("the Tailscale addresses changed ({addrs:?} -> {now:?}); exiting so the service restarts on the new ones");
            }
        }
    };
    tokio::select! {
        _ = shutdown_signal() => {
            log::info!("shutting down");
            Ok(())
        }
        r = watch => r,
        Some(why) = done_rx.recv() => bail!(why),
    }
}
