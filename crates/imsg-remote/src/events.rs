//! `GET /v1/events`: the bridge's WebSocket event stream (docs/BRIDGE.md,
//! Events), reconnecting with backoff.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{SinkExt, Stream, StreamExt};
use imessage_common::model::{Chat, HandleProfile, Message, SmsForwarding};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{self, Message as WsMessage, client::IntoClientRequest, http::HeaderValue};

/// What the bridge tells us, plus the stream's own `Connected` / `Lost`.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteEvent {
    /// The socket is up and the bridge said `hello`: at start and again
    /// after every reconnect. Anything may have been missed in between:
    /// reload the status, the chat list and the open chat's newest page
    /// (the resync rule).
    Connected { bridge: String, daemon_connected: bool },
    /// The socket closed or could not be opened; the stream retries
    /// (1 s, doubling to 30 s). Followed by `Connected` once it is back.
    Lost(String),
    MessageReceived { chat_id: String, message: Message },
    MessageUpdated { chat_id: String, message: Message },
    ChatUpdated(Chat),
    Typing { chat_id: String, handle: String, typing: bool },
    /// The daemon's connection state name (`connected`, `needs_login`, …).
    ConnectionState(String),
    TwoFactorRequired,
    BackgroundError(String),
    HandleProfileUpdated { handle: String, profile: Option<HandleProfile> },
    SmsForwardingChanged(SmsForwarding),
    /// The bridge's own D-Bus link to imessaged dropped (`connected:
    /// false`) or came back (`true`, then resync as for `Connected`).
    Daemon { connected: bool, detail: Option<String> },
}

impl RemoteEvent {
    /// Must the client reload everything (docs/BRIDGE.md, resync rule)?
    pub fn needs_resync(&self) -> bool {
        matches!(self, RemoteEvent::Connected { .. } | RemoteEvent::Daemon { connected: true, .. })
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Frame {
    Hello {
        #[serde(default)]
        bridge: String,
        #[serde(default)]
        daemon_connected: bool,
    },
    MessageReceived {
        chat_id: String,
        message: Message,
    },
    MessageUpdated {
        chat_id: String,
        message: Message,
    },
    ChatUpdated {
        chat: Chat,
    },
    Typing {
        chat_id: String,
        handle: String,
        typing: bool,
    },
    ConnectionState {
        state: serde_json::Value,
    },
    TwoFactorRequired,
    BackgroundError {
        #[serde(default)]
        message: String,
    },
    HandleProfileUpdated {
        handle: String,
        #[serde(default)]
        profile: Option<HandleProfile>,
    },
    SmsForwardingChanged {
        state: SmsForwarding,
    },
    Daemon {
        state: String,
        #[serde(default)]
        detail: Option<String>,
    },
}

/// Parse one text frame; `None` for unknown or malformed frames (logged,
/// skipped: a newer bridge may add types).
pub(crate) fn parse_frame(text: &str) -> Option<RemoteEvent> {
    let frame = match serde_json::from_str::<Frame>(text) {
        Ok(f) => f,
        Err(e) => {
            log::debug!("events: skipping a frame: {e}");
            return None;
        }
    };
    Some(match frame {
        Frame::Hello { bridge, daemon_connected } => RemoteEvent::Connected { bridge, daemon_connected },
        Frame::MessageReceived { chat_id, message } => RemoteEvent::MessageReceived { chat_id, message },
        Frame::MessageUpdated { chat_id, message } => RemoteEvent::MessageUpdated { chat_id, message },
        Frame::ChatUpdated { chat } => RemoteEvent::ChatUpdated(chat),
        Frame::Typing { chat_id, handle, typing } => RemoteEvent::Typing { chat_id, handle, typing },
        // The D-Bus signal carries the state name; accept the GetStatus
        // shape (`{"state": …, "detail": …}`) too.
        Frame::ConnectionState { state } => RemoteEvent::ConnectionState(match state {
            serde_json::Value::String(s) => s,
            serde_json::Value::Object(o) => o.get("state").and_then(|s| s.as_str()).unwrap_or("unknown").to_string(),
            _ => "unknown".into(),
        }),
        Frame::TwoFactorRequired => RemoteEvent::TwoFactorRequired,
        Frame::BackgroundError { message } => RemoteEvent::BackgroundError(message),
        Frame::HandleProfileUpdated { handle, profile } => RemoteEvent::HandleProfileUpdated { handle, profile },
        Frame::SmsForwardingChanged { state } => RemoteEvent::SmsForwardingChanged(state),
        Frame::Daemon { state, detail } => RemoteEvent::Daemon { connected: state == "connected", detail },
    })
}

/// Reconnect timing.
#[derive(Debug, Clone, Copy)]
pub struct EventOptions {
    /// First retry delay; doubles after every failed attempt.
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// Nothing at all (not even the bridge's 30 s ping) for this long =
    /// the connection is dead (e.g. the PC slept): reconnect.
    pub idle_timeout: Duration,
    pub connect_timeout: Duration,
}

impl Default for EventOptions {
    fn default() -> Self {
        EventOptions {
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(75),
            connect_timeout: Duration::from_secs(15),
        }
    }
}

/// The event stream. Implements [`Stream`]; [`EventStream::next_event`]
/// is the same without the trait. Dropping it stops the background task
/// and closes the socket.
pub struct EventStream {
    rx: mpsc::UnboundedReceiver<RemoteEvent>,
    task: tokio::task::JoinHandle<()>,
}

impl EventStream {
    pub(crate) fn start(ws_url: String, token: String, opts: EventOptions) -> EventStream {
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(ws_url, token, opts, tx));
        EventStream { rx, task }
    }

    /// The next event; `None` only after the task stopped (never on its own).
    pub async fn next_event(&mut self) -> Option<RemoteEvent> {
        self.rx.recv().await
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Stream for EventStream {
    type Item = RemoteEvent;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<RemoteEvent>> {
        self.rx.poll_recv(cx)
    }
}

enum End {
    /// The receiver was dropped: stop.
    Gone,
    /// The socket ended; `hello` = it had worked (reset the backoff).
    Lost { reason: String, hello: bool },
}

async fn run(url: String, token: String, opts: EventOptions, tx: mpsc::UnboundedSender<RemoteEvent>) {
    let mut delay = opts.initial_backoff;
    loop {
        match session(&url, &token, &opts, &tx).await {
            End::Gone => return,
            End::Lost { reason, hello } => {
                if hello {
                    delay = opts.initial_backoff;
                }
                if tx.send(RemoteEvent::Lost(reason)).is_err() {
                    return;
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = tx.closed() => return,
        }
        delay = (delay * 2).min(opts.max_backoff);
    }
}

fn describe(e: &tungstenite::Error) -> String {
    match e {
        tungstenite::Error::Http(r) => match r.status().as_u16() {
            401 => "the bridge refused the token (revoked?)".into(),
            403 => "the bridge refused this device (not an allowed tailnet peer)".into(),
            s => format!("the bridge answered HTTP {s}"),
        },
        tungstenite::Error::Io(e) => format!("could not reach the bridge: {e}"),
        e => e.to_string(),
    }
}

async fn session(url: &str, token: &str, opts: &EventOptions, tx: &mpsc::UnboundedSender<RemoteEvent>) -> End {
    let lost = |reason: String, hello: bool| End::Lost { reason, hello };
    let mut req = match url.into_client_request() {
        Ok(r) => r,
        Err(e) => return lost(format!("bad bridge URL: {e}"), false),
    };
    match HeaderValue::from_str(&format!("Bearer {token}")) {
        Ok(v) => {
            req.headers_mut().insert("Authorization", v);
        }
        Err(_) => return lost("the token has invalid characters".into(), false),
    }
    let connect = tokio_tungstenite::connect_async(req);
    let mut ws = tokio::select! {
        r = tokio::time::timeout(opts.connect_timeout, connect) => match r {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(e)) => return lost(describe(&e), false),
            Err(_) => return lost("timed out connecting to the bridge".into(), false),
        },
        _ = tx.closed() => return End::Gone,
    };
    let mut hello = false;
    loop {
        let next = tokio::select! {
            r = tokio::time::timeout(opts.idle_timeout, ws.next()) => r,
            _ = tx.closed() => {
                let _ = ws.close(None).await;
                return End::Gone;
            }
        };
        let msg = match next {
            Err(_) => return lost("the bridge stopped answering".into(), hello),
            Ok(None) => return lost("the bridge closed the connection".into(), hello),
            Ok(Some(Err(e))) => return lost(describe(&e), hello),
            Ok(Some(Ok(m))) => m,
        };
        match msg {
            WsMessage::Text(t) => {
                let Some(ev) = parse_frame(&t) else { continue };
                if matches!(ev, RemoteEvent::Connected { .. }) {
                    hello = true;
                }
                if tx.send(ev).is_err() {
                    let _ = ws.close(None).await;
                    return End::Gone;
                }
            }
            WsMessage::Close(f) => {
                let why = f.map(|f| f.reason.to_string()).filter(|r| !r.is_empty());
                return lost(why.map(|r| format!("the bridge closed the connection: {r}")).unwrap_or_else(|| "the bridge closed the connection".into()), hello);
            }
            // Pings are answered by tungstenite on the next read / flush.
            WsMessage::Ping(_) => {
                let _ = ws.flush().await;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_parse() {
        assert_eq!(
            parse_frame(r#"{"type":"hello","bridge":"0.1.14","daemon_connected":true}"#),
            Some(RemoteEvent::Connected { bridge: "0.1.14".into(), daemon_connected: true })
        );
        assert_eq!(parse_frame(r#"{"type":"connection_state","state":"needs_login"}"#), Some(RemoteEvent::ConnectionState("needs_login".into())));
        assert_eq!(
            parse_frame(r#"{"type":"connection_state","state":{"state":"error","detail":"x"}}"#),
            Some(RemoteEvent::ConnectionState("error".into()))
        );
        assert_eq!(parse_frame(r#"{"type":"two_factor_required"}"#), Some(RemoteEvent::TwoFactorRequired));
        assert_eq!(
            parse_frame(r#"{"type":"daemon","state":"lost","detail":"bus gone"}"#),
            Some(RemoteEvent::Daemon { connected: false, detail: Some("bus gone".into()) })
        );
        assert!(parse_frame(r#"{"type":"daemon","state":"connected"}"#).unwrap().needs_resync());
        assert_eq!(
            parse_frame(r#"{"type":"typing","chat_id":"tel:+1555","handle":"tel:+1555","typing":true}"#),
            Some(RemoteEvent::Typing { chat_id: "tel:+1555".into(), handle: "tel:+1555".into(), typing: true })
        );
        assert_eq!(
            parse_frame(r#"{"type":"handle_profile_updated","handle":"mailto:a@b.c","profile":null}"#),
            Some(RemoteEvent::HandleProfileUpdated { handle: "mailto:a@b.c".into(), profile: None })
        );
        let m = r#"{"type":"message_received","chat_id":"mailto:bob@example.com","message":{"id":"0F1E","chat_id":"mailto:bob@example.com",
            "sender":{"uri":"mailto:bob@example.com"},"sent_at":1,"text":"hi","parts":[{"type":"text","text":"hi"}],"subject":null,
            "reply_to":null,"is_from_me":false,"delivery":"delivered","edited":false,"unsent":false,"reactions":[],"effect":null}}"#;
        match parse_frame(m) {
            Some(RemoteEvent::MessageReceived { chat_id, message }) => {
                assert_eq!(chat_id, "mailto:bob@example.com");
                assert_eq!(message.text.as_deref(), Some("hi"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(parse_frame(r#"{"type":"from_the_future","x":1}"#), None);
        assert_eq!(parse_frame("not json"), None);
    }
}
