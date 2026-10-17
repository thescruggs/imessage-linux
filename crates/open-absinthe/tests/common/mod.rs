//! Shared test fixtures: pypush's published reference Mac identity and small
//! network/plist helpers. These values are JJTech's public identity (the same
//! one upstream pypush/rustpush tests use), not a private Mac.

#![allow(dead_code)]

use open_absinthe::nac::HardwareConfig;

/// The reference identity from pypush `emulated/data.plist`.
pub fn reference_hw() -> HardwareConfig {
    HardwareConfig {
        product_name: "Macmini7,1".into(),
        io_mac_address: [168, 96, 182, 31, 80, 38],
        platform_serial_number: "C07SW06DG1J1".into(),
        platform_uuid: "9CA28140-E736-59AB-9AC6-792E507D2184".into(),
        root_disk_uuid: "2CB26D1C-D73C-4F92-970B-F87341D4144E".into(),
        board_id: "Mac-35C5E08120C7EEAF".into(),
        os_build_num: "20G1225".into(),
        rom: vec![168, 96, 182, 234, 202, 10],
        mlb: "C076497007FGV6XDW".into(),
        platform_serial_number_enc: b64("zxsZESjSTtPw9SyBsELYoEM="),
        platform_uuid_enc: b64("N7rgxPQOa8kAoaUFzrgOoJI="),
        root_disk_uuid_enc: b64("RMD8Rrsnoq/nRCd5qG5pxvc="),
        rom_enc: b64("oygSjpinM48Z7vkz7Ttqaek="),
        mlb_enc: b64("mqJmiIGO40uj6irTvezB4K4="),
    }
}

/// Dependency-free base64 decoder for test fixtures.
pub fn b64(s: &str) -> Vec<u8> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let Some(v) = T.iter().position(|t| *t == c) else {
            continue;
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// `${XDG_DATA_HOME:-~/.local/share}/imessage-linux` (where tools/bootstrap.sh
/// puts IMDAppleServices and cert-1.0.plist).
pub fn data_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
    base.join("imessage-linux")
}

/// Apple's public validation cert chain from the bootstrap-placed
/// `cert-1.0.plist` (the `cert` data), or why it is unavailable.
pub fn local_cert() -> Result<Vec<u8>, String> {
    let path = data_dir().join("cert-1.0.plist");
    let raw = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let dict: plist::Dictionary = plist::from_bytes(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
    dict.get("cert")
        .and_then(|v| v.as_data())
        .map(<[u8]>::to_vec)
        .ok_or_else(|| format!("{}: no `cert` data", path.display()))
}

// Apple's IDS endpoints (init.ess / identity.ess) use Apple's PRIVATE PKI, which
// no public trust store validates; pypush and the reference port both talk to
// them with verification off. The library itself never touches the network
// (rustpush does the HTTP in production, with its own Apple-root handling), so
// rather than wiring a verification-skipping TLS client into this crate's
// dependency tree, the live test shells out to `curl -k`. Test glue only.

use std::process::Command;

/// HTTP GET returning the body bytes, or `None` on any failure (so network
/// tests can skip cleanly rather than fail when offline).
pub fn http_get(url: &str) -> Option<Vec<u8>> {
    let out = Command::new("curl").args(["-s", "-k", "--fail", "--max-time", "30", url]).output().ok()?;
    out.status.success().then_some(out.stdout)
}

/// HTTP POST of a plist body to an Apple endpoint, returning the body bytes.
pub fn http_post_plist(url: &str, body: &[u8]) -> Option<Vec<u8>> {
    use std::io::Write;
    let mut child = Command::new("curl")
        .args([
            "-s",
            "-k",
            "--fail",
            "--max-time",
            "30",
            // Apple answers bare urllib/curl with status 5003; a real client UA
            // and the apple-plist content type get a proper response.
            "-H",
            "User-Agent: com.apple.invitation-registration [Mac OS X,10.15.7,19H2,Macmini7,1]",
            "-H",
            "Content-Type: application/x-apple-plist",
            "--data-binary",
            "@-",
            url,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(body).ok()?;
    let out = child.wait_with_output().ok()?;
    out.status.success().then_some(out.stdout)
}

/// The IDS bag (`getBag?ix=1` -> inner `bag` plist), as rustpush reads it.
pub fn fetch_bag() -> Option<plist::Dictionary> {
    let raw = http_get("https://init.ess.apple.com/WebObjects/VCInit.woa/wa/getBag?ix=1")?;
    let outer: plist::Dictionary = plist::from_bytes(&raw).ok()?;
    plist::from_bytes(outer.get("bag")?.as_data()?).ok()
}

/// Fetch the identity cert chain through the IDS bag (matches rustpush):
/// `id-validation-cert` URL -> `cert` bytes.
pub fn fetch_cert_chain(bag: &plist::Dictionary) -> Option<Vec<u8>> {
    let raw = http_get(bag.get("id-validation-cert")?.as_string()?)?;
    let cert: plist::Dictionary = plist::from_bytes(&raw).ok()?;
    Some(cert.get("cert")?.as_data()?.to_vec())
}
