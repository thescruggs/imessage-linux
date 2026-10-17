//! Source of the 32-byte key that encrypts `state.enc` (see `state.rs`).
//!
//! Providers:
//! * **Secret Service** (KWallet via ksecretd / GNOME Keyring / KeePassXC),
//!   over D-Bus with the `secret-service` crate. The key is an item in the
//!   default collection with attributes `app=imessaged`, `purpose=state-key`,
//!   stored as 64 hex chars. Created on first use.
//! * **File**: `state_dir/state.key`, 32 random bytes, mode 0600. Protects
//!   only against copying `state.enc` alone (backups, sync); anyone who can
//!   read your home directory can read both files.
//!
//! `auto` tries Secret Service first and falls back to the file with a
//! one-time warning. Once a key exists in one place, `auto` keeps using it:
//! if `state.key` exists it is used (so a session without a keyring, e.g. a
//! headless boot, does not "lose" the key and orphan `state.enc`). The file
//! fallback only *creates* a key when there is no `state.enc` yet: if
//! `state.enc` exists without a `state.key`, its key is in the Secret
//! Service, and minting a new file key would shadow it forever.

use std::collections::HashMap;
use std::fmt;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use rand::RngCore;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::config::KeyProviderKind;

pub const KEY_FILE: &str = "state.key";
pub const KEY_LEN: usize = 32;
const SS_LABEL: &str = "imessaged state encryption key";
const SS_ATTRS: [(&str, &str); 2] = [("app", "imessaged"), ("purpose", "state-key")];
/// Unlock prompts may wait for a human; don't hang startup forever.
const SS_TIMEOUT: Duration = Duration::from_secs(60);

/// The state-encryption key. Zeroized on drop; `Debug` never prints it.
#[derive(Clone, Zeroize, ZeroizeOnDrop, PartialEq, Eq)]
pub struct StateKey([u8; KEY_LEN]);

impl StateKey {
    pub fn from_bytes(b: [u8; KEY_LEN]) -> StateKey {
        StateKey(b)
    }
    pub fn random() -> StateKey {
        let mut b = [0u8; KEY_LEN];
        rand::rngs::OsRng.fill_bytes(&mut b);
        StateKey(b)
    }
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
    fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
    fn from_hex(s: &str) -> Option<StateKey> {
        let s = s.trim();
        if s.len() != KEY_LEN * 2 || !s.is_ascii() {
            return None;
        }
        let mut b = [0u8; KEY_LEN];
        for (i, byte) in b.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(StateKey(b))
    }
}

impl fmt::Debug for StateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StateKey(<redacted>)")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("secret service unavailable: {0}")]
    SecretServiceUnavailable(String),
    #[error("secret service error: {0}")]
    SecretService(String),
    #[error("key file {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("key file {0} is corrupt (expected {KEY_LEN} bytes)")]
    Corrupt(PathBuf),
}

#[async_trait]
pub trait KeyProvider: Send + Sync {
    /// Return the existing key, or create and persist a new one.
    async fn get_or_create(&self) -> Result<StateKey, KeyError>;
    /// Short human description for logs (never includes the key).
    fn describe(&self) -> String;
}

// ---------------------------------------------------------------------------
// File provider
// ---------------------------------------------------------------------------

pub struct FileKeyProvider {
    pub path: PathBuf,
}

impl FileKeyProvider {
    pub fn new(path: impl Into<PathBuf>) -> FileKeyProvider {
        FileKeyProvider { path: path.into() }
    }

    fn io(&self, source: std::io::Error) -> KeyError {
        KeyError::Io { path: self.path.clone(), source }
    }

    fn read(&self) -> Result<Option<StateKey>, KeyError> {
        match std::fs::read(&self.path) {
            Ok(mut bytes) => {
                let r = if bytes.len() == KEY_LEN {
                    let mut b = [0u8; KEY_LEN];
                    b.copy_from_slice(&bytes);
                    Ok(Some(StateKey(b)))
                } else {
                    Err(KeyError::Corrupt(self.path.clone()))
                };
                bytes.zeroize();
                // Tighten permissions if someone loosened them.
                if let Ok(meta) = std::fs::metadata(&self.path)
                    && meta.permissions().mode() & 0o077 != 0
                {
                    log::warn!("{} had mode {:o}; resetting to 0600", self.path.display(), meta.permissions().mode() & 0o777);
                    let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
                }
                r
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(self.io(e)),
        }
    }

    fn create(&self) -> Result<StateKey, KeyError> {
        if let Some(dir) = self.path.parent() {
            crate::state::ensure_private_dir(dir).map_err(|e| self.io(e))?;
        }
        let key = StateKey::random();
        // create_new: never clobber a key that appeared concurrently.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&self.path)
            .map_err(|e| self.io(e))?;
        f.write_all(key.as_bytes()).map_err(|e| self.io(e))?;
        f.sync_all().map_err(|e| self.io(e))?;
        Ok(key)
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }
}

#[async_trait]
impl KeyProvider for FileKeyProvider {
    async fn get_or_create(&self) -> Result<StateKey, KeyError> {
        if let Some(k) = self.read()? {
            return Ok(k);
        }
        match self.create() {
            Ok(k) => Ok(k),
            // Lost a race with another creator: use theirs.
            Err(KeyError::Io { source, .. }) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                self.read()?.ok_or_else(|| KeyError::Corrupt(self.path.clone()))
            }
            Err(e) => Err(e),
        }
    }
    fn describe(&self) -> String {
        format!("file {}", self.path.display())
    }
}

// ---------------------------------------------------------------------------
// Secret Service provider
// ---------------------------------------------------------------------------

pub struct SecretServiceKeyProvider;

fn ss_err(e: secret_service::Error) -> KeyError {
    match e {
        secret_service::Error::Unavailable => KeyError::SecretServiceUnavailable("no provider on the session bus".into()),
        secret_service::Error::Zbus(z) => KeyError::SecretServiceUnavailable(z.to_string()),
        other => KeyError::SecretService(other.to_string()),
    }
}

impl SecretServiceKeyProvider {
    /// Cheap probe: is there a session bus with a Secret Service on it?
    pub async fn available() -> bool {
        matches!(
            tokio::time::timeout(Duration::from_secs(5), secret_service::SecretService::connect(secret_service::EncryptionType::Dh)).await,
            Ok(Ok(_))
        )
    }

    async fn inner(&self) -> Result<StateKey, KeyError> {
        use secret_service::{EncryptionType, SecretService};
        let ss = SecretService::connect(EncryptionType::Dh).await.map_err(ss_err)?;
        let attrs: HashMap<&str, &str> = SS_ATTRS.into_iter().collect();

        let found = ss.search_items(attrs.clone()).await.map_err(ss_err)?;
        let item = found.unlocked.into_iter().next().or_else(|| found.locked.into_iter().next());
        if let Some(item) = item {
            item.ensure_unlocked().await.map_err(ss_err)?;
            let mut secret = item.get_secret().await.map_err(ss_err)?;
            let parsed = std::str::from_utf8(&secret).ok().and_then(StateKey::from_hex);
            secret.zeroize();
            return parsed.ok_or_else(|| KeyError::SecretService("stored state key is malformed".into()));
        }

        let coll = ss.get_default_collection().await.map_err(ss_err)?;
        coll.ensure_unlocked().await.map_err(ss_err)?;
        let key = StateKey::random();
        let mut hex = key.to_hex();
        let r = coll.create_item(SS_LABEL, attrs, hex.as_bytes(), true, "text/plain").await;
        hex.zeroize();
        r.map_err(ss_err)?;
        log::info!("created state key in the Secret Service default collection");
        Ok(key)
    }
}

#[async_trait]
impl KeyProvider for SecretServiceKeyProvider {
    async fn get_or_create(&self) -> Result<StateKey, KeyError> {
        match tokio::time::timeout(SS_TIMEOUT, self.inner()).await {
            Ok(r) => r,
            Err(_) => Err(KeyError::SecretServiceUnavailable(format!("timed out after {}s (unlock prompt unanswered?)", SS_TIMEOUT.as_secs()))),
        }
    }
    fn describe(&self) -> String {
        "secret service (app=imessaged, purpose=state-key)".into()
    }
}

// ---------------------------------------------------------------------------
// Auto
// ---------------------------------------------------------------------------

static FALLBACK_WARNED: AtomicBool = AtomicBool::new(false);

/// Secret Service, else file. An existing key file always wins (see module doc).
pub struct AutoKeyProvider {
    pub file: FileKeyProvider,
    /// `state.enc`: if it exists, the file fallback must not create a key.
    pub state_path: PathBuf,
    /// Normally [`SecretServiceKeyProvider`]; injectable for tests.
    pub secret_service: Box<dyn KeyProvider>,
}

impl AutoKeyProvider {
    pub fn new(state_dir: &Path) -> AutoKeyProvider {
        AutoKeyProvider {
            file: FileKeyProvider::new(state_dir.join(KEY_FILE)),
            state_path: state_dir.join(crate::state::STATE_FILE),
            secret_service: Box::new(SecretServiceKeyProvider),
        }
    }
}

#[async_trait]
impl KeyProvider for AutoKeyProvider {
    async fn get_or_create(&self) -> Result<StateKey, KeyError> {
        if self.file.exists() {
            return self.file.get_or_create().await;
        }
        match self.secret_service.get_or_create().await {
            Ok(k) => Ok(k),
            Err(e) if self.state_path.exists() => Err(KeyError::SecretServiceUnavailable(format!(
                "{e}; {} exists and no {} exists; refusing to create a new key; start the Secret Service \
                 (keyring) or set key_provider=file only if you created the state with it",
                self.state_path.display(),
                self.file.path.display()
            ))),
            Err(e) => {
                if !FALLBACK_WARNED.swap(true, Ordering::Relaxed) {
                    log::warn!(
                        "Secret Service not usable ({e}); falling back to key file {} (mode 0600). \
                         Set key_provider = \"secret-service\" to require the keyring.",
                        self.file.path.display()
                    );
                }
                self.file.get_or_create().await
            }
        }
    }
    fn describe(&self) -> String {
        format!("auto (secret service, fallback {})", self.file.path.display())
    }
}

pub fn provider_for(kind: KeyProviderKind, state_dir: &Path) -> Box<dyn KeyProvider> {
    match kind {
        KeyProviderKind::File => Box::new(FileKeyProvider::new(state_dir.join(KEY_FILE))),
        KeyProviderKind::SecretService => Box::new(SecretServiceKeyProvider),
        KeyProviderKind::Auto => Box::new(AutoKeyProvider::new(state_dir)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_provider_creates_0600_and_reuses() {
        let dir = tempfile::tempdir().unwrap();
        let p = FileKeyProvider::new(dir.path().join("sub").join(KEY_FILE));
        let k1 = p.get_or_create().await.unwrap();
        let meta = std::fs::metadata(&p.path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(meta.len(), KEY_LEN as u64);
        let dmeta = std::fs::metadata(p.path.parent().unwrap()).unwrap();
        assert_eq!(dmeta.permissions().mode() & 0o077, 0, "state dir must be private");
        let k2 = p.get_or_create().await.unwrap();
        assert_eq!(k1, k2);
        // Fresh provider instance, same file.
        let k3 = FileKeyProvider::new(&p.path).get_or_create().await.unwrap();
        assert_eq!(k1, k3);
    }

    #[tokio::test]
    async fn file_provider_rejects_corrupt_and_fixes_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEY_FILE);
        std::fs::write(&path, b"short").unwrap();
        assert!(matches!(FileKeyProvider::new(&path).get_or_create().await, Err(KeyError::Corrupt(_))));
        std::fs::write(&path, [7u8; KEY_LEN]).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let k = FileKeyProvider::new(&path).get_or_create().await.unwrap();
        assert_eq!(k.as_bytes(), &[7u8; KEY_LEN]);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[tokio::test]
    async fn auto_prefers_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(KEY_FILE), [9u8; KEY_LEN]).unwrap();
        let k = provider_for(KeyProviderKind::Auto, dir.path()).get_or_create().await.unwrap();
        assert_eq!(k.as_bytes(), &[9u8; KEY_LEN]);
    }

    /// Stands in for a Secret Service that is down / not on the bus.
    struct NoSecretService;
    #[async_trait]
    impl KeyProvider for NoSecretService {
        async fn get_or_create(&self) -> Result<StateKey, KeyError> {
            Err(KeyError::SecretServiceUnavailable("test: no provider".into()))
        }
        fn describe(&self) -> String {
            "unavailable".into()
        }
    }

    fn auto_without_ss(dir: &Path) -> AutoKeyProvider {
        AutoKeyProvider { secret_service: Box::new(NoSecretService), ..AutoKeyProvider::new(dir) }
    }

    /// M2: state.enc exists, no state.key, Secret Service down => error, and
    /// no new state.key that would shadow the real (keyring) key.
    #[tokio::test]
    async fn auto_refuses_new_file_key_when_state_exists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(crate::state::STATE_FILE), b"IMSE\x01sealed-with-the-keyring-key").unwrap();
        let err = auto_without_ss(dir.path()).get_or_create().await.unwrap_err();
        assert!(matches!(&err, KeyError::SecretServiceUnavailable(m) if m.contains("refusing to create a new key")), "{err}");
        assert!(!dir.path().join(KEY_FILE).exists(), "must not create state.key");
    }

    /// Fresh install without a keyring still falls back to the file.
    #[tokio::test]
    async fn auto_falls_back_to_file_on_fresh_install() {
        let dir = tempfile::tempdir().unwrap();
        let k = auto_without_ss(dir.path()).get_or_create().await.unwrap();
        assert!(dir.path().join(KEY_FILE).exists());
        // ...and keeps using it afterwards, even once state.enc exists.
        std::fs::write(dir.path().join(crate::state::STATE_FILE), b"x").unwrap();
        assert_eq!(auto_without_ss(dir.path()).get_or_create().await.unwrap(), k);
    }

    #[test]
    fn key_debug_redacted_and_hex_roundtrip() {
        let k = StateKey::from_bytes([0xab; KEY_LEN]);
        assert_eq!(format!("{k:?}"), "StateKey(<redacted>)");
        assert_eq!(StateKey::from_hex(&k.to_hex()).unwrap(), k);
        assert!(StateKey::from_hex("zz").is_none());
    }

    /// Talks to a real Secret Service if one is on the session bus; SKIPs
    /// otherwise (CI, headless). Uses the real attributes, so on a desktop it
    /// creates/reads the same item the daemon would.
    #[tokio::test]
    async fn secret_service_roundtrip_or_skip() {
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() && std::env::var_os("XDG_RUNTIME_DIR").is_none() {
            eprintln!("SKIP secret_service_roundtrip_or_skip: no session bus");
            return;
        }
        if std::env::var_os("IMESSAGED_TEST_SECRET_SERVICE").is_none() {
            // Touching the user's real keyring (and possibly popping an
            // unlock prompt) must be opt-in.
            eprintln!("SKIP secret_service_roundtrip_or_skip: set IMESSAGED_TEST_SECRET_SERVICE=1 to run");
            return;
        }
        if !SecretServiceKeyProvider::available().await {
            eprintln!("SKIP secret_service_roundtrip_or_skip: no Secret Service provider");
            return;
        }
        let p = SecretServiceKeyProvider;
        match p.get_or_create().await {
            Ok(k1) => {
                let k2 = p.get_or_create().await.unwrap();
                assert_eq!(k1, k2);
            }
            Err(e) => eprintln!("SKIP secret_service_roundtrip_or_skip: {e}"),
        }
    }
}
