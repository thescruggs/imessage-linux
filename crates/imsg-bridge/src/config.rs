//! `config.toml` and the bridge's own directories (docs/BRIDGE.md, Running).
//!
//! Every key has a default, so a missing file is a valid configuration.
//! Directories follow XDG: config in `$XDG_CONFIG_HOME/imsg-bridge`, token
//! hashes and the access log in `$XDG_STATE_HOME/imsg-bridge`, uploads in
//! flight in `$XDG_CACHE_HOME/imsg-bridge/uploads`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

/// Default TCP port.
pub const DEFAULT_PORT: u16 = 7227;
/// The `listen` entry that means "this host's Tailscale addresses".
pub const TAILSCALE: &str = "tailscale";
/// The tag a portable client's ephemeral node carries (docs/PORTABLE-WINDOWS.md, Security).
pub const DEFAULT_TAG: &str = "tag:imsg-portable";

/// Where the bridge keeps its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_file: PathBuf,
    pub state_dir: PathBuf,
    pub cache_dir: PathBuf,
}

/// `$VAR` if set to an absolute path (relative values are ignored, as the
/// XDG spec says), else `~/<fallback>`.
fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| imessage_common::paths::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(fallback))
}

impl Paths {
    /// The XDG locations; `config_file` overrides only the config path.
    pub fn from_env(config_file: Option<PathBuf>) -> Paths {
        Paths {
            config_file: config_file.unwrap_or_else(|| xdg("XDG_CONFIG_HOME", ".config").join("imsg-bridge").join("config.toml")),
            state_dir: xdg("XDG_STATE_HOME", ".local/state").join("imsg-bridge"),
            cache_dir: xdg("XDG_CACHE_HOME", ".cache").join("imsg-bridge"),
        }
    }

    /// Everything under one directory (tests).
    pub fn under(dir: &Path) -> Paths {
        Paths { config_file: dir.join("config.toml"), state_dir: dir.join("state"), cache_dir: dir.join("cache") }
    }

    pub fn tokens_file(&self) -> PathBuf {
        self.state_dir.join("tokens.toml")
    }

    pub fn access_log(&self) -> PathBuf {
        self.state_dir.join("access.log")
    }

    pub fn uploads_dir(&self) -> PathBuf {
        self.cache_dir.join("uploads")
    }
}

/// One `listen` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listen {
    /// This host's Tailscale IPv4 + IPv6 on `port`.
    Tailscale,
    Addr(SocketAddr),
}

/// The parsed configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub listen: Vec<Listen>,
    pub port: u16,
    /// Empty and `allowed_tags` empty too: only this host's own Tailscale
    /// login (looked up at start) is allowed. See [`Config::load`].
    pub allowed_logins: Vec<String>,
    pub allowed_tags: Vec<String>,
    pub require_whois: bool,
    pub max_upload_mib: u64,
    pub daemon_state_dir: PathBuf,
    pub contacts_dirs: Vec<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    listen: Option<Vec<String>>,
    port: Option<u16>,
    allowed_logins: Option<Vec<String>>,
    allowed_tags: Option<Vec<String>>,
    require_whois: Option<bool>,
    max_upload_mib: Option<u64>,
    daemon_state_dir: Option<String>,
    contacts_dirs: Option<Vec<String>>,
}

impl Default for Config {
    fn default() -> Config {
        Config::from_raw(RawConfig::default()).expect("defaults are valid")
    }
}

impl Config {
    /// Read `path`; a missing file means all defaults.
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        Config::parse(&text).with_context(|| format!("bad configuration in {}", path.display()))
    }

    pub fn parse(text: &str) -> anyhow::Result<Config> {
        Config::from_raw(toml::from_str(text)?)
    }

    fn from_raw(raw: RawConfig) -> anyhow::Result<Config> {
        let listen = raw
            .listen
            .unwrap_or_else(|| vec![TAILSCALE.into()])
            .iter()
            .map(|s| parse_listen(s))
            .collect::<anyhow::Result<Vec<_>>>()?;
        if listen.is_empty() {
            bail!("`listen` is empty");
        }
        let max_upload_mib = raw.max_upload_mib.unwrap_or(100);
        if max_upload_mib == 0 {
            bail!("`max_upload_mib` must be at least 1");
        }
        let expand = imessage_common::contacts::expand_tilde;
        let daemon_state_dir = match raw.daemon_state_dir {
            Some(d) => expand(&d),
            None => match std::env::var_os("IMESSAGED_STATE_DIR").filter(|v| !v.is_empty()) {
                Some(d) => PathBuf::from(d),
                None => imessage_common::paths::default_state_dir(),
            },
        };
        let contacts_dirs = match raw.contacts_dirs {
            Some(d) => d.iter().map(|d| expand(d)).collect(),
            None => imessage_common::contacts::default_dirs(),
        };
        Ok(Config {
            listen,
            port: raw.port.unwrap_or(DEFAULT_PORT),
            allowed_logins: raw.allowed_logins.unwrap_or_default(),
            allowed_tags: raw.allowed_tags.unwrap_or_else(|| vec![DEFAULT_TAG.into()]),
            require_whois: raw.require_whois.unwrap_or(true),
            max_upload_mib,
            daemon_state_dir,
            contacts_dirs,
        })
    }

    pub fn max_upload_bytes(&self) -> u64 {
        self.max_upload_mib.saturating_mul(1024 * 1024)
    }
}

/// `"tailscale"` or `"ip:port"`. A wildcard address is refused: the bridge
/// must never listen on every interface.
fn parse_listen(s: &str) -> anyhow::Result<Listen> {
    let s = s.trim();
    if s.eq_ignore_ascii_case(TAILSCALE) {
        return Ok(Listen::Tailscale);
    }
    let addr: SocketAddr = s.parse().with_context(|| format!("listen entry {s:?} is neither \"tailscale\" nor ip:port"))?;
    if addr.ip().is_unspecified() {
        bail!("listen entry {s:?}: refusing to listen on all interfaces");
    }
    Ok(Listen::Addr(addr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.listen, vec![Listen::Tailscale]);
        assert_eq!(c.port, 7227);
        assert!(c.require_whois);
        assert_eq!(c.max_upload_bytes(), 100 * 1024 * 1024);
        assert!(c.allowed_logins.is_empty());
        assert_eq!(c.allowed_tags, vec!["tag:imsg-portable".to_string()]);
    }

    #[test]
    fn explicit_values_and_refusals() {
        let c = Config::parse(
            r#"listen = ["127.0.0.1:7300", "tailscale"]
               require_whois = false
               allowed_logins = ["a@b.c"]
               daemon_state_dir = "/x/y"
               contacts_dirs = ["/v"]"#,
        )
        .unwrap();
        assert_eq!(c.listen, vec![Listen::Addr("127.0.0.1:7300".parse().unwrap()), Listen::Tailscale]);
        assert!(!c.require_whois);
        assert_eq!(c.daemon_state_dir, PathBuf::from("/x/y"));
        assert_eq!(c.contacts_dirs, vec![PathBuf::from("/v")]);
        assert!(Config::parse(r#"listen = ["0.0.0.0:7227"]"#).is_err());
        assert!(Config::parse(r#"listen = ["[::]:7227"]"#).is_err());
        assert!(Config::parse(r#"listen = []"#).is_err());
        assert!(Config::parse(r#"bogus = 1"#).is_err());
    }
}
