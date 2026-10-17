//! Mac hardware identity ("hwconfig") as written by `tools/mac-hwinfo.sh`.
//!
//! The on-disk format is an XML property list whose layout mirrors rustpush's
//! `MacOSConfig` (with `open-absinthe`'s `HardwareConfig` nested under `inner`),
//! so the same file can later be handed to the rustpush engine unchanged:
//!
//! ```text
//! <dict>
//!   <key>inner</key>
//!   <dict>
//!     <key>product_name</key>               <string>Macmini7,1</string>
//!     <key>io_mac_address</key>             <data>…6 bytes…</data>
//!     <key>platform_serial_number</key>     <string>…</string>
//!     <key>platform_uuid</key>              <string>…</string>
//!     <key>root_disk_uuid</key>             <string>…</string>
//!     <key>board_id</key>                   <string>Mac-…</string>
//!     <key>os_build_num</key>               <string>22F66</string>
//!     <key>platform_serial_number_enc</key> <data>…</data>
//!     <key>platform_uuid_enc</key>          <data>…</data>
//!     <key>root_disk_uuid_enc</key>         <data>…</data>
//!     <key>rom</key>                        <data>…</data>
//!     <key>rom_enc</key>                    <data>…</data>
//!     <key>mlb</key>                        <string>…</string>
//!     <key>mlb_enc</key>                    <data>…</data>
//!   </dict>
//!   <key>version</key>          <string>13.4.1</string>
//!   <key>protocol_version</key> <integer>1640</integer>
//!   <key>device_id</key>        <string>…platform uuid…</string>
//!   <key>icloud_ua</key>        <string>…</string>
//!   <key>aoskit_version</key>   <string>…</string>
//!   <!-- udid: optional, omitted by the script -->
//! </dict>
//! ```
//!
//! Byte fields are (de)serialized via `serialize_bytes` / `deserialize_byte_buf`
//! so the `plist` crate reads and writes them as `<data>`.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The 14 hardware identifiers the NAC emulator presents to `IMDAppleServices`.
///
/// Field names and order match `open_absinthe::nac::HardwareConfig`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardwareConfig {
    pub product_name: String,
    #[serde(with = "bytes6")]
    pub io_mac_address: [u8; 6],
    pub platform_serial_number: String,
    pub platform_uuid: String,
    pub root_disk_uuid: String,
    pub board_id: String,
    pub os_build_num: String,
    #[serde(with = "bytes")]
    pub platform_serial_number_enc: Vec<u8>,
    #[serde(with = "bytes")]
    pub platform_uuid_enc: Vec<u8>,
    #[serde(with = "bytes")]
    pub root_disk_uuid_enc: Vec<u8>,
    #[serde(with = "bytes")]
    pub rom: Vec<u8>,
    #[serde(with = "bytes")]
    pub rom_enc: Vec<u8>,
    pub mlb: String,
    #[serde(with = "bytes")]
    pub mlb_enc: Vec<u8>,
}

/// Redacting `Debug`: this struct is a device credential; never log it verbatim.
impl fmt::Debug for HardwareConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HardwareConfig")
            .field("product_name", &self.product_name)
            .field("board_id", &self.board_id)
            .field("os_build_num", &self.os_build_num)
            .field("platform_serial_number", &mask(&self.platform_serial_number))
            .finish_non_exhaustive()
    }
}

/// Mac identity plus software-version info, mirroring rustpush's `MacOSConfig`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacOSConfig {
    pub inner: HardwareConfig,
    /// macOS product version, e.g. `13.4.1` (`sysctl kern.osproductversion`).
    pub version: String,
    /// IDS protocol version (1640 for macOS 13).
    pub protocol_version: u32,
    /// Device identifier; the script uses `IOPlatformUUID`.
    pub device_id: String,
    pub icloud_ua: String,
    pub aoskit_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udid: Option<String>,
}

/// Redacting `Debug`: `device_id` (platform UUID) and `udid` are masked; `inner`
/// uses [`HardwareConfig`]'s redacting `Debug`.
impl fmt::Debug for MacOSConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MacOSConfig")
            .field("inner", &self.inner)
            .field("version", &self.version)
            .field("protocol_version", &self.protocol_version)
            .field("device_id", &mask(&self.device_id))
            .field("icloud_ua", &self.icloud_ua)
            .field("aoskit_version", &self.aoskit_version)
            .field("udid", &self.udid.as_deref().map(mask))
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HwConfigError {
    #[error("plist error: {0}")]
    Plist(#[from] plist::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl MacOSConfig {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, HwConfigError> {
        Ok(plist::from_file(path)?)
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self, HwConfigError> {
        Ok(plist::from_bytes(bytes)?)
    }

    /// Serialize to an XML plist (same layout `tools/mac-hwinfo.sh` writes).
    pub fn to_xml(&self) -> Result<Vec<u8>, HwConfigError> {
        let mut out = Vec::new();
        plist::to_writer_xml(&mut out, self)?;
        Ok(out)
    }
}

/// Mask all but the last 3 characters of an identifier for display.
pub fn mask(s: &str) -> String {
    let n = s.chars().count();
    if n <= 3 {
        return "*".repeat(n);
    }
    let tail: String = s.chars().skip(n - 3).collect();
    format!("{}{}", "*".repeat(n - 3), tail)
}

// ---------------------------------------------------------------------------
// serde helpers: bytes <-> plist <data>
// ---------------------------------------------------------------------------

struct ByteBufVisitor;

impl<'de> serde::de::Visitor<'de> for ByteBufVisitor {
    type Value = Vec<u8>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a byte buffer (plist <data>)")
    }

    fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
        Ok(v.to_vec())
    }

    fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
        Ok(v)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(b) = seq.next_element::<u8>()? {
            out.push(b);
        }
        Ok(out)
    }
}

mod bytes {
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(v)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        d.deserialize_byte_buf(super::ByteBufVisitor)
    }
}

mod bytes6 {
    use serde::{Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(v: &[u8; 6], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(v)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 6], D::Error> {
        let v = d.deserialize_byte_buf(super::ByteBufVisitor)?;
        <[u8; 6]>::try_from(v.as_slice())
            .map_err(|_| D::Error::invalid_length(v.len(), &"exactly 6 bytes (MAC address)"))
    }
}

// ---------------------------------------------------------------------------
// Inspection / sanity checks (used by `imsg hwconfig import` and `imsg doctor`)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Level::Ok => "ok",
            Level::Warn => "WARN",
            Level::Fail => "FAIL",
        })
    }
}

/// One line of the inspection report.
#[derive(Clone, Debug)]
pub struct Check {
    /// Key path, e.g. `inner.rom`.
    pub field: String,
    pub level: Level,
    /// Safe-to-print description (lengths, masked values, problems).
    pub detail: String,
}

#[derive(Debug)]
pub struct Report {
    pub checks: Vec<Check>,
    /// Present when the file also deserialized into [`MacOSConfig`].
    pub config: Option<MacOSConfig>,
    /// Typed-deserialization error, if any.
    pub parse_error: Option<String>,
}

impl Report {
    pub fn worst(&self) -> Level {
        let w = self.checks.iter().map(|c| c.level).max().unwrap_or(Level::Ok);
        if self.config.is_none() { Level::Fail } else { w }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Str,
    Data,
    Int,
}

/// `(key, kind, required)` for every field of [`HardwareConfig`].
const INNER_FIELDS: &[(&str, Kind)] = &[
    ("product_name", Kind::Str),
    ("io_mac_address", Kind::Data),
    ("platform_serial_number", Kind::Str),
    ("platform_uuid", Kind::Str),
    ("root_disk_uuid", Kind::Str),
    ("board_id", Kind::Str),
    ("os_build_num", Kind::Str),
    ("platform_serial_number_enc", Kind::Data),
    ("platform_uuid_enc", Kind::Data),
    ("root_disk_uuid_enc", Kind::Data),
    ("rom", Kind::Data),
    ("rom_enc", Kind::Data),
    ("mlb", Kind::Str),
    ("mlb_enc", Kind::Data),
];

const OUTER_FIELDS: &[(&str, Kind)] = &[
    ("version", Kind::Str),
    ("protocol_version", Kind::Int),
    ("device_id", Kind::Str),
    ("icloud_ua", Kind::Str),
    ("aoskit_version", Kind::Str),
];

/// Number of hardware fields in [`HardwareConfig`].
const HARDWARE_FIELD_COUNT: usize = 14;
const _: () = assert!(INNER_FIELDS.len() == HARDWARE_FIELD_COUNT);

/// Inspect a hwconfig plist: presence/type of every key, byte lengths, and
/// plausibility checks. Never prints raw identifiers (serial/MLB are masked).
pub fn inspect_bytes(bytes: &[u8]) -> Result<Report, HwConfigError> {
    let value: plist::Value = plist::Value::from_reader(std::io::Cursor::new(bytes))?;
    let mut checks = Vec::new();

    let root = value.as_dictionary();
    if root.is_none() {
        checks.push(Check {
            field: "(root)".into(),
            level: Level::Fail,
            detail: "top-level object is not a dictionary".into(),
        });
    }
    let inner = root.and_then(|d| d.get("inner")).and_then(|v| v.as_dictionary());
    if root.is_some() && inner.is_none() {
        checks.push(Check {
            field: "inner".into(),
            level: Level::Fail,
            detail: "missing `inner` dictionary (HardwareConfig)".into(),
        });
    }

    let mut presence = |dict: Option<&plist::Dictionary>, prefix: &str, fields: &[(&str, Kind)]| {
        let Some(dict) = dict else { return };
        for (key, kind) in fields {
            let path = format!("{prefix}{key}");
            let (level, detail) = match (dict.get(key), kind) {
                (None, _) => (Level::Fail, "MISSING".to_string()),
                (Some(plist::Value::String(s)), Kind::Str) => {
                    (Level::Ok, format!("string, {} chars", s.chars().count()))
                }
                (Some(plist::Value::Data(d)), Kind::Data) => {
                    (Level::Ok, format!("data, {} bytes", d.len()))
                }
                (Some(plist::Value::Integer(i)), Kind::Int) => (Level::Ok, format!("integer {i}")),
                (Some(other), _) => (Level::Fail, format!("wrong type ({})", type_name(other))),
            };
            checks.push(Check { field: path, level, detail });
        }
    };
    presence(inner, "inner.", INNER_FIELDS);
    presence(root, "", OUTER_FIELDS);

    let (config, parse_error) = match plist::from_bytes::<MacOSConfig>(bytes) {
        Ok(c) => (Some(c), None),
        Err(e) => (None, Some(e.to_string())),
    };
    if let Some(c) = &config {
        checks.extend(sanity(c));
    }
    Ok(Report { checks, config, parse_error })
}

pub fn inspect_path(path: impl AsRef<Path>) -> Result<Report, HwConfigError> {
    inspect_bytes(&std::fs::read(path)?)
}

fn type_name(v: &plist::Value) -> &'static str {
    match v {
        plist::Value::Array(_) => "array",
        plist::Value::Dictionary(_) => "dict",
        plist::Value::Boolean(_) => "bool",
        plist::Value::Data(_) => "data",
        plist::Value::Date(_) => "date",
        plist::Value::Real(_) => "real",
        plist::Value::Integer(_) => "integer",
        plist::Value::String(_) => "string",
        plist::Value::Uid(_) => "uid",
        _ => "unknown",
    }
}

/// `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX` (hex, either case).
pub fn is_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    let lens = [8, 4, 4, 4, 12];
    groups.len() == 5
        && groups
            .iter()
            .zip(lens)
            .all(|(g, n)| g.len() == n && g.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Plausibility checks on a parsed config. Fail = registration will not work;
/// Warn = unusual, double-check the extraction.
pub fn sanity(c: &MacOSConfig) -> Vec<Check> {
    let h = &c.inner;
    let mut out = Vec::new();
    let mut push = |field: &str, level: Level, detail: String| {
        out.push(Check { field: field.to_string(), level, detail });
    };

    // Strings must not carry ioreg's NUL terminators or stray whitespace.
    let strings: [(&str, &str); 9] = [
        ("inner.product_name", &h.product_name),
        ("inner.platform_serial_number", &h.platform_serial_number),
        ("inner.platform_uuid", &h.platform_uuid),
        ("inner.root_disk_uuid", &h.root_disk_uuid),
        ("inner.board_id", &h.board_id),
        ("inner.os_build_num", &h.os_build_num),
        ("inner.mlb", &h.mlb),
        ("version", &c.version),
        ("device_id", &c.device_id),
    ];
    for (name, s) in strings {
        if s.is_empty() {
            push(name, Level::Fail, "empty".into());
        } else if s.contains('\0') || s.trim() != s {
            push(name, Level::Fail, "contains NUL or leading/trailing whitespace".into());
        }
    }

    // product_name: e.g. Macmini7,1 / MacBookPro18,3 / Mac14,3
    let pn_ok = h.product_name.contains(',')
        && h.product_name.starts_with("Mac")
        && h.product_name.chars().last().is_some_and(|c| c.is_ascii_digit());
    push(
        "inner.product_name",
        if pn_ok { Level::Ok } else { Level::Warn },
        format!("{:?}{}", h.product_name, if pn_ok { "" } else { " (expected like Macmini7,1)" }),
    );

    let sn = &h.platform_serial_number;
    let sn_ok = (10..=12).contains(&sn.len()) && sn.chars().all(|c| c.is_ascii_alphanumeric());
    push(
        "inner.platform_serial_number",
        if sn_ok { Level::Ok } else { Level::Fail },
        format!("{} ({} chars{})", mask(sn), sn.len(), if sn_ok { "" } else { "; expected 10-12 alphanumeric" }),
    );

    for (name, s) in [("inner.platform_uuid", &h.platform_uuid), ("inner.root_disk_uuid", &h.root_disk_uuid)] {
        let ok = is_uuid(s);
        push(name, if ok { Level::Ok } else { Level::Fail }, if ok { "well-formed UUID".into() } else { "not a UUID".into() });
    }

    let mac = h.io_mac_address;
    let mac_ok = mac != [0; 6] && mac != [0xff; 6] && mac[0] & 1 == 0;
    push(
        "inner.io_mac_address",
        if mac_ok { Level::Ok } else { Level::Fail },
        format!("6 bytes, OUI {:02x}:{:02x}:{:02x}{}", mac[0], mac[1], mac[2], if mac_ok { "" } else { " (zero/broadcast/multicast)" }),
    );

    let bid_ok = h.board_id.starts_with("Mac-");
    push(
        "inner.board_id",
        if bid_ok { Level::Ok } else { Level::Warn },
        format!("{:?}{}", h.board_id, if bid_ok { "" } else { " (expected Mac-…)" }),
    );

    let rom_ok = h.rom.len() == 6;
    push(
        "inner.rom",
        if rom_ok { Level::Ok } else { Level::Warn },
        format!("{} bytes{}", h.rom.len(), if rom_ok { "" } else { " (expected 6)" }),
    );

    let mlb_ok = (13..=17).contains(&h.mlb.len()) && h.mlb.chars().all(|c| c.is_ascii_alphanumeric());
    push(
        "inner.mlb",
        if mlb_ok { Level::Ok } else { Level::Warn },
        format!("{} ({} chars{})", mask(&h.mlb), h.mlb.len(), if mlb_ok { "" } else { "; expected 13-17 alphanumeric" }),
    );

    for (name, blob) in [
        ("inner.platform_serial_number_enc", &h.platform_serial_number_enc),
        ("inner.platform_uuid_enc", &h.platform_uuid_enc),
        ("inner.root_disk_uuid_enc", &h.root_disk_uuid_enc),
        ("inner.rom_enc", &h.rom_enc),
        ("inner.mlb_enc", &h.mlb_enc),
    ] {
        let ok = !blob.is_empty();
        push(name, if ok { Level::Ok } else { Level::Fail }, format!("{} bytes{}", blob.len(), if ok { "" } else { " (empty: registration fails with 6001)" }));
    }

    if c.device_id != h.platform_uuid {
        push("device_id", Level::Warn, "differs from inner.platform_uuid".into());
    }
    if c.protocol_version == 0 {
        push("protocol_version", Level::Fail, "zero".into());
    }
    let ver_ok = c.version.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    if !ver_ok {
        push("version", Level::Warn, format!("{:?} (expected like 13.4.1)", c.version));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_check() {
        assert!(is_uuid("00112233-4455-6677-8899-AABBCCDDEEFF"));
        assert!(!is_uuid("00112233-4455-6677-8899-AABBCCDDEEF"));
        assert!(!is_uuid("0011223344556677-8899-AABBCCDDEEFF"));
    }

    #[test]
    fn masking() {
        assert_eq!(mask("C02ABCDEF123"), "*********123");
        assert_eq!(mask("ab"), "**");
    }

    #[test]
    fn macos_config_debug_masks_device_ids() {
        let device_id = "FA4E0000-0000-4000-8000-00000000AB12";
        let udid = "00008103-001A2B3C4D5E6F70";
        let cfg = MacOSConfig {
            inner: HardwareConfig {
                product_name: "Macmini7,1".into(),
                io_mac_address: [2, 0, 0x5e, 1, 2, 3],
                platform_serial_number: "C02FAKE00001".into(),
                platform_uuid: device_id.into(),
                root_disk_uuid: "FA4E0000-0000-4000-8000-000000000002".into(),
                board_id: "Mac-35C5E08120C7EEAF".into(),
                os_build_num: "22F66".into(),
                platform_serial_number_enc: vec![1],
                platform_uuid_enc: vec![2],
                root_disk_uuid_enc: vec![3],
                rom: vec![4; 6],
                rom_enc: vec![5],
                mlb: "C02FAKEMLB000001".into(),
                mlb_enc: vec![6],
            },
            version: "13.4.1".into(),
            protocol_version: 1640,
            device_id: device_id.into(),
            icloud_ua: "ua".into(),
            aoskit_version: "aos".into(),
            udid: Some(udid.into()),
        };
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains(device_id), "device_id leaked: {dbg}");
        assert!(!dbg.contains(udid), "udid leaked: {dbg}");
        assert!(!dbg.contains("C02FAKE00001"), "serial leaked: {dbg}");
        assert!(dbg.contains("*B12"), "masked tail expected: {dbg}");
    }
}
