//! `imsg`: command-line client for the `imessaged` daemon.
//!
//! A thin D-Bus client (see docs/CLI.md and docs/DBUS.md): every messaging
//! command is one or two calls on `org.imessage.Daemon1`. `--json` prints
//! the daemon's JSON unchanged; human output is the default.

mod backgrounds;
mod client;
mod commands;
mod format;
mod import;
mod local;
mod resolve;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use imessage_common::paths;

use client::{Bus, CliError, exit};
use format::Style;

#[derive(Parser, Debug)]
#[command(name = "imsg", version, about = "iMessage on Linux: command-line client for imessaged")]
pub struct Cli {
    /// Daemon state directory (used by `hwconfig import` and `doctor`).
    /// Default: $XDG_STATE_HOME/imessaged or ~/.local/state/imessaged
    #[arg(long, global = true, env = "IMESSAGED_STATE_DIR")]
    pub state_dir: Option<PathBuf>,

    /// D-Bus address to use instead of the session bus (tests, containers).
    #[arg(long, global = true, env = "IMSG_BUS_ADDRESS", value_name = "ADDR")]
    pub bus_address: Option<String>,

    /// Print the daemon's JSON unchanged (one document per line for `watch`).
    #[arg(long, global = true)]
    pub json: bool,

    /// Print nothing on success except requested data (no confirmations or hints).
    #[arg(long, short = 'q', global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug, PartialEq)]
pub enum Command {
    /// Sign in with your Apple ID (prompts for password and 2FA code) and register with iMessage.
    Login {
        /// Apple ID email address (prompted for if omitted).
        #[arg(long)]
        apple_id: Option<String>,
        /// Read the password from the first line of stdin instead of the terminal.
        #[arg(long)]
        password_stdin: bool,
        /// Read two-factor codes from stdin (one per line) instead of the terminal.
        #[arg(long)]
        code_stdin: bool,
        /// Only submit a two-factor code for a login that is already waiting
        /// for one (e.g. the daemon's automatic re-login).
        #[arg(long, conflicts_with_all = ["apple_id", "password_stdin"])]
        code_only: bool,
    },
    /// Forget the account locally (message history is kept).
    Logout,
    /// Show connection and registration status.
    Status,
    /// List conversations, most recent first.
    Chats {
        /// Maximum number of chats (daemon default 50, max 500).
        #[arg(long, short = 'n', default_value_t = 50)]
        limit: u32,
        /// Skip this many chats.
        #[arg(long, default_value_t = 0)]
        offset: u32,
    },
    /// Show one conversation's details (participants, background, ...).
    Chat {
        /// Chat id, handle or name.
        chat: String,
    },
    /// Manage a conversation's transcript background.
    ChatBackground {
        /// Chat id, handle or name.
        chat: String,
        /// Set the background locally from this file without sending it
        /// (e.g. one recovered from a Mac); a later change from iMessage
        /// replaces it.
        #[arg(long, value_name = "FILE")]
        import: PathBuf,
    },
    /// Show messages in a conversation (oldest first).
    Messages {
        /// Chat id, handle (alice@example.com, +15551234567) or chat name.
        chat: String,
        /// Maximum number of messages.
        #[arg(long, short = 'n', default_value_t = 50)]
        limit: u32,
        /// Page back: the cursor printed after the previous page.
        #[arg(long, value_name = "CURSOR")]
        before: Option<String>,
        /// Newest first.
        #[arg(long, short = 'r')]
        reverse: bool,
    },
    /// Send a text message.
    Send {
        /// Chat id or name, a handle, or several handles separated by commas (a group).
        to: String,
        /// Message text (several words are joined with spaces; `-` reads
        /// stdin; put `--` first for text starting with `-`).
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        /// Reply inline to this message id.
        #[arg(long, value_name = "MSG_ID")]
        reply_to: Option<String>,
    },
    /// Send a file as an attachment.
    SendFile {
        /// Chat id or name, a handle, or several handles separated by commas.
        to: String,
        /// File to send (must be readable by the daemon).
        file: PathBuf,
        /// MIME type (default: guessed from the extension).
        #[arg(long)]
        mime: Option<String>,
    },
    /// Add (or remove) a tapback.
    React {
        /// Chat id, handle or name.
        chat: String,
        /// Message id.
        message: String,
        /// love | like | dislike | laugh | emphasize | question | an emoji
        /// (aliases include +1 / -1).
        #[arg(allow_negative_numbers = true)]
        kind: String,
        /// Remove the reaction instead.
        #[arg(long)]
        remove: bool,
    },
    /// Edit one of your messages.
    Edit {
        /// Chat id, handle or name.
        chat: String,
        /// Message id.
        message: String,
        /// New text (`-` reads stdin).
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
    },
    /// Unsend one of your messages.
    Unsend {
        /// Chat id, handle or name.
        chat: String,
        /// Message id.
        message: String,
    },
    /// Send a typing indicator.
    Typing {
        /// Chat id, handle or name.
        chat: String,
        /// on | off
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Mark a conversation read (up to a message, or all of it).
    Read {
        /// Chat id, handle or name.
        chat: String,
        /// Last message to mark read (default: everything).
        up_to: Option<String>,
    },
    /// Check whether handles are reachable on iMessage.
    Lookup {
        /// Email addresses / phone numbers.
        #[arg(required = true)]
        handles: Vec<String>,
    },
    /// Search message text.
    Search {
        /// Text to search for (case-insensitive substring of message text and subject).
        query: String,
        /// Maximum number of results.
        #[arg(long, short = 'n', default_value_t = 50)]
        limit: u32,
    },
    /// Download an attachment and print its local path.
    Download {
        /// Attachment id (`<message id>:<part>`, shown by `imsg messages`).
        attachment: String,
    },
    /// Stream daemon events until Ctrl-C.
    Watch {
        /// Only events for this chat (id, handle or name).
        #[arg(long)]
        chat: Option<String>,
    },
    /// Check the local setup and the daemon.
    Doctor,
    /// One-time import of recent history from a Mac's Messages database (over ssh).
    ImportMac {
        /// ssh destination of the Mac (`mymac.local`, `user@mac`, a Host alias).
        host: String,
        /// Import the last N days.
        #[arg(long, default_value_t = 10, conflicts_with_all = ["since", "backgrounds"])]
        days: u32,
        /// Import from this date on instead (YYYY-MM-DD, local time, or RFC 3339).
        #[arg(long, value_name = "DATE", conflicts_with = "backgrounds")]
        since: Option<String>,
        /// Skip attachment files larger than this many MB.
        #[arg(long, default_value_t = 50, value_name = "MB", conflicts_with = "backgrounds")]
        max_attachment_mb: u64,
        /// Abort if the whole export is larger than this many MB.
        #[arg(long, default_value_t = 2048, value_name = "MB")]
        max_total_mb: u64,
        /// Leave out SMS / RCS conversations relayed by the iPhone.
        #[arg(long, conflicts_with = "backgrounds")]
        imessage_only: bool,
        /// Fetch and convert, print what would be imported, import nothing.
        #[arg(long)]
        dry_run: bool,
        /// Import the chats' transcript backgrounds instead of messages
        /// (locally only; nothing is sent).
        #[arg(long)]
        backgrounds: bool,
        /// With --backgrounds: only this chat (id or handle, or a name the
        /// daemon knows); repeatable.
        #[arg(long = "chat", value_name = "CHAT", requires = "backgrounds")]
        chats: Vec<String>,
        /// With --backgrounds: create chats the daemon does not have yet
        /// (default: skip them).
        #[arg(long, requires = "backgrounds")]
        create: bool,
    },
    /// Message store maintenance.
    Store {
        #[command(subcommand)]
        command: StoreCommand,
    },
    /// Manage the Mac hardware identity.
    Hwconfig {
        #[command(subcommand)]
        command: HwconfigCommand,
    },
    /// Print the imsg and imessaged versions.
    Version,
}

#[derive(Subcommand, Debug, PartialEq)]
pub enum StoreCommand {
    /// Remove chats whose id is not a valid mailto:/tel:/group: id (left by
    /// a bad import) with their messages, reactions and attachment files.
    /// Only reports what it would remove unless --yes is given.
    PurgeInvalidChats {
        /// Actually delete (default: dry run).
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug, PartialEq)]
pub enum HwconfigCommand {
    /// Validate a hwconfig.plist written by tools/mac-hwinfo.sh and install it
    /// into the daemon state directory (mode 0600).
    Import {
        /// Path to hwconfig.plist.
        file: PathBuf,
        /// Replace an existing, different hwconfig.
        #[arg(long)]
        force: bool,
    },
}

/// Output settings shared by all commands.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub json: bool,
    pub quiet: bool,
    /// Style for stdout.
    pub style: Style,
    /// Style for stderr hints (decided per stream).
    pub err_style: Style,
    pub bus: Bus,
}

fn main() -> ExitCode {
    // Die quietly on a closed pipe (`imsg watch | head`) like other CLIs.
    // SAFETY: called before any other thread exists.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let cli = Cli::parse();
    let state_dir = cli.state_dir.clone().unwrap_or_else(paths::default_state_dir);
    let ctx = Ctx {
        json: cli.json,
        quiet: cli.quiet,
        style: Style::detect(),
        err_style: Style::detect_stderr(),
        bus: match cli.bus_address.clone().filter(|a| !a.is_empty()) {
            Some(a) => Bus::Address(a),
            None => Bus::Session,
        },
    };

    let result: anyhow::Result<u8> = match cli.command {
        Command::Hwconfig { command: HwconfigCommand::Import { file, force } } => local::hwconfig_import(&file, &state_dir, force),
        command => {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build();
            match rt {
                Ok(rt) => rt.block_on(async {
                    match command {
                        Command::Doctor => local::doctor(&state_dir, &ctx.bus, ctx.json, ctx.style).await,
                        Command::ImportMac { host, backgrounds: true, chats, create, max_total_mb, dry_run, .. } => {
                            backgrounds::run(backgrounds::BackgroundArgs { host, chats, create, max_total_mb, dry_run }, &ctx).await
                        }
                        Command::ImportMac { host, days, since, max_attachment_mb, max_total_mb, imessage_only, dry_run, .. } => {
                            import::run(import::ImportArgs { host, days, since, max_attachment_mb, max_total_mb, imessage_only, dry_run }, &ctx).await
                        }
                        other => commands::run(other, &ctx).await,
                    }
                }),
                Err(e) => Err(anyhow::anyhow!("cannot start the async runtime: {e}")),
            }
        }
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            let code = e.downcast_ref::<CliError>().map(|c| c.code).unwrap_or(exit::ERROR);
            eprintln!("imsg: error: {e:#}");
            ExitCode::from(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("imsg").chain(args.iter().copied()))
    }

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parse_send() {
        let c = parse(&["send", "a@x.com,b@x.com", "hello", "world", "--reply-to", "ID"]).unwrap();
        assert_eq!(
            c.command,
            Command::Send { to: "a@x.com,b@x.com".into(), text: vec!["hello".into(), "world".into()], reply_to: Some("ID".into()) }
        );
        let c = parse(&["send", "bob@x.com", "--", "-1", "--not-a-flag"]).unwrap();
        assert!(matches!(c.command, Command::Send { text, .. } if text == vec!["-1".to_string(), "--not-a-flag".to_string()]));
        let c = parse(&["send", "bob@x.com", "-"]).unwrap();
        assert!(matches!(c.command, Command::Send { text, .. } if text == vec!["-".to_string()]));
        assert!(parse(&["send", "bob@x.com"]).is_err(), "text required");
    }

    #[test]
    fn parse_global_flags_anywhere() {
        let c = parse(&["status", "--json", "--bus-address", "unix:path=/x", "-q"]).unwrap();
        assert!(c.json && c.quiet);
        assert_eq!(c.bus_address.as_deref(), Some("unix:path=/x"));
        let c = parse(&["--json", "doctor"]).unwrap();
        assert!(c.json && c.command == Command::Doctor);
    }

    #[test]
    fn parse_messages_and_chats() {
        let c = parse(&["messages", "alice@example.com", "-n", "5", "--before", "123-4", "--reverse"]).unwrap();
        assert_eq!(c.command, Command::Messages { chat: "alice@example.com".into(), limit: 5, before: Some("123-4".into()), reverse: true });
        let c = parse(&["chats", "--limit", "10", "--offset", "20"]).unwrap();
        assert_eq!(c.command, Command::Chats { limit: 10, offset: 20 });
        let c = parse(&["chats"]).unwrap();
        assert_eq!(c.command, Command::Chats { limit: 50, offset: 0 });
    }

    #[test]
    fn parse_login() {
        let c = parse(&["login", "--apple-id", "me@x.com", "--password-stdin", "--code-stdin"]).unwrap();
        assert_eq!(c.command, Command::Login { apple_id: Some("me@x.com".into()), password_stdin: true, code_stdin: true, code_only: false });
        assert!(parse(&["login", "--code-only", "--apple-id", "x"]).is_err());
        assert!(parse(&["login", "--code-only", "--code-stdin"]).is_ok());
    }

    #[test]
    fn parse_actions() {
        assert!(parse(&["typing", "bob@x.com", "maybe"]).is_err());
        assert!(parse(&["typing", "bob@x.com", "on"]).is_ok());
        let c = parse(&["react", "c", "M", "🔥", "--remove"]).unwrap();
        assert_eq!(c.command, Command::React { chat: "c".into(), message: "M".into(), kind: "🔥".into(), remove: true });
        let c = parse(&["react", "c", "M", "-1"]).unwrap();
        assert_eq!(c.command, Command::React { chat: "c".into(), message: "M".into(), kind: "-1".into(), remove: false });
        let c = parse(&["read", "c"]).unwrap();
        assert_eq!(c.command, Command::Read { chat: "c".into(), up_to: None });
        assert!(parse(&["lookup"]).is_err());
        let c = parse(&["watch", "--chat", "group:X", "--json"]).unwrap();
        assert!(c.json && c.command == Command::Watch { chat: Some("group:X".into()) });
        let c = parse(&["hwconfig", "import", "f.plist", "--force"]).unwrap();
        assert_eq!(c.command, Command::Hwconfig { command: HwconfigCommand::Import { file: "f.plist".into(), force: true } });
        assert!(parse(&["send-file", "bob@x.com", "/tmp/a.png", "--mime", "image/png"]).is_ok());
        assert!(parse(&["nonsense"]).is_err());
        let c = parse(&["import-mac", "mac.local", "--since", "2026-10-01", "--dry-run"]).unwrap();
        assert_eq!(
            c.command,
            Command::ImportMac {
                host: "mac.local".into(),
                days: 10,
                since: Some("2026-10-01".into()),
                max_attachment_mb: 50,
                max_total_mb: 2048,
                imessage_only: false,
                dry_run: true,
                backgrounds: false,
                chats: vec![],
                create: false,
            }
        );
        assert!(parse(&["import-mac", "mac.local", "--since", "2026-10-01", "--days", "3"]).is_err());
        let c = parse(&["import-mac", "mac.local", "--backgrounds", "--chat", "+12055550100", "--chat", "group:X", "--create", "--dry-run"]).unwrap();
        assert!(matches!(c.command, Command::ImportMac { backgrounds: true, create: true, dry_run: true, ref chats, .. } if chats == &["+12055550100", "group:X"]));
        assert!(parse(&["import-mac", "mac.local", "--chat", "x"]).is_err(), "--chat needs --backgrounds");
        assert!(parse(&["import-mac", "mac.local", "--create"]).is_err());
        assert!(parse(&["import-mac", "mac.local", "--backgrounds", "--since", "2026-10-01"]).is_err());
        assert!(parse(&["import-mac", "mac.local", "--backgrounds", "--days", "3"]).is_err());
        let c = parse(&["store", "purge-invalid-chats"]).unwrap();
        assert_eq!(c.command, Command::Store { command: StoreCommand::PurgeInvalidChats { yes: false } });
        let c = parse(&["store", "purge-invalid-chats", "--yes"]).unwrap();
        assert_eq!(c.command, Command::Store { command: StoreCommand::PurgeInvalidChats { yes: true } });
    }
}
