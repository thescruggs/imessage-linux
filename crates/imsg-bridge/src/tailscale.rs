//! tailscaled's LocalAPI over its unix socket: this host's Tailscale
//! addresses (`/localapi/v0/status`) and who a peer is
//! (`/localapi/v0/whois`). A plain HTTP/1.0 request per call (the server
//! then answers without chunking and closes), so no HTTP client is needed.
//! The socket is world-writable; reading status and whois needs no
//! privileges.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Deserialize;

pub const DEFAULT_SOCKET: &str = "/run/tailscale/tailscaled.sock";
/// LocalAPI requests must carry this Host header.
const HOST: &str = "local-tailscaled.sock";
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct LocalApi {
    socket: PathBuf,
}

/// This host, from `status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfInfo {
    /// `BackendState == "Running"`.
    pub running: bool,
    pub ips: Vec<IpAddr>,
    /// The login name of the user this node belongs to.
    pub login: Option<String>,
}

/// A peer, from `whois`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub login: Option<String>,
    pub tags: Vec<String>,
    /// The peer node's name (for the access log).
    pub node: Option<String>,
}

impl Peer {
    pub fn allowed(&self, logins: &[String], tags: &[String]) -> bool {
        self.login.as_ref().is_some_and(|l| logins.iter().any(|a| a.eq_ignore_ascii_case(l))) || self.tags.iter().any(|t| tags.contains(t))
    }

    /// `login` or `tag:a,tag:b`, for the access log.
    pub fn describe(&self) -> String {
        if !self.tags.is_empty() {
            return self.tags.join(",");
        }
        self.login.clone().unwrap_or_else(|| "?".into())
    }
}

impl Default for LocalApi {
    fn default() -> LocalApi {
        LocalApi { socket: PathBuf::from(DEFAULT_SOCKET) }
    }
}

impl LocalApi {
    pub fn new(socket: impl Into<PathBuf>) -> LocalApi {
        LocalApi { socket: socket.into() }
    }

    /// `GET path` → (status code, body).
    #[cfg(unix)]
    async fn get(&self, path: &str) -> anyhow::Result<(u16, Vec<u8>)> {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let fut = async {
            let mut s = tokio::net::UnixStream::connect(&self.socket).await.with_context(|| format!("cannot connect to {}", self.socket.display()))?;
            s.write_all(format!("GET {path} HTTP/1.0\r\nHost: {HOST}\r\n\r\n").as_bytes()).await?;
            let mut buf = Vec::new();
            s.take(16 * 1024 * 1024).read_to_end(&mut buf).await?;
            anyhow::Ok(buf)
        };
        let buf = tokio::time::timeout(TIMEOUT, fut).await.context("tailscaled LocalAPI timed out")??;
        parse_response(&buf)
    }

    #[cfg(not(unix))]
    async fn get(&self, _path: &str) -> anyhow::Result<(u16, Vec<u8>)> {
        bail!("the tailscaled LocalAPI socket is only supported on Unix")
    }

    pub async fn status(&self) -> anyhow::Result<SelfInfo> {
        let (code, body) = self.get("/localapi/v0/status?peers=false").await?;
        if code != 200 {
            bail!("tailscaled status: HTTP {code}");
        }
        parse_status(&body)
    }

    /// Who is connecting from `addr`; `Ok(None)` if tailscaled does not
    /// know the address (not a tailnet peer).
    pub async fn whois(&self, addr: SocketAddr) -> anyhow::Result<Option<Peer>> {
        let q = percent_encoding::utf8_percent_encode(&addr.to_string(), percent_encoding::NON_ALPHANUMERIC).to_string();
        let (code, body) = self.get(&format!("/localapi/v0/whois?addr={q}")).await?;
        match code {
            200 => parse_whois(&body).map(Some),
            404 | 400 => Ok(None),
            c => bail!("tailscaled whois: HTTP {c}"),
        }
    }
}

fn parse_response(buf: &[u8]) -> anyhow::Result<(u16, Vec<u8>)> {
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").context("tailscaled sent no HTTP header")?;
    let head = std::str::from_utf8(&buf[..split]).context("tailscaled sent a non-UTF-8 header")?;
    let status = head.lines().next().unwrap_or("");
    let code = status.split_whitespace().nth(1).and_then(|c| c.parse().ok()).with_context(|| format!("bad status line {status:?}"))?;
    Ok((code, buf[split + 4..].to_vec()))
}

fn parse_status(body: &[u8]) -> anyhow::Result<SelfInfo> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Status {
        #[serde(default)]
        backend_state: String,
        #[serde(rename = "Self")]
        this: Option<Node>,
        #[serde(default)]
        user: std::collections::HashMap<String, User>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Node {
        #[serde(default, rename = "TailscaleIPs")]
        tailscale_ips: Option<Vec<String>>,
        #[serde(rename = "UserID")]
        user_id: Option<u64>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct User {
        login_name: Option<String>,
    }
    let s: Status = serde_json::from_slice(body).context("bad tailscaled status JSON")?;
    let (ips, uid) = match s.this {
        Some(n) => (n.tailscale_ips.unwrap_or_default().iter().filter_map(|i| i.parse().ok()).collect(), n.user_id),
        None => (Vec::new(), None),
    };
    let login = uid.and_then(|u| s.user.get(&u.to_string())).and_then(|u| u.login_name.clone());
    Ok(SelfInfo { running: s.backend_state == "Running", ips, login })
}

fn parse_whois(body: &[u8]) -> anyhow::Result<Peer> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Whois {
        node: Option<Node>,
        user_profile: Option<Profile>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Node {
        #[serde(default)]
        tags: Option<Vec<String>>,
        name: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Profile {
        login_name: Option<String>,
    }
    let w: Whois = serde_json::from_slice(body).context("bad tailscaled whois JSON")?;
    let (tags, node) = match w.node {
        Some(n) => (n.tags.unwrap_or_default(), n.name),
        None => (Vec::new(), None),
    };
    // Tagged nodes report the placeholder user "tagged-devices": not a login.
    let login = w.user_profile.and_then(|p| p.login_name).filter(|l| tags.is_empty() || l.contains('@'));
    Ok(Peer { login, tags, node })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_parse() {
        let (c, b) = parse_response(b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{}").unwrap();
        assert_eq!((c, b.as_slice()), (200, b"{}".as_slice()));
        assert!(parse_response(b"garbage").is_err());
        let st = parse_status(
            br#"{"BackendState":"Running","Self":{"TailscaleIPs":["100.64.0.10","fd7a:115c:a1e0::1"],"UserID":54},
                "User":{"54":{"LoginName":"me@example.com"}}}"#,
        )
        .unwrap();
        assert!(st.running);
        assert_eq!(st.ips.len(), 2);
        assert_eq!(st.login.as_deref(), Some("me@example.com"));
        let st = parse_status(br#"{"BackendState":"Stopped","Self":{"TailscaleIPs":null}}"#).unwrap();
        assert!(!st.running && st.ips.is_empty());
    }

    #[test]
    fn whois_and_policy() {
        let p = parse_whois(br#"{"Node":{"Name":"stick.ts.net.","Tags":["tag:imsg-portable"]},"UserProfile":{"LoginName":"tagged-devices"}}"#).unwrap();
        assert_eq!(p.login, None);
        assert!(p.allowed(&[], &["tag:imsg-portable".into()]));
        assert!(!p.allowed(&["me@example.com".into()], &[]));
        let p = parse_whois(br#"{"Node":{"Name":"laptop"},"UserProfile":{"LoginName":"Me@Example.com"}}"#).unwrap();
        assert!(p.allowed(&["me@example.com".into()], &[]));
        assert!(!p.allowed(&["other@example.com".into()], &["tag:x".into()]));
        assert_eq!(p.describe(), "Me@Example.com");
    }
}
