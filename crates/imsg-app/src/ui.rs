//! The glue between Slint (UI thread) and the bridge (tokio runtime).
//!
//! The controller [`Ctrl`] lives in a thread-local on the UI thread and
//! owns the plain-Rust [`State`] plus the Slint models. Async work runs on
//! the tokio runtime; results come back through
//! `slint::invoke_from_event_loop` ([`post`]). Every async result carries
//! the session generation, so results that arrive after a sign-out or
//! re-pair are dropped.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use imessage_common::model::{Chat, ConnectionState, Message, Service};
use imsg_remote::{ContactsReply, RemoteClient, RemoteEvent, Target, TwoFactorResult};
use slint::winit_030::{EventResult, WinitWindowAccessor, winit};
use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode, VecModel, Weak};
use zeroize::Zeroizing;

use crate::appdir::{AppDirs, safe_file_name};
use crate::config::{Config, FontSize, ThemeMode};
use crate::images::{self, Corners, Rgba};
use crate::logging::redact;
use crate::notify::Notifier;
use crate::secrets::{self, SealKey, Secrets, SecretsError};
use crate::state::{self, Change, MsgView, PartView, State};
use crate::{AppWindow, ChatItem, MsgItem, PartItem, ReactionItem, Suggestion, Tapbacks, Theme};

thread_local! {
    static CTRL: RefCell<Option<Ctrl>> = const { RefCell::new(None) };
}

/// Run `f` with the controller (UI thread). If the controller is busy
/// (a Slint callback fired while it was updating the UI), try again on
/// the next event-loop turn.
fn with<F: FnOnce(&mut Ctrl) + 'static>(f: F) {
    CTRL.with(|c| match c.try_borrow_mut() {
        Ok(mut g) => {
            if let Some(c) = g.as_mut() {
                f(c);
            }
        }
        Err(_) => Timer::single_shot(Duration::ZERO, move || with(f)),
    });
}

/// Run `f` with the controller, from any thread.
fn post<F: FnOnce(&mut Ctrl) + Send + 'static>(f: F) {
    let _ = slint::invoke_from_event_loop(move || with(f));
}

enum Img {
    Loading,
    Loaded(Image, u32, u32),
    Failed,
}

pub struct Ctrl {
    ui: Weak<AppWindow>,
    rt: tokio::runtime::Handle,
    dirs: AppDirs,
    config: Config,
    st: State,
    client: Option<RemoteClient>,
    seal: Option<SealKey>,
    secrets: Option<Secrets>,
    events_task: Option<tokio::task::JoinHandle<()>>,
    /// Bumped on sign-out / re-pair: stale async results are dropped.
    generation: u64,
    chats_model: Rc<VecModel<ChatItem>>,
    msgs_model: Rc<VecModel<MsgItem>>,
    suggestions_model: Rc<VecModel<Suggestion>>,
    /// What each transcript row was built from (to update only changed rows).
    rendered: Vec<(MsgView, Vec<u8>)>,
    images: HashMap<String, Img>,
    image_slots: Arc<tokio::sync::Semaphore>,
    /// Chat id -> photo (`None` = none / failed); present = asked for.
    avatars: HashMap<String, Option<Image>>,
    /// Handle URI -> shared photo, for group senders (as `avatars`).
    handle_photos: HashMap<String, Option<Image>>,
    /// The bridge answered at least once this session (footer wording).
    bridge_seen: bool,
    /// imessaged's state from the last status (`None` = not known yet).
    daemon_conn: Option<ConnectionState>,
    /// The bridge can reach imessaged.
    daemon_reachable: bool,
    /// Development only: `IMSG_APP_DEV_THEME` beats the setting.
    theme_override: Option<ThemeMode>,
    drafts: HashMap<String, String>,
    notifier: Box<dyn Notifier>,
    focused: bool,
    bridge_ok: bool,
    typing_sent: Option<(String, Instant)>,
    typing_timer: Timer,
    tick: Timer,
    read_pending: HashSet<String>,
    open_counter: u32,
    /// Development only: `IMSG_APP_DEV_SCREENSHOT` ([`Ctrl::dev_screenshot`]).
    dev_shot: Option<DevShot>,
}

/// Development only (debug builds): take one screenshot once the data
/// has loaded, then quit (tools/app-screenshots.sh).
struct DevShot {
    path: PathBuf,
    open: Option<String>,
    opened: bool,
    /// `IMSG_APP_DEV_PICKER`: open this message's tapback picker first.
    picker: Option<String>,
    started: Instant,
    ready_since: Option<Instant>,
    timer: Timer,
}

/// Minimize tracking from winit `Resized` sizes: Windows reports a
/// minimize as 0×0 and the restore as the old size (winit sends no
/// `Occluded` there). Returns (minimized now, just restored).
fn minimize_step(was_minimized: bool, width: u32, height: u32) -> (bool, bool) {
    let now = width == 0 || height == 0;
    (now, was_minimized && !now)
}

/// Build the controller and wire the callbacks.
pub fn start(ui: &AppWindow, rt: tokio::runtime::Handle, dirs: AppDirs, config: Config, config_error: Option<String>) {
    let weak = ui.as_weak();
    let notifier = crate::notify::create(Box::new(|| post(|c| c.activate_window())));
    let ctrl = Ctrl {
        ui: weak.clone(),
        rt,
        dirs,
        config,
        st: State::default(),
        client: None,
        seal: None,
        secrets: None,
        events_task: None,
        generation: 0,
        chats_model: Rc::new(VecModel::default()),
        msgs_model: Rc::new(VecModel::default()),
        suggestions_model: Rc::new(VecModel::default()),
        rendered: Vec::new(),
        images: HashMap::new(),
        image_slots: Arc::new(tokio::sync::Semaphore::new(3)),
        avatars: HashMap::new(),
        handle_photos: HashMap::new(),
        bridge_seen: false,
        daemon_conn: None,
        daemon_reachable: true,
        theme_override: None,
        drafts: HashMap::new(),
        notifier,
        focused: true,
        bridge_ok: false,
        typing_sent: None,
        typing_timer: Timer::default(),
        tick: Timer::default(),
        read_pending: HashSet::new(),
        open_counter: 0,
        dev_shot: None,
    };
    ui.set_chats(ModelRc::from(ctrl.chats_model.clone()));
    ui.set_messages(ModelRc::from(ctrl.msgs_model.clone()));
    ui.set_suggestions(ModelRc::from(ctrl.suggestions_model.clone()));
    CTRL.with(|c| *c.borrow_mut() = Some(ctrl));

    ui.on_pair(|s, a, b| with(move |c| c.pair(s.to_string(), Zeroizing::new(a.to_string()), Zeroizing::new(b.to_string()))));
    ui.on_unlock(|p| with(move |c| c.unlock(Zeroizing::new(p.to_string()))));
    ui.on_forget_pairing(|| with(|c| c.sign_out()));
    ui.on_select_chat(|id| with(move |c| c.select_chat(&id)));
    ui.on_load_older(|| with(|c| c.load_older()));
    ui.on_send(|t| with(move |c| c.send(t.to_string())));
    ui.on_composer_edited(|t| with(move |c| c.composer_edited(&t)));
    ui.on_attach(|| with(|c| c.attach()));
    ui.on_open_attachment(|id, name| with(move |c| c.open_attachment(id.to_string(), name.to_string())));
    ui.on_save_attachment(|id, name| with(move |c| c.save_attachment(id.to_string(), name.to_string())));
    ui.on_open_url(|u| with(move |c| c.open_url(&u)));
    ui.on_react(|id, kind| with(move |c| c.react(id.to_string(), kind.to_string())));
    ui.on_search_edited(|t| with(move |c| c.search(&t)));
    ui.on_new_to_edited(|t| with(move |c| c.new_to_edited(&t)));
    ui.on_send_new(|to, text| with(move |c| c.send_new(to.to_string(), text.to_string())));
    ui.on_submit_two_factor(|code| with(move |c| c.submit_two_factor(code.to_string())));
    ui.on_save_bridge_url(|u| with(move |c| c.save_bridge_url(u.to_string())));
    ui.on_repair(|s| with(move |c| c.repair(s.to_string())));
    ui.on_change_passphrase(|o, a, b| {
        with(move |c| c.change_passphrase(Zeroizing::new(o.to_string()), Zeroizing::new(a.to_string()), Zeroizing::new(b.to_string())))
    });
    ui.on_settings_changed(|| with(|c| c.settings_changed()));
    ui.on_clear_cache(|| with(|c| c.clear_cache()));
    ui.on_sign_out(|| with(|c| c.sign_out()));
    ui.on_overlays_closed(|| with(|c| c.overlays_closed()));

    // Blank / stale window after minimize -> restore on Windows (Slint
    // 1.18 software renderer; docs/PORTABLE-WINDOWS.md): winit reports the
    // minimize as a 0×0 resize, frames drawn meanwhile reach softbuffer's
    // own bitmap but not the screen, and on the restore nothing is dirty,
    // so nothing is presented. Making the whole window dirty on the
    // restore (repaint-nonce) gives one full, fully presented frame. Not
    // dropping RedrawRequested while minimized: Slint would keep its
    // pending-redraw flag and its frame-throttle timer would spin at the
    // refresh rate until the restore.
    let mut minimized = false;
    ui.window().on_winit_window_event(move |w, ev| {
        match ev {
            winit::event::WindowEvent::Resized(s) => {
                let (now, restored) = minimize_step(minimized, s.width, s.height);
                minimized = now;
                if restored {
                    log::debug!("window restored: full repaint");
                    with(|c| c.repaint_all());
                    w.request_redraw();
                }
            }
            winit::event::WindowEvent::Focused(f) => {
                let f = *f;
                with(move |c| c.on_focus(f));
            }
            _ => {}
        }
        EventResult::Propagate
    });

    with(move |c| c.init(config_error));
}

/// At exit: remove the tray icon, stop the event stream.
pub fn shutdown() {
    with(|c| {
        if let Some((id, _)) = c.typing_sent.take()
            && let Some(cl) = c.client.clone()
        {
            // Best effort, bounded: the window is already gone.
            let _ = c.rt.block_on(async { tokio::time::timeout(Duration::from_millis(500), cl.set_typing(&id, false)).await });
        }
        if let Some(t) = c.events_task.take() {
            t.abort();
        }
        c.notifier.shutdown();
    });
    CTRL.with(|c| c.borrow_mut().take());
}

fn connection_name(c: &ConnectionState) -> String {
    match c {
        ConnectionState::Disconnected => "disconnected".into(),
        ConnectionState::Connecting => "connecting".into(),
        ConnectionState::Connected => "connected".into(),
        ConnectionState::NeedsLogin => state::connection_label("needs_login"),
        ConnectionState::NeedsTwoFactor => state::connection_label("needs_two_factor"),
        ConnectionState::Registering => "registering".into(),
        ConnectionState::Error(d) => format!("error: {}", imessage_common::redact::redact_line(d)),
    }
}

fn to_image(r: &Rgba) -> Image {
    Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&r.pixels, r.width, r.height))
}

/// Make `model` equal to `rows`, touching only rows that differ.
fn sync_rows<T: Clone + 'static>(model: &VecModel<T>, rows: Vec<T>, same: impl Fn(usize, &T) -> bool) {
    let old = model.row_count();
    for (i, r) in rows.iter().enumerate() {
        if i < old {
            if !same(i, r) {
                model.set_row_data(i, r.clone());
            }
        } else {
            model.push(r.clone());
        }
    }
    for i in (rows.len()..old).rev() {
        model.remove(i);
    }
}

impl Ctrl {
    fn ui(&self) -> Option<AppWindow> {
        self.ui.upgrade()
    }

    /// Run `fut` on the runtime, then `done` on the UI thread (dropped
    /// if the session changed meanwhile).
    fn run<T: Send + 'static>(&self, fut: impl Future<Output = T> + Send + 'static, done: impl FnOnce(&mut Ctrl, T) + Send + 'static) {
        let generation = self.generation;
        self.rt.spawn(async move {
            let v = fut.await;
            post(move |c| {
                if c.generation == generation {
                    done(c, v)
                }
            });
        });
    }

    fn notice(&self, s: impl Into<String>) {
        if let Some(ui) = self.ui() {
            ui.set_notice(s.into().into());
        }
    }

    fn init(&mut self, config_error: Option<String>) {
        let Some(ui) = self.ui() else { return };
        self.apply_font();
        if cfg!(debug_assertions)
            && let Ok(t) = std::env::var("IMSG_APP_DEV_THEME")
        {
            match ThemeMode::parse(&t) {
                Some(m) => self.theme_override = Some(m),
                None => eprintln!("imsg-app: IMSG_APP_DEV_THEME={t}: use light, dark or system"),
            }
        }
        self.apply_theme();
        let mut info = format!(
            "imsg {} · data folder: {}{}",
            env!("CARGO_PKG_VERSION"),
            self.dirs.root.display(),
            if self.dirs.portable { " (portable)" } else { "" }
        );
        if let Some(e) = config_error {
            info.push_str(&format!("\nSettings file not read: {e}"));
        }
        ui.set_app_info(info.into());
        ui.set_has_default_bridge(self.config.bridge.is_some());
        self.load_settings_ui();
        ui.set_screen(if self.dirs.secrets_file().exists() { 1 } else { 0 });
        ui.invoke_focus_passphrase();
        self.dev_screenshot();
        self.dev_autostart();
        // Typing indicators expire; chat-list times roll over.
        self.tick.start(TimerMode::Repeated, Duration::from_secs(5), || with(|c| c.on_tick()));
    }

    /// Development only (debug builds): `IMSG_APP_DEV_PASSPHRASE` unlocks
    /// at start, or pairs with `IMSG_APP_DEV_PAIRING` when there is no
    /// secrets.bin yet. Lets the UI be driven headless (Xvfb) for checks.
    fn dev_autostart(&mut self) {
        if !cfg!(debug_assertions) {
            return;
        }
        let Ok(pass) = std::env::var("IMSG_APP_DEV_PASSPHRASE") else { return };
        if self.dirs.secrets_file().exists() {
            self.unlock(Zeroizing::new(pass));
        } else if let Ok(pairing) = std::env::var("IMSG_APP_DEV_PAIRING") {
            self.pair(pairing, Zeroizing::new(pass.clone()), Zeroizing::new(pass));
        }
    }

    /// Development only (debug builds): `IMSG_APP_DEV_SCREENSHOT=<dir or
    /// file.png>` waits until the chats (and, with
    /// `IMSG_APP_DEV_OPEN_CHAT=<chat id>`, that chat with its pictures)
    /// have loaded, writes a PNG of the window (1120×780) and quits. In a
    /// directory the file is `chats.png` or `<chat id>.png`.
    /// `IMSG_APP_DEV_THEME=light|dark` forces the colours (read in
    /// `init`). `IMSG_APP_DEV_OVERLAY=settings|new` opens that dialog.
    /// `IMSG_APP_DEV_PICKER=<message id>`
    /// opens that message's tapback picker before the shot. Used by
    /// tools/app-screenshots.sh.
    fn dev_screenshot(&mut self) {
        if !cfg!(debug_assertions) {
            return;
        }
        let Some(target) = std::env::var_os("IMSG_APP_DEV_SCREENSHOT").map(PathBuf::from) else { return };
        let Some(ui) = self.ui() else { return };
        let open = std::env::var("IMSG_APP_DEV_OPEN_CHAT").ok().filter(|s| !s.is_empty());
        let path = if target.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")) {
            target
        } else {
            let name = open.as_deref().map(|id| id.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "_")).unwrap_or_else(|| "chats".into());
            target.join(format!("{name}.png"))
        };
        ui.window().set_size(slint::LogicalSize::new(1120.0, 780.0));
        let picker = std::env::var("IMSG_APP_DEV_PICKER").ok().filter(|s| !s.is_empty());
        match std::env::var("IMSG_APP_DEV_OVERLAY").as_deref() {
            Ok("settings") => ui.set_settings_open(true),
            Ok("new") => ui.set_new_open(true),
            _ => {}
        }
        let shot = DevShot { path, open, opened: false, picker, started: Instant::now(), ready_since: None, timer: Timer::default() };
        shot.timer.start(TimerMode::Repeated, Duration::from_millis(200), || with(|c| c.dev_screenshot_poll()));
        self.dev_shot = Some(shot);
    }

    fn dev_screenshot_poll(&mut self) {
        let Some(d) = self.dev_shot.as_ref() else { return };
        let timed_out = d.started.elapsed() > Duration::from_secs(30);
        if self.st.chats.is_empty() && !timed_out {
            return;
        }
        if let Some(id) = d.open.clone().filter(|_| !d.opened) {
            if let Some(d) = self.dev_shot.as_mut() {
                d.opened = true;
            }
            self.select_chat(&id);
            return;
        }
        let pictures_pending = self.images.values().any(|i| matches!(i, Img::Loading));
        let ready = d.open.is_none() || (!self.rendered.is_empty() && !pictures_pending);
        let Some(d) = self.dev_shot.as_mut() else { return };
        if !ready && !timed_out {
            d.ready_since = None;
            return;
        }
        // Let late arrivals (avatars, layout) settle.
        let since = *d.ready_since.get_or_insert_with(Instant::now);
        if since.elapsed() < Duration::from_millis(1200) && !timed_out {
            return;
        }
        // Then open the picker on the laid-out bubble. Repeated items run
        // `changed` handlers lazily, on a render: force one, settle again.
        if let Some(id) = d.picker.take() {
            d.ready_since = Some(Instant::now());
            if let Some(ui) = self.ui.upgrade() {
                ui.set_dev_picker(id.into());
                let _ = ui.window().take_snapshot();
            }
            return;
        }
        let Some(d) = self.dev_shot.take() else { return };
        d.timer.stop();
        if timed_out {
            eprintln!("imsg-app: screenshot: data not loaded after 30 s; taking it anyway");
        }
        let result = self
            .ui()
            .ok_or_else(|| "no window".to_string())
            .and_then(|ui| ui.window().take_snapshot().map_err(|e| e.to_string()))
            .and_then(|buf| {
                let img = image::RgbaImage::from_raw(buf.width(), buf.height(), buf.as_bytes().to_vec()).ok_or("bad snapshot size")?;
                if let Some(dir) = d.path.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                }
                img.save_with_format(&d.path, image::ImageFormat::Png).map_err(|e| e.to_string())
            });
        match result {
            Ok(()) => {
                eprintln!("imsg-app: screenshot: {}", d.path.display());
                let _ = slint::quit_event_loop();
            }
            Err(e) => {
                eprintln!("imsg-app: screenshot failed: {e}");
                std::process::exit(2);
            }
        }
    }

    fn apply_font(&self) {
        if let Some(ui) = self.ui() {
            ui.global::<Theme>().set_base(self.config.font_size.px());
            // Monochrome emoji outlines for emoji tapbacks (the software
            // renderer draws no colour glyphs and has no per-glyph fallback).
            let emoji = if cfg!(windows) { "Segoe UI Emoji" } else { "" };
            ui.global::<Tapbacks>().set_emoji_font(emoji.into());
        }
    }

    /// Light or dark: the setting (or the dev override). In "system" mode
    /// nothing is pinned: the std-widgets palette follows the system's
    /// setting on its own (winit's initial theme and `ThemeChanged`,
    /// the desktop portal on Linux), live.
    fn apply_theme(&self) {
        if let Some(ui) = self.ui() {
            ui.invoke_set_theme_mode(self.theme_override.unwrap_or(self.config.theme).index());
        }
    }

    fn load_settings_ui(&self) {
        let Some(ui) = self.ui() else { return };
        ui.set_set_theme(self.config.theme.index());
        ui.set_set_notify(self.config.notifications.enabled);
        ui.set_set_notify_text(self.config.notifications.show_text);
        ui.set_set_log(self.config.log);
        ui.set_set_font(self.config.font_size.index());
        let url = self.secrets.as_ref().map(|s| s.bridge_url.clone()).or_else(|| self.config.bridge.clone()).unwrap_or_default();
        ui.set_set_bridge_url(url.into());
    }

    // ===================== pairing / unlocking =====================

    fn pair(&mut self, pairing: String, p1: Zeroizing<String>, p2: Zeroizing<String>) {
        let Some(ui) = self.ui() else { return };
        ui.set_error("".into());
        if let Err(e) = secrets::check_new_passphrase(&p1, &p2) {
            ui.set_error(e.into());
            return;
        }
        let pairing = pairing.trim().to_string();
        let parsed = if pairing.to_ascii_lowercase().starts_with("imsg+bridge://") {
            imsg_remote::parse_pairing(&pairing).map_err(|e| e.to_string())
        } else if imsg_remote::valid_token(&pairing) {
            match self.config.bridge.as_deref().and_then(imsg_remote::normalize_base_url) {
                Some(url) => Ok(imsg_remote::Pairing { bridge_url: url, token: pairing.clone() }),
                None => Err("Paste the whole pairing string (imsg+bridge://…#…): no bridge is set in imsg-app.toml.".to_string()),
            }
        } else {
            Err("That is not a pairing string (imsg+bridge://host:port#token).".to_string())
        };
        let p = match parsed {
            Ok(p) => p,
            Err(e) => {
                ui.set_error(e.into());
                return;
            }
        };
        let client = match RemoteClient::new(&p.bridge_url, &p.token) {
            Ok(c) => c,
            Err(e) => {
                ui.set_error(e.to_string().into());
                return;
            }
        };
        ui.set_busy("Checking the bridge…".into());
        let secrets = Secrets { bridge_url: p.bridge_url.clone(), token: p.token.clone() };
        let c2 = client.clone();
        self.run(async move { c2.status().await }, move |c, r| {
            let Some(ui) = c.ui() else { return };
            let warn = match r {
                Err(e) if e.is_auth() => {
                    ui.set_busy("".into());
                    ui.set_error(format!("The bridge refused this pairing: {}", e.message).into());
                    return;
                }
                Err(e) => Some(format!("Could not check the bridge yet ({}); saved anyway.", e.message)),
                Ok(_) => None,
            };
            ui.set_busy("Encrypting…".into());
            let path = c.dirs.secrets_file();
            let s2 = secrets.clone();
            c.run(
                async move {
                    tokio::task::spawn_blocking(move || {
                        let key = SealKey::derive_new(&p1)?;
                        secrets::save(&path, &key, &s2)?;
                        Ok::<_, SecretsError>(key)
                    })
                    .await
                    .unwrap_or_else(|e| Err(SecretsError::Io(e.to_string())))
                },
                move |c, r| {
                    let Some(ui) = c.ui() else { return };
                    ui.set_busy("".into());
                    match r {
                        Ok(key) => {
                            if c.config.bridge.is_none() {
                                c.config.bridge = Some(secrets.bridge_url.clone());
                                let _ = c.config.save(&c.dirs.config_file());
                            }
                            c.enter_main(secrets, key);
                            if let Some(w) = warn {
                                c.notice(w);
                            }
                        }
                        Err(e) => ui.set_error(format!("Could not save: {e}").into()),
                    }
                },
            );
        });
    }

    fn unlock(&mut self, pass: Zeroizing<String>) {
        let Some(ui) = self.ui() else { return };
        if pass.is_empty() {
            return;
        }
        ui.set_error("".into());
        ui.set_busy("Unlocking…".into());
        let path = self.dirs.secrets_file();
        self.run(
            async move { tokio::task::spawn_blocking(move || secrets::load(&path, &pass)).await.unwrap_or_else(|e| Err(SecretsError::Io(e.to_string()))) },
            |c, r| {
                let Some(ui) = c.ui() else { return };
                ui.set_busy("".into());
                match r {
                    Ok((s, key)) => c.enter_main(s, key),
                    Err(SecretsError::WrongPassphrase) => {
                        log::info!("unlock: wrong passphrase");
                        ui.set_error("Wrong passphrase.".into());
                    }
                    Err(e) => ui.set_error(e.to_string().into()),
                }
            },
        );
    }

    fn enter_main(&mut self, s: Secrets, key: SealKey) {
        let Some(ui) = self.ui() else { return };
        let client = match RemoteClient::new(&s.bridge_url, &s.token) {
            Ok(c) => c,
            Err(e) => {
                ui.set_error(e.to_string().into());
                return;
            }
        };
        log::info!("connecting to the bridge at {}", client.base_url());
        self.client = Some(client);
        self.secrets = Some(s);
        self.seal = Some(key);
        self.load_settings_ui();
        ui.set_error("".into());
        ui.set_screen(2);
        // The passphrase field loses its focus with its screen; without a
        // focused item the keyboard shortcuts (Ctrl+N, Esc, …) are dead.
        ui.invoke_focus_search();
        self.bridge_ok = false;
        self.bridge_seen = false;
        self.daemon_conn = None;
        self.daemon_reachable = true;
        self.refresh_status();
        self.start_events();
    }

    fn start_events(&mut self) {
        if let Some(t) = self.events_task.take() {
            t.abort();
        }
        let Some(client) = self.client.clone() else { return };
        let generation = self.generation;
        self.events_task = Some(self.rt.spawn(async move {
            let mut ev = client.events();
            while let Some(e) = ev.next_event().await {
                post(move |c| {
                    if c.generation == generation {
                        c.on_event(e)
                    }
                });
            }
        }));
    }

    /// Forget the pairing: back to the pairing screen.
    fn sign_out(&mut self) {
        self.generation += 1;
        if let Some(t) = self.events_task.take() {
            t.abort();
        }
        if let Err(e) = secrets::forget(&self.dirs.secrets_file()) {
            log::warn!("could not delete secrets.bin: {e}");
        }
        self.client = None;
        self.secrets = None;
        self.seal = None;
        self.st = State::default();
        self.images.clear();
        self.avatars.clear();
        self.handle_photos.clear();
        self.drafts.clear();
        self.rendered.clear();
        self.read_pending.clear();
        self.typing_sent = None;
        self.chats_model.set_vec(Vec::new());
        self.msgs_model.set_vec(Vec::new());
        if let Some(ui) = self.ui() {
            ui.set_settings_open(false);
            ui.set_new_open(false);
            ui.set_twofa_open(false);
            ui.set_chat_open(false);
            ui.set_composer_text("".into());
            ui.set_search_text("".into());
            ui.set_notice("".into());
            ui.set_error("".into());
            ui.set_busy("".into());
            ui.set_window_title("imsg".into());
            ui.set_screen(0);
            ui.invoke_focus_passphrase();
        }
        self.notifier.set_tooltip("imsg");
        log::info!("signed out");
    }

    fn activate_window(&self) {
        if let Some(ui) = self.ui() {
            let _ = ui.show();
            ui.window().with_winit_window(|w| {
                w.set_minimized(false);
                w.focus_window();
            });
        }
    }

    fn on_focus(&mut self, focused: bool) {
        self.focused = focused;
        if focused {
            // Covers what the minimize hook cannot see (lock screen, RDP
            // reconnect, Win+D).
            self.repaint_all();
            self.maybe_mark_read();
        }
    }

    /// Make the whole window dirty: the next frame is a full repaint.
    fn repaint_all(&self) {
        if let Some(ui) = self.ui() {
            ui.set_repaint_nonce(ui.get_repaint_nonce().wrapping_add(1));
        }
    }

    // ===================== events =====================

    fn on_event(&mut self, e: RemoteEvent) {
        let Some(ui) = self.ui() else { return };
        match e {
            RemoteEvent::Connected { daemon_connected, .. } => {
                log::info!("bridge: connected (daemon connected: {daemon_connected})");
                self.bridge_ok = true;
                self.bridge_seen = true;
                self.daemon_reachable = daemon_connected;
                self.refresh_status();
                self.resync();
            }
            RemoteEvent::Lost(r) => {
                log::info!("bridge: lost ({})", imessage_common::redact::redact_line(&r));
                self.bridge_ok = false;
                self.refresh_status();
            }
            RemoteEvent::Daemon { connected, detail } => {
                self.daemon_reachable = connected;
                self.refresh_status();
                if connected {
                    self.resync();
                } else if let Some(d) = detail {
                    log::info!("bridge: imessaged lost ({})", imessage_common::redact::redact_line(&d));
                }
            }
            RemoteEvent::ConnectionState(s) => {
                // The status that follows has the details.
                if let Some(c) = state::parse_connection_state(&s) {
                    self.daemon_conn = Some(c);
                    self.refresh_status();
                }
                self.load_status();
            }
            RemoteEvent::TwoFactorRequired => {
                ui.set_dialog_error("".into());
                ui.set_twofa_open(true);
                self.notifier.notify("imsg", "A verification code is needed");
                self.attention();
            }
            RemoteEvent::BackgroundError(m) => {
                self.notice(format!("imessaged: {}", imessage_common::redact::redact_line(&m)));
                if m.contains("resync") {
                    self.resync();
                }
            }
            RemoteEvent::HandleProfileUpdated { handle, .. } => {
                let ids: Vec<String> = self.st.chats.iter().filter(|c| c.participants.iter().any(|p| p.uri == handle)).map(|c| c.id.0.clone()).collect();
                for id in ids {
                    self.avatars.remove(&id);
                }
                self.handle_photos.remove(&handle);
            }
            RemoteEvent::SmsForwardingChanged(_) => self.load_status(),
            RemoteEvent::ChatUpdated(chat) => self.on_chat_updated(chat),
            RemoteEvent::Typing { chat_id, handle, typing } => {
                if !self.st.is_me(&imessage_common::model::Handle::new(&handle)) {
                    self.st.set_typing(&chat_id, &handle, typing, Instant::now());
                    self.refresh_chats();
                    self.refresh_typing();
                }
            }
            RemoteEvent::MessageReceived { message, .. } => self.on_message(message, false),
            RemoteEvent::MessageUpdated { message, .. } => self.on_message(message, true),
        }
    }

    fn on_chat_updated(&mut self, chat: Chat) {
        let id = chat.id.0.clone();
        if let Some(old) = self.st.chat(&id)
            && (old.icon_path != chat.icon_path || old.participants != chat.participants)
        {
            self.avatars.remove(&id);
        }
        self.st.merge_chat(chat);
        self.refresh_chats();
        self.fetch_previews();
        self.fetch_avatars();
        if self.st.open_id() == Some(id.as_str()) {
            self.refresh_header();
        }
    }

    fn on_message(&mut self, m: Message, updated: bool) {
        let chat_id = m.chat_id.0.clone();
        let mine = m.is_from_me || self.st.is_me(&m.sender);
        let known = self.st.chat(&chat_id).is_some();
        let (title, body) = (self.notification_title(&m), state::snippet(&m));
        let change = self.st.merge_message(m, updated);
        self.render_transcript(change);
        self.refresh_chats();
        self.refresh_typing();
        if !known {
            self.load_chats();
        }
        if updated || mine {
            return;
        }
        let visible = self.focused && self.st.open_id() == Some(chat_id.as_str());
        if visible {
            self.mark_read(chat_id);
            return;
        }
        if self.config.notifications.enabled {
            let body = if self.config.notifications.show_text { body } else { "New message".to_string() };
            self.notifier.notify(&title, &body);
        }
        if !self.focused {
            self.attention();
        }
    }

    fn notification_title(&self, m: &Message) -> String {
        let sender = self.st.name(&m.sender);
        match self.st.chat(&m.chat_id.0) {
            Some(c) if c.is_group => format!("{sender} in {}", state::chat_title(&self.st.contacts, c)),
            _ => sender,
        }
    }

    /// Flash the taskbar button.
    fn attention(&self) {
        if let Some(ui) = self.ui() {
            ui.window().with_winit_window(|w| w.request_user_attention(Some(winit::window::UserAttentionType::Informational)));
        }
    }

    fn on_tick(&mut self) {
        let now = Instant::now();
        let before = self.st.typing.len();
        self.st.typing.retain(|_, (_, at)| now.duration_since(*at) < state::TYPING_TTL);
        if before != self.st.typing.len() {
            self.refresh_chats();
            self.refresh_typing();
        }
    }

    // ===================== loading =====================

    /// Reload everything (docs/BRIDGE.md resync rule).
    fn resync(&mut self) {
        self.load_status();
        self.load_chats();
        self.load_contacts();
        if let Some(id) = self.st.reload_open() {
            self.load_page(id, None);
        }
    }

    fn load_status(&self) {
        let Some(cl) = self.client.clone() else { return };
        self.run(async move { cl.status().await }, |c, r| match r {
            Ok(s) => {
                log::info!("imessaged: {}", connection_name(&s.connection));
                c.daemon_conn = Some(s.connection.clone());
                c.st.set_status(s);
                c.refresh_status();
                c.render_transcript(Change::Updated);
            }
            Err(e) => {
                c.daemon_conn = Some(ConnectionState::Error(e.message.clone()));
                c.refresh_status();
            }
        });
    }

    /// The status footer (dot + "Connected as …").
    fn refresh_status(&self) {
        let Some(ui) = self.ui() else { return };
        let own = self.st.own_handle();
        let (text, level) = state::status_line(self.bridge_ok, self.bridge_seen, self.daemon_conn.as_ref(), self.daemon_reachable, own.as_deref());
        ui.set_status_text(text.into());
        ui.set_status_level(level as i32);
    }

    fn load_chats(&self) {
        let Some(cl) = self.client.clone() else { return };
        self.run(async move { cl.all_chats().await }, |c, r| match r {
            Ok(chats) => {
                c.st.set_chats(chats);
                c.refresh_chats();
                c.fetch_previews();
                c.fetch_avatars();
                c.refresh_header();
                c.maybe_mark_read();
            }
            Err(e) => c.notice(format!("Chats: {}", e.message)),
        });
    }

    fn load_contacts(&self) {
        let Some(cl) = self.client.clone() else { return };
        let etag = self.st.contacts_etag.clone();
        self.run(async move { cl.contacts(etag.as_deref()).await }, |c, r| match r {
            Ok(ContactsReply::Fresh { contacts, etag }) => {
                c.st.contacts = imessage_common::contacts::Contacts::from_people(contacts.into_iter().map(Into::into).collect());
                c.st.contacts_etag = etag;
                c.refresh_chats();
                c.refresh_header();
                c.render_transcript(Change::Updated);
            }
            Ok(ContactsReply::NotModified) => {}
            Err(e) => log::info!("contacts: {}", e.name),
        });
    }

    fn fetch_previews(&mut self) {
        let ids = self.st.take_stale_previews(120);
        if ids.is_empty() {
            return;
        }
        let Some(cl) = self.client.clone() else { return };
        let generation = self.generation;
        self.rt.spawn(async move {
            for id in ids {
                let m = cl.messages(&id, None, 1).await.ok().and_then(|mut p| p.messages.pop());
                post(move |c| {
                    if c.generation == generation {
                        c.st.preview_done(&id, m);
                        c.refresh_chats();
                    }
                });
            }
        });
    }

    /// Group photos and contacts' shared photos for the first chats.
    fn fetch_avatars(&mut self) {
        let Some(cl) = self.client.clone() else { return };
        let mut jobs: Vec<(String, Option<String>)> = Vec::new();
        for c in self.st.chats.iter().take(120) {
            let id = c.id.0.clone();
            if self.avatars.contains_key(&id) {
                continue;
            }
            if c.is_group {
                if c.icon_path.is_some() {
                    jobs.push((id, None));
                }
            } else if let Some(p) = c.participants.first().filter(|p| p.profile.as_ref().is_some_and(|pr| pr.photo.is_some())) {
                jobs.push((id, Some(p.uri.clone())));
            }
        }
        for (id, _) in &jobs {
            self.avatars.insert(id.clone(), None);
        }
        if jobs.is_empty() {
            return;
        }
        let generation = self.generation;
        self.rt.spawn(async move {
            for (id, handle) in jobs {
                let data = match &handle {
                    Some(h) => cl.handle_photo(h).await,
                    None => cl.chat_icon(&id).await,
                };
                let Ok(Some(data)) = data else { continue };
                let img = tokio::task::spawn_blocking(move || images::decode_avatar(&data)).await.ok().flatten();
                if let Some(img) = img {
                    post(move |c| {
                        if c.generation == generation {
                            c.avatars.insert(id, Some(to_image(&img)));
                            c.refresh_chats();
                        }
                    });
                }
            }
        });
    }

    /// Shared photos of the group senders whose avatar the transcript
    /// shows (others keep their initials).
    fn fetch_sender_photos(&mut self, views: &[MsgView]) {
        let Some(cl) = self.client.clone() else { return };
        let mut uris: Vec<String> = Vec::new();
        for v in views.iter().filter(|v| v.show_avatar) {
            if self.handle_photos.contains_key(&v.sender_uri) || uris.contains(&v.sender_uri) {
                continue;
            }
            let has_photo = self
                .st
                .chats
                .iter()
                .flat_map(|c| c.participants.iter())
                .any(|h| h.uri == v.sender_uri && h.profile.as_ref().is_some_and(|p| p.photo.is_some()));
            if has_photo {
                uris.push(v.sender_uri.clone());
            }
            self.handle_photos.insert(v.sender_uri.clone(), None);
        }
        if uris.is_empty() {
            return;
        }
        let generation = self.generation;
        self.rt.spawn(async move {
            for uri in uris {
                let Ok(Some(data)) = cl.handle_photo(&uri).await else { continue };
                let img = tokio::task::spawn_blocking(move || images::decode_avatar(&data)).await.ok().flatten();
                if let Some(img) = img {
                    post(move |c| {
                        if c.generation == generation {
                            c.handle_photos.insert(uri, Some(to_image(&img)));
                            c.render_transcript(Change::Updated);
                        }
                    });
                }
            }
        });
    }

    fn load_page(&self, id: String, before: Option<String>) {
        let Some(cl) = self.client.clone() else { return };
        let older = before.is_some();
        if let Some(ui) = self.ui() {
            if older {
                ui.set_loading_older(true);
            } else {
                ui.set_loading_chat(true);
            }
        }
        let id2 = id.clone();
        self.run(async move { cl.messages(&id2, before.as_deref(), state::PAGE).await }, move |c, r| {
            if let Some(ui) = c.ui() {
                if older {
                    ui.set_loading_older(false);
                } else {
                    ui.set_loading_chat(false);
                }
            }
            match r {
                Ok(page) => {
                    if let Some(m) = page.messages.last()
                        && !older
                    {
                        c.st.apply_preview(&m.clone());
                    }
                    let change = c.st.apply_page(&id, page, older);
                    c.render_transcript(change);
                    c.refresh_header();
                }
                Err(e) => {
                    c.st.page_failed(&id, older);
                    c.notice(format!("Messages: {}", e.message));
                    c.render_transcript(Change::Updated);
                }
            }
        });
    }

    fn load_older(&mut self) {
        if let Some((id, cursor)) = self.st.begin_older() {
            self.load_page(id, Some(cursor));
        }
    }

    // ===================== chat list =====================

    fn refresh_chats(&mut self) {
        let Some(ui) = self.ui() else { return };
        let open = self.st.open_id().map(str::to_string);
        let rows: Vec<ChatItem> = self
            .st
            .chat_views(chrono::Local::now(), Instant::now())
            .into_iter()
            .map(|v| {
                let avatar = self.avatars.get(&v.id).cloned().flatten();
                ChatItem {
                    selected: open.as_deref() == Some(v.id.as_str()),
                    id: v.id.into(),
                    title: v.title.into(),
                    initials: v.initials.into(),
                    snippet: v.snippet.into(),
                    time: v.time.into(),
                    unread: v.unread as i32,
                    pinned: v.pinned,
                    is_group: v.is_group,
                    sms: v.sms,
                    typing: v.typing,
                    has_avatar: avatar.is_some(),
                    avatar: avatar.unwrap_or_default(),
                    section: v.section.unwrap_or_default().into(),
                    tint: v.tint,
                }
            })
            .collect();
        let model = self.chats_model.clone();
        sync_rows(&model, rows, |i, r| {
            model.row_data(i).is_some_and(|o| {
                o.id == r.id
                    && o.title == r.title
                    && o.snippet == r.snippet
                    && o.time == r.time
                    && o.unread == r.unread
                    && o.pinned == r.pinned
                    && o.selected == r.selected
                    && o.typing == r.typing
                    && o.has_avatar == r.has_avatar
                    && o.initials == r.initials
                    && o.section == r.section
                    && o.tint == r.tint
                    && o.is_group == r.is_group
                    && (!r.has_avatar || o.avatar == r.avatar)
            })
        });
        let unread = self.st.total_unread();
        let title = if unread > 0 { format!("imsg ({unread})") } else { "imsg".into() };
        if ui.get_window_title() != title.as_str() {
            ui.set_window_title(title.clone().into());
            self.notifier.set_tooltip(&if unread > 0 { format!("imsg – {unread} unread") } else { "imsg".into() });
        }
    }

    fn search(&mut self, q: &str) {
        self.st.filter = q.to_string();
        self.refresh_chats();
    }

    // ===================== the open chat =====================

    fn select_chat(&mut self, id: &str) {
        let Some(ui) = self.ui() else { return };
        if self.st.open_id() == Some(id) {
            self.maybe_mark_read();
            ui.invoke_focus_composer();
            return;
        }
        self.stop_typing();
        if let Some(cur) = self.st.open_id().map(str::to_string) {
            let draft = ui.get_composer_text().to_string();
            if draft.is_empty() {
                self.drafts.remove(&cur);
            } else {
                self.drafts.insert(cur, draft);
            }
        }
        if self.images.len() > 300 {
            self.images.clear();
        }
        self.st.open_chat(id);
        self.rendered.clear();
        self.msgs_model.set_vec(Vec::new());
        ui.set_composer_text(self.drafts.get(id).cloned().unwrap_or_default().into());
        ui.set_chat_open(true);
        ui.set_can_load_older(false);
        self.refresh_header();
        self.refresh_chats();
        self.refresh_typing();
        self.load_page(id.to_string(), None);
        // GetChat: the daemon refreshes the chat's photos / profiles.
        if let Some(cl) = self.client.clone() {
            let id2 = id.to_string();
            self.run(async move { cl.chat(&id2).await }, |c, r| {
                if let Ok(chat) = r {
                    c.on_chat_updated(chat);
                }
            });
        }
        self.maybe_mark_read();
        ui.invoke_focus_composer();
    }

    fn refresh_header(&self) {
        let Some(ui) = self.ui() else { return };
        let Some(id) = self.st.open_id() else {
            ui.set_chat_open(false);
            return;
        };
        ui.set_chat_title(self.st.title(id).into());
        let subtitle = match self.st.chat(id) {
            Some(c) if c.is_group => c.participants.iter().map(|p| self.st.name(p)).collect::<Vec<_>>().join(", "),
            Some(c) => c.participants.first().map(|p| p.address().to_string()).unwrap_or_default(),
            None => imessage_common::model::Handle::new(id).address().to_string(),
        };
        let sms = self.st.open.as_ref().and_then(|o| o.messages.last()).map(|m| m.service == Service::Sms).unwrap_or(false);
        ui.set_service_label(if sms { "Text Message" } else { "iMessage" }.into());
        ui.set_chat_subtitle(subtitle.into());
    }

    fn refresh_typing(&self) {
        let Some(ui) = self.ui() else { return };
        let t = self.st.open_id().and_then(|id| self.st.typing_in(id, Instant::now())).unwrap_or_default();
        ui.set_typing(t.into());
    }

    /// Picture states that make up a row (to see whether it changed):
    /// inline pictures, link previews, the sender's photo.
    fn image_key(&self, v: &MsgView) -> Vec<u8> {
        let state = |id: &str| match self.images.get(id) {
            None | Some(Img::Loading) => 0,
            Some(Img::Loaded(..)) => 1,
            Some(Img::Failed) => 2,
        };
        let mut k: Vec<u8> = v
            .parts
            .iter()
            .filter_map(|p| match p {
                PartView::Image { att_id, .. } => Some(state(att_id)),
                PartView::Link { image: Some(id), .. } => Some(state(id)),
                _ => None,
            })
            .collect();
        if v.show_avatar {
            k.push(u8::from(self.handle_photos.get(&v.sender_uri).is_some_and(Option::is_some)));
        }
        k
    }

    /// A loaded picture: (image, width, height, loaded, failed).
    fn picture(&self, att_id: &str) -> (Image, i32, i32, bool, bool) {
        match self.images.get(att_id) {
            Some(Img::Loaded(i, w, h)) => (i.clone(), *w as i32, *h as i32, true, false),
            Some(Img::Failed) => (Image::default(), 0, 0, false, true),
            _ => (Image::default(), 0, 0, false, false),
        }
    }

    fn msg_item(&self, v: &MsgView) -> MsgItem {
        let (links, parts): (Vec<PartItem>, Vec<PartItem>) = v
            .parts
            .iter()
            .map(|p| match p {
                PartView::Image { att_id, filename, size } => {
                    let (image, w, h, loaded, failed) = self.picture(att_id);
                    PartItem {
                        kind: 0,
                        att_id: att_id.into(),
                        name: filename.into(),
                        detail: state::human_size(*size).into(),
                        image,
                        img_w: w,
                        img_h: h,
                        loaded,
                        failed,
                        ..Default::default()
                    }
                }
                PartView::File { att_id, filename, size, .. } => PartItem {
                    kind: 1,
                    att_id: att_id.into(),
                    name: filename.into(),
                    detail: state::human_size(*size).into(),
                    ..Default::default()
                },
                PartView::Link { url, title, site, image } => {
                    let (img, w, h, loaded, failed) = image.as_deref().map(|id| self.picture(id)).unwrap_or_default();
                    PartItem {
                        kind: 2,
                        url: url.into(),
                        title: title.into(),
                        site: site.into(),
                        has_image: image.is_some() && !failed,
                        image: img,
                        img_w: w,
                        img_h: h,
                        loaded,
                        failed,
                        ..Default::default()
                    }
                }
            })
            .partition(|p| p.kind == 2);
        let photo = if v.show_avatar { self.handle_photos.get(&v.sender_uri).cloned().flatten() } else { None };
        MsgItem {
            id: v.id.as_str().into(),
            header: v.header.clone().unwrap_or_default().into(),
            sender: v.sender.clone().unwrap_or_default().into(),
            who: v.who.as_str().into(),
            in_group: v.in_group,
            show_avatar: v.show_avatar,
            sender_initials: v.sender_initials.as_str().into(),
            sender_tint: v.sender_tint,
            has_sender_photo: photo.is_some(),
            sender_photo: photo.unwrap_or_default(),
            has_link: v.parts.iter().any(|p| matches!(p, PartView::Link { .. })),
            failed: v.failed,
            from_me: v.from_me,
            sms: v.sms,
            text: v.text.clone().unwrap_or_default().into(),
            subject: v.subject.clone().unwrap_or_default().into(),
            parts: ModelRc::new(VecModel::from(parts)),
            links: ModelRc::new(VecModel::from(links)),
            unsent: v.unsent,
            edited: v.edited,
            unverified: v.unverified,
            effect: v.effect.clone().unwrap_or_default().into(),
            reply_quote: v.reply_quote.clone().unwrap_or_default().into(),
            reactions: ModelRc::new(VecModel::from(
                v.reactions
                    .iter()
                    .map(|r| ReactionItem {
                        kind: r.kind.as_str().into(),
                        label: r.label.as_str().into(),
                        count: r.count as i32,
                        mine: r.mine,
                        senders: r.senders.as_str().into(),
                    })
                    .collect::<Vec<_>>(),
            )),
            can_react: v.can_react,
            my_reaction: v.my_reaction.clone().unwrap_or_default().into(),
            delivery: v.delivery.clone().unwrap_or_default().into(),
            time: v.time.as_str().into(),
            first_in_run: v.first_in_run,
            last_in_run: v.last_in_run,
        }
    }

    fn render_transcript(&mut self, change: Change) {
        let Some(ui) = self.ui() else { return };
        if change == Change::None {
            return;
        }
        let views = self.st.transcript(chrono::Local::now());
        // Ask for pictures not seen yet.
        let wanted: Vec<(String, Corners)> = views
            .iter()
            .flat_map(|v| v.parts.iter())
            .filter_map(|p| match p {
                PartView::Image { att_id, .. } => Some((att_id.clone(), Corners::All)),
                PartView::Link { image: Some(id), .. } => Some((id.clone(), Corners::Top)),
                _ => None,
            })
            .filter(|(id, _)| !self.images.contains_key(id))
            .collect();
        for (id, corners) in wanted {
            self.load_image(id, corners);
        }
        self.fetch_sender_photos(&views);
        let new: Vec<(MsgView, Vec<u8>)> = views.into_iter().map(|v| {
            let k = self.image_key(&v);
            (v, k)
        }).collect();
        let model = self.msgs_model.clone();
        let old_ids: Vec<&str> = self.rendered.iter().map(|(v, _)| v.id.as_str()).collect();
        let prepended = new.len().saturating_sub(old_ids.len());
        let is_prepend = change == Change::Prepended
            && prepended > 0
            && !old_ids.is_empty()
            && new[prepended..].iter().map(|(v, _)| v.id.as_str()).eq(old_ids.iter().copied());
        if is_prepend {
            ui.set_anchor_prepend(true);
            for (i, (v, _)) in new[..prepended].iter().enumerate() {
                model.insert(i, self.msg_item(v));
            }
            // Rows below may have changed their grouping.
            for (j, old) in self.rendered.iter().enumerate() {
                let row = &new[j + prepended];
                if old != row {
                    model.set_row_data(j + prepended, self.msg_item(&row.0));
                }
            }
        } else if change == Change::Reset && self.rendered.is_empty() {
            model.set_vec(new.iter().map(|(v, _)| self.msg_item(v)).collect::<Vec<_>>());
        } else {
            let rows: Vec<MsgItem> = new.iter().map(|(v, _)| self.msg_item(v)).collect();
            let rendered = &self.rendered;
            sync_rows(&model, rows, |i, _| rendered.get(i) == new.get(i));
        }
        let first_fill = change == Change::Reset && self.rendered.is_empty();
        self.rendered = new;
        ui.set_can_load_older(self.st.has_older());
        if first_fill {
            ui.invoke_scroll_to_bottom();
            // Again once the layout has settled (pictures, wrapping).
            let w = self.ui.clone();
            Timer::single_shot(Duration::from_millis(60), move || {
                if let Some(ui) = w.upgrade() {
                    ui.invoke_scroll_to_bottom();
                }
            });
        }
        self.refresh_header();
    }

    fn load_image(&mut self, att_id: String, corners: Corners) {
        let Some(cl) = self.client.clone() else { return };
        self.images.insert(att_id.clone(), Img::Loading);
        let slots = self.image_slots.clone();
        let id2 = att_id.clone();
        self.run(
            async move {
                let _permit = slots.acquire_owned().await.ok();
                let data = cl.download(&id2).await.ok()?;
                tokio::task::spawn_blocking(move || images::decode_picture(&data, images::INLINE_MAX, corners)).await.ok().flatten()
            },
            move |c, r| {
                let img = match r {
                    Some(rgba) => Img::Loaded(to_image(&rgba), rgba.width, rgba.height),
                    None => Img::Failed,
                };
                c.images.insert(att_id, img);
                c.render_transcript(Change::Updated);
            },
        );
    }

    // ===================== read receipts, typing =====================

    fn maybe_mark_read(&mut self) {
        if !self.focused {
            return;
        }
        if let Some(id) = self.st.open_id().map(str::to_string)
            && self.st.open_has_unread()
        {
            self.mark_read(id);
        }
    }

    fn mark_read(&mut self, id: String) {
        if !self.read_pending.insert(id.clone()) {
            return;
        }
        let Some(cl) = self.client.clone() else { return };
        let id2 = id.clone();
        self.run(async move { cl.mark_read(&id2, None).await }, move |c, r| {
            c.read_pending.remove(&id);
            if let Err(e) = r {
                log::info!("mark read {}: {}", redact(&id), e.name);
            }
        });
    }

    fn send_typing(&self, chat_id: String, typing: bool) {
        let Some(cl) = self.client.clone() else { return };
        // Unsupported (SMS chat) or a hiccup: not worth telling the user.
        self.rt.spawn(async move {
            let _ = cl.set_typing(&chat_id, typing).await;
        });
    }

    fn stop_typing(&mut self) {
        self.typing_timer.stop();
        if let Some((id, _)) = self.typing_sent.take() {
            self.send_typing(id, false);
        }
    }

    fn composer_edited(&mut self, text: &str) {
        let Some(id) = self.st.open_id().map(str::to_string) else { return };
        if text.trim().is_empty() {
            self.stop_typing();
            return;
        }
        let fresh = match &self.typing_sent {
            Some((sent, at)) => *sent != id || at.elapsed() > Duration::from_secs(20),
            None => true,
        };
        if fresh {
            if let Some((other, _)) = self.typing_sent.take().filter(|(s, _)| *s != id) {
                self.send_typing(other, false);
            }
            self.send_typing(id.clone(), true);
            self.typing_sent = Some((id, Instant::now()));
        }
        // Stop after 5 s without a keystroke.
        self.typing_timer.start(TimerMode::SingleShot, Duration::from_secs(5), || with(|c| c.stop_typing()));
    }

    // ===================== sending =====================

    fn send(&mut self, text: String) {
        let Some(ui) = self.ui() else { return };
        let Some(id) = self.st.open_id().map(str::to_string) else { return };
        let body = text.trim_end().to_string();
        if body.trim().is_empty() {
            return;
        }
        let Some(cl) = self.client.clone() else { return };
        ui.set_composer_text("".into());
        self.drafts.remove(&id);
        self.typing_timer.stop();
        // Sending clears the indicator on the other side anyway.
        self.typing_sent = None;
        let target = Target::Chat(id.clone());
        let b2 = body.clone();
        self.run(async move { cl.send_text(&target, &b2, None, None).await }, move |c, r| {
            if let Err(e) = r {
                log::info!("send to {}: {}", redact(&id), e.name);
                c.notice(format!("Not sent: {}", e.message));
                // Give the text back if the box is still empty.
                if let Some(ui) = c.ui()
                    && c.st.open_id() == Some(id.as_str())
                    && ui.get_composer_text().is_empty()
                {
                    ui.set_composer_text(body.into());
                }
            }
        });
    }

    /// A tapback from the picker or a chip: picking our current one takes
    /// it back, anything else replaces it. The bridge's `message_updated`
    /// re-renders the chips (no optimistic update).
    fn react(&mut self, msg_id: String, input: String) {
        let Some((chat_id, kind, remove)) = self.st.reaction_request(&msg_id, &input) else {
            if state::tapback_kind(&input).is_none() {
                self.notice("Type a single emoji to react with");
            }
            return;
        };
        let Some(cl) = self.client.clone() else { return };
        self.notice(if remove { "Removing reaction…" } else { "Reacting…" });
        self.run(async move { cl.react(&chat_id, &msg_id, &kind, remove).await }, move |c, r| match r {
            Ok(()) => c.notice(""),
            Err(e) => {
                log::info!("react: {}", e.name);
                c.notice(format!("Reaction not sent: {}", e.message));
            }
        });
    }

    fn attach(&mut self) {
        let Some(id) = self.st.open_id().map(str::to_string) else { return };
        let generation = self.generation;
        std::thread::spawn(move || {
            let Some(path) = rfd::FileDialog::new().set_title("Send a file").pick_file() else { return };
            post(move |c| {
                if c.generation == generation {
                    c.send_file(id, path)
                }
            });
        });
    }

    fn send_file(&mut self, chat_id: String, path: PathBuf) {
        let Some(cl) = self.client.clone() else { return };
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
        self.notice(format!("Sending {name}…"));
        let mime = state::guess_mime(&name);
        self.run(async move { cl.send_file(&Target::Chat(chat_id), &path, Some(&name), mime, None).await }, |c, r| match r {
            Ok(_) => c.notice(""),
            Err(e) => c.notice(format!("File not sent: {}", e.message)),
        });
    }

    fn new_to_edited(&mut self, text: &str) {
        let Some(ui) = self.ui() else { return };
        let mut input = text.to_string();
        // A picked suggestion arrives as "\0<address>": replace the word
        // being typed with it.
        if let Some(addr) = text.strip_prefix('\0') {
            let cur = ui.get_new_to().to_string();
            let head = match cur.rfind([',', ';']) {
                Some(i) => format!("{}, ", cur[..i].trim_end()),
                None => String::new(),
            };
            input = format!("{head}{addr}, ");
            ui.set_new_to(input.clone().into());
        }
        let sugg: Vec<Suggestion> =
            self.st.contact_suggestions(&input, 8).into_iter().map(|(n, a)| Suggestion { name: n.into(), address: a.into() }).collect();
        self.suggestions_model.set_vec(sugg);
    }

    fn send_new(&mut self, to: String, text: String) {
        let Some(ui) = self.ui() else { return };
        ui.set_dialog_error("".into());
        let recipients = match self.st.resolve_recipients(&to) {
            Ok(r) => r,
            Err(e) => {
                ui.set_dialog_error(e.into());
                return;
            }
        };
        if text.trim().is_empty() {
            return;
        }
        let Some(cl) = self.client.clone() else { return };
        ui.set_dialog_busy("Sending…".into());
        self.run(async move { cl.send_text(&Target::Participants(recipients), text.trim_end(), None, None).await }, |c, r| {
            let Some(ui) = c.ui() else { return };
            ui.set_dialog_busy("".into());
            match r {
                Ok(sent) => {
                    ui.set_new_open(false);
                    ui.set_new_to("".into());
                    ui.set_new_text("".into());
                    c.suggestions_model.set_vec(Vec::new());
                    c.select_chat(&sent.chat_id);
                    c.load_chats();
                }
                Err(e) => ui.set_dialog_error(format!("Not sent: {}", e.message).into()),
            }
        });
    }

    fn submit_two_factor(&mut self, code: String) {
        let Some(ui) = self.ui() else { return };
        let code: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
        if code.len() < 6 {
            ui.set_dialog_error("Enter the 6-digit code.".into());
            return;
        }
        let Some(cl) = self.client.clone() else { return };
        ui.set_dialog_error("".into());
        ui.set_dialog_busy("Checking…".into());
        self.run(async move { cl.two_factor(&code).await }, |c, r| {
            let Some(ui) = c.ui() else { return };
            ui.set_dialog_busy("".into());
            match r {
                Ok(TwoFactorResult::Ok) => {
                    ui.set_twofa_open(false);
                    c.notice("Verification code accepted.");
                }
                Ok(TwoFactorResult::AnotherCode) => ui.set_dialog_error("Accepted; Apple wants another code.".into()),
                Err(e) if e.is("BadTwoFactorCode") => ui.set_dialog_error("Wrong code. Try again.".into()),
                Err(e) if e.is("NoPendingTwoFactor") => {
                    ui.set_twofa_open(false);
                    c.notice("No code is needed any more.");
                }
                Err(e) => ui.set_dialog_error(e.message.into()),
            }
        });
    }

    fn overlays_closed(&mut self) {
        if let Some(ui) = self.ui() {
            ui.set_dialog_error("".into());
            ui.set_dialog_busy("".into());
            ui.set_settings_message("".into());
            self.suggestions_model.set_vec(Vec::new());
            ui.invoke_focus_composer();
        }
    }

    // ===================== attachments, links =====================

    fn open_attachment(&mut self, att_id: String, name: String) {
        let Some(cl) = self.client.clone() else { return };
        self.open_counter += 1;
        let dir = self.dirs.open_dir().join(format!("{}", self.open_counter));
        let dest = dir.join(safe_file_name(&name));
        self.notice(format!("Opening {name}…"));
        self.run(
            async move {
                crate::appdir::create_private_dir(&dir).map_err(|e| e.to_string())?;
                cl.download_to(&att_id, &dest).await.map_err(|e| e.message)?;
                Ok::<_, String>(dest)
            },
            |c, r| match r {
                Ok(p) => match imessage_common::platform::desktop::open_with_default(&p.to_string_lossy()) {
                    Ok(imessage_common::platform::desktop::Opened::Folder) => c.notice("Not a known document type: opened its folder instead."),
                    Ok(_) => c.notice(""),
                    Err(e) => c.notice(format!("Could not open: {e}")),
                },
                Err(e) => c.notice(format!("Could not download: {e}")),
            },
        );
    }

    fn save_attachment(&mut self, att_id: String, name: String) {
        let generation = self.generation;
        let suggested = safe_file_name(&name);
        std::thread::spawn(move || {
            let Some(path) = rfd::FileDialog::new().set_title("Save attachment").set_file_name(&suggested).save_file() else { return };
            post(move |c| {
                if c.generation != generation {
                    return;
                }
                let Some(cl) = c.client.clone() else { return };
                c.notice(format!("Saving {}…", path.display()));
                let p2 = path.clone();
                c.run(async move { cl.download_to(&att_id, &p2).await }, move |c, r| match r {
                    Ok(_) => c.notice(format!("Saved {}", path.display())),
                    Err(e) => c.notice(format!("Not saved: {}", e.message)),
                });
            });
        });
    }

    fn open_url(&self, url: &str) {
        let l = url.to_ascii_lowercase();
        if !(l.starts_with("https://") || l.starts_with("http://")) {
            return;
        }
        if let Err(e) = imessage_common::platform::desktop::open_with_default(url) {
            self.notice(format!("Could not open the link: {e}"));
        }
    }

    // ===================== settings =====================

    fn settings_message(&self, s: impl Into<String>) {
        if let Some(ui) = self.ui() {
            ui.set_settings_message(s.into().into());
        }
    }

    fn save_config(&self) -> bool {
        match self.config.save(&self.dirs.config_file()) {
            Ok(()) => true,
            Err(e) => {
                self.settings_message(format!("Could not save settings: {e}"));
                false
            }
        }
    }

    fn settings_changed(&mut self) {
        let Some(ui) = self.ui() else { return };
        let log_before = self.config.log;
        self.config.notifications.enabled = ui.get_set_notify();
        self.config.notifications.show_text = ui.get_set_notify_text();
        self.config.log = ui.get_set_log();
        self.config.font_size = FontSize::from_index(ui.get_set_font());
        self.config.theme = ThemeMode::from_index(ui.get_set_theme());
        self.apply_font();
        self.apply_theme();
        if self.save_config() {
            self.settings_message(if self.config.log != log_before { "Saved. Logging changes at the next start." } else { "Saved." });
        }
    }

    fn reseal(&mut self, s: Secrets) -> bool {
        let Some(key) = &self.seal else { return false };
        match secrets::save(&self.dirs.secrets_file(), key, &s) {
            Ok(()) => {
                self.secrets = Some(s);
                true
            }
            Err(e) => {
                self.settings_message(format!("Could not save: {e}"));
                false
            }
        }
    }

    /// New bridge URL or token: reconnect with it.
    fn reconnect_with(&mut self, s: Secrets) {
        let client = match RemoteClient::new(&s.bridge_url, &s.token) {
            Ok(c) => c,
            Err(e) => {
                self.settings_message(e.to_string());
                return;
            }
        };
        if !self.reseal(s) {
            return;
        }
        self.generation += 1;
        self.client = Some(client);
        self.read_pending.clear();
        self.bridge_ok = false;
        self.refresh_status();
        self.start_events();
        self.load_settings_ui();
    }

    fn save_bridge_url(&mut self, url: String) {
        let Some(url) = imsg_remote::normalize_base_url(&url) else {
            self.settings_message("Not a bridge URL (http://host:port).");
            return;
        };
        let Some(token) = self.secrets.as_ref().map(|s| s.token.clone()) else { return };
        self.config.bridge = Some(url.clone());
        self.save_config();
        self.reconnect_with(Secrets { bridge_url: url, token });
        self.settings_message("Bridge URL saved; reconnecting.");
    }

    fn repair(&mut self, s: String) {
        match imsg_remote::parse_pairing(&s) {
            Ok(p) => {
                self.reconnect_with(Secrets { bridge_url: p.bridge_url.clone(), token: p.token.clone() });
                self.settings_message("Paired again; reconnecting.");
            }
            Err(e) => self.settings_message(e.to_string()),
        }
    }

    fn change_passphrase(&mut self, old: Zeroizing<String>, a: Zeroizing<String>, b: Zeroizing<String>) {
        if let Err(e) = secrets::check_new_passphrase(&a, &b) {
            self.settings_message(e);
            return;
        }
        let (Some(key), Some(s)) = (self.seal.clone(), self.secrets.clone()) else { return };
        let path = self.dirs.secrets_file();
        self.settings_message("Changing the passphrase…");
        self.run(
            async move {
                tokio::task::spawn_blocking(move || {
                    if !key.matches(&old) {
                        return Err("The current passphrase is wrong.".to_string());
                    }
                    let new = SealKey::derive_new(&a).map_err(|e| e.to_string())?;
                    secrets::save(&path, &new, &s).map_err(|e| e.to_string())?;
                    Ok(new)
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
            },
            |c, r| match r {
                Ok(k) => {
                    c.seal = Some(k);
                    c.settings_message("Passphrase changed.");
                }
                Err(e) => c.settings_message(e),
            },
        );
    }

    fn clear_cache(&mut self) {
        let r = self.dirs.clear_cache();
        self.images.clear();
        self.avatars.clear();
        self.handle_photos.clear();
        self.render_transcript(Change::Updated);
        self.refresh_chats();
        self.fetch_avatars();
        match r {
            Ok(()) => self.settings_message("Cache cleared."),
            Err(e) => self.settings_message(format!("Could not clear the cache: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::minimize_step;

    #[test]
    fn minimize_then_restore_is_seen_once() {
        // Normal resizes: never minimized, never "restored".
        assert_eq!(minimize_step(false, 1120, 780), (false, false));
        // Windows: minimize = Resized(0, 0) (either side zero counts).
        assert_eq!(minimize_step(false, 0, 0), (true, false));
        assert_eq!(minimize_step(false, 1120, 0), (true, false));
        // Repeated zero sizes while minimized change nothing.
        assert_eq!(minimize_step(true, 0, 0), (true, false));
        // Restore = Resized(old size): restored exactly once.
        assert_eq!(minimize_step(true, 1120, 780), (false, true));
        assert_eq!(minimize_step(false, 1120, 780), (false, false));
    }
}
