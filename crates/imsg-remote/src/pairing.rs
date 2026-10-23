//! Pairing strings: `imsg+bridge://<host>:<port>#<token>` (docs/BRIDGE.md,
//! Authentication), printed once by `imsg-bridge token new`.

use std::fmt;

/// The bridge's default port.
pub const DEFAULT_PORT: u16 = 7227;
pub const SCHEME: &str = "imsg+bridge://";

/// A parsed pairing string: where the bridge is and the bearer token.
#[derive(Clone, PartialEq, Eq)]
pub struct Pairing {
    /// `http://<host>:<port>`, no trailing slash.
    pub bridge_url: String,
    pub token: String,
}

impl fmt::Debug for Pairing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pairing").field("bridge_url", &self.bridge_url).field("token", &"<redacted>").finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PairingError {
    #[error("a pairing string starts with imsg+bridge://")]
    Scheme,
    #[error("the pairing string has no host")]
    Host,
    #[error("the pairing string has a bad port")]
    Port,
    #[error("the pairing string has no token (the part after #)")]
    NoToken,
    #[error("the token is not valid (expected 43 characters of A-Z a-z 0-9 - _)")]
    BadToken,
}

/// Is `t` shaped like a bridge token (base64url without padding; 32 bytes
/// are 43 characters, a little slack for future lengths)?
pub fn valid_token(t: &str) -> bool {
    (32..=128).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Parse `imsg+bridge://host[:port][/]#token`. Whitespace around it is
/// ignored (pasted from a terminal); IPv6 hosts are written `[addr]`.
pub fn parse_pairing(s: &str) -> Result<Pairing, PairingError> {
    let s = s.trim();
    let rest = s
        .get(..SCHEME.len())
        .filter(|p| p.eq_ignore_ascii_case(SCHEME))
        .map(|_| &s[SCHEME.len()..])
        .ok_or(PairingError::Scheme)?;
    let (authority, token) = rest.split_once('#').ok_or(PairingError::NoToken)?;
    let authority = authority.trim_end_matches('/');
    if authority.contains(['/', '?', '@']) {
        return Err(PairingError::Host);
    }
    let (host, port) = split_host_port(authority)?;
    let token = token.trim();
    if token.is_empty() {
        return Err(PairingError::NoToken);
    }
    if !valid_token(token) {
        return Err(PairingError::BadToken);
    }
    Ok(Pairing { bridge_url: format!("http://{host}:{port}"), token: token.to_string() })
}

fn split_host_port(a: &str) -> Result<(&str, u16), PairingError> {
    let (host, port) = if let Some(r) = a.strip_prefix('[') {
        let (h, after) = r.split_once(']').ok_or(PairingError::Host)?;
        if h.is_empty() || !h.contains(':') {
            return Err(PairingError::Host);
        }
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':').ok_or(PairingError::Port)?),
        };
        (&a[..h.len() + 2], port)
    } else {
        match a.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (a, None),
        }
    };
    if host.is_empty() || host.contains(char::is_whitespace) || (!host.starts_with('[') && host.contains(':')) {
        return Err(PairingError::Host);
    }
    let port = match port {
        None => DEFAULT_PORT,
        Some(p) => p.parse::<u16>().ok().filter(|p| *p != 0).ok_or(PairingError::Port)?,
    };
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "Tq3xY0abcdefghijklmnopqrstuvwxyz-_ABCDEFGHI";

    #[test]
    fn parses() {
        assert_eq!(T.len(), 43);
        let p = parse_pairing(&format!("imsg+bridge://100.64.0.10:7227#{T}")).unwrap();
        assert_eq!(p.bridge_url, "http://100.64.0.10:7227");
        assert_eq!(p.token, T);
        // The plan's variant with a slash, whitespace, upper-case scheme.
        let p = parse_pairing(&format!("  IMSG+BRIDGE://vash:8000/#{T}\n")).unwrap();
        assert_eq!(p.bridge_url, "http://vash:8000");
        // Default port, IPv6.
        assert_eq!(parse_pairing(&format!("imsg+bridge://vash#{T}")).unwrap().bridge_url, "http://vash:7227");
        let p = parse_pairing(&format!("imsg+bridge://[fd7a:115c::1]:7227#{T}")).unwrap();
        assert_eq!(p.bridge_url, "http://[fd7a:115c::1]:7227");
        assert_eq!(parse_pairing(&format!("imsg+bridge://[fd7a::1]#{T}")).unwrap().bridge_url, "http://[fd7a::1]:7227");
        assert!(!format!("{p:?}").contains(T));
    }

    #[test]
    fn rejects() {
        use PairingError::*;
        let cases = [
            (format!("http://vash:7227#{T}"), Scheme),
            ("imsg+bridge://vash:7227".to_string(), NoToken),
            ("imsg+bridge://vash:7227#".to_string(), NoToken),
            ("imsg+bridge://vash:7227#short".to_string(), BadToken),
            (format!("imsg+bridge://vash:7227#{T}!"), BadToken),
            (format!("imsg+bridge://:7227#{T}"), Host),
            (format!("imsg+bridge://vash:x#{T}"), Port),
            (format!("imsg+bridge://vash:0#{T}"), Port),
            (format!("imsg+bridge://vash:70000#{T}"), Port),
            (format!("imsg+bridge://a/b:1#{T}"), Host),
            (format!("imsg+bridge://u@vash:1#{T}"), Host),
            (format!("imsg+bridge://fd7a::1:7227#{T}"), Host),
            (format!("imsg+bridge://[fd7a::1]x#{T}"), Port),
            (format!("imsg+bridge://[]:1#{T}"), Host),
        ];
        for (s, want) in cases {
            assert_eq!(parse_pairing(&s), Err(want), "{s}");
        }
    }
}
