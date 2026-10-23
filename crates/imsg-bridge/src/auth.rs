//! Request authentication (docs/BRIDGE.md, Authentication): the tailnet
//! peer (LocalAPI whois, cached 60 s) when `require_whois`, then the bearer
//! token. Writes the access log: one line per authenticated TCP
//! connection and per refusal, never a body or query string.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use imessage_common::platform::fs::private_options;

use crate::Bridge;
use crate::error::ApiError;
use crate::tailscale::{LocalApi, Peer};
use crate::tokens::{TokenEntry, TokenStore, verify};

const WHOIS_TTL: Duration = Duration::from_secs(60);
const FAILURE_DELAY: Duration = Duration::from_secs(1);
/// Connections remembered for "log once per connection"; cleared when full.
const SEEN_MAX: usize = 4096;

/// Which tailnet peers may connect.
#[derive(Debug, Clone)]
pub struct PeerPolicy {
    pub localapi: LocalApi,
    pub allowed_logins: Vec<String>,
    pub allowed_tags: Vec<String>,
}

/// Token entries plus the token file's (mtime, len) they were read at.
type TokenCache = (Option<(SystemTime, u64)>, Vec<TokenEntry>);

pub struct Auth {
    store: TokenStore,
    tokens: Mutex<TokenCache>,
    /// `None`: `require_whois = false`.
    policy: Option<PeerPolicy>,
    whois: Mutex<HashMap<IpAddr, (Instant, Option<Peer>)>>,
    log_path: PathBuf,
    seen: Mutex<HashSet<SocketAddr>>,
}

impl Auth {
    pub fn new(store: TokenStore, policy: Option<PeerPolicy>, log_path: PathBuf) -> Auth {
        Auth {
            store,
            tokens: Mutex::new((None, Vec::new())),
            policy,
            whois: Mutex::new(HashMap::new()),
            log_path,
            seen: Mutex::new(HashSet::new()),
        }
    }

    /// The token's entry, re-reading the file if it changed (so `token
    /// revoke` applies at once).
    fn check_token(&self, presented: &str) -> Option<TokenEntry> {
        let stamp = std::fs::metadata(self.store.path()).ok().map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
        let mut cache = self.tokens.lock().unwrap();
        if cache.0 != stamp || stamp.is_none() {
            match self.store.load() {
                Ok(entries) => *cache = (stamp, entries),
                Err(e) => {
                    log::error!("{e:#}");
                    *cache = (None, Vec::new());
                }
            }
        }
        let name = verify(&cache.1, presented)?;
        cache.1.iter().find(|t| t.name == name).cloned()
    }

    /// The tailnet peer behind `addr`, cached per IP.
    async fn whois(&self, policy: &PeerPolicy, addr: SocketAddr) -> anyhow::Result<Option<Peer>> {
        let ip = addr.ip().to_canonical();
        if let Some((at, p)) = self.whois.lock().unwrap().get(&ip)
            && at.elapsed() < WHOIS_TTL
        {
            return Ok(p.clone());
        }
        let peer = policy.localapi.whois(SocketAddr::new(ip, addr.port())).await?;
        let mut cache = self.whois.lock().unwrap();
        cache.retain(|_, (at, _)| at.elapsed() < WHOIS_TTL);
        cache.insert(ip, (Instant::now(), peer.clone()));
        Ok(peer)
    }

    /// Append one line to the access log (and the journal).
    pub fn log(&self, line: &str) {
        log::info!("access: {line}");
        let stamped = format!("{} {line}\n", utc_timestamp(SystemTime::now()));
        let r = private_options().create(true).append(true).open(&self.log_path).and_then(|mut f| f.write_all(stamped.as_bytes()));
        if let Err(e) = r {
            log::warn!("cannot write {}: {e}", self.log_path.display());
        }
    }

    /// First request on this TCP connection?
    fn first_on_connection(&self, addr: SocketAddr) -> bool {
        let mut seen = self.seen.lock().unwrap();
        if seen.len() >= SEEN_MAX {
            seen.clear();
        }
        seen.insert(addr)
    }
}

/// The middleware in front of every route.
pub async fn require_auth(State(bridge): State<Arc<Bridge>>, ConnectInfo(addr): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Response {
    let auth = &bridge.auth;
    let what = format!("{} {}", req.method(), req.uri().path());
    let mut who = String::new();
    if let Some(policy) = &auth.policy {
        match auth.whois(policy, addr).await {
            Ok(Some(p)) if p.allowed(&policy.allowed_logins, &policy.allowed_tags) => who = format!(" peer_id={}", p.describe()),
            Ok(p) => {
                let id = p.as_ref().map(Peer::describe).unwrap_or_else(|| "not-a-tailnet-peer".into());
                auth.log(&format!("forbidden peer={addr} peer_id={id} {what}"));
                return ApiError::new("Forbidden", "not an allowed tailnet peer").into_response();
            }
            Err(e) => {
                log::warn!("whois {addr}: {e:#}");
                return ApiError::new("Transient", "cannot verify the tailnet peer (tailscaled LocalAPI)").into_response();
            }
        }
    }
    let presented = req.headers().get(AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(str::trim);
    let entry = presented.and_then(|t| auth.check_token(t));
    let Some(entry) = entry else {
        let why = if presented.is_some() { "unknown-token" } else { "no-token" };
        auth.log(&format!("unauthorized peer={addr}{who} reason={why} {what}"));
        tokio::time::sleep(FAILURE_DELAY).await;
        return ApiError::new("Unauthorized", "missing or unknown bearer token").into_response();
    };
    if auth.first_on_connection(addr) {
        auth.log(&format!("ok token={} peer={addr}{who} {what}", entry.name));
    }
    let peer = addr.to_string();
    // The cached entry is current (re-read when the file changes), so most
    // requests skip the locked read-modify-write.
    if entry.needs_touch(&peer, crate::tokens::now_secs()) {
        let store = auth.store.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = store.touch(&entry.name, &peer) {
                log::warn!("cannot record token use: {e:#}");
            }
        });
    }
    next.run(req).await
}

/// `2026-10-22T15:23:35Z`.
pub fn utc_timestamp(t: SystemTime) -> String {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(utc_timestamp(SystemTime::UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(utc_timestamp(SystemTime::UNIX_EPOCH + Duration::from_secs(1_792_682_615)), "2026-10-22T15:23:35Z");
        assert_eq!(utc_timestamp(SystemTime::UNIX_EPOCH + Duration::from_secs(951_782_400)), "2000-02-29T00:00:00Z");
    }
}
