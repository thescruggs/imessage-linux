//! Per-device bearer tokens (docs/BRIDGE.md, Authentication).
//!
//! A token is 32 random bytes, base64url without padding; `tokens.toml`
//! (0600, replaced atomically) keeps only its SHA-256 plus bookkeeping.
//! The running bridge and `imsg-bridge token …` share the file: every
//! read-modify-write holds `tokens.lock`, and the server re-reads the file
//! when it changed, so a revoke takes effect on the next request.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use base64::Engine as _;
use imessage_common::platform::fs::{create_private_dir_all, lock_exclusive, private_options, set_mode};
use imessage_common::platform::ipc::constant_time_eq;
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One device's token, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenEntry {
    pub name: String,
    /// Hex SHA-256 of the token string.
    pub sha256: String,
    /// Unix seconds.
    pub created: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_peer: Option<String>,
}

impl TokenEntry {
    /// Is a use from `peer` at `now` worth writing down?
    pub fn needs_touch(&self, peer: &str, now: u64) -> bool {
        self.last_used.is_none_or(|u| now.saturating_sub(u) >= TOUCH_INTERVAL) || self.last_peer.as_deref() != Some(peer)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TokenFile {
    #[serde(default, rename = "token")]
    tokens: Vec<TokenEntry>,
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// A fresh token: 32 random bytes, base64url, no padding (43 characters).
pub fn generate() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// `tokens.toml` and its lock file.
#[derive(Debug, Clone)]
pub struct TokenStore {
    path: PathBuf,
}

/// `last_used` / `last_peer` are written at most this often per token
/// (unless the peer changed), so busy clients do not rewrite the file on
/// every request.
pub const TOUCH_INTERVAL: u64 = 60;

impl TokenStore {
    pub fn new(path: impl Into<PathBuf>) -> TokenStore {
        TokenStore { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// All entries (an absent file = none).
    pub fn load(&self) -> anyhow::Result<Vec<TokenEntry>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", self.path.display())),
        };
        let f: TokenFile = toml::from_str(&text).with_context(|| format!("bad token file {}", self.path.display()))?;
        Ok(f.tokens)
    }

    /// Write via a temp file + rename, mode 0600.
    fn save(&self, tokens: &[TokenEntry]) -> anyhow::Result<()> {
        let dir = self.path.parent().context("token file has no parent directory")?;
        create_private_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let text = toml::to_string(&TokenFile { tokens: tokens.to_vec() })?;
        let tmp = dir.join(format!(".tokens.toml.{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let mut f = private_options().write(true).create_new(true).open(&tmp).with_context(|| format!("cannot write {}", tmp.display()))?;
        f.write_all(b"# imsg-bridge tokens (SHA-256 only). Manage with `imsg-bridge token new|list|revoke`.\n")?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        drop(f);
        set_mode(&tmp, 0o600)?;
        std::fs::rename(&tmp, &self.path).with_context(|| format!("cannot replace {}", self.path.display()))?;
        Ok(())
    }

    /// Run `f` on the entries under the lock; saved if it returns `true`.
    fn update<T>(&self, f: impl FnOnce(&mut Vec<TokenEntry>) -> anyhow::Result<(T, bool)>) -> anyhow::Result<T> {
        let dir = self.path.parent().context("token file has no parent directory")?;
        create_private_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let lock_path = self.path.with_extension("lock");
        let mut tries = 0;
        let _lock = loop {
            match lock_exclusive(&lock_path).with_context(|| format!("cannot lock {}", lock_path.display()))? {
                Some(l) => break l,
                None if tries < 250 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(20));
                }
                None => bail!("{} is held by another process", lock_path.display()),
            }
        };
        let mut tokens = self.load()?;
        let (out, changed) = f(&mut tokens)?;
        if changed {
            self.save(&tokens)?;
        }
        Ok(out)
    }

    /// Create a token for `name` and return it (shown once, never stored).
    pub fn create(&self, name: &str) -> anyhow::Result<String> {
        let name = name.trim();
        if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.@".contains(c)) {
            bail!("token name {name:?}: use 1-64 characters from A-Z a-z 0-9 - _ . @");
        }
        self.update(|tokens| {
            if tokens.iter().any(|t| t.name == name) {
                bail!("a token named {name:?} already exists (revoke it first)");
            }
            let token = generate();
            tokens.push(TokenEntry { name: name.into(), sha256: sha256_hex(&token), created: now_secs(), last_used: None, last_peer: None });
            Ok((token, true))
        })
    }

    /// Remove `name`; `false` if there was no such token.
    pub fn revoke(&self, name: &str) -> anyhow::Result<bool> {
        self.update(|tokens| {
            let before = tokens.len();
            tokens.retain(|t| t.name != name);
            let removed = tokens.len() != before;
            Ok((removed, removed))
        })
    }

    /// Record a use of `name` from `peer` (throttled, see [`TOUCH_INTERVAL`]).
    pub fn touch(&self, name: &str, peer: &str) -> anyhow::Result<()> {
        let now = now_secs();
        self.update(|tokens| {
            let Some(t) = tokens.iter_mut().find(|t| t.name == name) else { return Ok(((), false)) };
            if !t.needs_touch(peer, now) {
                return Ok(((), false));
            }
            t.last_used = Some(now);
            t.last_peer = Some(peer.into());
            Ok(((), true))
        })
    }
}

/// The name of the entry whose hash matches `presented`. Every entry is
/// compared (no early exit), each in constant time.
pub fn verify<'a>(entries: &'a [TokenEntry], presented: &str) -> Option<&'a str> {
    let hash = sha256_hex(presented);
    let mut found = None;
    for t in entries {
        if constant_time_eq(t.sha256.as_bytes(), hash.as_bytes()) && found.is_none() {
            found = Some(t.name.as_str());
        }
    }
    found
}

/// Pairing string for a new token (docs/BRIDGE.md, Authentication).
pub fn pairing_string(host: &str, port: u16, token: &str) -> String {
    let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
    format!("imsg+bridge://{host}:{port}#{token}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_verify_touch_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path().join("st").join("tokens.toml"));
        let tok = store.create("work-stick").unwrap();
        assert_eq!(tok.len(), 43);
        assert!(store.create("work-stick").is_err());
        assert!(store.create("bad name").is_err());
        let entries = store.load().unwrap();
        assert_eq!(verify(&entries, &tok), Some("work-stick"));
        assert_eq!(verify(&entries, "nope"), None);
        assert!(!std::fs::read_to_string(store.path()).unwrap().contains(&tok));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(store.path()).unwrap().permissions().mode() & 0o777, 0o600);
        }
        store.touch("work-stick", "100.1.2.3:4").unwrap();
        let e = &store.load().unwrap()[0];
        assert_eq!(e.last_peer.as_deref(), Some("100.1.2.3:4"));
        assert!(e.last_used.is_some());
        assert!(store.revoke("work-stick").unwrap());
        assert!(!store.revoke("work-stick").unwrap());
        assert_eq!(verify(&store.load().unwrap(), &tok), None);
    }

    #[test]
    fn pairing() {
        assert_eq!(pairing_string("100.64.0.10", 7227, "T"), "imsg+bridge://100.64.0.10:7227#T");
        assert_eq!(pairing_string("fd7a::1", 7227, "T"), "imsg+bridge://[fd7a::1]:7227#T");
    }
}
