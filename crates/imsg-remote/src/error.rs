//! The one error type of the client: the bridge's `{"error", "message"}`
//! reply (docs/BRIDGE.md, Conventions) or a local failure.

/// What went wrong. `name` is the bridge's / daemon's error short name
/// (`NotFound`, `Unauthorized`, `NoDaemon`, …) for replies, or one of the
/// client's own for local failures:
///
/// | `name` | |
/// |---|---|
/// | `Transport` | could not reach the bridge, or the connection broke / timed out |
/// | `Decode` | the reply was not what docs/BRIDGE.md promises |
/// | `Io` | a local file could not be read or written |
/// | `InvalidUrl` | the bridge URL is not `http://host:port` |
/// | `Http` | an error status without a JSON body (a proxy, …) |
///
/// Act on `name` (see [`Error::is`]), not on `status`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} ({name})")]
pub struct Error {
    /// The HTTP status, `None` for local failures.
    pub status: Option<u16>,
    pub name: String,
    pub message: String,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(name: &str, message: impl Into<String>) -> Error {
        Error { status: None, name: name.into(), message: message.into() }
    }

    /// Does this error carry that short name?
    pub fn is(&self, name: &str) -> bool {
        self.name == name
    }

    /// The bridge could not be reached (or did not answer in time): worth
    /// retrying later, nothing the user did wrong.
    pub fn is_transport(&self) -> bool {
        self.name == "Transport"
    }

    /// The token was refused (revoked, or a different bridge).
    pub fn is_auth(&self) -> bool {
        self.name == "Unauthorized" || self.name == "Forbidden"
    }

    pub(crate) fn transport(e: reqwest::Error) -> Error {
        let what = if e.is_timeout() {
            "timed out"
        } else if e.is_connect() {
            "could not connect"
        } else {
            "connection failed"
        };
        // reqwest's Display includes the URL, which holds ids (handles):
        // keep the cause only.
        let cause = std::error::Error::source(&e).map(|s| s.to_string()).unwrap_or_default();
        let message = if cause.is_empty() { format!("bridge {what}") } else { format!("bridge {what}: {cause}") };
        Error { status: e.status().map(|s| s.as_u16()), name: if e.is_decode() { "Decode".into() } else { "Transport".into() }, message }
    }

    pub(crate) fn io(e: std::io::Error) -> Error {
        Error::new("Io", e.to_string())
    }

    /// An error reply: the JSON body if there is one, else the status.
    pub(crate) fn from_reply(status: u16, body: &[u8]) -> Error {
        #[derive(serde::Deserialize)]
        struct Body {
            error: String,
            #[serde(default)]
            message: String,
        }
        match serde_json::from_slice::<Body>(body) {
            Ok(b) => Error { status: Some(status), name: b.error, message: b.message },
            Err(_) => {
                let name = match status {
                    401 => "Unauthorized",
                    403 => "Forbidden",
                    404 => "NotFound",
                    413 => "TooLarge",
                    _ => "Http",
                };
                Error { status: Some(status), name: name.into(), message: format!("HTTP {status}") }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_parse() {
        let e = Error::from_reply(404, br#"{"error":"ChatNotFound","message":"no such chat"}"#);
        assert!(e.is("ChatNotFound"));
        assert_eq!(e.status, Some(404));
        assert_eq!(e.to_string(), "no such chat (ChatNotFound)");
        let e = Error::from_reply(401, b"");
        assert!(e.is("Unauthorized") && e.is_auth());
        let e = Error::from_reply(500, b"<html>");
        assert!(e.is("Http"));
        assert!(!e.is_transport());
    }
}
