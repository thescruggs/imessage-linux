//! `imsg setup`: put Apple's files where the daemon reads them, the native
//! counterpart of `tools/bootstrap.sh --download-only` (which Windows
//! cannot run): `IMDAppleServices` (the NAC emulator's input, sha1-pinned)
//! and `cert-1.0.plist` (sha256-pinned; served over plain http) into the
//! data directory (`%LOCALAPPDATA%\imessage-linux`, Linux
//! `~/.local/share/imessage-linux`). Downloaded from the same URLs as
//! bootstrap.sh, or copied from a folder (`--from`, e.g. that directory
//! taken from a Linux install). Never redistributed: each user fetches them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use imessage_common::paths;
use imessage_common::platform::fs as pfs;
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::client::exit;

/// Same URLs as tools/bootstrap.sh (keep in sync).
const NAC_URL: &str = "https://github.com/JJTech0130/nacserver/raw/main/IMDAppleServices";
const CERT_URL: &str = "http://static.ess.apple.com/identity/validation/cert-1.0.plist";
/// Far above either file (IMDAppleServices is ~6 MB).
const MAX_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hash {
    Sha1,
    Sha256,
}

struct AppleFile {
    name: &'static str,
    url: &'static str,
    hash: Hash,
    expected: &'static str,
    dest: PathBuf,
}

fn files() -> [AppleFile; 2] {
    [
        AppleFile {
            name: "IMDAppleServices",
            url: NAC_URL,
            hash: Hash::Sha1,
            expected: open_absinthe::nac::BINARY_SHA1,
            // Honours $OPEN_ABSINTHE_BINARY, as the daemon does.
            dest: open_absinthe::nac::binary_path(),
        },
        AppleFile {
            name: paths::VALIDATION_CERT_FILE,
            url: CERT_URL,
            hash: Hash::Sha256,
            expected: paths::VALIDATION_CERT_SHA256,
            dest: paths::data_dir().join(paths::VALIDATION_CERT_FILE),
        },
    ]
}

fn digest(hash: Hash, bytes: &[u8]) -> String {
    let raw: Vec<u8> = match hash {
        Hash::Sha1 => Sha1::digest(bytes).to_vec(),
        Hash::Sha256 => Sha256::digest(bytes).to_vec(),
    };
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

fn hash_name(hash: Hash) -> &'static str {
    match hash {
        Hash::Sha1 => "sha1",
        Hash::Sha256 => "sha256",
    }
}

/// `Ok(())` if `bytes` hash to `f.expected`.
fn verify(f: &AppleFile, bytes: &[u8], from: &str) -> anyhow::Result<()> {
    let got = digest(f.hash, bytes);
    if got != f.expected {
        bail!("{} from {from}: {} {got}, expected {} (not installed)", f.name, hash_name(f.hash), f.expected);
    }
    Ok(())
}

/// Atomically write `bytes` to `dest` (owner-only; parent created 0700).
fn install(dest: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let dir = dest.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    pfs::create_private_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.part.{}", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut f = pfs::private_options().write(true).create(true).truncate(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, dest)
    };
    write()
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
        .with_context(|| format!("writing {}", dest.display()))
}

async fn download(client: &reqwest::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    let mut resp = client.get(url).send().await.and_then(|r| r.error_for_status()).with_context(|| format!("downloading {url}"))?;
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.with_context(|| format!("downloading {url}"))? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_BYTES {
            bail!("{url}: more than {} MB, not the expected file", MAX_BYTES / (1024 * 1024));
        }
    }
    Ok(body)
}

/// Install both files; `from`: copy from this folder instead of
/// downloading. A file already in place with the right hash is kept unless
/// `force`.
pub async fn run(from: Option<&Path>, force: bool) -> anyhow::Result<u8> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(20))
        .user_agent(concat!("imsg/", env!("CARGO_PKG_VERSION")))
        .build()?;
    for f in files() {
        if !force
            && let Ok(existing) = std::fs::read(&f.dest)
            && digest(f.hash, &existing) == f.expected
        {
            println!("{}: present and verified ({})", f.name, f.dest.display());
            continue;
        }
        let bytes = match from {
            Some(dir) => {
                let src = dir.join(f.name);
                let bytes = std::fs::read(&src).with_context(|| format!("reading {}", src.display()))?;
                verify(&f, &bytes, &src.display().to_string())?;
                bytes
            }
            None => {
                println!("{}: downloading {}", f.name, f.url);
                let bytes = download(&client, f.url).await?;
                verify(&f, &bytes, f.url)?;
                bytes
            }
        };
        install(&f.dest, &bytes)?;
        println!("{}: installed {} ({} bytes, {} ok)", f.name, f.dest.display(), bytes.len(), hash_name(f.hash));
    }
    println!("next: imsg hwconfig import <hwconfig.plist>, then imsg doctor");
    Ok(exit::OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests() {
        assert_eq!(digest(Hash::Sha1, b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(digest(Hash::Sha256, b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[test]
    fn wrong_hash_is_refused_and_install_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let f = AppleFile { name: "x", url: "", hash: Hash::Sha256, expected: paths::VALIDATION_CERT_SHA256, dest: dir.path().join("d/x") };
        let e = verify(&f, b"not the cert", "here").unwrap_err().to_string();
        assert!(e.contains("expected") && e.contains("not installed"), "{e}");
        install(&f.dest, b"data").unwrap();
        assert_eq!(std::fs::read(&f.dest).unwrap(), b"data");
        assert!(pfs::is_private(&std::fs::metadata(&f.dest).unwrap()));
        assert_eq!(std::fs::read_dir(dir.path().join("d")).unwrap().count(), 1, "no temp file left");
    }

    #[test]
    fn same_urls_and_hashes_as_bootstrap() {
        let script = include_str!("../../../tools/bootstrap.sh");
        for (k, v) in [
            ("NAC_URL", NAC_URL),
            ("NAC_SHA1", open_absinthe::nac::BINARY_SHA1),
            ("CERT_URL", CERT_URL),
            ("CERT_SHA256", paths::VALIDATION_CERT_SHA256),
        ] {
            assert!(script.contains(&format!("{k}=\"{v}\"")), "tools/bootstrap.sh {k} differs from {v}");
        }
    }
}
