//! Everything that talks to `imessaged`: one [`Backend`] wrapping the
//! shared D-Bus client, plus the signal loop that turns the daemon's
//! signals into [`DaemonEvent`]s for the UI.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use imessage_common::model::{Chat, Message};
use imsg_client::{Bus, Client, MessagePage, Result, SentMessage, Status, Target};
use tokio::sync::mpsc;

/// What the daemon tells us while the app runs (docs/DBUS.md, Signals).
#[derive(Debug, Clone)]
pub enum DaemonEvent {
    /// A new message row (incoming, from another of our devices, or sent here).
    MessageReceived(Message),
    /// An existing message changed (delivery, reaction, edit, unsend).
    MessageUpdated(Message),
    /// Chat created, renamed, new activity, unread count, pins.
    ChatUpdated(Chat),
    Typing { chat_id: String, handle: String, typing: bool },
    /// `ConnectionStateChanged`.
    ConnectionState(String),
    BackgroundError(String),
    /// The signal streams ended (the bus went away); the loop reconnects.
    Lost(String),
    /// Subscribed to the daemon's signals (at start, and again after
    /// `Lost`): anything that happened in between must be reloaded.
    Subscribed,
}

/// The bus connection, made on first use (and again after it was lost).
#[derive(Clone)]
pub struct Backend {
    inner: Arc<Inner>,
}

struct Inner {
    bus: Bus,
    client: tokio::sync::Mutex<Option<Arc<Client>>>,
}

/// Messages per page.
pub const PAGE: u32 = 60;

impl Backend {
    /// A backend that connects on first use.
    pub fn new(bus: Bus) -> Backend {
        Backend { inner: Arc::new(Inner { bus, client: tokio::sync::Mutex::new(None) }) }
    }

    /// A backend that is connected now (so a missing bus is reported at start).
    pub async fn connect(bus: Bus) -> Result<Backend> {
        let b = Backend::new(bus);
        b.client().await?;
        Ok(b)
    }

    async fn client(&self) -> Result<Arc<Client>> {
        let mut guard = self.inner.client.lock().await;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        let c = Arc::new(Client::connect(&self.inner.bus).await?);
        *guard = Some(c.clone());
        Ok(c)
    }

    /// Drop the current connection and make a new one.
    async fn reconnect(&self) -> Result<Arc<Client>> {
        *self.inner.client.lock().await = None;
        self.client().await
    }

    pub async fn status(&self) -> Result<Status> {
        Ok(self.client().await?.status().await?.value)
    }

    /// Every chat the daemon has (pages of 500), newest activity first.
    pub async fn chats(&self) -> Result<Vec<Chat>> {
        self.client().await?.all_chats().await
    }

    /// One page of a chat, oldest first; `before` = the previous page's cursor.
    pub async fn messages(&self, chat_id: &str, before: Option<&str>, limit: u32) -> Result<MessagePage> {
        Ok(self.client().await?.list_messages(chat_id, before.unwrap_or(""), limit).await?.value)
    }

    /// The newest message of a chat (for the chat list preview).
    pub async fn newest(&self, chat_id: &str) -> Result<Option<Message>> {
        Ok(self.messages(chat_id, None, 1).await?.messages.pop())
    }

    pub async fn search(&self, query: &str) -> Result<Vec<Message>> {
        Ok(self.client().await?.search(query, 100).await?.value)
    }

    pub async fn send_text(&self, target: &Target, text: &str, reply_to: Option<&str>) -> Result<SentMessage> {
        Ok(self.client().await?.send_text(target, text, reply_to.unwrap_or("")).await?.value)
    }

    pub async fn send_file(&self, target: &Target, path: &str, mime: &str) -> Result<SentMessage> {
        Ok(self.client().await?.send_file(target, path, mime).await?.value)
    }

    pub async fn react(&self, chat_id: &str, message_id: &str, kind: &str, remove: bool) -> Result<()> {
        self.client().await?.react(chat_id, message_id, kind, remove).await
    }

    pub async fn mark_read(&self, chat_id: &str) -> Result<()> {
        self.client().await?.mark_read(chat_id, "").await
    }

    pub async fn set_typing(&self, chat_id: &str, typing: bool) -> Result<()> {
        self.client().await?.set_typing(chat_id, typing).await
    }

    /// Download (once) and return the daemon's local copy of an attachment.
    pub async fn download(&self, attachment_id: &str) -> Result<PathBuf> {
        Ok(PathBuf::from(self.client().await?.download_attachment(attachment_id).await?))
    }

    /// Forward the daemon's signals forever. If the streams end (the bus
    /// connection dropped), reconnects with backoff and keeps going.
    pub async fn signal_loop(&self, tx: mpsc::UnboundedSender<DaemonEvent>) {
        let mut delay = 1;
        let mut fresh = false;
        loop {
            let client = if fresh { self.reconnect().await } else { self.client().await };
            fresh = true;
            match client {
                Ok(client) => {
                    delay = 1;
                    match run_signals(&client, &tx).await {
                        Ok(()) => return, // receiver gone: the app quit
                        Err(e) => {
                            if tx.send(DaemonEvent::Lost(e)).is_err() {
                                return;
                            }
                        }
                    }
                }
                Err(e) => {
                    if tx.send(DaemonEvent::Lost(e.message)).is_err() {
                        return;
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(delay)).await;
            delay = (delay * 2).min(30);
        }
    }
}

/// `Ok(())` when the app dropped the receiver; `Err` when a stream ended.
async fn run_signals(client: &Client, tx: &mpsc::UnboundedSender<DaemonEvent>) -> std::result::Result<(), String> {
    let p = &client.proxy;
    let err = |e| imsg_client::map_zbus(e).message;
    let mut received = p.receive_message_received().await.map_err(err)?;
    let mut updated = p.receive_message_updated().await.map_err(err)?;
    let mut chats = p.receive_chat_updated().await.map_err(err)?;
    let mut typing = p.receive_typing().await.map_err(err)?;
    let mut conn = p.receive_connection_state_signal().await.map_err(err)?;
    let mut bg = p.receive_background_error().await.map_err(err)?;
    if tx.send(DaemonEvent::Subscribed).is_err() {
        return Ok(());
    }
    loop {
        let ev = tokio::select! {
            s = received.next() => match s {
                Some(s) => s.args().ok().and_then(|a| serde_json::from_str::<Message>(&a.message_json).ok()).map(DaemonEvent::MessageReceived),
                None => return Err("lost the connection to imessaged".into()),
            },
            s = updated.next() => match s {
                Some(s) => s.args().ok().and_then(|a| serde_json::from_str::<Message>(&a.message_json).ok()).map(DaemonEvent::MessageUpdated),
                None => return Err("lost the connection to imessaged".into()),
            },
            s = chats.next() => match s {
                Some(s) => s.args().ok().and_then(|a| serde_json::from_str::<Chat>(&a.chat_json).ok()).map(DaemonEvent::ChatUpdated),
                None => return Err("lost the connection to imessaged".into()),
            },
            s = typing.next() => match s {
                Some(s) => s.args().ok().map(|a| DaemonEvent::Typing { chat_id: a.chat_id.clone(), handle: a.handle.clone(), typing: a.typing }),
                None => return Err("lost the connection to imessaged".into()),
            },
            s = conn.next() => match s {
                Some(s) => s.args().ok().map(|a| DaemonEvent::ConnectionState(a.state.clone())),
                None => return Err("lost the connection to imessaged".into()),
            },
            s = bg.next() => match s {
                Some(s) => s.args().ok().map(|a| DaemonEvent::BackgroundError(a.message.clone())),
                None => return Err("lost the connection to imessaged".into()),
            },
        };
        if let Some(ev) = ev
            && tx.send(ev).is_err() {
                return Ok(());
            }
    }
}

/// MIME type from a file extension (`""` = let the daemon default). Same
/// table as the CLI's `send-file`.
pub fn guess_mime(path: &std::path::Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "webp" => "image/webp",
        "tif" | "tiff" => "image/tiff",
        "mov" => "video/quicktime",
        "mp4" | "m4v" => "video/mp4",
        "m4a" => "audio/mp4",
        "caf" => "audio/x-caf",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "vcf" => "text/vcard",
        "zip" => "application/zip",
        _ => "",
    }
}
