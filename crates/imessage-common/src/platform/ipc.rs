//! How clients reach `imessaged` (docs/WINDOWS-PLAN.md, phase W2).
//!
//! Linux: the D-Bus session bus, name `org.imessage.Daemon1`. Windows has no
//! session bus, so the daemon serves the same interface peer-to-peer: one
//! D-Bus connection per client over TCP on 127.0.0.1, `nonce-tcp` style. It
//! writes a random nonce to [`NONCE_FILE`] and its address to
//! [`BUS_ADDRESS_FILE`], both in the state directory; a client reads the
//! address, connects, sends the nonce first and is served only if it
//! matches. `IMESSAGED_TRANSPORT=p2p` selects the peer-to-peer transport on
//! Linux too (tests), `=session` the session bus on Windows (a shipped
//! `dbus-daemon`).

use std::path::Path;

/// Environment variable read by `imessaged`, `imsg` and `imsg-tui`.
pub const TRANSPORT_ENV: &str = "IMESSAGED_TRANSPORT";

/// Daemon address for peer-to-peer clients, in the state directory.
pub const BUS_ADDRESS_FILE: &str = "bus-address";

/// The nonce a peer-to-peer client sends first, in the state directory.
pub const NONCE_FILE: &str = "bus-nonce";

/// Held (exclusively locked) by the peer-to-peer daemon of a state
/// directory while it runs.
pub const LOCK_FILE: &str = "imessaged.lock";

/// Bytes in the nonce (as dbus-daemon's `nonce-tcp`).
pub const NONCE_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// The D-Bus session bus (Linux default).
    SessionBus,
    /// Peer-to-peer over `nonce-tcp` on 127.0.0.1 (Windows default).
    PeerToPeer,
}

impl Transport {
    /// Session bus on Unix, peer-to-peer on Windows.
    pub fn platform_default() -> Transport {
        if cfg!(windows) { Transport::PeerToPeer } else { Transport::SessionBus }
    }

    /// `session` | `p2p` (case-insensitive; `bus`, `peer` also accepted).
    pub fn parse(s: &str) -> Option<Transport> {
        match s.trim().to_ascii_lowercase().as_str() {
            "session" | "bus" | "dbus" => Some(Transport::SessionBus),
            "p2p" | "peer" | "nonce-tcp" => Some(Transport::PeerToPeer),
            _ => None,
        }
    }

    /// [`TRANSPORT_ENV`] if set (an unknown value is an error), else
    /// [`Transport::platform_default`].
    pub fn from_env() -> Result<Transport, String> {
        match std::env::var(TRANSPORT_ENV) {
            Ok(v) if !v.trim().is_empty() => {
                Transport::parse(&v).ok_or_else(|| format!("{TRANSPORT_ENV}={v:?}: expected \"session\" or \"p2p\""))
            }
            _ => Ok(Transport::platform_default()),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Transport::SessionBus => "session",
            Transport::PeerToPeer => "p2p",
        }
    }
}

impl std::str::FromStr for Transport {
    type Err = String;
    fn from_str(s: &str) -> Result<Transport, String> {
        Transport::parse(s).ok_or_else(|| format!("{s:?}: expected \"session\" or \"p2p\""))
    }
}

/// D-Bus address escaping: bytes outside `[-0-9A-Za-z_/.*]` become `%xx`
/// (so a Windows path's `:` and `\` survive the `key=value,` syntax).
pub fn escape_address_value(v: &[u8]) -> String {
    let mut out = String::with_capacity(v.len());
    for &b in v {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'/' | b'.' | b'*') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02x}"));
        }
    }
    out
}

/// `nonce-tcp:host=127.0.0.1,port=<port>,noncefile=<escaped>,guid=<guid>`.
pub fn nonce_tcp_address(port: u16, nonce_file: &Path, guid: &str) -> String {
    let file = nonce_file.as_os_str().to_string_lossy();
    format!("nonce-tcp:host=127.0.0.1,port={port},noncefile={},guid={guid}", escape_address_value(file.as_bytes()))
}

/// Equal-length byte strings compared without an early exit (the nonce
/// check must not leak how many leading bytes matched).
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transports() {
        assert_eq!(Transport::parse("P2P"), Some(Transport::PeerToPeer));
        assert_eq!(Transport::parse(" session "), Some(Transport::SessionBus));
        assert_eq!(Transport::parse("tcp"), None);
        assert_eq!("p2p".parse::<Transport>(), Ok(Transport::PeerToPeer));
        assert_eq!(Transport::platform_default() == Transport::PeerToPeer, cfg!(windows));
    }

    #[test]
    fn address_escapes_windows_paths() {
        assert_eq!(escape_address_value(br"C:\Users\Me Too\x"), "C%3a%5cUsers%5cMe%20Too%5cx");
        assert_eq!(escape_address_value(b"/tmp/a-b_c.d*"), "/tmp/a-b_c.d*");
        assert_eq!(escape_address_value("é,=;".as_bytes()), "%c3%a9%2c%3d%3b");
        let a = nonce_tcp_address(4711, Path::new("/s/bus-nonce"), "0123456789abcdef0123456789abcdef");
        assert_eq!(a, "nonce-tcp:host=127.0.0.1,port=4711,noncefile=/s/bus-nonce,guid=0123456789abcdef0123456789abcdef");
    }

    #[test]
    fn constant_time() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}
