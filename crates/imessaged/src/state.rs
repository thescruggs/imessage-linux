//! Persistent engine state, encrypted at rest.
//!
//! Everything rustpush needs to come back online without a fresh Apple ID
//! login lives in one [`SavedState`], serialized as an XML plist and sealed
//! with AES-256-GCM into `state_dir/state.enc`:
//!
//! ```text
//! offset  len  field
//! 0       4    magic  "IMSE"
//! 4       1    format version (1)
//! 5       12   random nonce (fresh for every write)
//! 17      n    AES-256-GCM ciphertext || 16-byte tag
//!              plaintext = XML plist of SavedState
//!              AAD       = the 5 header bytes (magic + version)
//! ```
//!
//! Writes are atomic: temp file in the same directory (mode 0600, `O_EXCL`),
//! `fsync`, `rename` over the target, `fsync` the directory.
//!
//! The key comes from a [`crate::keyring::KeyProvider`].

use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use imessage_common::model::{RegistrationState, SmsForwarding};
use rand::RngCore;
use rustpush::{APSState, IDSNGMIdentity, IDSUser};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::keyring::StateKey;

pub const STATE_FILE: &str = "state.enc";
pub const MAGIC: &[u8; 4] = b"IMSE";
pub const FORMAT_VERSION: u8 = 1;
const HEADER_LEN: usize = 5;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

/// Everything needed to resume. All key material referenced by `push`,
/// `users` (as keystore aliases) lives in `keystore`; `identity` holds its
/// keys inline.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct SavedState {
    /// APNs push token + keypair (keypair private part is a keystore alias).
    #[serde(default, deserialize_with = "de_push")]
    pub push: APSState,
    /// IDS users: auth certs and per-service registrations (`IDSUser`).
    #[serde(default)]
    pub users: Vec<IDSUser>,
    /// Our device's message-encryption identity (`IDSNGMIdentity`).
    #[serde(default)]
    pub identity: Option<IDSNGMIdentity>,
    /// icloud-auth `PersistAccountData` as a plist value (username, SRP
    /// password hash, GSA tokens, dsid). Lets the daemon re-login silently
    /// when IDS demands it (error 6005) unless Apple asks for 2FA again.
    #[serde(default)]
    pub account: Option<plist::Value>,
    /// rustpush `TokenProviderState` as a plist value: the MobileMe delegate
    /// tokens (`cloudKitToken`, `mmeAuthToken`, ...) CloudKit needs for
    /// contacts' shared names and photos, and when they were fetched.
    /// Dropped with `account` when `store_credentials = false`.
    #[serde(default)]
    pub cloudkit_tokens: Option<plist::Value>,
    /// keystore `SoftwareKeystoreState` as a plist value: the private keys
    /// for the APS and IDS certificates.
    #[serde(default)]
    pub keystore: Option<plist::Value>,
    /// Summary for clients, recomputed after every registration.
    #[serde(default)]
    pub registration: RegistrationState,
    /// Text Message Forwarding: requested / confirmed by the iPhone.
    #[serde(default)]
    pub sms: SmsForwarding,
}

/// rustpush serializes `APSState::token = None` by omitting the key, but its
/// `deserialize_with` makes the key mandatory, so a state saved before the
/// first APNs connection could never be loaded again. Accept a missing
/// token / keypair.
fn de_push<'de, D: serde::Deserializer<'de>>(d: D) -> Result<APSState, D::Error> {
    use serde::de::Error;
    let mut dict = match plist::Value::deserialize(d)? {
        plist::Value::Dictionary(dict) => dict,
        other => return Err(D::Error::custom(format!("push: expected a dictionary, got {other:?}"))),
    };
    let mut st = APSState::default();
    if let Some(t) = dict.remove("token") {
        let bytes = t.into_data().ok_or_else(|| D::Error::custom("push.token: expected data"))?;
        st.token = Some(bytes.try_into().map_err(|_| D::Error::custom("push.token: expected 32 bytes"))?);
    }
    if let Some(k) = dict.remove("keypair") {
        st.keypair = Some(plist::from_value(&k).map_err(D::Error::custom)?);
    }
    Ok(st)
}

impl fmt::Debug for SavedState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SavedState")
            .field("push_token", &self.push.token.map(|_| "<redacted>"))
            .field("users", &self.users.len())
            .field("identity", &self.identity.as_ref().map(|_| "<redacted>"))
            .field("account", &self.account.as_ref().map(|_| "<redacted>"))
            .field("keystore", &self.keystore.as_ref().map(|_| "<redacted>"))
            .field("registration", &self.registration)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("state file is not an imessaged state file (bad magic)")]
    BadMagic,
    #[error("state file format version {0} is not supported (expected {FORMAT_VERSION})")]
    UnsupportedVersion(u8),
    #[error("state file is truncated")]
    Truncated,
    #[error("state file failed authentication (wrong key or tampered)")]
    Decrypt,
    #[error("state plist: {0}")]
    Plist(#[from] plist::Error),
}

/// Seal any serializable value (as XML plist) with `key`.
pub fn seal<T: Serialize>(key: &StateKey, value: &T) -> Result<Vec<u8>, StateError> {
    let mut plaintext = Vec::new();
    plist::to_writer_xml(&mut plaintext, value)?;
    let out = seal_bytes(key, &plaintext);
    zeroize::Zeroize::zeroize(&mut plaintext);
    Ok(out)
}

pub fn seal_bytes(key: &StateKey, plaintext: &[u8]) -> Vec<u8> {
    let cipher = Aes256Gcm::new_from_slice(key.as_bytes()).expect("32-byte key");
    let mut nonce = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let mut header = [0u8; HEADER_LEN];
    header[..4].copy_from_slice(MAGIC);
    header[4] = FORMAT_VERSION;
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext, aad: &header })
        .expect("AES-GCM encryption cannot fail for in-memory buffers");
    let mut out = Vec::with_capacity(HEADER_LEN + NONCE_LEN + ct.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

pub fn open_bytes(key: &StateKey, data: &[u8]) -> Result<Vec<u8>, StateError> {
    if data.len() < HEADER_LEN {
        return Err(StateError::Truncated);
    }
    if &data[..4] != MAGIC {
        return Err(StateError::BadMagic);
    }
    if data[4] != FORMAT_VERSION {
        return Err(StateError::UnsupportedVersion(data[4]));
    }
    if data.len() < HEADER_LEN + NONCE_LEN + TAG_LEN {
        return Err(StateError::Truncated);
    }
    let cipher = Aes256Gcm::new_from_slice(key.as_bytes()).expect("32-byte key");
    let nonce = &data[HEADER_LEN..HEADER_LEN + NONCE_LEN];
    cipher
        .decrypt(Nonce::from_slice(nonce), Payload { msg: &data[HEADER_LEN + NONCE_LEN..], aad: &data[..HEADER_LEN] })
        .map_err(|_| StateError::Decrypt)
}

pub fn open<T: DeserializeOwned>(key: &StateKey, data: &[u8]) -> Result<T, StateError> {
    let mut plaintext = open_bytes(key, data)?;
    let r = plist::from_bytes(&plaintext);
    zeroize::Zeroize::zeroize(&mut plaintext);
    Ok(r?)
}

/// `mkdir -p` with mode 0700 for anything created; tighten `dir` itself to
/// 0700 if it is group/world accessible.
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    if !dir.exists() {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let meta = fs::metadata(dir)?;
    if meta.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Atomically replace `path` with `bytes` (mode 0600), creating/tightening
/// the parent directory to 0700 (state files).
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_private_dir(dir)?;
    atomic_write_in_place(path, bytes)
}

/// Atomically replace `path` with `bytes` (mode 0600) without touching the
/// parent directory's permissions (which must already exist). Used for
/// downloads into user-chosen directories.
pub fn atomic_write_in_place(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "state".into());
    let tmp = dir.join(format!(".{name}.tmp-{}-{:08x}", std::process::id(), rand::random::<u32>()));
    let result = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        // Persist the rename itself.
        if let Ok(d) = fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Encrypted state file handle. Cheap to clone (path + key) so saves can run
/// on the blocking pool.
#[derive(Clone)]
pub struct StateStore {
    path: PathBuf,
    key: StateKey,
}

impl fmt::Debug for StateStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StateStore").field("path", &self.path).finish_non_exhaustive()
    }
}

impl StateStore {
    pub fn new(path: impl Into<PathBuf>, key: StateKey) -> StateStore {
        StateStore { path: path.into(), key }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn io(&self, source: std::io::Error) -> StateError {
        StateError::Io { path: self.path.clone(), source }
    }

    /// `Ok(None)` if there is no state file yet.
    pub fn load<T: DeserializeOwned>(&self) -> Result<Option<T>, StateError> {
        match fs::read(&self.path) {
            Ok(data) => open(&self.key, &data).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(self.io(e)),
        }
    }

    pub fn save<T: Serialize>(&self, value: &T) -> Result<(), StateError> {
        let sealed = seal(&self.key, value)?;
        atomic_write(&self.path, &sealed).map_err(|e| self.io(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Demo {
        a: String,
        b: Vec<u8>,
    }

    fn key(b: u8) -> StateKey {
        StateKey::from_bytes([b; 32])
    }

    #[test]
    fn roundtrip() {
        let d = Demo { a: "hello".into(), b: vec![1, 2, 3] };
        let sealed = seal(&key(1), &d).unwrap();
        assert_eq!(&sealed[..4], MAGIC);
        assert_eq!(sealed[4], FORMAT_VERSION);
        assert!(!sealed.windows(5).any(|w| w == b"hello"), "plaintext leaked");
        let back: Demo = open(&key(1), &sealed).unwrap();
        assert_eq!(back, d);
        // fresh nonce each time
        assert_ne!(seal(&key(1), &d).unwrap(), sealed);
    }

    #[test]
    fn tamper_and_wrong_key_fail() {
        let sealed = seal(&key(1), &Demo { a: "x".into(), b: vec![] }).unwrap();
        for i in [5usize, 17, sealed.len() - 1] {
            let mut t = sealed.clone();
            t[i] ^= 0x01;
            assert!(matches!(open::<Demo>(&key(1), &t), Err(StateError::Decrypt)), "byte {i}");
        }
        assert!(matches!(open::<Demo>(&key(2), &sealed), Err(StateError::Decrypt)));
        // header tampering
        let mut t = sealed.clone();
        t[4] = 9;
        assert!(matches!(open::<Demo>(&key(1), &t), Err(StateError::UnsupportedVersion(9))));
        let mut t = sealed.clone();
        t[0] = b'X';
        assert!(matches!(open::<Demo>(&key(1), &t), Err(StateError::BadMagic)));
        assert!(matches!(open::<Demo>(&key(1), &sealed[..20]), Err(StateError::Truncated)));
    }

    #[test]
    fn saved_state_roundtrip_through_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("st").join(STATE_FILE), key(3));
        assert!(store.load::<SavedState>().unwrap().is_none());

        let mut s = SavedState::default();
        s.push.token = Some([7u8; 32]);
        s.account = Some(plist::Value::String("acct".into()));
        s.registration.handles = vec![imessage_common::model::Handle::new("me@example.com")];
        s.registration.registered_at = Some(10);
        store.save(&s).unwrap();

        let meta = fs::metadata(store.path()).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let back: SavedState = store.load().unwrap().unwrap();
        assert_eq!(back.push.token, Some([7u8; 32]));
        assert_eq!(back.registration, s.registration);
        assert_eq!(back.account, s.account);
        assert!(back.identity.is_none());

        let dbg = format!("{back:?}");
        assert!(dbg.contains("<redacted>") && !dbg.contains("acct"));

        // wrong key on the same file
        let other = StateStore::new(store.path(), key(4));
        assert!(matches!(other.load::<SavedState>(), Err(StateError::Decrypt)));
    }

    /// A state saved before APNs ever connected (no push token) must load.
    #[test]
    fn saved_state_without_push_token_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join(STATE_FILE), key(5));
        store.save(&SavedState::default()).unwrap();
        let back: SavedState = store.load().unwrap().unwrap();
        assert!(back.push.token.is_none() && back.push.keypair.is_none());
    }

    #[test]
    fn atomic_write_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.enc");
        atomic_write(&p, b"one").unwrap();
        atomic_write(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        let names: Vec<_> = fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("f.enc")]);
        // a failing write (target is a directory) cleans its temp file up
        let d = dir.path().join("adir");
        fs::create_dir(&d).unwrap();
        fs::write(d.join("x"), b"").unwrap();
        assert!(atomic_write(&d, b"x").is_err());
        let names: Vec<_> = fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(names.iter().all(|n| !n.contains(".tmp-")), "{names:?}");
    }
}
