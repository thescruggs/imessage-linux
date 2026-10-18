//! Daemon configuration.
//!
//! Precedence (highest first): command line, environment (`IMESSAGED_*`),
//! `state_dir/config.toml`, built-in defaults. The state dir itself comes only
//! from the command line / env (it is where the file lives).
//!
//! `config.toml` keys (all optional):
//!
//! ```toml
//! anisette_url      = "https://ani.sidestore.io"   # remote anisette v3 server
//! anisette_command  = "ssh -F ~/.ssh/config -o BatchMode=yes -o LogLevel=ERROR mac.local .local/bin/imessage-anisette"
//!                     # command printing native Mac anisette headers as JSON
//!                     # (tools/mac-anisette-install.sh); when set, anisette_url
//!                     # is not used
//! key_provider      = "auto"                       # auto | secret-service | file
//! log_level         = "info,rustpush=warn,..."     # env_logger filter syntax (see below)
//! store_credentials = true                         # keep the SRP hash for silent re-login
//! sender_handle     = "me@example.com"             # registered handle new conversations start from
//!                     # (default: the first registered handle)
//! other_handles     = ["+15551234567"]             # more addresses of yours this device is
//!                     # not registered for (e.g. your iPhone's number): messages
//!                     # from them are your own; never sent from
//! sms_relay_number  = "+15551234567"               # your iPhone's number that relays text
//!                     # messages (Text Message Forwarding); default: the first
//!                     # phone number in other_handles
//! mac_host          = "mac.local"                  # ssh destination of your Mac for
//!                     # `imsg sync-mac` (contacts, pins); only reported
//!                     # in GetStatus, the daemon never connects to it
//! ```
//!
//! **Logging and privacy.** The default filter ([`DEFAULT_LOG_LEVEL`]) keeps
//! the protocol crates at `warn`: at `info`/`debug` rustpush logs decrypted
//! message bodies (`MessageInst` Display), the APNs connect response
//! including the push token, IDS send payloads and alias responses, and
//! icloud_auth logs the decrypted SPD (GSA tokens). A user `log_level`
//! replaces the default *entirely*; `log_level = "debug"` (or `info`)
//! therefore raises those crates too. Use e.g.
//! `"debug,rustpush=warn,icloud_auth=warn,omnisette=warn,keystore=warn"` to
//! debug the daemon alone.
//!
//! The Mac hardware identity lives in `imessage_common::hwconfig` (shared with
//! the `imsg` CLI) and is re-exported here.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

pub use imessage_common::hwconfig::{HardwareConfig, MacOSConfig};
use imessage_common::model::Handle;
pub use imessage_common::paths::{HWCONFIG_FILE, default_state_dir};

/// Default remote anisette v3 server (omnisette's `DEFAULT_ANISETTE_URL_V3`).
pub const DEFAULT_ANISETTE_URL: &str = "https://ani.sidestore.io";
/// Crates that log secrets / message contents below `warn` (see module doc).
pub const QUIET_CRATES: [&str; 4] = ["rustpush", "icloud_auth", "omnisette", "keystore"];
/// `info` for the daemon, `warn` for [`QUIET_CRATES`].
/// `zbus` / `tracing` (zbus spans forwarded to `log`) are only noisy, not
/// secret: they log D-Bus headers and body signatures, never bodies.
pub const DEFAULT_LOG_LEVEL: &str = "info,rustpush=warn,icloud_auth=warn,omnisette=warn,keystore=warn,zbus=warn,tracing=warn";
pub const CONFIG_FILE: &str = "config.toml";

/// Where the 32-byte state-encryption key comes from (see `keyring.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeyProviderKind {
    /// Secret Service (KWallet / GNOME Keyring), falling back to the file.
    #[default]
    Auto,
    /// Secret Service only; fail if unavailable.
    SecretService,
    /// `state_dir/state.key` (mode 0600) only.
    File,
}

impl FromStr for KeyProviderKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "secret-service" | "secret_service" | "secretservice" | "kwallet" => Ok(Self::SecretService),
            "file" => Ok(Self::File),
            other => Err(format!("unknown key provider {other:?} (expected auto | secret-service | file)")),
        }
    }
}

/// env_logger level names -> verbosity (off = 0 .. trace = 5).
fn log_level_rank(s: &str) -> Option<u8> {
    match s.trim().to_ascii_lowercase().as_str() {
        "off" => Some(0),
        "error" => Some(1),
        "warn" => Some(2),
        "info" => Some(3),
        "debug" => Some(4),
        "trace" => Some(5),
        _ => None,
    }
}

/// Whether the env_logger filter `filter` keeps `krate` (and all of its
/// modules) at `warn` or quieter. Mirrors env_logger's matching: the
/// directive with the longest name that is a string prefix of the target
/// wins (later ones win ties), a bare level is global, a bare module name
/// means `trace`, unparseable levels are ignored, an empty filter means
/// `error`, and an unmatched target is off. Directives naming something
/// *inside* the crate (`rustpush::aps=debug`) must be quiet too.
pub fn filter_quiets_crate(filter: &str, krate: &str) -> bool {
    let spec = filter.split('/').next().unwrap_or("");
    let mut directives: Vec<(Option<&str>, u8)> = vec![];
    for d in spec.split(',').map(str::trim).filter(|d| !d.is_empty()) {
        match d.split_once('=') {
            Some((name, lvl)) => {
                if let Some(l) = log_level_rank(lvl) {
                    let name = name.trim();
                    directives.push((if name.is_empty() { None } else { Some(name) }, l));
                }
            }
            None => match log_level_rank(d) {
                Some(l) => directives.push((None, l)),
                None => directives.push((Some(d), 5)),
            },
        }
    }
    if directives.is_empty() {
        return true; // env_logger defaults to error
    }
    let mut best: Option<(usize, u8)> = None;
    for (name, level) in &directives {
        let len = match name {
            None => 0,
            Some(n) if krate.starts_with(n) => n.len(),
            Some(n) if n.starts_with(krate) && *level > 2 => return false, // a loud sub-module
            Some(_) => continue,
        };
        if best.is_none_or(|(l, _)| len >= l) {
            best = Some((len, *level));
        }
    }
    best.map_or(0, |b| b.1) <= 2
}

/// [`filter_quiets_crate`] for every one of [`QUIET_CRATES`].
pub fn filter_quiets_protocol_crates(filter: &str) -> bool {
    QUIET_CRATES.iter().all(|c| filter_quiets_crate(filter, c))
}

/// The filter env_logger will actually use: `RUST_LOG` if set, else ours.
pub fn effective_log_filter(rust_log: Option<String>, configured: &str) -> String {
    rust_log.unwrap_or_else(|| configured.to_string())
}

/// Contents of `config.toml`. Every field optional so partial files work.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub anisette_url: Option<String>,
    pub anisette_command: Option<String>,
    pub key_provider: Option<KeyProviderKind>,
    pub log_level: Option<String>,
    pub store_credentials: Option<bool>,
    pub sender_handle: Option<String>,
    pub other_handles: Option<Vec<String>>,
    pub sms_relay_number: Option<String>,
    pub mac_host: Option<String>,
}

impl FileConfig {
    pub fn parse(text: &str) -> Result<FileConfig, toml::de::Error> {
        toml::from_str(text)
    }

    /// Missing file = defaults; unreadable/invalid file = error.
    pub fn load(path: &Path) -> anyhow::Result<FileConfig> {
        match std::fs::read_to_string(path) {
            Ok(text) => FileConfig::parse(&text)
                .map_err(|e| anyhow::anyhow!("invalid {}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
            Err(e) => Err(anyhow::anyhow!("cannot read {}: {e}", path.display())),
        }
    }
}

/// Overrides from the command line / environment (filled by clap in main.rs).
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub anisette_url: Option<String>,
    pub anisette_command: Option<String>,
    pub key_provider: Option<KeyProviderKind>,
    pub log_level: Option<String>,
    pub store_credentials: Option<bool>,
    pub sender_handle: Option<String>,
    /// Replaces the file's list when `Some`.
    pub other_handles: Option<Vec<String>>,
    pub sms_relay_number: Option<String>,
    pub mac_host: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    pub state_dir: PathBuf,
    pub anisette_url: String,
    /// Command printing anisette headers as JSON (see engine::anisette).
    /// `Some` means the remote server at `anisette_url` is not used. An
    /// empty string (e.g. `IMESSAGED_ANISETTE_COMMAND=`) means unset.
    pub anisette_command: Option<String>,
    pub key_provider: KeyProviderKind,
    pub log_level: String,
    /// Keep icloud-auth `PersistAccountData` (username + SRP password hash +
    /// GSA tokens) in `state.enc` so the daemon can re-login silently after
    /// IDS error 6005. `false`: it is never persisted, and a forced
    /// re-login needs the password again.
    pub store_credentials: bool,
    /// Handle to send new messages from, as an IDS URI (`mailto:…` /
    /// `tel:…`). Used only if it is one of the registered handles; otherwise
    /// the engine warns and falls back to the first registered handle.
    /// `None` (or an empty string in the file / env) = that default.
    pub sender_handle: Option<String>,
    /// More addresses of the user's (IDS URIs) that this device is not
    /// registered for, e.g. the iPhone's phone number. Messages from them
    /// are the user's own (`is_from_me`, never a read receipt, chat ids as
    /// if they were registered); they are never registered or sent from.
    pub other_handles: Vec<String>,
    /// The iPhone number (`tel:` URI) that relays text messages; `None` =
    /// the first `tel:` entry of `other_handles` (see [`DaemonConfig::sms_number`]).
    pub sms_relay_number: Option<String>,
    /// ssh destination of the user's Mac (`mac.local`, `me@mac`, a `Host`
    /// alias) for `imsg sync-mac`. The daemon only reports it
    /// (`GetStatus.mac_host`); it never connects to it. `None` = unset.
    pub mac_host: Option<String>,
}

impl DaemonConfig {
    /// Defaults for `state_dir`.
    pub fn new(state_dir: PathBuf) -> DaemonConfig {
        DaemonConfig {
            state_dir,
            anisette_url: DEFAULT_ANISETTE_URL.to_string(),
            anisette_command: None,
            key_provider: KeyProviderKind::Auto,
            log_level: DEFAULT_LOG_LEVEL.to_string(),
            store_credentials: true,
            sender_handle: None,
            other_handles: vec![],
            sms_relay_number: None,
            mac_host: None,
        }
    }

    /// Merge defaults <- file <- overrides.
    pub fn resolve(state_dir: PathBuf, file: FileConfig, over: Overrides) -> DaemonConfig {
        let mut c = DaemonConfig::new(state_dir);
        if let Some(v) = over.anisette_url.or(file.anisette_url) {
            c.anisette_url = v.trim_end_matches('/').to_string();
        }
        if let Some(v) = over.anisette_command.or(file.anisette_command) {
            let v = v.trim().to_string();
            c.anisette_command = if v.is_empty() { None } else { Some(v) };
        }
        if let Some(v) = over.key_provider.or(file.key_provider) {
            c.key_provider = v;
        }
        if let Some(v) = over.log_level.or(file.log_level) {
            c.log_level = v;
        }
        if let Some(v) = over.store_credentials.or(file.store_credentials) {
            c.store_credentials = v;
        }
        if let Some(v) = over.sender_handle.or(file.sender_handle) {
            // Normalized to the IDS URI; an invalid value is kept as typed
            // so `load` can report it.
            let v = v.trim();
            c.sender_handle = (!v.is_empty()).then(|| Handle::try_normalize(v).unwrap_or_else(|| v.to_string()));
        }
        if let Some(v) = over.other_handles.or(file.other_handles) {
            // Normalized and de-duplicated; invalid values kept for `load`.
            for h in v.iter().map(|h| h.trim()).filter(|h| !h.is_empty()) {
                let h = Handle::try_normalize(h).unwrap_or_else(|| h.to_string());
                if !c.other_handles.contains(&h) {
                    c.other_handles.push(h);
                }
            }
        }
        if let Some(v) = over.sms_relay_number.or(file.sms_relay_number) {
            let v = v.trim();
            c.sms_relay_number = (!v.is_empty()).then(|| Handle::try_normalize(v).unwrap_or_else(|| v.to_string()));
        }
        if let Some(v) = over.mac_host.or(file.mac_host) {
            let v = v.trim();
            c.mac_host = (!v.is_empty()).then(|| v.to_string());
        }
        c
    }

    /// The iPhone number text messages are relayed through: the configured
    /// `sms_relay_number`, else the first phone number in `other_handles`.
    pub fn sms_number(&self) -> Option<String> {
        self.sms_relay_number.clone().or_else(|| self.other_handles.iter().find(|h| h.starts_with("tel:")).cloned())
    }

    /// Load `state_dir/config.toml` and apply overrides. A `sender_handle`
    /// or `other_handles` entry that is neither an email address nor a
    /// phone number is an error.
    pub fn load(state_dir: PathBuf, over: Overrides) -> anyhow::Result<DaemonConfig> {
        let file = FileConfig::load(&state_dir.join(CONFIG_FILE))?;
        let c = DaemonConfig::resolve(state_dir, file, over);
        c.validate()?;
        Ok(c)
    }

    /// Checks `resolve` cannot report.
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(h) = &self.sender_handle
            && Handle::try_normalize(h).is_none()
        {
            anyhow::bail!("invalid sender_handle {h:?}: expected an email address or phone number");
        }
        if let Some(h) = self.other_handles.iter().find(|h| Handle::try_normalize(h).is_none()) {
            anyhow::bail!("invalid other_handles entry {h:?}: expected an email address or phone number");
        }
        if let Some(n) = &self.sms_relay_number
            && !Handle::try_normalize(n).is_some_and(|h| h.starts_with("tel:"))
        {
            anyhow::bail!("invalid sms_relay_number {n:?}: expected a phone number");
        }
        if let Some(h) = &self.mac_host
            && !imessage_common::ssh::valid_host(h)
        {
            anyhow::bail!("invalid mac_host {h:?}: expected an ssh destination such as mac.local or me@mac.local");
        }
        Ok(())
    }

    pub fn hwconfig_path(&self) -> PathBuf {
        self.state_dir.join(HWCONFIG_FILE)
    }
    pub fn config_path(&self) -> PathBuf {
        self.state_dir.join(CONFIG_FILE)
    }
    pub fn state_path(&self) -> PathBuf {
        self.state_dir.join(crate::state::STATE_FILE)
    }
    pub fn key_path(&self) -> PathBuf {
        self.state_dir.join(crate::keyring::KEY_FILE)
    }
    /// rustpush's IDS peer-key cache (public keys of contacts; not encrypted).
    pub fn id_cache_path(&self) -> PathBuf {
        self.state_dir.join("id_cache.plist")
    }
    /// SQLite message store.
    pub fn store_path(&self) -> PathBuf {
        self.state_dir.join(crate::store::DB_FILE)
    }
    /// Downloaded attachments (`<id>/<filename>`, 0600 files in 0700 dirs).
    pub fn attachments_dir(&self) -> PathBuf {
        self.state_dir.join(crate::hub::ATTACHMENTS_DIR)
    }
    /// omnisette's anisette provisioning state directory (written by omnisette).
    pub fn anisette_dir(&self) -> PathBuf {
        self.state_dir.join("anisette")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let c = DaemonConfig::resolve("/s".into(), FileConfig::default(), Overrides::default());
        assert_eq!(c.anisette_url, DEFAULT_ANISETTE_URL);
        assert_eq!(c.key_provider, KeyProviderKind::Auto);
        assert_eq!(c.log_level, DEFAULT_LOG_LEVEL);
        assert!(c.store_credentials);
        assert_eq!(c.hwconfig_path(), PathBuf::from("/s/hwconfig.plist"));
        assert_eq!(c.state_path(), PathBuf::from("/s/state.enc"));
        assert_eq!(c.id_cache_path(), PathBuf::from("/s/id_cache.plist"));
    }

    #[test]
    fn file_then_overrides() {
        let f = FileConfig::parse(
            "anisette_url = \"https://ani.example/\"\nkey_provider = \"file\"\nlog_level = \"debug\"\n",
        )
        .unwrap();
        let c = DaemonConfig::resolve("/s".into(), f.clone(), Overrides::default());
        assert_eq!(c.anisette_url, "https://ani.example");
        assert_eq!(c.key_provider, KeyProviderKind::File);
        assert_eq!(c.log_level, "debug");

        let o = Overrides {
            anisette_url: Some("http://localhost:6969".into()),
            anisette_command: None,
            key_provider: Some(KeyProviderKind::SecretService),
            log_level: None,
            store_credentials: Some(false),
            sender_handle: None,
            other_handles: None,
            sms_relay_number: None,
            mac_host: None,
        };
        let c = DaemonConfig::resolve("/s".into(), f, o);
        assert_eq!(c.anisette_url, "http://localhost:6969");
        assert_eq!(c.key_provider, KeyProviderKind::SecretService);
        assert_eq!(c.log_level, "debug");
        assert!(!c.store_credentials);
        let f = FileConfig::parse("store_credentials = false").unwrap();
        assert!(!DaemonConfig::resolve("/s".into(), f, Overrides::default()).store_credentials);
    }

    /// B1: the protocol crates log message bodies, push tokens and GSA
    /// tokens at info/debug; the default filter must keep each at warn.
    #[test]
    fn default_log_filter_quiets_protocol_crates() {
        for c in QUIET_CRATES {
            assert!(DEFAULT_LOG_LEVEL.split(',').any(|d| d == format!("{c}=warn")), "{c} missing from {DEFAULT_LOG_LEVEL}");
        }
        for c in ["rustpush", "icloud_auth", "omnisette", "keystore"] {
            assert!(DEFAULT_LOG_LEVEL.contains(&format!("{c}=warn")));
        }
        assert!(DEFAULT_LOG_LEVEL.starts_with("info,"));
    }

    /// M-C / N4: the warning evaluates the effective filter and accepts a
    /// quiet global level.
    #[test]
    fn quiet_filter_detection() {
        assert!(filter_quiets_protocol_crates(DEFAULT_LOG_LEVEL));
        for quiet in ["warn", "error", "off", "", "WARN", "debug,rustpush=warn,icloud_auth=error,omnisette=off,keystore=warn", "imessaged=debug"] {
            assert!(filter_quiets_protocol_crates(quiet), "{quiet:?} is quiet");
        }
        for loud in [
            "info",
            "debug",
            "trace",
            "warn,rustpush=info",
            "rustpush",
            "debug,rustpush=warn,icloud_auth=warn,omnisette=warn",
            "warn,rustpush::aps=debug",
            "warn,rust=debug",
            "info,rustpush=warn,icloud_auth=warn,omnisette=warn,keystore=warn,rustpush=debug",
        ] {
            assert!(!filter_quiets_protocol_crates(loud), "{loud:?} is loud");
        }
        // a crate-specific directive beats the global one either way
        assert!(filter_quiets_crate("debug,rustpush=warn", "rustpush"));
        assert!(!filter_quiets_crate("warn,rustpush=debug", "rustpush"));
        assert!(filter_quiets_crate("rustpush=bogus", "rustpush"), "invalid level ignored -> unmatched -> off");
        assert_eq!(effective_log_filter(Some("trace".into()), "warn"), "trace");
        assert_eq!(effective_log_filter(None, "warn"), "warn");
    }

    #[test]
    fn partial_and_invalid_files() {
        assert_eq!(FileConfig::parse("").unwrap(), FileConfig::default());
        assert_eq!(
            FileConfig::parse("key_provider = \"secret-service\"").unwrap().key_provider,
            Some(KeyProviderKind::SecretService)
        );
        assert!(FileConfig::parse("key_provider = \"bogus\"").is_err());
        assert!(FileConfig::parse("unknown_key = 1").is_err());
    }

    #[test]
    fn load_missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let c = DaemonConfig::load(dir.path().into(), Overrides::default()).unwrap();
        assert_eq!(c.anisette_url, DEFAULT_ANISETTE_URL);
        std::fs::write(dir.path().join(CONFIG_FILE), "log_level = \"warn\"").unwrap();
        let c = DaemonConfig::load(dir.path().into(), Overrides::default()).unwrap();
        assert_eq!(c.log_level, "warn");
        std::fs::write(dir.path().join(CONFIG_FILE), "log_level = [").unwrap();
        assert!(DaemonConfig::load(dir.path().into(), Overrides::default()).is_err());
    }

    #[test]
    fn sms_relay_number_resolution() {
        let c = DaemonConfig::resolve("/s".into(), FileConfig::default(), Overrides::default());
        assert_eq!(c.sms_number(), None);
        let f = FileConfig::parse("other_handles = [\"me@example.com\", \"+1 555 555 0100\"]").unwrap();
        let c = DaemonConfig::resolve("/s".into(), f.clone(), Overrides::default());
        assert_eq!(c.sms_number().as_deref(), Some("tel:+15555550100"));
        let o = Overrides { sms_relay_number: Some("(555) 555-0199".into()), ..Default::default() };
        let c = DaemonConfig::resolve("/s".into(), f, o);
        assert_eq!(c.sms_number().as_deref(), Some("tel:5555550199"));
        c.validate().unwrap();
        let f = FileConfig::parse("sms_relay_number = \"me@example.com\"").unwrap();
        assert!(DaemonConfig::resolve("/s".into(), f, Overrides::default()).validate().is_err());
    }

    #[test]
    fn mac_host_resolution() {
        let c = DaemonConfig::resolve("/s".into(), FileConfig::default(), Overrides::default());
        assert_eq!(c.mac_host, None);
        let f = FileConfig::parse("mac_host = \" bebop.local \"").unwrap();
        let c = DaemonConfig::resolve("/s".into(), f.clone(), Overrides::default());
        assert_eq!(c.mac_host.as_deref(), Some("bebop.local"));
        c.validate().unwrap();
        let o = Overrides { mac_host: Some("me@mac".into()), ..Default::default() };
        assert_eq!(DaemonConfig::resolve("/s".into(), f.clone(), o).mac_host.as_deref(), Some("me@mac"));
        // IMESSAGED_MAC_HOST= means unset
        let o = Overrides { mac_host: Some(" ".into()), ..Default::default() };
        assert_eq!(DaemonConfig::resolve("/s".into(), f, o).mac_host, None);
        for bad in ["-oProxyCommand=x", "mac local", "mac;rm"] {
            let o = Overrides { mac_host: Some(bad.into()), ..Default::default() };
            assert!(DaemonConfig::resolve("/s".into(), FileConfig::default(), o).validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn sender_handle_resolution() {
        let c = DaemonConfig::resolve("/s".into(), FileConfig::default(), Overrides::default());
        assert_eq!(c.sender_handle, None);
        // bare email / phone accepted, stored as the IDS URI
        let f = FileConfig::parse("sender_handle = \" Me@Example.com \"").unwrap();
        let c = DaemonConfig::resolve("/s".into(), f.clone(), Overrides::default());
        assert_eq!(c.sender_handle.as_deref(), Some("mailto:me@example.com"));
        assert!(c.validate().is_ok());
        let o = Overrides { sender_handle: Some("+1 (555) 123-4567".into()), ..Default::default() };
        let c = DaemonConfig::resolve("/s".into(), f.clone(), o);
        assert_eq!(c.sender_handle.as_deref(), Some("tel:+15551234567"));
        // an empty override (IMESSAGED_SENDER_HANDLE=) means the default
        let o = Overrides { sender_handle: Some("  ".into()), ..Default::default() };
        assert_eq!(DaemonConfig::resolve("/s".into(), f, o).sender_handle, None);
        // garbage is kept as typed and rejected by load()
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), "sender_handle = \"bob\"").unwrap();
        let e = DaemonConfig::load(dir.path().into(), Overrides::default()).unwrap_err();
        assert!(e.to_string().contains("invalid sender_handle"), "{e}");
        std::fs::write(dir.path().join(CONFIG_FILE), "sender_handle = \"mailto:me@example.com\"").unwrap();
        let c = DaemonConfig::load(dir.path().into(), Overrides::default()).unwrap();
        assert_eq!(c.sender_handle.as_deref(), Some("mailto:me@example.com"));
    }

    #[test]
    fn other_handles_resolution() {
        let c = DaemonConfig::resolve("/s".into(), FileConfig::default(), Overrides::default());
        assert!(c.other_handles.is_empty());
        let f = FileConfig::parse("other_handles = [\"+1 (555) 123-4567\", \" Me@Example.com \", \"tel:+15551234567\", \"\"]").unwrap();
        let c = DaemonConfig::resolve("/s".into(), f.clone(), Overrides::default());
        assert_eq!(c.other_handles, vec!["tel:+15551234567".to_string(), "mailto:me@example.com".to_string()]);
        assert!(c.validate().is_ok());
        // CLI / env replace the file's list
        let o = Overrides { other_handles: Some(vec!["+15550000000".into()]), ..Default::default() };
        assert_eq!(DaemonConfig::resolve("/s".into(), f, o).other_handles, vec!["tel:+15550000000".to_string()]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), "other_handles = [\"+15551234567\", \"bob\"]").unwrap();
        let e = DaemonConfig::load(dir.path().into(), Overrides::default()).unwrap_err();
        assert!(e.to_string().contains("invalid other_handles entry \"bob\""), "{e}");
    }

    #[test]
    fn key_provider_from_str() {
        assert_eq!("AUTO".parse::<KeyProviderKind>().unwrap(), KeyProviderKind::Auto);
        assert_eq!("kwallet".parse::<KeyProviderKind>().unwrap(), KeyProviderKind::SecretService);
        assert!("x".parse::<KeyProviderKind>().is_err());
    }
}
