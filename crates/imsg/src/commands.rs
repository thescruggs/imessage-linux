//! The daemon-backed subcommands.

use std::io::{BufRead, IsTerminal, Read, Write};

use chrono::Local;
use futures_util::StreamExt;
use imessage_common::model::{Chat, Message};
use zeroize::Zeroizing;

use crate::client::{self, CliError, Client, LoginOutcome, exit};
use crate::format::{self, Style};
use crate::resolve;
use crate::{Command, Ctx, SmsCommand, StoreCommand};

/// Wrong two-factor codes tolerated before giving up.
const MAX_BAD_CODES: u32 = 3;
/// Rounds of "Apple wants another code" before giving up.
const MAX_CODE_ROUNDS: u32 = 6;

pub async fn run(cmd: Command, ctx: &Ctx) -> anyhow::Result<u8> {
    let client = Client::connect(&ctx.bus).await?;
    match cmd {
        Command::Login { apple_id, password_stdin, code_stdin, code_only } => login(&client, ctx, apple_id, password_stdin, code_stdin, code_only).await,
        Command::Logout => {
            client.logout().await?;
            confirm(ctx, "logged out (message history kept)");
            Ok(exit::OK)
        }
        Command::Status => status(&client, ctx).await,
        Command::Register => register(&client, ctx).await,
        Command::Chats { limit, offset } => {
            let chats = client.list_chats(limit, offset).await?;
            if ctx.json {
                println!("{}", chats.raw);
            } else {
                print!("{}", format::chats_table(&chats.value, ctx.style, &Local::now()));
            }
            Ok(exit::OK)
        }
        Command::Chat { chat } => {
            let id = resolve::resolve_chat(&client, &chat).await?;
            let c = client.get_chat(&id).await?.ok_or_else(|| CliError::new(exit::NOT_FOUND, format!("no such chat: {id}")))?;
            if ctx.json {
                println!("{}", c.raw);
            } else {
                print!("{}", format::chat_details(&c.value, ctx.style, &Local::now()));
            }
            Ok(exit::OK)
        }
        Command::ChatBackground { chat, import } => {
            let path = std::fs::canonicalize(&import).map_err(|e| CliError::usage(format!("{}: {e}", import.display())))?;
            if !path.is_file() {
                return Err(CliError::usage(format!("{}: not a regular file", path.display())).into());
            }
            let path_s = path.to_str().ok_or_else(|| CliError::usage(format!("{}: path is not UTF-8", path.display())))?;
            let id = resolve::resolve_chat(&client, &chat).await?;
            let c = client.import_chat_background(&id, path_s).await?;
            if ctx.json {
                println!("{}", c.raw);
            } else {
                let p = c.value.background.as_ref().and_then(|b| b.path.clone()).unwrap_or_default();
                confirm(ctx, &format!("background of {} set locally ({p})", c.value.id));
            }
            Ok(exit::OK)
        }
        Command::Messages { chat, limit, before, reverse } => messages(&client, ctx, &chat, limit, before.as_deref().unwrap_or(""), reverse).await,
        Command::Send { to, text, reply_to, sms, imessage } => {
            let text = text_arg(&text)?;
            let target = resolve::resolve_target(&client, &to).await?;
            let reply_to = reply_to.as_deref().unwrap_or("");
            let sent = match service_arg(sms, imessage) {
                Some(s) => client.send_text_as(&target, &text, reply_to, s).await?,
                None => client.send_text(&target, &text, reply_to).await?,
            };
            if ctx.json {
                println!("{}", sent.raw);
            } else {
                confirm(ctx, &format!("sent {} to {}{}", sent.value.id, sent.value.chat_id, as_text_message(&sent.value)));
            }
            Ok(exit::OK)
        }
        Command::Sms { command } => sms(&client, ctx, command).await,
        Command::SendFile { to, file, mime, sms, imessage } => {
            let path = std::fs::canonicalize(&file).map_err(|e| CliError::usage(format!("{}: {e}", file.display())))?;
            if !path.is_file() {
                return Err(CliError::usage(format!("{}: not a regular file", path.display())).into());
            }
            let mime = mime.unwrap_or_else(|| resolve::guess_mime(&path).to_string());
            let target = resolve::resolve_target(&client, &to).await?;
            let path_s = path.to_str().ok_or_else(|| CliError::usage(format!("{}: path is not UTF-8", path.display())))?;
            let sent = match service_arg(sms, imessage) {
                Some(s) => client.send_file_as(&target, path_s, &mime, s).await?,
                None => client.send_file(&target, path_s, &mime).await?,
            };
            if ctx.json {
                println!("{}", sent.raw);
            } else {
                confirm(ctx, &format!("sent {} ({}) to {}{}", path.display(), sent.value.id, sent.value.chat_id, as_text_message(&sent.value)));
            }
            Ok(exit::OK)
        }
        Command::React { chat, message, kind, remove } => {
            let kind = resolve::reaction_kind(&kind)?;
            let chat = resolve::resolve_chat(&client, &chat).await?;
            client.react(&chat, &message, &kind, remove).await?;
            confirm(ctx, if remove { "reaction removed" } else { "reacted" });
            Ok(exit::OK)
        }
        Command::Edit { chat, message, text } => {
            let text = text_arg(&text)?;
            let chat = resolve::resolve_chat(&client, &chat).await?;
            client.edit(&chat, &message, &text).await?;
            confirm(ctx, "edited");
            Ok(exit::OK)
        }
        Command::Unsend { chat, message } => {
            let chat = resolve::resolve_chat(&client, &chat).await?;
            client.unsend(&chat, &message).await?;
            confirm(ctx, "unsent");
            Ok(exit::OK)
        }
        Command::Typing { chat, state } => {
            let chat = resolve::resolve_chat_or_handle(&client, &chat).await?;
            client.set_typing(&chat, state == "on").await?;
            confirm(ctx, &format!("typing {state}"));
            Ok(exit::OK)
        }
        Command::Read { chat, up_to } => {
            let chat = resolve::resolve_chat(&client, &chat).await?;
            client.mark_read(&chat, up_to.as_deref().unwrap_or("")).await?;
            confirm(ctx, "marked read");
            Ok(exit::OK)
        }
        Command::Lookup { handles } => {
            let r = client.lookup_handles(&handles).await?;
            if ctx.json {
                println!("{}", r.raw);
            } else {
                print!("{}", format::lookup_table(&r.value, ctx.style));
            }
            Ok(exit::OK)
        }
        Command::Profile { handle } => {
            let h = imessage_common::model::Handle::parse(&handle).ok_or_else(|| CliError::usage(format!("{handle:?} is not an email address or phone number")))?;
            let p = client.get_handle_profile(&h.uri).await?;
            if ctx.json {
                println!("{}", p.raw);
            } else {
                print!("{}", format::profile_details(&h.uri, p.value.as_ref(), ctx.style));
            }
            Ok(exit::OK)
        }
        Command::Search { query, limit } => {
            let r = client.search(&query, limit).await?;
            if ctx.json {
                println!("{}", r.raw);
            } else {
                print_messages(&r.value, ctx.style, true);
                if r.value.is_empty() && !ctx.quiet {
                    eprintln!("no matches");
                }
            }
            Ok(exit::OK)
        }
        Command::Download { attachment } => {
            let path = client.download_attachment(&attachment).await?;
            if ctx.json {
                println!("{}", serde_json::to_string(&path)?);
            } else {
                println!("{path}");
            }
            Ok(exit::OK)
        }
        Command::Watch { chat } => watch(&client, ctx, chat.as_deref()).await,
        Command::Version => {
            let daemon = client.version().await;
            if ctx.json {
                let d = daemon.as_ref().ok();
                println!("{}", serde_json::json!({ "imsg": env!("CARGO_PKG_VERSION"), "imessaged": d }));
            } else {
                println!("imsg {}", env!("CARGO_PKG_VERSION"));
                match daemon {
                    Ok(v) => println!("imessaged {v}"),
                    Err(e) => println!("imessaged: {e}"),
                }
            }
            Ok(exit::OK)
        }
        Command::Store { command: StoreCommand::PurgeInvalidChats { yes } } => {
            let r = client.purge_invalid_chats(!yes).await?;
            if ctx.json {
                println!("{}", r.raw);
            } else {
                print!("{}", purge_summary(&r.value));
            }
            Ok(exit::OK)
        }
        Command::Doctor | Command::Hwconfig { .. } | Command::ImportMac { .. } => unreachable!("handled in main"),
    }
}

/// Human form of a `PurgeInvalidChats` summary.
fn purge_summary(v: &serde_json::Value) -> String {
    let n = |k: &str| v[k].as_u64().unwrap_or(0);
    let dry = v["dry_run"].as_bool().unwrap_or(true);
    let chats = v["chats"].as_array().cloned().unwrap_or_default();
    if chats.is_empty() {
        return "no chats with invalid ids\n".into();
    }
    let mut out = String::new();
    for c in &chats {
        let m = |k: &str| c[k].as_u64().unwrap_or(0);
        out.push_str(&format!("  {}  {} message(s), {} reaction(s), {} attachment(s)\n", c["id"].as_str().unwrap_or("?"), m("messages"), m("reactions"), m("attachments")));
    }
    out.push_str(&format!(
        "{} {} chat(s), {} message(s), {} reaction(s), {} attachment(s), {} file(s), {} handle(s)\n",
        if dry { "would remove" } else { "removed" },
        chats.len(),
        n("messages"),
        n("reactions"),
        n("attachments"),
        n("files"),
        n("handles")
    ));
    if dry {
        out.push_str("dry run: nothing was deleted; run again with --yes to delete\n");
    }
    out
}

/// A confirmation line on stdout (not in `--json` / `--quiet` mode).
fn confirm(ctx: &Ctx, msg: &str) {
    if !ctx.json && !ctx.quiet {
        println!("{msg}");
    }
}

/// Text from arguments; a single `-` reads all of stdin (one trailing
/// newline removed).
fn text_arg(words: &[String]) -> client::Result<String> {
    let text = if words.len() == 1 && words[0] == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).map_err(|e| CliError::usage(format!("reading stdin: {e}")))?;
        strip_newline(s)
    } else {
        words.join(" ")
    };
    if text.trim().is_empty() {
        return Err(CliError::usage("empty message"));
    }
    Ok(text)
}

fn strip_newline(mut s: String) -> String {
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
    s
}

fn print_messages(ms: &[Message], st: Style, show_chat: bool) {
    let now = Local::now();
    for m in ms {
        print!("{}", format::message(m, st, &now, show_chat));
    }
}

/// `--sms` / `--imessage` -> the SendTextAs service; `None` = the daemon picks.
fn service_arg(sms: bool, imessage: bool) -> Option<&'static str> {
    if sms {
        Some("sms")
    } else if imessage {
        Some("imessage")
    } else {
        None
    }
}

fn as_text_message(s: &client::SentMessage) -> &'static str {
    if s.service == imessage_common::model::Service::Sms { " as a text message (via your iPhone)" } else { "" }
}

async fn sms(client: &Client, ctx: &Ctx, command: SmsCommand) -> anyhow::Result<u8> {
    let s = match command {
        SmsCommand::Enable => client.enable_sms_forwarding(true).await?,
        SmsCommand::Disable => client.enable_sms_forwarding(false).await?,
        SmsCommand::Status => client.sms_forwarding().await?,
    };
    if ctx.json {
        println!("{}", s.raw);
        return Ok(exit::OK);
    }
    print!("{}", format::sms_table(&s.value, ctx.style));
    if command == SmsCommand::Enable && s.value.state == imessage_common::model::SmsState::Requested && !ctx.quiet {
        eprintln!(
            "{}",
            ctx.err_style.dim(
                "now, on your iPhone: Settings > Messages > Text Message Forwarding, and turn this device on.\n\
                 `imsg sms status` says `active` once the iPhone confirms or relays its first text."
            )
        );
    }
    Ok(exit::OK)
}

async fn register(client: &Client, ctx: &Ctx) -> anyhow::Result<u8> {
    let r = client.reregister().await?;
    if ctx.json {
        println!("{}", r.raw);
        return Ok(exit::OK);
    }
    status(client, ctx).await
}

async fn status(client: &Client, ctx: &Ctx) -> anyhow::Result<u8> {
    let s = client.status().await?;
    if ctx.json {
        println!("{}", s.raw);
    } else {
        print!("{}", format::status_table(&s.value, ctx.style));
    }
    Ok(exit::OK)
}

async fn messages(client: &Client, ctx: &Ctx, chat: &str, limit: u32, before: &str, reverse: bool) -> anyhow::Result<u8> {
    let chat_id = resolve::resolve_chat(client, chat).await?;
    let page = client.list_messages(&chat_id, before, limit).await?;
    if ctx.json {
        println!("{}", page.raw);
        return Ok(exit::OK);
    }
    let mut ms = page.value.messages;
    if reverse {
        ms.reverse();
    }
    if ms.is_empty() && !ctx.quiet {
        eprintln!("no messages");
    }
    print_messages(&ms, ctx.style, false);
    if let Some(c) = page.value.next_cursor
        && !ctx.quiet
    {
        eprintln!("{}", ctx.err_style.dim(&format!("older: imsg messages {chat_id} --before {c}")));
    }
    Ok(exit::OK)
}

// ---------------------------------------------------------------------------
// login
// ---------------------------------------------------------------------------

/// Where secrets come from: piped stdin lines, or the terminal.
struct Input {
    stdin: Option<std::io::StdinLock<'static>>,
}

impl Input {
    fn new(use_stdin: bool) -> Input {
        Input { stdin: use_stdin.then(|| std::io::stdin().lock()) }
    }

    /// Next stdin line (trailing newline removed), `None` at EOF.
    fn stdin_line(&mut self) -> client::Result<Option<String>> {
        let lock = self.stdin.get_or_insert_with(|| std::io::stdin().lock());
        let mut s = String::new();
        match lock.read_line(&mut s) {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(strip_newline(s))),
            Err(e) => Err(CliError::usage(format!("reading stdin: {e}"))),
        }
    }
}

/// Visible prompt + line from the controlling terminal.
fn tty_line(prompt: &str) -> client::Result<String> {
    let no_tty = || CliError::usage("no terminal to prompt on; use --apple-id / --password-stdin / --code-stdin");
    let mut tty = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty").map_err(|_| no_tty())?;
    tty.write_all(prompt.as_bytes()).map_err(|_| no_tty())?;
    tty.flush().map_err(|_| no_tty())?;
    let mut line = String::new();
    std::io::BufReader::new(tty).read_line(&mut line).map_err(|_| no_tty())?;
    Ok(line.trim().to_string())
}

fn read_code(input: &mut Input, code_stdin: bool) -> client::Result<String> {
    let code = if code_stdin {
        input.stdin_line()?.ok_or_else(|| CliError::new(exit::TWO_FACTOR, "two-factor code required, but stdin has no more lines"))?
    } else {
        tty_line("Two-factor code: ").map_err(|_| CliError::new(exit::TWO_FACTOR, "two-factor code required; pass it with --code-stdin"))?
    };
    let code: String = code.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    if code.is_empty() {
        return Err(CliError::new(exit::TWO_FACTOR, "empty two-factor code"));
    }
    Ok(code)
}

async fn two_factor_loop(client: &Client, ctx: &Ctx, input: &mut Input, code_stdin: bool, mut outcome: LoginOutcome) -> client::Result<()> {
    let (mut bad, mut rounds) = (0, 0);
    // Announce the code only for a fresh TwoFactorRequired, not when
    // re-prompting after a wrong code (no new code was sent).
    let mut fresh = true;
    while outcome == LoginOutcome::TwoFactorRequired {
        rounds += 1;
        if rounds > MAX_CODE_ROUNDS {
            return Err(CliError::new(exit::TWO_FACTOR, "Apple keeps asking for another two-factor code; giving up"));
        }
        if fresh && !ctx.quiet && !code_stdin {
            eprintln!("A verification code was sent to your trusted devices.");
        }
        fresh = true;
        let code = read_code(input, code_stdin)?;
        match client.submit_two_factor(&code).await {
            Ok(o) => outcome = o,
            Err(e) if e.is_daemon("BadTwoFactorCode") => {
                fresh = false;
                bad += 1;
                if bad >= MAX_BAD_CODES {
                    return Err(CliError { message: format!("wrong two-factor code ({bad} attempts); run `imsg login` again"), ..e });
                }
                eprintln!("imsg: wrong code, try again ({bad}/{MAX_BAD_CODES})");
                rounds -= 1;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

async fn login(client: &Client, ctx: &Ctx, apple_id: Option<String>, password_stdin: bool, code_stdin: bool, code_only: bool) -> anyhow::Result<u8> {
    let mut input = Input::new(password_stdin || code_stdin);
    if code_only {
        two_factor_loop(client, ctx, &mut input, code_stdin, LoginOutcome::TwoFactorRequired).await?;
    } else {
        let apple_id = match apple_id {
            Some(a) => a,
            None if password_stdin => return Err(CliError::usage("--password-stdin needs --apple-id").into()),
            None => tty_line("Apple ID: ")?,
        };
        if apple_id.trim().is_empty() {
            return Err(CliError::usage("empty Apple ID").into());
        }
        // Zeroized on drop. Best effort: the stdin path's BufReader buffer
        // and any intermediate copies (D-Bus message buffers) are not wiped.
        let password: Zeroizing<String> = Zeroizing::new(if password_stdin {
            if std::io::stdin().is_terminal() {
                return Err(CliError::usage("--password-stdin: stdin is a terminal; pipe the password or drop the flag").into());
            }
            input.stdin_line()?.ok_or_else(|| CliError::usage("no password on stdin"))?
        } else {
            rpassword::prompt_password(format!("Password for {apple_id}: "))
                .map_err(|e| CliError::usage(format!("cannot read password from the terminal ({e}); use --password-stdin")))?
        });
        if password.is_empty() {
            return Err(CliError::usage("empty password").into());
        }
        let outcome = client.login(apple_id.trim(), &password).await?;
        drop(password);
        two_factor_loop(client, ctx, &mut input, code_stdin, outcome).await?;
    }
    let s = client.status().await?;
    if ctx.json {
        println!("{}", s.raw);
    } else if !ctx.quiet {
        let handles: Vec<String> = s.value.registration.handles.iter().map(format::short_handle).collect();
        println!(
            "logged in; connection {}{}",
            format::connection_colored(&s.value.connection, ctx.style),
            if handles.is_empty() { String::new() } else { format!("; iMessage handles: {}", handles.join(", ")) }
        );
    }
    Ok(exit::OK)
}

// ---------------------------------------------------------------------------
// watch
// ---------------------------------------------------------------------------

/// `--chat` filter: an exact chat id. Handles are used as-is (a 1:1 chat
/// may not exist yet); group ids and names go through `resolve_chat` like
/// every other command (case-insensitive / prefix matching).
async fn watch_filter(client: &Client, arg: &str) -> client::Result<String> {
    resolve::resolve_chat_or_handle(client, arg).await
}

/// One `--json` watch line: `{"signal":…, <fields>}`, daemon JSON embedded
/// verbatim.
pub fn json_event(signal: &str, fields: &[(&str, JsonField)]) -> String {
    let mut s = format!("{{\"signal\":{}", serde_json::Value::from(signal));
    for (k, v) in fields {
        let v = match v {
            JsonField::Str(x) => serde_json::Value::from(x.as_str()).to_string(),
            JsonField::Bool(b) => b.to_string(),
            JsonField::Raw(r) => r.clone(),
        };
        s.push_str(&format!(",{}:{v}", serde_json::Value::from(*k)));
    }
    s.push('}');
    s
}

pub enum JsonField {
    Str(String),
    Bool(bool),
    /// Already JSON (the daemon's payload).
    Raw(String),
}

fn chat_line(c: &Chat, st: Style) -> String {
    let unread = if c.unread_count > 0 { format!(", {} unread", c.unread_count) } else { String::new() };
    format!("{} {} ({}){unread}", st.dim("chat"), format::chat_name(c), c.id)
}

async fn watch(client: &Client, ctx: &Ctx, chat: Option<&str>) -> anyhow::Result<u8> {
    let p = &client.proxy;
    // Subscribe first, then make a call: nothing between the call and the
    // first event is missed, and the call starts the daemon if activatable.
    let mut received = p.receive_message_received().await.map_err(client::map_zbus)?;
    let mut updated = p.receive_message_updated().await.map_err(client::map_zbus)?;
    let mut chats = p.receive_chat_updated().await.map_err(client::map_zbus)?;
    let mut typing = p.receive_typing().await.map_err(client::map_zbus)?;
    let mut conn = p.receive_connection_state_signal().await.map_err(client::map_zbus)?;
    let mut two_factor = p.receive_two_factor_required().await.map_err(client::map_zbus)?;
    let mut bg = p.receive_background_error().await.map_err(client::map_zbus)?;
    let mut profiles = p.receive_handle_profile_updated().await.map_err(client::map_zbus)?;
    let mut sms = p.receive_sms_forwarding_changed().await.map_err(client::map_zbus)?;
    let state = client.connection_state().await?;
    let filter = match chat {
        Some(c) => Some(watch_filter(client, c).await?),
        None => None,
    };
    if !ctx.json && !ctx.quiet {
        eprintln!("{}", ctx.err_style.dim(&format!("watching (connection {state}); Ctrl-C to stop")));
    }
    let wanted = |id: &str| filter.as_deref().is_none_or(|f| f == id);
    let st = ctx.style;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        let line: Option<String> = tokio::select! {
            _ = &mut ctrl_c => return Ok(exit::OK),
            Some(s) = received.next() => {
                let a = s.args().map_err(client::map_zbus)?;
                if !wanted(&a.chat_id) { None }
                else if ctx.json {
                    Some(json_event("MessageReceived", &[("chat_id", JsonField::Str(a.chat_id.clone())), ("message", JsonField::Raw(a.message_json.clone()))]))
                } else {
                    let m: Message = client::parse("message", a.message_json.clone())?.value;
                    Some(format::message(&m, st, &Local::now(), filter.is_none()).trim_end().to_string())
                }
            }
            Some(s) = updated.next() => {
                let a = s.args().map_err(client::map_zbus)?;
                if !wanted(&a.chat_id) { None }
                else if ctx.json {
                    Some(json_event("MessageUpdated", &[("chat_id", JsonField::Str(a.chat_id.clone())), ("message", JsonField::Raw(a.message_json.clone()))]))
                } else {
                    let m: Message = client::parse("message", a.message_json.clone())?.value;
                    Some(format!("{} {}", st.yellow("updated"), format::message(&m, st, &Local::now(), filter.is_none()).trim_end()))
                }
            }
            Some(s) = chats.next() => {
                let a = s.args().map_err(client::map_zbus)?;
                let c: Chat = client::parse("chat", a.chat_json.clone())?.value;
                if !wanted(&c.id.0) { None }
                else if ctx.json { Some(json_event("ChatUpdated", &[("chat", JsonField::Raw(a.chat_json.clone()))])) }
                else { Some(chat_line(&c, st)) }
            }
            Some(s) = typing.next() => {
                let a = s.args().map_err(client::map_zbus)?;
                if !wanted(&a.chat_id) { None }
                else if ctx.json {
                    Some(json_event("Typing", &[("chat_id", JsonField::Str(a.chat_id.clone())), ("handle", JsonField::Str(a.handle.clone())), ("typing", JsonField::Bool(a.typing))]))
                } else {
                    let h = imessage_common::model::Handle::new(&a.handle);
                    Some(st.dim(&format!("{} {} typing in {}", h.address(), if a.typing { "is" } else { "stopped" }, a.chat_id)))
                }
            }
            Some(s) = conn.next() => {
                let a = s.args().map_err(client::map_zbus)?;
                if ctx.json { Some(json_event("ConnectionStateChanged", &[("state", JsonField::Str(a.state.clone()))])) }
                else { Some(format!("{} {}", st.dim("connection"), a.state)) }
            }
            Some(_) = two_factor.next() => {
                if ctx.json { Some(json_event("TwoFactorRequired", &[])) }
                else { Some(st.yellow("two-factor code required: run `imsg login --code-only`")) }
            }
            Some(s) = bg.next() => {
                let a = s.args().map_err(client::map_zbus)?;
                if ctx.json { Some(json_event("BackgroundError", &[("message", JsonField::Str(a.message.clone()))])) }
                else { Some(format!("{} {}", st.red("daemon error:"), a.message)) }
            }
            Some(s) = profiles.next() => {
                let a = s.args().map_err(client::map_zbus)?;
                if filter.is_some() { None }
                else if ctx.json {
                    Some(json_event("HandleProfileUpdated", &[("handle", JsonField::Str(a.handle.clone())), ("profile", JsonField::Raw(a.profile_json.clone()))]))
                } else {
                    let p: Option<imessage_common::model::HandleProfile> = client::parse("profile", a.profile_json.clone())?.value;
                    let h = imessage_common::model::Handle::new(&a.handle);
                    Some(st.dim(&match p.as_ref().and_then(|p| p.display_name()) {
                        Some(n) => format!("{} shares their name: {n}", h.address()),
                        None if p.is_some() => format!("{} shares a photo", h.address()),
                        None => format!("{} stopped sharing their name and photo", h.address()),
                    }))
                }
            }
            Some(s) = sms.next() => {
                let a = s.args().map_err(client::map_zbus)?;
                if filter.is_some() { None }
                else if ctx.json { Some(json_event("SmsForwardingChanged", &[("state", JsonField::Raw(a.state_json.clone()))])) }
                else {
                    let f: imessage_common::model::SmsForwarding = client::parse("sms forwarding", a.state_json.clone())?.value;
                    Some(format!("{} {}", st.dim("sms forwarding"), format::sms_state_colored(&f, st)))
                }
            }
            else => return Err(CliError::new(exit::NO_DAEMON, "signal streams ended (bus connection lost)").into()),
        };
        if let Some(l) = line {
            let mut out = std::io::stdout().lock();
            writeln!(out, "{l}")?;
            out.flush()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_events_embed_payload_verbatim() {
        let raw = r#"{"z":1,"a":"x"}"#;
        assert_eq!(
            json_event("MessageReceived", &[("chat_id", JsonField::Str("mailto:a\"b".into())), ("message", JsonField::Raw(raw.into()))]),
            r#"{"signal":"MessageReceived","chat_id":"mailto:a\"b","message":{"z":1,"a":"x"}}"#
        );
        assert_eq!(json_event("TwoFactorRequired", &[]), r#"{"signal":"TwoFactorRequired"}"#);
        assert_eq!(json_event("Typing", &[("typing", JsonField::Bool(true))]), r#"{"signal":"Typing","typing":true}"#);
    }

    #[test]
    fn purge_summary_text() {
        assert_eq!(purge_summary(&serde_json::json!({"dry_run": true, "chats": []})), "no chats with invalid ids\n");
        let v = serde_json::json!({"dry_run": true, "chats": [{"id": "46339(smsft)", "messages": 2, "reactions": 0, "attachments": 1}], "messages": 2, "reactions": 0, "attachments": 1, "files": 1, "handles": 1});
        let t = purge_summary(&v);
        assert!(t.contains("46339(smsft)  2 message(s)") && t.contains("would remove 1 chat(s)") && t.contains("--yes"), "{t}");
        let t = purge_summary(&serde_json::json!({"dry_run": false, "chats": [{"id": "x"}]}));
        assert!(t.contains("removed 1 chat(s)") && !t.contains("--yes"), "{t}");
    }

    #[test]
    fn newline_stripping() {
        assert_eq!(strip_newline("a\n".into()), "a");
        assert_eq!(strip_newline("a\r\n".into()), "a");
        assert_eq!(strip_newline("a\n\n".into()), "a\n");
        assert_eq!(strip_newline("a".into()), "a");
    }

    #[test]
    fn text_args_join() {
        assert_eq!(text_arg(&["hello".into(), "world".into()]).unwrap(), "hello world");
        assert_eq!(text_arg(&["  ".into()]).unwrap_err().code, exit::USAGE);
    }
}
