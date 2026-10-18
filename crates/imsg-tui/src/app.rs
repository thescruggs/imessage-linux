//! Application state and key handling. All daemon work is spawned onto
//! tokio and comes back as [`AppEvent`]s; the daemon's signals arrive as
//! [`DaemonEvent`]s. Nothing here draws (see `ui.rs`).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use imessage_common::model::{Chat, Handle, Message, MessageId, ReactionKind, Service};
use imsg_client::{MessagePage, SentMessage, Status, Target};
use tokio::sync::mpsc;
use tui_textarea::TextArea;

use crate::backend::{Backend, DaemonEvent, PAGE, guess_mime};
use crate::contacts::{Contacts, expand_tilde};
use crate::view;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Chats,
    Messages,
    Compose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    ChatFilter,
    Emoji,
    Contacts,
    Attach,
    Help,
    Search,
    React,
}

pub enum AppEvent {
    Status(std::result::Result<Status, String>),
    Chats(std::result::Result<Vec<Chat>, String>),
    Messages { chat_id: String, page: MessagePage, older: bool },
    MessagesFailed { chat_id: String, message: String },
    /// The newest message of a chat, for the chat list.
    Preview { chat_id: String, message: Option<Message> },
    Contacts(Contacts),
    Sent(std::result::Result<SentMessage, String>),
    Search(std::result::Result<Vec<Message>, String>),
    Daemon(DaemonEvent),
    Notice(String),
}

/// A conversation that does not exist yet (Ctrl+N to someone new).
#[derive(Debug, Clone)]
pub struct NewChat {
    /// Normalized IDS URI.
    pub uri: String,
    pub name: String,
}

/// What the chat list shows under a chat's name.
#[derive(Debug, Clone, Default)]
pub struct Preview {
    pub text: String,
    pub from_me: bool,
    pub at: u64,
    pub message_id: String,
    pub service: Service,
}

/// The message a reply will quote.
#[derive(Debug, Clone)]
pub struct ReplyTarget {
    pub id: MessageId,
    pub quote: String,
}

/// What the composer held when a send started.
struct Draft {
    lines: Vec<String>,
    files: Vec<PathBuf>,
    reply_to: Option<ReplyTarget>,
}

const PLACEHOLDER: &str = "iMessage  (Enter to send, Alt+Enter newline, Ctrl+E emoji, Ctrl+F file, ? help)";
/// How long a typing indicator without refresh is shown.
const TYPING_TTL: Duration = Duration::from_secs(60);

pub struct App {
    pub backend: Backend,
    pub tx: mpsc::UnboundedSender<AppEvent>,
    pub contacts: Contacts,
    /// Where `Ctrl+R` re-reads the vCards from.
    pub contact_dirs: Vec<PathBuf>,
    pub daemon: Option<Status>,
    /// `GetStatus` failed (no daemon on the bus): shown in the status bar.
    pub daemon_error: Option<String>,
    /// The daemon's `ConnectionState` (`connected`, `needs_login`, …).
    pub conn_state: String,
    /// The bus connection and signal subscription are up.
    pub daemon_ok: bool,
    /// The subscription was lost since the last load: reload on resubscribe.
    lost: bool,
    /// The first `Subscribed` loads the chat list (nothing can be missed
    /// between the list and the signals that way).
    started: bool,
    /// Our own handles (IDS URIs), from the daemon's registration.
    pub me: HashSet<String>,
    pub chats: Vec<Chat>,
    pub previews: HashMap<String, Preview>,
    preview_pending: HashSet<String>,
    pub chat_sel: usize,
    pub chat_filter: String,
    pub messages: HashMap<String, Vec<Message>>,
    /// `Some(None)` = oldest page reached; absent = never loaded.
    pub next_cursor: HashMap<String, Option<String>>,
    pub loading: HashSet<String>,
    /// Live messages for a chat whose first page is still loading (the
    /// page may have been computed before they were stored).
    pending_live: HashMap<String, Vec<Message>>,
    /// chat id -> (handle, when) of someone typing there.
    pub typing: HashMap<String, (String, Instant)>,
    /// The chat our typing indicator is on, if any.
    typing_sent: Option<String>,
    pub scroll: usize,
    pub msg_sel: Option<usize>,
    pub compose: TextArea<'static>,
    pub pending_files: Vec<PathBuf>,
    pub reply_to: Option<ReplyTarget>,
    /// The draft being sent, restored if the send fails.
    in_flight: Option<Draft>,
    pub focus: Focus,
    pub mode: Mode,
    pub status: String,
    pub status_at: Instant,
    pub picker_query: String,
    pub picker_sel: usize,
    pub emoji_query: String,
    pub emoji_sel: usize,
    pub attach_input: String,
    pub search_query: String,
    pub search_results: Vec<Message>,
    pub search_sel: usize,
    pub react_input: String,
    pub new_chat: Option<NewChat>,
    /// Select this chat as soon as it shows up in the list.
    pending_select: Option<String>,
    pub sending: bool,
    pub should_quit: bool,
    pub force_redraw: bool,
    /// Set by the renderer so key handling knows the viewport height.
    pub msg_view_height: usize,
    pub msg_total_lines: usize,
    /// (first_line, last_line) per rendered message index, filled by the renderer.
    pub msg_line_ranges: Vec<(usize, usize)>,
}

fn fresh_compose() -> TextArea<'static> {
    let mut t = TextArea::default();
    t.set_placeholder_text(PLACEHOLDER);
    t.set_cursor_line_style(ratatui::style::Style::default());
    t
}

impl App {
    pub fn new(backend: Backend, tx: mpsc::UnboundedSender<AppEvent>, contacts: Contacts, contact_dirs: Vec<PathBuf>) -> Self {
        App {
            backend,
            tx,
            contacts,
            contact_dirs,
            daemon: None,
            daemon_error: None,
            conn_state: String::new(),
            daemon_ok: false,
            lost: false,
            started: false,
            me: HashSet::new(),
            chats: vec![],
            previews: HashMap::new(),
            preview_pending: HashSet::new(),
            chat_sel: 0,
            chat_filter: String::new(),
            messages: HashMap::new(),
            next_cursor: HashMap::new(),
            loading: HashSet::new(),
            pending_live: HashMap::new(),
            typing: HashMap::new(),
            typing_sent: None,
            scroll: 0,
            msg_sel: None,
            compose: fresh_compose(),
            pending_files: vec![],
            reply_to: None,
            in_flight: None,
            focus: Focus::Chats,
            mode: Mode::Normal,
            status: "connecting to imessaged…".into(),
            status_at: Instant::now(),
            picker_query: String::new(),
            picker_sel: 0,
            emoji_query: String::new(),
            emoji_sel: 0,
            attach_input: String::new(),
            search_query: String::new(),
            search_results: vec![],
            search_sel: 0,
            react_input: String::new(),
            new_chat: None,
            pending_select: None,
            sending: false,
            should_quit: false,
            force_redraw: false,
            msg_view_height: 20,
            msg_total_lines: 0,
            msg_line_ranges: vec![],
        }
    }

    pub fn set_status(&mut self, s: impl Into<String>) {
        self.status = s.into();
        self.status_at = Instant::now();
    }

    // ----- names -----

    pub fn name(&self, h: &Handle) -> String {
        if self.is_me(h) { "You".into() } else { view::handle_name(&self.contacts, h) }
    }

    pub fn is_me(&self, h: &Handle) -> bool {
        self.me.contains(&h.uri)
    }

    pub fn chat_title(&self, c: &Chat) -> String {
        view::chat_title(&self.contacts, c)
    }

    /// Who is typing in a chat right now, if anyone.
    pub fn typing_in(&self, chat_id: &str) -> Option<String> {
        let (handle, at) = self.typing.get(chat_id)?;
        (at.elapsed() < TYPING_TTL).then(|| view::handle_name(&self.contacts, &Handle::new(handle)))
    }

    pub fn total_unread(&self) -> u32 {
        self.chats.iter().map(|c| c.unread_count).sum()
    }

    // ----- chat helpers -----

    fn sort_chats(&mut self) {
        self.chats.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then_with(|| a.pin_order.unwrap_or(u32::MAX).cmp(&b.pin_order.unwrap_or(u32::MAX)))
                .then_with(|| b.last_message_at.unwrap_or(0).cmp(&a.last_message_at.unwrap_or(0)))
                .then_with(|| a.id.0.cmp(&b.id.0))
        });
    }

    pub fn filtered_chats(&self) -> Vec<usize> {
        if self.chat_filter.is_empty() {
            return (0..self.chats.len()).collect();
        }
        let q = self.chat_filter.to_lowercase();
        self.chats
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                self.chat_title(c).to_lowercase().contains(&q)
                    || c.participants.iter().any(|p| p.uri.to_lowercase().contains(&q))
                    || self.previews.get(&c.id.0).is_some_and(|p| p.text.to_lowercase().contains(&q))
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn current_chat(&self) -> Option<&Chat> {
        if self.new_chat.is_some() {
            return None;
        }
        let f = self.filtered_chats();
        f.get(self.chat_sel).and_then(|i| self.chats.get(*i))
    }

    pub fn current_chat_id(&self) -> Option<String> {
        self.current_chat().map(|c| c.id.0.clone())
    }

    pub fn current_messages(&self) -> &[Message] {
        match self.current_chat_id() {
            Some(id) => self.messages.get(&id).map(|v| v.as_slice()).unwrap_or(&[]),
            None => &[],
        }
    }

    pub fn has_more(&self, chat_id: &str) -> bool {
        matches!(self.next_cursor.get(chat_id), Some(Some(_)))
    }

    /// Select a chat by id (clearing the filter); `false` if it is not listed.
    pub fn select_chat(&mut self, chat_id: &str) -> bool {
        let Some(pos) = self.chats.iter().position(|c| c.id.0 == chat_id) else { return false };
        self.chat_filter.clear();
        if self.mode == Mode::ChatFilter {
            self.mode = Mode::Normal;
        }
        self.chat_sel = pos;
        self.open_chat();
        true
    }

    // ----- async actions -----

    pub fn load_status(&self) {
        let b = self.backend.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(AppEvent::Status(b.status().await.map_err(|e| e.message)));
        });
    }

    pub fn load_chats(&self) {
        let b = self.backend.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(AppEvent::Chats(b.chats().await.map_err(|e| e.message)));
        });
    }

    pub fn reload_contacts(&self) {
        let tx = self.tx.clone();
        let dirs = self.contact_dirs.clone();
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(AppEvent::Contacts(Contacts::load(&dirs)));
        });
    }

    /// Everything again: after the daemon came back.
    pub fn refresh_all(&mut self) {
        self.load_status();
        self.load_chats();
        self.messages.clear();
        self.next_cursor.clear();
        self.loading.clear();
        self.pending_live.clear();
        self.ensure_messages();
    }

    pub fn ensure_messages(&mut self) {
        let Some(id) = self.current_chat_id() else { return };
        if self.messages.contains_key(&id) || self.loading.contains(&id) {
            return;
        }
        self.loading.insert(id.clone());
        let b = self.backend.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let ev = match b.messages(&id, None, PAGE).await {
                Ok(page) => AppEvent::Messages { chat_id: id, page, older: false },
                Err(e) => AppEvent::MessagesFailed { chat_id: id, message: format!("messages: {e}") },
            };
            let _ = tx.send(ev);
        });
    }

    pub fn load_older(&mut self) {
        let Some(id) = self.current_chat_id() else { return };
        if self.loading.contains(&id) {
            return;
        }
        let Some(Some(cursor)) = self.next_cursor.get(&id).cloned() else { return };
        self.loading.insert(id.clone());
        let b = self.backend.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let ev = match b.messages(&id, Some(&cursor), PAGE).await {
                Ok(page) => AppEvent::Messages { chat_id: id, page, older: true },
                Err(e) => AppEvent::MessagesFailed { chat_id: id, message: format!("messages: {e}") },
            };
            let _ = tx.send(ev);
        });
    }

    pub fn refresh_current(&mut self) {
        if let Some(id) = self.current_chat_id() {
            self.messages.remove(&id);
            self.next_cursor.remove(&id);
            // A page still in flight is stale now: its reply is dropped
            // (`Messages` ignores an older page for a chat without history).
            self.loading.remove(&id);
            self.ensure_messages();
        }
        self.load_status();
        self.load_chats();
        self.reload_contacts();
    }

    /// Fetch the newest message of every chat whose preview is missing or
    /// older than the chat's last activity, one after the other.
    fn fetch_previews(&mut self) {
        let mut ids = Vec::new();
        for c in &self.chats {
            let id = &c.id.0;
            if self.preview_pending.contains(id) {
                continue;
            }
            let stale = match self.previews.get(id) {
                None => true,
                Some(p) => c.last_message_at.is_some_and(|at| at > p.at),
            };
            if stale {
                ids.push(id.clone());
            }
        }
        if ids.is_empty() {
            return;
        }
        self.preview_pending.extend(ids.iter().cloned());
        let b = self.backend.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            for id in ids {
                let message = b.newest(&id).await.ok().flatten();
                if tx.send(AppEvent::Preview { chat_id: id, message }).is_err() {
                    return;
                }
            }
        });
    }

    fn mark_read(&self, chat_id: String) {
        let b = self.backend.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Err(e) = b.mark_read(&chat_id).await {
                let _ = tx.send(AppEvent::Notice(format!("mark read: {e}")));
            }
        });
    }

    fn send_typing(&self, chat_id: String, typing: bool) {
        let b = self.backend.clone();
        tokio::spawn(async move {
            // Unsupported (SMS chat) or a daemon hiccup: not worth a status line.
            let _ = b.set_typing(&chat_id, typing).await;
        });
    }

    /// Take back our typing indicator, wherever it is.
    fn stop_typing(&mut self) {
        if let Some(id) = self.typing_sent.take() {
            self.send_typing(id, false);
        }
    }

    fn send(&mut self) {
        let raw = self.compose.lines().join("\n");
        let text = expand_shortcodes(&raw).trim().to_string();
        if text.is_empty() && self.pending_files.is_empty() {
            return;
        }
        let target = if let Some(nc) = &self.new_chat {
            Target::Participants(vec![nc.uri.clone()])
        } else if let Some(c) = self.current_chat() {
            Target::Chat(c.id.0.clone())
        } else {
            self.set_status("no conversation selected");
            return;
        };
        let reply_to = self.reply_to.as_ref().map(|r| r.id.0.clone());
        let files = self.pending_files.clone();
        self.in_flight = Some(Draft { lines: self.compose.lines().to_vec(), files: std::mem::take(&mut self.pending_files), reply_to: self.reply_to.take() });
        self.compose = fresh_compose();
        self.stop_typing();
        self.sending = true;
        self.set_status("sending…");
        let b = self.backend.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let mut target = target;
            let mut last: std::result::Result<SentMessage, String> = Err("nothing to send".into());
            if !text.is_empty() {
                match b.send_text(&target, &text, reply_to.as_deref()).await {
                    Ok(s) => {
                        target = Target::Chat(s.chat_id.clone());
                        last = Ok(s);
                    }
                    Err(e) => {
                        let _ = tx.send(AppEvent::Sent(Err(e.message)));
                        return;
                    }
                }
            }
            for f in files {
                let name = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                let _ = tx.send(AppEvent::Notice(format!("sending {name}…")));
                match b.send_file(&target, &f.to_string_lossy(), guess_mime(&f)).await {
                    Ok(s) => {
                        target = Target::Chat(s.chat_id.clone());
                        last = Ok(s);
                    }
                    Err(e) => {
                        let _ = tx.send(AppEvent::Sent(Err(format!("{name}: {e}"))));
                        return;
                    }
                }
            }
            let _ = tx.send(AppEvent::Sent(last));
        });
    }

    fn attach(&mut self, path: String) {
        let p = expand_tilde(path.trim());
        let p = if p.is_absolute() { p } else { std::env::current_dir().map(|d| d.join(&p)).unwrap_or(p) };
        if !p.is_file() {
            self.set_status(format!("no such file: {}", p.display()));
            return;
        }
        let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        self.set_status(format!("attached {} ({})", p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), view::human_size(size)));
        self.pending_files.push(p);
        self.focus = Focus::Compose;
    }

    fn selected_message(&self) -> Option<&Message> {
        self.msg_sel.and_then(|i| self.current_messages().get(i))
    }

    fn open_selected(&mut self) {
        let Some(m) = self.selected_message() else { return };
        let atts = view::attachments(m);
        // Only web links: a sender-built preview could name any scheme.
        let url = view::link(m).map(|(u, ..)| u.to_string()).filter(|u| is_web_url(u)).or_else(|| {
            m.text.as_deref().and_then(|t| t.split_whitespace().find(|w| is_web_url(w)).map(str::to_string))
        });
        if atts.is_empty() {
            match url {
                Some(u) => {
                    open_with_default(&u);
                    self.set_status(format!("opened {u}"));
                }
                None => self.set_status("nothing to open on this message"),
            }
            return;
        }
        self.set_status(format!("downloading {} attachment(s)…", atts.len()));
        let b = self.backend.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            for (id, _, name, _) in atts {
                match b.download(&id.0).await {
                    Ok(p) => {
                        open_with_default(&p.to_string_lossy());
                        let _ = tx.send(AppEvent::Notice(format!("opened {name}")));
                    }
                    Err(e) => {
                        let _ = tx.send(AppEvent::Notice(format!("{name}: {e}")));
                    }
                }
            }
        });
    }

    fn save_selected(&mut self) {
        let Some(m) = self.selected_message() else { return };
        let atts = view::attachments(m);
        if atts.is_empty() {
            self.set_status("no attachments on this message");
            return;
        }
        let b = self.backend.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let dl = downloads_dir();
            let _ = std::fs::create_dir_all(&dl);
            for (id, _, name, _) in atts {
                match b.download(&id.0).await {
                    Ok(p) => {
                        // The daemon's file name is sanitized; the part's
                        // `filename` is the sender's and may hold `/` or `..`.
                        let safe = p.file_name().map(|n| n.to_string_lossy().into_owned()).filter(|n| !n.is_empty() && n != "." && n != "..").unwrap_or_else(|| "attachment".into());
                        let dest = unique_path(&dl, &safe);
                        let _ = tx.send(AppEvent::Notice(match std::fs::copy(&p, &dest) {
                            Ok(_) => format!("saved {}", dest.display()),
                            Err(e) => format!("save failed: {e}"),
                        }));
                    }
                    Err(e) => {
                        let _ = tx.send(AppEvent::Notice(format!("{name}: {e}")));
                    }
                }
            }
        });
    }

    fn copy_selected(&mut self) {
        let Some(m) = self.selected_message() else { return };
        match m.text.clone() {
            Some(t) if !t.is_empty() => match copy_to_clipboard(&t) {
                Ok(tool) => self.set_status(format!("copied ({tool})")),
                Err(e) => self.set_status(e),
            },
            _ => self.set_status("no text on this message"),
        }
    }

    fn start_reply(&mut self) {
        let Some(m) = self.selected_message() else { return };
        let quote = format!("{}: {}", self.name(&m.sender), view::snippet(m));
        self.reply_to = Some(ReplyTarget { id: m.id.clone(), quote });
        self.focus = Focus::Compose;
    }

    /// Our current reaction on a message, as the daemon's `kind` string.
    fn my_reaction(&self, m: &Message) -> Option<String> {
        m.reactions.iter().find(|r| r.added && self.is_me(&r.sender)).map(|r| reaction_arg(&r.kind))
    }

    fn react(&mut self, input: &str) {
        let Some(kind) = parse_reaction(input) else {
            self.set_status("reaction: 1-6, love/like/dislike/laugh/emphasize/question, or an emoji");
            return;
        };
        let Some(m) = self.selected_message() else { return };
        let remove = self.my_reaction(m).as_deref() == Some(kind.as_str());
        let (chat_id, msg_id) = (m.chat_id.0.clone(), m.id.0.clone());
        let b = self.backend.clone();
        let tx = self.tx.clone();
        self.set_status(if remove { "removing reaction…" } else { "reacting…" });
        tokio::spawn(async move {
            let _ = tx.send(AppEvent::Notice(match b.react(&chat_id, &msg_id, &kind, remove).await {
                Ok(()) => if remove { "reaction removed".to_string() } else { "reacted".to_string() },
                Err(e) => format!("react: {e}"),
            }));
        });
    }

    fn run_search(&mut self) {
        let q = self.search_query.trim().to_string();
        if q.is_empty() {
            return;
        }
        let b = self.backend.clone();
        let tx = self.tx.clone();
        self.set_status(format!("searching \"{q}\"…"));
        tokio::spawn(async move {
            let _ = tx.send(AppEvent::Search(b.search(&q).await.map_err(|e| e.message)));
        });
    }

    // ----- event handling -----

    pub fn on_app_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Status(Ok(st)) => {
                self.daemon_error = None;
                self.me = st.registration.handles.iter().map(|h| h.uri.clone()).chain(st.registration.other_handles.iter().cloned()).collect();
                self.conn_state = connection_name(&st.connection).to_string();
                if !self.daemon_ok || self.daemon.is_none() {
                    self.set_status(format!("imessaged {} ({})", st.version, self.conn_state));
                }
                self.daemon = Some(st);
            }
            AppEvent::Status(Err(e)) => {
                self.daemon_error = Some(e.clone());
                self.set_status(e);
            }
            AppEvent::Chats(Ok(chats)) => self.merge_chats(chats),
            AppEvent::Chats(Err(e)) => self.set_status(format!("chats: {e}")),
            AppEvent::Messages { chat_id, page, older } => {
                self.loading.remove(&chat_id);
                self.force_redraw = true;
                if older && !self.messages.contains_key(&chat_id) {
                    // The history was dropped (Ctrl+R) while this page was in
                    // flight: start over from the newest page instead.
                    self.ensure_messages();
                    return;
                }
                self.next_cursor.insert(chat_id.clone(), page.next_cursor);
                let live = self.pending_live.remove(&chat_id).unwrap_or_default();
                let entry = self.messages.entry(chat_id).or_default();
                if older {
                    let added = page.messages.len();
                    let mut v = page.messages;
                    v.append(entry);
                    *entry = v;
                    if let Some(s) = self.msg_sel.as_mut() {
                        *s += added;
                    }
                } else {
                    *entry = page.messages;
                    for m in live {
                        if let Some(pos) = entry.iter().position(|x| x.id == m.id) {
                            entry[pos] = m;
                        } else {
                            entry.push(m);
                        }
                    }
                }
                entry.sort_by_key(|m| m.sent_at);
            }
            AppEvent::MessagesFailed { chat_id, message } => {
                self.loading.remove(&chat_id);
                self.force_redraw = true;
                self.set_status(message);
            }
            AppEvent::Preview { chat_id, message } => {
                self.preview_pending.remove(&chat_id);
                if let Some(m) = message {
                    self.apply_preview(&m);
                }
            }
            AppEvent::Contacts(c) => {
                self.set_status(format!("{} contacts from {} file(s)", c.people().len(), c.files));
                self.contacts = c;
            }
            AppEvent::Sent(r) => {
                self.sending = false;
                let draft = self.in_flight.take();
                match r {
                    Ok(s) => {
                        self.set_status("sent");
                        if self.new_chat.take().is_some() && !self.select_chat(&s.chat_id) {
                            self.pending_select = Some(s.chat_id);
                            self.load_chats();
                        }
                    }
                    Err(e) => {
                        self.set_status(format!("send failed: {e}"));
                        // Give the draft back (unless a new one was started).
                        if let Some(d) = draft
                            && self.compose.lines().iter().all(|l| l.is_empty())
                            && self.pending_files.is_empty()
                        {
                            self.compose = fresh_compose();
                            self.compose.insert_str(d.lines.join("\n"));
                            self.pending_files = d.files;
                            self.reply_to = d.reply_to;
                            self.focus = Focus::Compose;
                        }
                    }
                }
            }
            AppEvent::Search(Ok(r)) => {
                self.set_status(format!("{} result(s)", r.len()));
                self.search_results = r;
                self.search_sel = 0;
            }
            AppEvent::Search(Err(e)) => self.set_status(format!("search: {e}")),
            AppEvent::Notice(s) => self.set_status(s),
            AppEvent::Daemon(ev) => self.on_daemon(ev),
        }
    }

    fn apply_preview(&mut self, m: &Message) {
        let cur = self.previews.get(&m.chat_id.0);
        let same = cur.is_some_and(|p| p.message_id == m.id.0);
        if !same && cur.is_some_and(|p| p.at > m.sent_at) {
            return;
        }
        self.previews.insert(
            m.chat_id.0.clone(),
            Preview { text: view::snippet(m), from_me: m.is_from_me || self.is_me(&m.sender), at: m.sent_at, message_id: m.id.0.clone(), service: m.service },
        );
    }

    fn merge_chats(&mut self, chats: Vec<Chat>) {
        let cur = self.current_chat_id();
        // A reload replaces the list; chats the daemon no longer lists go.
        self.chats = chats;
        self.sort_chats();
        self.after_chats_changed(cur);
        self.fetch_previews();
    }

    fn merge_chat(&mut self, chat: Chat) {
        let cur = self.current_chat_id();
        let stale_preview = match self.previews.get(&chat.id.0) {
            None => true,
            Some(p) => chat.last_message_at.is_some_and(|at| at > p.at),
        };
        match self.chats.iter().position(|c| c.id == chat.id) {
            Some(pos) => self.chats[pos] = chat,
            None => self.chats.push(chat),
        }
        self.sort_chats();
        self.after_chats_changed(cur);
        if stale_preview {
            self.fetch_previews();
        }
    }

    /// Keep the selection on the same chat; honour a pending selection.
    fn after_chats_changed(&mut self, cur: Option<String>) {
        if let Some(id) = self.pending_select.clone()
            && self.select_chat(&id) {
                self.pending_select = None;
                return;
            }
        if let Some(id) = cur {
            let f = self.filtered_chats();
            if let Some(pos) = f.iter().position(|i| self.chats[*i].id.0 == id) {
                self.chat_sel = pos;
            }
        }
        if self.new_chat.is_none() {
            self.ensure_messages();
        }
    }

    /// Replace or insert a message in a loaded chat; `false` if the chat's
    /// history is not loaded (then nothing is cached: a later load gets it).
    fn merge_message(&mut self, m: Message) -> bool {
        let Some(list) = self.messages.get_mut(&m.chat_id.0) else { return false };
        if let Some(pos) = list.iter().position(|x| x.id == m.id) {
            list[pos] = m;
        } else {
            list.push(m);
            list.sort_by_key(|x| x.sent_at);
        }
        true
    }

    fn on_daemon(&mut self, ev: DaemonEvent) {
        match ev {
            DaemonEvent::Subscribed => {
                self.daemon_ok = true;
                if std::mem::take(&mut self.lost) {
                    self.set_status("reconnected to imessaged");
                    self.refresh_all();
                } else if !std::mem::replace(&mut self.started, true) {
                    // Subscribed first, then listed: nothing in between is missed.
                    self.load_status();
                    self.load_chats();
                }
            }
            DaemonEvent::Lost(e) => {
                self.daemon_ok = false;
                self.lost = true;
                self.set_status(e);
            }
            DaemonEvent::ConnectionState(s) => {
                self.conn_state = connection_label(&s).to_string();
                self.load_status();
            }
            DaemonEvent::BackgroundError(e) => self.set_status(format!("imessaged: {e}")),
            DaemonEvent::ChatUpdated(c) => self.merge_chat(c),
            DaemonEvent::Typing { chat_id, handle, typing } => {
                if typing {
                    self.typing.insert(chat_id, (handle, Instant::now()));
                } else {
                    self.typing.remove(&chat_id);
                }
            }
            DaemonEvent::MessageReceived(m) => {
                let chat_id = m.chat_id.0.clone();
                if self.typing.get(&chat_id).is_some_and(|(h, _)| *h == m.sender.uri) {
                    self.typing.remove(&chat_id);
                }
                self.apply_preview(&m);
                let mine = m.is_from_me || self.is_me(&m.sender);
                let current = self.current_chat_id().as_deref() == Some(chat_id.as_str());
                if !self.merge_message(m.clone()) {
                    if self.loading.contains(&chat_id) {
                        // The first page is on its way and may predate this
                        // row: merge it when the page lands.
                        self.pending_live.entry(chat_id.clone()).or_default().push(m);
                    } else if current {
                        self.ensure_messages();
                    }
                }
                if current && !mine {
                    self.mark_read(chat_id.clone());
                }
                if !self.chats.iter().any(|c| c.id.0 == chat_id) {
                    self.load_chats();
                }
            }
            DaemonEvent::MessageUpdated(m) => {
                if self.previews.get(&m.chat_id.0).is_some_and(|p| p.message_id == m.id.0) {
                    self.apply_preview(&m);
                }
                self.merge_message(m);
            }
        }
    }

    pub fn on_key(&mut self, ev: Event) -> Result<()> {
        let Event::Key(key) = ev.clone() else { return Ok(()) };
        if key.kind != crossterm::event::KeyEventKind::Press {
            return Ok(());
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Global keys
        match (key.code, ctrl) {
            (KeyCode::Char('c'), true) | (KeyCode::Char('q'), true) => {
                self.should_quit = true;
                return Ok(());
            }
            (KeyCode::Char('r'), true) => {
                self.refresh_current();
                self.set_status("refreshing…");
                return Ok(());
            }
            (KeyCode::Char('l'), true) => {
                self.force_redraw = true;
                return Ok(());
            }
            (KeyCode::Char('n'), true) => {
                self.open_contacts();
                return Ok(());
            }
            (KeyCode::Char('e'), true) => {
                self.mode = Mode::Emoji;
                self.emoji_query.clear();
                self.emoji_sel = 0;
                return Ok(());
            }
            (KeyCode::Char('f'), true) => {
                self.mode = Mode::Attach;
                self.attach_input.clear();
                return Ok(());
            }
            (KeyCode::Char('s'), true) => {
                self.mode = Mode::Search;
                return Ok(());
            }
            _ => {}
        }
        match self.mode {
            Mode::Help => {
                self.mode = Mode::Normal;
                Ok(())
            }
            Mode::ChatFilter => self.key_chat_filter(key),
            Mode::Emoji => self.key_emoji(key),
            Mode::Contacts => self.key_contacts(key),
            Mode::Attach => self.key_attach(key),
            Mode::Search => self.key_search(key),
            Mode::React => self.key_react(key),
            Mode::Normal => match self.focus {
                Focus::Chats => self.key_chats(key),
                Focus::Messages => self.key_messages(key),
                Focus::Compose => self.key_compose(key, ev),
            },
        }
    }

    fn open_contacts(&mut self) {
        self.mode = Mode::Contacts;
        self.picker_query.clear();
        self.picker_sel = 0;
    }

    fn key_chats(&mut self, key: KeyEvent) -> Result<()> {
        let n = self.filtered_chats().len();
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if n > 0 {
                    self.chat_sel = (self.chat_sel + 1).min(n - 1);
                }
                self.open_chat();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.chat_sel = self.chat_sel.saturating_sub(1);
                self.open_chat();
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.chat_sel = 0;
                self.open_chat();
            }
            KeyCode::Char('G') | KeyCode::End => {
                if n > 0 {
                    self.chat_sel = n - 1;
                }
                self.open_chat();
            }
            KeyCode::PageDown => {
                if n > 0 {
                    self.chat_sel = (self.chat_sel + 10).min(n - 1);
                }
                self.open_chat();
            }
            KeyCode::PageUp => {
                self.chat_sel = self.chat_sel.saturating_sub(10);
                self.open_chat();
            }
            KeyCode::Enter | KeyCode::Char('i') => self.focus = Focus::Compose,
            KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right => self.focus = Focus::Messages,
            KeyCode::Char('/') => self.mode = Mode::ChatFilter,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc
                if !self.chat_filter.is_empty() => {
                    self.chat_filter.clear();
                    self.chat_sel = 0;
                    self.open_chat();
                }
            _ => {}
        }
        Ok(())
    }

    /// The selection moved: show that chat (no read receipt yet).
    fn on_chat_changed(&mut self) {
        self.new_chat = None;
        self.scroll = 0;
        self.msg_sel = None;
        self.reply_to = None;
        self.force_redraw = true;
        self.stop_typing();
        self.ensure_messages();
    }

    /// The user opened the selected chat (moved to it, confirmed a
    /// filter, picked a search result): show it and mark it read.
    fn open_chat(&mut self) {
        self.on_chat_changed();
        if let Some(c) = self.current_chat()
            && c.unread_count > 0
        {
            self.mark_read(c.id.0.clone());
        }
    }

    fn key_messages(&mut self, key: KeyEvent) -> Result<()> {
        let n = self.current_messages().len();
        match key.code {
            KeyCode::Char('k') | KeyCode::Up => {
                match self.msg_sel {
                    None => {
                        if n > 0 {
                            self.msg_sel = Some(n - 1);
                        }
                    }
                    Some(0) => self.load_older(),
                    Some(i) => self.msg_sel = Some(i - 1),
                }
                self.scroll_to_selected();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                match self.msg_sel {
                    None => {}
                    Some(i) if i + 1 >= n => {
                        self.msg_sel = None;
                        self.scroll = 0;
                    }
                    Some(i) => self.msg_sel = Some(i + 1),
                }
                self.scroll_to_selected();
            }
            KeyCode::PageUp | KeyCode::Char('u') => {
                self.msg_sel = None;
                let max = self.msg_total_lines.saturating_sub(self.msg_view_height);
                self.scroll = (self.scroll + self.msg_view_height / 2).min(max);
                if self.scroll >= max {
                    self.load_older();
                }
            }
            KeyCode::PageDown | KeyCode::Char('d') => {
                self.msg_sel = None;
                self.scroll = self.scroll.saturating_sub(self.msg_view_height / 2);
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.msg_sel = None;
                self.scroll = self.msg_total_lines.saturating_sub(self.msg_view_height);
                self.load_older();
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.msg_sel = None;
                self.scroll = 0;
            }
            KeyCode::Char('o') | KeyCode::Enter if self.msg_sel.is_some() => self.open_selected(),
            KeyCode::Char('s') if self.msg_sel.is_some() => self.save_selected(),
            KeyCode::Char('y') if self.msg_sel.is_some() => self.copy_selected(),
            KeyCode::Char('r') if self.msg_sel.is_some() => {
                self.mode = Mode::React;
                self.react_input.clear();
            }
            KeyCode::Char('R') if self.msg_sel.is_some() => self.start_reply(),
            KeyCode::Enter | KeyCode::Char('i') => self.focus = Focus::Compose,
            KeyCode::Tab => self.focus = Focus::Compose,
            KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left => self.focus = Focus::Chats,
            KeyCode::Esc => {
                if self.msg_sel.is_some() {
                    self.msg_sel = None;
                } else {
                    self.focus = Focus::Chats;
                }
            }
            KeyCode::Char('/') => {
                self.focus = Focus::Chats;
                self.mode = Mode::ChatFilter;
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('q') => self.should_quit = true,
            _ => {}
        }
        Ok(())
    }

    /// Adjust scroll so the selected message is visible.
    pub fn scroll_to_selected(&mut self) {
        let Some(i) = self.msg_sel else { return };
        let Some(&(first, last)) = self.msg_line_ranges.get(i) else { return };
        let total = self.msg_total_lines;
        let h = self.msg_view_height.max(1);
        // Visible window in line indices: [total - h - scroll, total - scroll)
        let top = total.saturating_sub(h + self.scroll);
        let bottom = total.saturating_sub(self.scroll);
        if first < top {
            self.scroll = total.saturating_sub(h + first);
        } else if last >= bottom {
            self.scroll = total.saturating_sub(last + 1);
        }
    }

    fn key_compose(&mut self, key: KeyEvent, ev: Event) -> Result<()> {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Esc => {
                if self.reply_to.is_some() {
                    self.reply_to = None;
                } else {
                    self.focus = Focus::Messages;
                }
            }
            KeyCode::Enter if alt || shift => self.compose.insert_newline(),
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => self.compose.insert_newline(),
            KeyCode::Enter => self.send(),
            KeyCode::Tab => self.focus = Focus::Chats,
            KeyCode::BackTab => self.focus = Focus::Messages,
            KeyCode::Up if self.compose.lines().len() <= 1 && self.compose.lines()[0].is_empty() => self.focus = Focus::Messages,
            KeyCode::Backspace if self.compose.lines().join("").is_empty() && !self.pending_files.is_empty() => {
                self.pending_files.pop();
            }
            _ => {
                self.compose.input(ev);
                self.maybe_expand_shortcode();
                self.update_typing();
            }
        }
        Ok(())
    }

    /// Tell the other side we are typing (once per draft) and when we stop.
    fn update_typing(&mut self) {
        let empty = self.compose.lines().iter().all(|l| l.is_empty());
        let chat = self.current_chat_id();
        if empty || chat != self.typing_sent {
            self.stop_typing();
        }
        if !empty
            && self.typing_sent.is_none()
            && let Some(id) = chat
        {
            self.typing_sent = Some(id.clone());
            self.send_typing(id, true);
        }
    }

    /// When the user finishes typing `:shortcode:` replace it inline with the emoji.
    fn maybe_expand_shortcode(&mut self) {
        let (row, col) = self.compose.cursor();
        let line = match self.compose.lines().get(row) {
            Some(l) => l.clone(),
            None => return,
        };
        let before: String = line.chars().take(col).collect();
        if !before.ends_with(':') {
            return;
        }
        let trimmed = &before[..before.len() - 1];
        let Some(start) = trimmed.rfind(':') else { return };
        let code = &trimmed[start + 1..];
        if code.is_empty() || code.contains(' ') {
            return;
        }
        if let Some(e) = emojis::get_by_shortcode(code) {
            let n = code.chars().count() + 2;
            for _ in 0..n {
                self.compose.delete_char();
            }
            self.compose.insert_str(e.as_str());
        }
    }

    fn key_chat_filter(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.chat_filter.clear();
                self.mode = Mode::Normal;
                self.chat_sel = 0;
                self.open_chat();
            }
            KeyCode::Enter | KeyCode::Down | KeyCode::Tab => {
                self.mode = Mode::Normal;
                self.focus = Focus::Chats;
                self.open_chat();
            }
            KeyCode::Backspace => {
                self.chat_filter.pop();
                self.chat_sel = 0;
                self.on_chat_changed();
            }
            KeyCode::Char(c) => {
                self.chat_filter.push(c);
                self.chat_sel = 0;
                self.on_chat_changed();
            }
            _ => {}
        }
        Ok(())
    }

    pub fn emoji_matches(&self) -> Vec<&'static emojis::Emoji> {
        let q = self.emoji_query.to_lowercase();
        let mut out: Vec<&'static emojis::Emoji> = vec![];
        let mut exact: Vec<&'static emojis::Emoji> = vec![];
        for e in emojis::iter() {
            if q.is_empty() {
                out.push(e);
            } else if e.shortcodes().any(|s| s == q) {
                exact.push(e);
            } else if e.name().to_lowercase().contains(&q) || e.shortcodes().any(|s| s.contains(&q)) {
                out.push(e);
            }
            if out.len() + exact.len() > 400 {
                break;
            }
        }
        exact.extend(out);
        exact
    }

    fn key_emoji(&mut self, key: KeyEvent) -> Result<()> {
        let n = self.emoji_matches().len();
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                if let Some(e) = self.emoji_matches().get(self.emoji_sel).copied() {
                    self.compose.insert_str(e.as_str());
                    self.mode = Mode::Normal;
                    self.focus = Focus::Compose;
                    self.update_typing();
                }
            }
            KeyCode::Down | KeyCode::Tab => {
                if n > 0 {
                    self.emoji_sel = (self.emoji_sel + 1) % n;
                }
            }
            KeyCode::Up | KeyCode::BackTab => {
                if n > 0 {
                    self.emoji_sel = (self.emoji_sel + n - 1) % n;
                }
            }
            KeyCode::Backspace => {
                self.emoji_query.pop();
                self.emoji_sel = 0;
            }
            KeyCode::Char(c) => {
                self.emoji_query.push(c);
                self.emoji_sel = 0;
            }
            _ => {}
        }
        Ok(())
    }

    /// Contact picker rows: `(display name, address)`.
    pub fn contact_matches(&self) -> Vec<(String, String)> {
        let q = self.picker_query.trim().to_lowercase();
        let mut out = vec![];
        for p in self.contacts.people() {
            let hit = q.is_empty() || p.name.to_lowercase().contains(&q) || p.phones.iter().chain(p.emails.iter()).any(|a| a.to_lowercase().contains(&q));
            if !hit {
                continue;
            }
            for a in p.phones.iter().chain(p.emails.iter()) {
                out.push((p.name.clone(), a.clone()));
            }
            if out.len() > 200 {
                break;
            }
        }
        // Allow raw phone/email entry.
        let raw = self.picker_query.trim();
        if let Some(uri) = Handle::try_normalize(raw) {
            out.insert(0, (format!("Send to \"{raw}\""), uri));
        }
        out
    }

    fn key_contacts(&mut self, key: KeyEvent) -> Result<()> {
        let matches = self.contact_matches();
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                if let Some((name, addr)) = matches.get(self.picker_sel).cloned() {
                    self.start_conversation(name, addr);
                    self.mode = Mode::Normal;
                }
            }
            KeyCode::Down | KeyCode::Tab => {
                if !matches.is_empty() {
                    self.picker_sel = (self.picker_sel + 1) % matches.len();
                }
            }
            KeyCode::Up | KeyCode::BackTab => {
                if !matches.is_empty() {
                    self.picker_sel = (self.picker_sel + matches.len() - 1) % matches.len();
                }
            }
            KeyCode::Backspace => {
                self.picker_query.pop();
                self.picker_sel = 0;
            }
            KeyCode::Char(c) => {
                self.picker_query.push(c);
                self.picker_sel = 0;
            }
            _ => {}
        }
        Ok(())
    }

    fn start_conversation(&mut self, name: String, address: String) {
        let Some(uri) = Handle::try_normalize(&address) else {
            self.set_status(format!("{address:?} is not an email address or phone number"));
            return;
        };
        if self.select_chat(&uri) {
            self.focus = Focus::Compose;
            return;
        }
        let name = self.contacts.name_for(&uri).map(str::to_string).unwrap_or(if name.starts_with("Send to") { Handle::new(&uri).address().to_string() } else { name });
        self.chat_filter.clear();
        self.new_chat = Some(NewChat { uri, name });
        self.scroll = 0;
        self.msg_sel = None;
        self.reply_to = None;
        self.focus = Focus::Compose;
    }

    fn key_attach(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                let p = self.attach_input.clone();
                self.mode = Mode::Normal;
                if !p.trim().is_empty() {
                    self.attach(p);
                }
            }
            KeyCode::Backspace => {
                self.attach_input.pop();
            }
            KeyCode::Tab => {
                if let Some(c) = complete_path(&self.attach_input) {
                    self.attach_input = c;
                }
            }
            KeyCode::Char(c) => self.attach_input.push(c),
            _ => {}
        }
        Ok(())
    }

    fn key_search(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                if self.search_results.is_empty() {
                    self.run_search();
                } else if let Some(m) = self.search_results.get(self.search_sel).cloned() {
                    if self.select_chat(&m.chat_id.0) {
                        self.focus = Focus::Messages;
                    } else {
                        self.set_status("that conversation is not in the list");
                    }
                    self.mode = Mode::Normal;
                }
            }
            KeyCode::Down | KeyCode::Tab => {
                if !self.search_results.is_empty() {
                    self.search_sel = (self.search_sel + 1) % self.search_results.len();
                }
            }
            KeyCode::Up | KeyCode::BackTab => {
                if !self.search_results.is_empty() {
                    self.search_sel = (self.search_sel + self.search_results.len() - 1) % self.search_results.len();
                }
            }
            KeyCode::Backspace => {
                self.search_query.pop();
                self.search_results.clear();
            }
            KeyCode::Char(c) => {
                self.search_query.push(c);
                self.search_results.clear();
            }
            _ => {}
        }
        Ok(())
    }

    fn key_react(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                let input = self.react_input.clone();
                self.mode = Mode::Normal;
                self.react(&input);
            }
            KeyCode::Backspace => {
                self.react_input.pop();
            }
            KeyCode::Char(c @ '1'..='6') if self.react_input.is_empty() => {
                self.mode = Mode::Normal;
                self.react(&c.to_string());
            }
            KeyCode::Char(c) => self.react_input.push(c),
            _ => {}
        }
        Ok(())
    }
}

/// The daemon's `kind` argument for a reaction we already have.
fn reaction_arg(k: &ReactionKind) -> String {
    match k {
        ReactionKind::Love => "love".into(),
        ReactionKind::Like => "like".into(),
        ReactionKind::Dislike => "dislike".into(),
        ReactionKind::Laugh => "laugh".into(),
        ReactionKind::Emphasize => "emphasize".into(),
        ReactionKind::Question => "question".into(),
        ReactionKind::Emoji(e) => e.clone(),
    }
}

/// `1`-`6`, a tapback name / alias, or an emoji -> `React`'s `kind`.
pub fn parse_reaction(input: &str) -> Option<String> {
    let a = input.trim();
    let k = match a.to_ascii_lowercase().as_str() {
        "1" | "love" | "heart" => "love",
        "2" | "like" | "thumbsup" | "+1" => "like",
        "3" | "dislike" | "thumbsdown" | "-1" => "dislike",
        "4" | "laugh" | "haha" | "lol" => "laugh",
        "5" | "emphasize" | "emphasis" | "!!" => "emphasize",
        "6" | "question" | "?" => "question",
        _ => {
            if let Some(code) = a.strip_prefix(':').and_then(|r| r.strip_suffix(':')) {
                return emojis::get_by_shortcode(code).map(|e| e.as_str().to_string());
            }
            if !a.is_empty() && !a.is_ascii() {
                return Some(a.to_string());
            }
            return None;
        }
    };
    Some(k.to_string())
}

/// The `ConnectionStateChanged` value (`needs_login`) as the status bar shows it.
pub fn connection_label(state: &str) -> &str {
    match state {
        "needs_login" => "needs login",
        "needs_two_factor" => "needs 2FA code",
        other => other,
    }
}

pub fn is_web_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

pub fn connection_name(c: &imessage_common::model::ConnectionState) -> &'static str {
    use imessage_common::model::ConnectionState::*;
    match c {
        Disconnected => "disconnected",
        Connecting => "connecting",
        Connected => "connected",
        NeedsLogin => "needs login",
        NeedsTwoFactor => "needs 2FA code",
        Registering => "registering",
        Error(_) => "error",
    }
}

pub fn expand_shortcodes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(':') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(j) = after.find(':') {
            let code = &after[..j];
            if !code.is_empty() && !code.contains(char::is_whitespace)
                && let Some(e) = emojis::get_by_shortcode(code) {
                    out.push_str(e.as_str());
                    rest = &after[j + 1..];
                    continue;
                }
        }
        out.push(':');
        rest = after;
    }
    out.push_str(rest);
    out
}

fn complete_path(input: &str) -> Option<String> {
    let p = expand_tilde(input);
    let (dir, prefix) = if input.ends_with('/') { (p.clone(), String::new()) } else { (p.parent()?.to_path_buf(), p.file_name()?.to_string_lossy().to_string()) };
    let dir = if dir.as_os_str().is_empty() { PathBuf::from(".") } else { dir };
    let mut matches: Vec<_> = std::fs::read_dir(&dir).ok()?.flatten().filter(|e| e.file_name().to_string_lossy().starts_with(&prefix)).collect();
    matches.sort_by_key(|e| e.file_name());
    let e = matches.first()?;
    let mut s = e.path().to_string_lossy().to_string();
    if e.path().is_dir() {
        s.push('/');
    }
    Some(s)
}

fn downloads_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("Downloads")
}

/// `name`, or `name (2)`, … so an existing download is never overwritten.
fn unique_path(dir: &std::path::Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    (2..).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).unwrap()
}

fn open_with_default(target: &str) {
    let _ = std::process::Command::new("xdg-open").arg(target).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
}

/// Wayland first, then X11 tools, else the terminal itself (OSC 52, which
/// also works over ssh and in tmux when it allows it). Returns what was used.
fn copy_to_clipboard(s: &str) -> std::result::Result<&'static str, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let tools: [(&str, &[&str]); 3] = [("wl-copy", &[]), ("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"])];
    for (tool, args) in tools {
        let Ok(mut c) = Command::new(tool).args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else { continue };
        if let Some(mut i) = c.stdin.take() {
            let _ = i.write_all(s.as_bytes());
        }
        if c.wait().map(|st| st.success()).unwrap_or(false) {
            return Ok(tool);
        }
    }
    let mut out = std::io::stdout();
    out.write_all(format!("\x1b]52;c;{}\x07", base64(s.as_bytes())).as_bytes()).and_then(|_| out.flush()).map_err(|e| format!("clipboard: {e}"))?;
    Ok("terminal")
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use imessage_common::model::{ChatId, DeliveryState, MessagePart};

    async fn app() -> App {
        let (tx, _rx) = mpsc::unbounded_channel();
        // No bus: the tasks the app spawns fail quietly into the dropped receiver.
        let backend = Backend::new(imsg_client::Bus::Address("unix:path=/nonexistent/imsg-tui-test".into()));
        App::new(backend, tx, Contacts::default(), vec![])
    }

    pub fn chat(id: &str, at: u64) -> Chat {
        Chat {
            id: ChatId(id.into()),
            participants: vec![Handle::new(id)],
            display_name: None,
            is_group: false,
            last_message_at: Some(at),
            unread_count: 0,
            background: None,
            icon_path: None,
            pinned: false,
            pin_order: None,
        }
    }

    pub fn message(id: &str, chat: &str, at: u64, text: &str) -> Message {
        Message {
            id: MessageId::new(id),
            chat_id: ChatId(chat.into()),
            sender: Handle::new("bob@example.com"),
            sent_at: at,
            text: Some(text.into()),
            parts: vec![MessagePart::Text { text: text.into() }],
            subject: None,
            reply_to: None,
            is_from_me: false,
            delivery: DeliveryState::Delivered,
            edited: false,
            unsent: false,
            reactions: vec![],
            effect: None,
            verified: true,
            service: Service::Imessage,
        }
    }

    #[tokio::test]
    async fn chats_sort_pinned_first_then_by_activity_and_keep_selection() {
        let mut app = app().await;
        app.merge_chats(vec![chat("mailto:a@x", 100), chat("mailto:b@x", 300), chat("mailto:c@x", 200)]);
        assert_eq!(app.chats.iter().map(|c| c.id.0.as_str()).collect::<Vec<_>>(), vec!["mailto:b@x", "mailto:c@x", "mailto:a@x"]);
        app.chat_sel = 1; // c
        let mut pinned = chat("mailto:a@x", 100);
        pinned.pinned = true;
        pinned.pin_order = Some(0);
        app.on_daemon(DaemonEvent::ChatUpdated(pinned));
        assert_eq!(app.chats[0].id.0, "mailto:a@x", "pinned first");
        assert_eq!(app.current_chat_id().as_deref(), Some("mailto:c@x"), "selection follows the chat");
        // New activity on a reorders the rest, a stays pinned on top.
        app.on_daemon(DaemonEvent::ChatUpdated(chat("mailto:c@x", 900)));
        assert_eq!(app.chats.iter().map(|c| c.id.0.as_str()).collect::<Vec<_>>(), vec!["mailto:a@x", "mailto:c@x", "mailto:b@x"]);
        assert_eq!(app.current_chat_id().as_deref(), Some("mailto:c@x"));
    }

    #[tokio::test]
    async fn signal_for_unopened_chat_is_not_cached_and_loaded_chat_updates() {
        let mut app = app().await;
        app.on_daemon(DaemonEvent::MessageReceived(message("M1", "mailto:b@x", 5, "hi")));
        assert!(!app.messages.contains_key("mailto:b@x"));
        assert_eq!(app.previews["mailto:b@x"].text, "hi", "the preview is kept anyway");
        app.messages.insert("mailto:b@x".into(), vec![message("M0", "mailto:b@x", 1, "old")]);
        app.on_daemon(DaemonEvent::MessageReceived(message("M1", "mailto:b@x", 5, "hi")));
        assert_eq!(app.messages["mailto:b@x"].len(), 2);
        // An update replaces by id and keeps the order by time.
        let mut edited = message("M0", "mailto:b@x", 1, "old (edited)");
        edited.edited = true;
        app.on_daemon(DaemonEvent::MessageUpdated(edited));
        let list = &app.messages["mailto:b@x"];
        assert_eq!(list.len(), 2);
        assert!(list[0].edited && list[0].text.as_deref() == Some("old (edited)"));
        assert_eq!(app.previews["mailto:b@x"].text, "hi", "the newest message's preview stays");
        app.on_daemon(DaemonEvent::MessageUpdated(message("M1", "mailto:b@x", 5, "hi!")));
        assert_eq!(app.previews["mailto:b@x"].text, "hi!", "an update to the newest message refreshes the preview");
    }

    #[tokio::test]
    async fn older_pages_prepend_and_keep_the_selection() {
        let mut app = app().await;
        app.merge_chats(vec![chat("mailto:b@x", 10)]);
        app.loading.clear();
        app.on_app_event(AppEvent::Messages { chat_id: "mailto:b@x".into(), page: MessagePage { messages: vec![message("M3", "mailto:b@x", 3, "c")], next_cursor: Some("3-0".into()) }, older: false });
        assert!(app.has_more("mailto:b@x"));
        app.msg_sel = Some(0);
        app.on_app_event(AppEvent::Messages { chat_id: "mailto:b@x".into(), page: MessagePage { messages: vec![message("M1", "mailto:b@x", 1, "a"), message("M2", "mailto:b@x", 2, "b")], next_cursor: None }, older: true });
        assert!(!app.has_more("mailto:b@x"));
        assert_eq!(app.current_messages().iter().map(|m| m.id.0.as_str()).collect::<Vec<_>>(), vec!["M1", "M2", "M3"]);
        assert_eq!(app.msg_sel, Some(2));
    }

    #[tokio::test]
    async fn message_failure_clears_loading_for_retry() {
        let mut app = app().await;
        app.merge_chats(vec![chat("mailto:b@x", 10)]);
        assert!(app.loading.contains("mailto:b@x"));
        app.on_app_event(AppEvent::MessagesFailed { chat_id: "mailto:b@x".into(), message: "messages: failed".into() });
        assert!(!app.loading.contains("mailto:b@x"));
        app.ensure_messages();
        assert!(app.loading.contains("mailto:b@x"));
    }

    #[tokio::test]
    async fn sending_to_someone_new_selects_the_chat_once_it_exists() {
        let mut app = app().await;
        app.merge_chats(vec![chat("mailto:b@x", 10)]);
        app.start_conversation("Send to \"carol@x\"".into(), "Carol@X".into());
        let nc = app.new_chat.clone().expect("new chat");
        assert_eq!(nc.uri, "mailto:carol@x");
        assert_eq!(nc.name, "carol@x");
        assert_eq!(app.focus, Focus::Compose);
        let sent = SentMessage { id: "S1".into(), chat_id: "mailto:carol@x".into(), sent_at: 20, sender: Handle::new("me@x"), service: Service::Imessage };
        app.on_app_event(AppEvent::Sent(Ok(sent)));
        assert!(app.new_chat.is_none());
        assert_eq!(app.pending_select.as_deref(), Some("mailto:carol@x"));
        app.on_daemon(DaemonEvent::ChatUpdated(chat("mailto:carol@x", 20)));
        assert_eq!(app.current_chat_id().as_deref(), Some("mailto:carol@x"));
        assert!(app.pending_select.is_none());
        // Starting a conversation with someone we already talk to selects it.
        app.start_conversation("Bob".into(), "b@x".into());
        assert!(app.new_chat.is_none());
        assert_eq!(app.current_chat_id().as_deref(), Some("mailto:b@x"));
        app.start_conversation("x".into(), "not an address".into());
        assert!(app.status.contains("not an email address"));
    }

    #[tokio::test]
    async fn status_fills_our_handles() {
        let mut app = app().await;
        let st: Status = serde_json::from_str(r#"{"version":"0.1.13","connection":{"state":"connected"},"registered":true,"registration":{"handles":[{"uri":"mailto:me@x"}],"registered_at":1,"expires_at":2,"other_handles":["tel:+15550001111"]},"unread":0}"#).unwrap();
        app.on_app_event(AppEvent::Status(Ok(st)));
        assert!(app.is_me(&Handle::new("me@x")));
        assert!(app.is_me(&Handle::new("+1 555 000 1111")));
        assert_eq!(app.name(&Handle::new("me@x")), "You");
        assert_eq!(app.conn_state, "connected");
    }

    #[test]
    fn reactions_parse() {
        assert_eq!(parse_reaction("1").as_deref(), Some("love"));
        assert_eq!(parse_reaction("Haha").as_deref(), Some("laugh"));
        assert_eq!(parse_reaction("?").as_deref(), Some("question"));
        assert_eq!(parse_reaction("🔥").as_deref(), Some("🔥"));
        assert_eq!(parse_reaction(":fire:").as_deref(), Some("🔥"));
        assert_eq!(parse_reaction("nope"), None);
        assert_eq!(parse_reaction(""), None);
        assert_eq!(reaction_arg(&ReactionKind::Emoji("🔥".into())), "🔥");
        assert_eq!(reaction_arg(&ReactionKind::Emphasize), "emphasize");
    }

    #[test]
    fn shortcodes_expand() {
        assert_eq!(expand_shortcodes("on my way :thumbsup: ok"), "on my way 👍 ok");
        assert_eq!(expand_shortcodes("10:30 and 11:45"), "10:30 and 11:45");
        assert_eq!(expand_shortcodes(":nope: :fire:"), ":nope: 🔥");
    }

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64("hi 👍".as_bytes()), "aGkg8J+RjQ==");
    }

    #[test]
    fn unique_download_names() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(unique_path(dir.path(), "a.jpg"), dir.path().join("a.jpg"));
        std::fs::write(dir.path().join("a.jpg"), b"x").unwrap();
        assert_eq!(unique_path(dir.path(), "a.jpg"), dir.path().join("a (2).jpg"));
        std::fs::write(dir.path().join("noext"), b"x").unwrap();
        assert_eq!(unique_path(dir.path(), "noext"), dir.path().join("noext (2)"));
    }
}
