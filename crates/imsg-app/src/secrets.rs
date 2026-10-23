//! `secrets.bin`: the bridge URL and token, encrypted under a passphrase.
//!
//! Layout (all integers little-endian):
//!
//! | bytes | |
//! |---|---|
//! | 8 | magic `IMSGSEC\0` |
//! | 1 | format version (1) |
//! | 1 | KDF (1 = argon2id v0x13) |
//! | 4 | argon2 memory, KiB (default 256 MiB) |
//! | 4 | argon2 iterations (default 3) |
//! | 4 | argon2 lanes (default 1) |
//! | 16 | salt |
//! | 12 | AES-GCM nonce |
//! | rest | AES-256-GCM ciphertext + tag of the JSON [`Secrets`]; the header above is the AAD |
//!
//! A new salt and nonce on every write. The file is replaced atomically.

use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

pub const MAGIC: &[u8; 8] = b"IMSGSEC\0";
pub const VERSION: u8 = 1;
const KDF_ARGON2ID: u8 = 1;
const HEADER_LEN: usize = 8 + 1 + 1 + 4 + 4 + 4 + 16 + 12;

/// argon2id memory cost (KiB): 256 MiB in release builds; small in debug
/// and test builds so they stay fast (the parameters are stored in the
/// file, so either build reads either file).
pub const KDF_M_KIB: u32 = if cfg!(debug_assertions) { 8 * 1024 } else { 256 * 1024 };
pub const KDF_T: u32 = 3;
pub const KDF_P: u32 = 1;

/// What is protected.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct Secrets {
    pub bridge_url: String,
    pub token: String,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secrets").field("bridge_url", &self.bridge_url).field("token", &"<redacted>").finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretsError {
    WrongPassphrase,
    NotASecretsFile,
    Unsupported(String),
    Io(String),
}

impl std::error::Error for SecretsError {}

impl std::fmt::Display for SecretsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecretsError::WrongPassphrase => f.write_str("wrong passphrase (or the file was changed)"),
            SecretsError::NotASecretsFile => f.write_str("secrets.bin is not an imsg-app secrets file"),
            SecretsError::Unsupported(s) => write!(f, "secrets.bin: {s}"),
            SecretsError::Io(s) => f.write_str(s),
        }
    }
}

/// The argon2 parameters and salt of a file, and the key derived from
/// them: kept after unlocking so settings can be re-saved without
/// running the KDF (and without keeping the passphrase).
#[derive(Clone)]
pub struct SealKey {
    m_kib: u32,
    t: u32,
    p: u32,
    salt: [u8; 16],
    key: Zeroizing<[u8; 32]>,
}

impl std::fmt::Debug for SealKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SealKey(argon2id m={} KiB t={} p={}, <key redacted>)", self.m_kib, self.t, self.p)
    }
}

impl SealKey {
    /// A fresh salt and the key for `passphrase` (slow: run off the UI thread).
    pub fn derive_new(passphrase: &str) -> Result<SealKey, SecretsError> {
        Self::derive_with(passphrase, KDF_M_KIB, KDF_T, KDF_P)
    }

    pub fn derive_with(passphrase: &str, m_kib: u32, t: u32, p: u32) -> Result<SealKey, SecretsError> {
        let mut salt = [0u8; 16];
        getrandom::getrandom(&mut salt).map_err(|e| SecretsError::Io(format!("no random numbers: {e}")))?;
        Self::derive(passphrase, m_kib, t, p, salt)
    }

    fn derive(passphrase: &str, m_kib: u32, t: u32, p: u32, salt: [u8; 16]) -> Result<SealKey, SecretsError> {
        let params = argon2::Params::new(m_kib, t, p, Some(32)).map_err(|e| SecretsError::Unsupported(format!("argon2 parameters: {e}")))?;
        let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        let mut key = Zeroizing::new([0u8; 32]);
        argon.hash_password_into(passphrase.as_bytes(), &salt, key.as_mut()).map_err(|e| SecretsError::Unsupported(format!("argon2: {e}")))?;
        Ok(SealKey { m_kib, t, p, salt, key })
    }

    /// Encrypt (new nonce each time).
    pub fn seal(&self, secrets: &Secrets) -> Result<Vec<u8>, SecretsError> {
        let mut nonce = [0u8; 12];
        getrandom::getrandom(&mut nonce).map_err(|e| SecretsError::Io(format!("no random numbers: {e}")))?;
        let mut out = Vec::with_capacity(HEADER_LEN + 256);
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.push(KDF_ARGON2ID);
        out.extend_from_slice(&self.m_kib.to_le_bytes());
        out.extend_from_slice(&self.t.to_le_bytes());
        out.extend_from_slice(&self.p.to_le_bytes());
        out.extend_from_slice(&self.salt);
        out.extend_from_slice(&nonce);
        let plain = Zeroizing::new(serde_json::to_vec(secrets).map_err(|e| SecretsError::Io(e.to_string()))?);
        let cipher = Aes256Gcm::new_from_slice(self.key.as_ref()).expect("32-byte key");
        let ct = cipher
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: &plain, aad: &out })
            .map_err(|_| SecretsError::Io("encryption failed".into()))?;
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Is `passphrase` the one this key was derived from (constant time)?
    pub fn matches(&self, passphrase: &str) -> bool {
        match Self::derive(passphrase, self.m_kib, self.t, self.p, self.salt) {
            Ok(k) => k.key.iter().zip(self.key.iter()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0,
            Err(_) => false,
        }
    }
}

struct Header {
    m_kib: u32,
    t: u32,
    p: u32,
    salt: [u8; 16],
    nonce: [u8; 12],
}

fn parse_header(data: &[u8]) -> Result<Header, SecretsError> {
    if data.len() < HEADER_LEN + 16 || &data[..8] != MAGIC {
        return Err(SecretsError::NotASecretsFile);
    }
    if data[8] != VERSION {
        return Err(SecretsError::Unsupported(format!("format version {} (this app reads {VERSION})", data[8])));
    }
    if data[9] != KDF_ARGON2ID {
        return Err(SecretsError::Unsupported(format!("unknown KDF {}", data[9])));
    }
    let u = |i: usize| u32::from_le_bytes(data[i..i + 4].try_into().unwrap());
    let (m_kib, t, p) = (u(10), u(14), u(18));
    // Refuse absurd costs from a damaged / crafted file before allocating.
    if !(1024..=4 * 1024 * 1024).contains(&m_kib) || !(1..=16).contains(&t) || !(1..=16).contains(&p) {
        return Err(SecretsError::Unsupported(format!("unreasonable argon2 parameters m={m_kib} KiB t={t} p={p}")));
    }
    Ok(Header { m_kib, t, p, salt: data[22..38].try_into().unwrap(), nonce: data[38..50].try_into().unwrap() })
}

/// Decrypt `data` with `passphrase` (slow: run off the UI thread). Also
/// returns the key, for re-saving.
pub fn open(data: &[u8], passphrase: &str) -> Result<(Secrets, SealKey), SecretsError> {
    let h = parse_header(data)?;
    let key = SealKey::derive(passphrase, h.m_kib, h.t, h.p, h.salt)?;
    let cipher = Aes256Gcm::new_from_slice(key.key.as_ref()).expect("32-byte key");
    let plain = Zeroizing::new(
        cipher
            .decrypt(Nonce::from_slice(&h.nonce), Payload { msg: &data[HEADER_LEN..], aad: &data[..HEADER_LEN] })
            .map_err(|_| SecretsError::WrongPassphrase)?,
    );
    let secrets: Secrets = serde_json::from_slice(&plain).map_err(|e| SecretsError::Unsupported(format!("contents: {e}")))?;
    Ok((secrets, key))
}

pub fn load(path: &Path, passphrase: &str) -> Result<(Secrets, SealKey), SecretsError> {
    let data = std::fs::read(path).map_err(|e| SecretsError::Io(format!("{}: {e}", path.display())))?;
    open(&data, passphrase)
}

pub fn save(path: &Path, key: &SealKey, secrets: &Secrets) -> Result<(), SecretsError> {
    let data = key.seal(secrets)?;
    write_atomic(path, &data).map_err(|e| SecretsError::Io(format!("{}: {e}", path.display())))
}

/// Forget: overwrite with zeros (best effort; flash media may keep old
/// blocks), then delete.
pub fn forget(path: &Path) -> std::io::Result<()> {
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::write(path, vec![0u8; meta.len() as usize]);
    }
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Write `path.tmp`, flush it to disk, rename over `path`.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        crate::appdir::create_private_dir(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// A passphrase the pairing / change screens accept.
pub fn check_new_passphrase(a: &str, b: &str) -> Result<(), &'static str> {
    if a != b {
        return Err("The two passphrases differ.");
    }
    if a.chars().count() < 8 {
        return Err("Use at least 8 characters.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s() -> Secrets {
        Secrets { bridge_url: "http://100.64.0.10:7227".into(), token: "Tq3xY0abcdefghijklmnopqrstuvwxyz-_ABCDEFGHI".into() }
    }

    #[test]
    fn round_trip_and_wrong_passphrase() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("secrets.bin");
        let key = SealKey::derive_with("correct horse", 1024, 1, 1).unwrap();
        save(&p, &key, &s()).unwrap();
        let data = std::fs::read(&p).unwrap();
        assert_eq!(&data[..8], MAGIC);
        // The token is not in the file in clear.
        assert!(!data.windows(10).any(|w| w == b"Tq3xY0abcd"));
        let (back, key2) = load(&p, "correct horse").unwrap();
        assert_eq!(back, s());
        assert!(key2.matches("correct horse") && !key2.matches("wrong"));
        assert_eq!(load(&p, "Correct horse").unwrap_err(), SecretsError::WrongPassphrase);
        // Re-saving with the kept key: new nonce, same passphrase.
        save(&p, &key2, &Secrets { token: "other".into(), bridge_url: s().bridge_url.clone() }).unwrap();
        assert_ne!(std::fs::read(&p).unwrap()[38..50], data[38..50]);
        assert_eq!(load(&p, "correct horse").unwrap().0.token, "other");
        assert!(!d.path().join("secrets.bin.tmp").exists());
        forget(&p).unwrap();
        assert!(!p.exists());
        forget(&p).unwrap();
    }

    #[test]
    fn tampering_and_garbage() {
        let key = SealKey::derive_with("pw", 1024, 1, 1).unwrap();
        let mut data = key.seal(&s()).unwrap();
        // A changed header (AAD) or ciphertext fails like a wrong passphrase.
        let mut d2 = data.clone();
        d2[30] ^= 1;
        assert_eq!(open(&d2, "pw").unwrap_err(), SecretsError::WrongPassphrase);
        let n = data.len();
        data[n - 1] ^= 1;
        assert_eq!(open(&data, "pw").unwrap_err(), SecretsError::WrongPassphrase);
        assert_eq!(open(b"hello", "pw").unwrap_err(), SecretsError::NotASecretsFile);
        let mut big = key.seal(&s()).unwrap();
        big[10..14].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(open(&big, "pw").unwrap_err(), SecretsError::Unsupported(_)));
        let mut v2 = key.seal(&s()).unwrap();
        v2[8] = 2;
        assert!(matches!(open(&v2, "pw").unwrap_err(), SecretsError::Unsupported(_)));
    }

    #[test]
    fn default_parameters_are_strong_in_release() {
        if !cfg!(debug_assertions) {
            assert_eq!(KDF_M_KIB, 256 * 1024);
        }
        assert_eq!((KDF_T, KDF_P), (3, 1));
        assert!(check_new_passphrase("abcdefgh", "abcdefgh").is_ok());
        assert!(check_new_passphrase("abcdefgh", "abcdefgi").is_err());
        assert!(check_new_passphrase("short", "short").is_err());
    }
}
