//! `GET /v1/events`: the WebSocket event stream (docs/BRIDGE.md, Events).
//!
//! Each socket subscribes to the shared broadcast of [`crate::daemon`]
//! before its `hello` frame, so nothing emitted after `hello` is missed.
//! A socket is closed (and the client must reconnect and resync) when:
//! it falls more than the broadcast buffer behind (close code 1013, "lagged"),
//! a send does not complete within 10 s, or nothing (not even a pong) came
//! from the client for 75 s, i.e. two pings went unanswered.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::response::Response;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;

use crate::{Bridge, VERSION};

const PING_EVERY: Duration = Duration::from_secs(30);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_LIMIT: Duration = Duration::from_secs(75);

pub async fn ws(State(b): State<Arc<Bridge>>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| run(b, socket))
}

async fn run(b: Arc<Bridge>, socket: WebSocket) {
    let mut events = b.daemon.subscribe();
    let (mut tx, mut rx) = socket.split();
    let hello = json!({"type": "hello", "bridge": VERSION, "daemon_connected": b.daemon.is_connected()});
    if !matches!(tokio::time::timeout(SEND_TIMEOUT, tx.send(Message::Text(hello.to_string().into()))).await, Ok(Ok(()))) {
        return;
    }
    let mut ping = tokio::time::interval_at(Instant::now() + PING_EVERY, PING_EVERY);
    let mut last_heard = Instant::now();
    loop {
        let out = tokio::select! {
            ev = events.recv() => match ev {
                Ok(frame) => Message::Text(frame),
                Err(RecvError::Lagged(n)) => {
                    log::warn!("event socket fell {n} frames behind; closing it (the client reconnects and resyncs)");
                    let close = CloseFrame { code: close_code::AGAIN, reason: "lagged: reconnect and resync".into() };
                    let _ = tokio::time::timeout(SEND_TIMEOUT, tx.send(Message::Close(Some(close)))).await;
                    return;
                }
                Err(RecvError::Closed) => return,
            },
            _ = ping.tick() => {
                if last_heard.elapsed() > IDLE_LIMIT {
                    log::info!("event socket silent for {} s; closing it", IDLE_LIMIT.as_secs());
                    return;
                }
                Message::Ping(Default::default())
            }
            m = rx.next() => match m {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                // Pongs (and anything else, which is ignored) show the client is alive.
                Some(Ok(_)) => {
                    last_heard = Instant::now();
                    continue;
                }
            },
        };
        if !matches!(tokio::time::timeout(SEND_TIMEOUT, tx.send(out)).await, Ok(Ok(()))) {
            log::info!("event socket not readable for {} s (or gone); dropping it", SEND_TIMEOUT.as_secs());
            return;
        }
    }
}
