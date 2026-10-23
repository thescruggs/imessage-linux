//! HTTP errors: a status plus `{"error": "<Name>", "message": "<text>"}`
//! (docs/BRIDGE.md, Conventions). `<Name>` is the daemon's error short name
//! or one of the bridge's own; the status follows from the name.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use imsg_client::{CliError, exit};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub name: String,
    pub message: String,
}

/// The status for an error name (docs/BRIDGE.md, error table).
pub fn status_for(name: &str) -> StatusCode {
    match name {
        "InvalidInput" | "InvalidHandle" | "BadRequest" => StatusCode::BAD_REQUEST,
        "Unauthorized" => StatusCode::UNAUTHORIZED,
        "Forbidden" => StatusCode::FORBIDDEN,
        "NotFound" | "ChatNotFound" => StatusCode::NOT_FOUND,
        "NotLoggedIn" | "NoHardwareConfig" | "NoPendingTwoFactor" | "BadTwoFactorCode" | "Unsupported" => StatusCode::CONFLICT,
        "TooLarge" => StatusCode::PAYLOAD_TOO_LARGE,
        "RangeNotSatisfiable" => StatusCode::RANGE_NOT_SATISFIABLE,
        "NoDaemon" | "Transient" => StatusCode::SERVICE_UNAVAILABLE,
        "Timeout" => StatusCode::GATEWAY_TIMEOUT,
        _ => StatusCode::BAD_GATEWAY,
    }
}

impl ApiError {
    pub fn new(name: &str, message: impl Into<String>) -> ApiError {
        ApiError { name: name.into(), message: message.into() }
    }

    pub fn bad_request(message: impl Into<String>) -> ApiError {
        ApiError::new("BadRequest", message)
    }

    pub fn not_found(message: impl Into<String>) -> ApiError {
        ApiError::new("NotFound", message)
    }

    pub fn status(&self) -> StatusCode {
        status_for(&self.name)
    }
}

impl From<CliError> for ApiError {
    /// A daemon error keeps its short name; "not on the bus" becomes
    /// `NoDaemon`; anything else (malformed reply, unknown D-Bus error) is
    /// a 502 `DaemonError`.
    fn from(e: CliError) -> ApiError {
        let name = match (&e.daemon_error, e.code) {
            (Some(n), _) => n.clone(),
            (None, exit::NO_DAEMON) => "NoDaemon".into(),
            (None, _) => "DaemonError".into(),
        };
        ApiError { name, message: e.message }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({"error": self.name, "message": self.message});
        (self.status(), axum::Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_map_to_statuses() {
        let cases = [
            ("org.imessage.Daemon1.Error.NotFound", "NotFound", 404),
            ("org.imessage.Daemon1.Error.ChatNotFound", "ChatNotFound", 404),
            ("org.imessage.Daemon1.Error.InvalidHandle", "InvalidHandle", 400),
            ("org.imessage.Daemon1.Error.NotLoggedIn", "NotLoggedIn", 409),
            ("org.imessage.Daemon1.Error.BadTwoFactorCode", "BadTwoFactorCode", 409),
            ("org.imessage.Daemon1.Error.Unsupported", "Unsupported", 409),
            ("org.imessage.Daemon1.Error.Transient", "Transient", 503),
            ("org.imessage.Daemon1.Error.Protocol", "Protocol", 502),
            ("org.imessage.Daemon1.Error.Store", "Store", 502),
            ("org.freedesktop.DBus.Error.ServiceUnknown", "NoDaemon", 503),
            ("org.freedesktop.DBus.Error.Failed", "DaemonError", 502),
        ];
        for (dbus, name, status) in cases {
            let e = ApiError::from(imsg_client::map_error_name(dbus, Some("x")));
            assert_eq!((e.name.as_str(), e.status().as_u16()), (name, status), "{dbus}");
        }
        assert_eq!(ApiError::new("TooLarge", "").status(), 413);
        assert_eq!(ApiError::new("Timeout", "").status(), 504);
        assert_eq!(ApiError::new("Unauthorized", "").status(), 401);
        assert_eq!(ApiError::new("Forbidden", "").status(), 403);
    }
}
