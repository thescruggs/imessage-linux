//! `imsg-tui`: your iMessages as bubbles in a terminal, backed by the
//! `imessaged` daemon over D-Bus (see docs/TUI.md).

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event, EventStream};
use futures_util::StreamExt;
use imsg_client::Bus;
use imsg_tui::app::{self, App, AppEvent};
use imsg_tui::{backend, contacts, ui};
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(name = "imsg-tui", version, about = "iMessage in your terminal (talks to imessaged)")]
struct Args {
    /// D-Bus address to use instead of the session bus (tests, containers).
    /// Windows (and IMESSAGED_TRANSPORT=p2p): default is the daemon's
    /// peer-to-peer address in <state dir>/bus-address.
    #[arg(long, env = "IMSG_BUS_ADDRESS", value_name = "ADDR")]
    bus_address: Option<String>,
    /// The daemon's state directory, where a peer-to-peer daemon writes its
    /// address. Default: $XDG_STATE_HOME/imessaged (Windows:
    /// %LOCALAPPDATA%\imessaged).
    #[arg(long, env = "IMESSAGED_STATE_DIR", value_name = "DIR")]
    state_dir: Option<PathBuf>,
    /// Folder(s) of vCards for contact names, `:`-separated (`;` on Windows)
    /// (default: ~/.local/share/kpeoplevcard, where `imsg sync-mac` writes).
    #[arg(long, env = "IMSG_CONTACTS_DIR", value_name = "DIR")]
    contacts_dir: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    // Logging to stderr would corrupt the screen: only RUST_LOG=… to a file
    // makes sense here (env_logger writes to stderr; redirect it yourself).
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("off")).init();
    let args = Args::parse();
    let bus = match &args.bus_address {
        Some(a) => Bus::Address(a.clone()),
        None => {
            let state_dir = args.state_dir.clone().unwrap_or_else(imessage_common::paths::default_state_dir);
            match Bus::for_state_dir(&state_dir) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("imsg-tui: {e}");
                    return ExitCode::from(e.code);
                }
            }
        }
    };
    let backend = match backend::Backend::connect(bus).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("imsg-tui: {e}");
            return ExitCode::from(e.code);
        }
    };
    let dirs: Vec<PathBuf> = match &args.contacts_dir {
        Some(d) => contacts::split_dirs(d),
        None => contacts::default_dirs(),
    };
    let contacts = contacts::Contacts::load(&dirs);
    match run(backend, contacts, dirs).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("imsg-tui: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(backend: backend::Backend, contacts: contacts::Contacts, contact_dirs: Vec<PathBuf>) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
    let mut app = App::new(backend.clone(), tx.clone(), contacts, contact_dirs);
    // The status and chat list load once the signal subscription is up
    // (DaemonEvent::Subscribed), so nothing in between can be missed.
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let (dtx, mut drx) = mpsc::unbounded_channel();
            let b = backend.clone();
            tokio::spawn(async move { b.signal_loop(dtx).await });
            while let Some(ev) = drx.recv().await {
                if tx.send(AppEvent::Daemon(ev)).is_err() {
                    break;
                }
            }
        });
    }

    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), EnableBracketedPaste);
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut status_poll = tokio::time::interval(std::time::Duration::from_secs(30));
    status_poll.tick().await; // the first tick fires at once; status is already loading
    let result: Result<()> = loop {
        if app.force_redraw {
            app.force_redraw = false;
            let _ = terminal.clear();
        }
        if let Err(e) = terminal.draw(|f| ui::draw(f, &mut app)) {
            break Err(e.into());
        }
        tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(Event::Key(k))) => {
                    if let Err(e) = app.on_key(Event::Key(k)) {
                        app.set_status(format!("{e}"));
                    }
                }
                Some(Ok(Event::Paste(s))) => {
                    if app.focus == app::Focus::Compose && app.mode == app::Mode::Normal {
                        app.compose.insert_str(&s);
                    }
                }
                Some(Ok(Event::Resize(..))) => app.force_redraw = true,
                Some(Ok(_)) => {}
                Some(Err(e)) => break Err(e.into()),
                None => break Ok(()),
            },
            Some(ev) = rx.recv() => app.on_app_event(ev),
            _ = status_poll.tick() => app.load_status(),
            _ = tick.tick() => {}
        }
        if app.should_quit {
            break Ok(());
        }
    };
    let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}
