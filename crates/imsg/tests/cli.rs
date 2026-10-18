//! The `imsg` binary end to end: `imessaged` runs in-process with
//! `MockEngine` on a private `dbus-daemon`, and each test runs the built
//! `imsg` against it with `--bus-address`. Prints `SKIP:` and passes when
//! `dbus-daemon` is not installed.

mod common;

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use imessage_common::model::{Chat, ChatId, DeliveryState, Handle, Message, MessageId, MessagePart};
use imessaged::config::DaemonConfig;
use imessaged::daemon::{Daemon, RunningDaemon};
use imessaged::dbus::{BUS_NAME, BusChoice};
use imessaged::engine::mock::{MockCall, MockEngine};
use imessaged::engine::{ChatTarget, Engine, EngineEvent};
use imessaged::keyring::StateKey;
use imessaged::store::{Store, StoreHandle};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const IMSG: &str = env!("CARGO_BIN_EXE_imsg");
const BOB: &str = "mailto:bob@example.com";

struct Harness {
    bus: common::PrivateBus,
    dir: tempfile::TempDir,
    mock: Arc<MockEngine>,
    running: Option<RunningDaemon>,
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn json(&self) -> Value {
        serde_json::from_str(self.stdout.trim()).unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {:?} / stderr {:?}", self.stdout, self.stderr))
    }
}

impl Harness {
    async fn start() -> Option<Harness> {
        let bus = common::PrivateBus::start()?;
        let dir = tempfile::tempdir().unwrap();
        let mock = Arc::new(MockEngine::new());
        let store = StoreHandle::new(Store::open(dir.path(), StateKey::from_bytes([7; 32])).unwrap());
        let running = Daemon::with_engine(DaemonConfig::new(dir.path().into()), mock.clone())
            .with_store(store)
            .with_dbus(BusChoice::Address(bus.address.clone()))
            .start()
            .await
            .unwrap();
        let conn = zbus::connection::Builder::address(bus.address.as_str()).unwrap().build().await.unwrap();
        common::wait_for_name(&conn, BUS_NAME, 5).await;
        Some(Harness { bus, dir, mock, running: Some(running) })
    }

    async fn logged_in() -> Option<Harness> {
        let h = Harness::start().await?;
        h.mock.login("me@example.com", "pw").await.unwrap();
        Some(h)
    }

    fn command(&self, args: &[&str]) -> tokio::process::Command {
        imsg_command(&self.bus.address, self.dir.path(), args)
    }

    async fn run(&self, args: &[&str]) -> Out {
        self.run_stdin(args, None).await
    }

    async fn run_stdin(&self, args: &[&str], stdin: Option<&str>) -> Out {
        run(self.command(args), stdin).await
    }

    async fn shutdown(mut self) {
        self.running.take().unwrap().shutdown().await.unwrap();
    }
}

fn imsg_command(bus: &str, state: &std::path::Path, args: &[&str]) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(IMSG);
    c.arg("--bus-address")
        .arg(bus)
        .args(args)
        .env("NO_COLOR", "1")
        .env("IMESSAGED_STATE_DIR", state)
        // doctor: no network in tests (port 1 refuses at once -> WARN)
        .env("IMESSAGED_ANISETTE_URL", "http://127.0.0.1:1")
        .env_remove("IMSG_BUS_ADDRESS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    c
}

async fn run(mut cmd: tokio::process::Command, stdin: Option<&str>) -> Out {
    let mut child = cmd.spawn().expect("spawn imsg");
    let mut pipe = child.stdin.take().unwrap();
    if let Some(s) = stdin {
        pipe.write_all(s.as_bytes()).await.unwrap();
    }
    drop(pipe);
    let out = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await.expect("imsg timed out").unwrap();
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn incoming(id: &str, at: u64, text: &str) -> EngineEvent {
    EngineEvent::MessageReceived {
        message: Message {
            id: MessageId::new(id),
            chat_id: ChatId::direct(BOB),
            sender: Handle::new(BOB),
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
            service: Default::default(),
        },
        chat: Chat { id: ChatId::direct(BOB), participants: vec![Handle::new(BOB)], display_name: None, is_group: false, last_message_at: Some(at), unread_count: 0, background: None, icon_path: None, pinned: false, pin_order: None },
        attachments: vec![],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_json_and_human() {
    let Some(h) = Harness::start().await else { return };
    let o = h.run(&["status", "--json"]).await;
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v = o.json();
    assert_eq!(v["connection"]["state"], "needs_login");
    assert_eq!(v["registered"], false);
    let o = h.run(&["status"]).await;
    assert_eq!(o.code, 0);
    assert!(o.stdout.contains("connection") && o.stdout.contains("needs_login"), "{}", o.stdout);
    assert!(!o.stdout.contains('\x1b'), "no colour when piped");
    let o = h.run(&["version", "--json"]).await;
    assert_eq!(o.json()["imessaged"], env!("CARGO_PKG_VERSION"));
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn errors_map_to_exit_codes() {
    let Some(h) = Harness::start().await else { return };
    // not logged in -> 3
    let o = h.run(&["send", "bob@example.com", "hi"]).await;
    assert_eq!(o.code, 3, "{}", o.stderr);
    assert!(o.stderr.contains("not logged in"), "{}", o.stderr);
    // unknown chat -> 6
    let o = h.run(&["messages", "nobody@example.com"]).await;
    assert_eq!(o.code, 6, "{}", o.stderr);
    // invalid reaction kind -> 2 (before any call)
    let o = h.run(&["react", "bob@example.com", "X", "wow"]).await;
    assert_eq!(o.code, 2, "{}", o.stderr);
    // clap usage error -> 2
    let o = h.run(&["typing", "bob@example.com", "maybe"]).await;
    assert_eq!(o.code, 2);
    // daemon gone -> 5
    let addr = h.bus.address.clone();
    let dir = h.dir.path().to_path_buf();
    let bus_kept = {
        let Harness { bus, mut running, .. } = h;
        running.take().unwrap().shutdown().await.unwrap();
        bus
    };
    let o = run(imsg_command(&addr, &dir, &["status"]), None).await;
    assert_eq!(o.code, 5, "{}", o.stderr);
    assert!(o.stderr.contains("not running"), "{}", o.stderr);
    drop(bus_kept);
    // no bus at all -> 5
    let o = run(imsg_command("unix:path=/nonexistent/imsg-test-bus", &dir, &["status"]), None).await;
    assert_eq!(o.code, 5, "{}", o.stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_then_messages() {
    let Some(h) = Harness::logged_in().await else { return };
    let o = h.run(&["send", "Bob@Example.com", "hello", "from", "cli"]).await;
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.starts_with("sent "), "{}", o.stdout);
    assert!(h.mock.calls().iter().any(|c| matches!(c, MockCall::SendText { text, chat: ChatTarget::Participants(p), .. }
        if text == "hello from cli" && p == &vec!["Bob@Example.com".to_string()])));

    // stdin text + --json + reply
    let first_id = {
        let o = h.run(&["messages", "bob@example.com", "--json"]).await;
        assert_eq!(o.code, 0, "{}", o.stderr);
        let v = o.json();
        assert_eq!(v["messages"][0]["text"], "hello from cli");
        assert_eq!(v["messages"][0]["is_from_me"], true);
        v["messages"][0]["id"].as_str().unwrap().to_string()
    };
    let o = h.run_stdin(&["send", BOB, "-", "--json", "--reply-to", &first_id], Some("line one\nline two\n")).await;
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.json()["chat_id"], BOB);
    assert!(h.mock.calls().iter().any(|c| matches!(c, MockCall::SendText { text, reply_to: Some(r), chat: ChatTarget::Participants(p) }
        if text == "line one\nline two" && r.0 == first_id && p == &vec![BOB.to_string()])));

    // human transcript
    let o = h.run(&["messages", "bob@example.com"]).await;
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("me: hello from cli"), "{}", o.stdout);
    assert!(o.stdout.contains(&format!("id {first_id}")), "{}", o.stdout);
    assert!(o.stdout.contains("line two"), "{}", o.stdout);

    // group send: several handles -> new group chat id
    let o = h.run(&["send", "a@example.com,b@example.com", "hi all", "--json"]).await;
    assert_eq!(o.code, 0, "{}", o.stderr);
    let group = o.json()["chat_id"].as_str().unwrap().to_string();
    assert!(group.starts_with("group:"), "{group}");

    // chats, by name prefix of the id
    let o = h.run(&["chats", "--json"]).await;
    let ids: Vec<String> = o.json().as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap().to_string()).collect();
    assert!(ids.contains(&BOB.to_string()) && ids.contains(&group), "{ids:?}");
    let o = h.run(&["chats"]).await;
    assert!(o.stdout.starts_with("ID") && o.stdout.contains("a@example.com, b@example.com"), "{}", o.stdout);
    let o = h.run(&["messages", &group[..12], "--json"]).await;
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.json()["messages"][0]["text"], "hi all");

    // react / edit / unsend / typing / read / search / lookup
    let o = h.run(&["react", "bob@example.com", &first_id, "love"]).await;
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = h.run(&["edit", "bob@example.com", &first_id, "edited", "text"]).await;
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = h.run(&["messages", "bob@example.com"]).await;
    assert!(o.stdout.contains("me: edited text") && o.stdout.contains("❤️1") && o.stdout.contains("[edited]"), "{}", o.stdout);
    let o = h.run(&["typing", "bob@example.com", "on", "-q"]).await;
    assert_eq!((o.code, o.stdout.as_str()), (0, ""), "{}", o.stderr);
    assert!(h.mock.calls().iter().any(|c| matches!(c, MockCall::SetTyping { typing: true, .. })));
    let o = h.run(&["read", "bob@example.com"]).await;
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = h.run(&["search", "edited", "--json"]).await;
    assert_eq!(o.json().as_array().unwrap().len(), 1);
    h.mock.set_reachable("carol@example.com", true);
    let o = h.run(&["lookup", "carol@example.com", "dave@example.com", "--json"]).await;
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v = o.json();
    assert_eq!(v[0]["handle"], "mailto:carol@example.com");
    assert_eq!(v[0]["reachable"], true);
    assert_eq!(v[1]["reachable"], false);
    let o = h.run(&["unsend", "bob@example.com", &first_id]).await;
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = h.run(&["messages", "bob@example.com", "-n", "1", "--reverse"]).await;
    assert!(o.stdout.contains("line one"), "newest only: {}", o.stdout);
    assert!(o.stderr.contains("--before"), "cursor hint: {}", o.stderr);

    // send-file: a relative path (resolved against imsg's cwd) is made absolute
    let f = h.dir.path().join("photo.png");
    std::fs::write(&f, b"png").unwrap();
    let abs = std::fs::canonicalize(&f).unwrap();
    let mut cmd = h.command(&["send-file", "bob@example.com", "photo.png", "--json"]);
    cmd.current_dir(h.dir.path());
    let o = run(cmd, None).await;
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        h.mock.calls().iter().any(|c| matches!(c, MockCall::SendAttachment { path, mime, .. } if path == &abs && mime == "image/png")),
        "expected SendAttachment with {}: {:?}",
        abs.display(),
        h.mock.calls()
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_json_receives_message() {
    let Some(h) = Harness::logged_in().await else { return };
    let mut child = h.command(&["watch", "--json", "--chat", "bob@example.com"]).spawn().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    // The watcher subscribes asynchronously: inject until a line arrives.
    let got = tokio::time::timeout(Duration::from_secs(15), async {
        let mut n = 0u64;
        loop {
            n += 1;
            h.mock.inject(incoming(&format!("00000000-0000-4000-A000-{n:012}"), 1_700_000_000_000 + n, "ping from bob"));
            if let Ok(Some(l)) = tokio::time::timeout(Duration::from_millis(300), lines.next_line()).await.map(|r| r.unwrap()) {
                return l;
            }
        }
    })
    .await
    .expect("watch printed nothing within 15s");
    let v: Value = serde_json::from_str(&got).unwrap();
    assert_eq!(v["signal"], "MessageReceived");
    assert_eq!(v["chat_id"], BOB);
    assert_eq!(v["message"]["text"], "ping from bob");
    // the filter drops other chats
    let other = h.run(&["send", "carol@example.com", "not for the watcher", "-q"]).await;
    assert_eq!(other.code, 0, "{}", other.stderr);
    h.mock.inject(incoming("00000000-0000-4000-B000-000000000001", 1_800_000_000_000, "after filter"));
    let next = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let l = lines.next_line().await.unwrap().unwrap();
            if l.contains("after filter") || l.contains("not for the watcher") {
                return l;
            }
        }
    })
    .await
    .unwrap();
    assert!(next.contains("after filter"), "{next}");
    child.kill().await.unwrap();
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_with_piped_password_and_codes() {
    let Some(h) = Harness::start().await else { return };
    h.mock.set_require_two_factor(true);
    // first code wrong (mock wants 6 digits), second right
    let o = h
        .run_stdin(&["login", "--apple-id", "me@example.com", "--password-stdin", "--code-stdin"], Some("hunter2\n12\n123 456\n"))
        .await;
    assert_eq!(o.code, 0, "stdout {} stderr {}", o.stdout, o.stderr);
    assert!(o.stderr.contains("wrong code, try again (1/3)"), "{}", o.stderr);
    assert!(o.stdout.contains("logged in"), "{}", o.stdout);
    for s in [&o.stdout, &o.stderr] {
        assert!(!s.contains("hunter2") && !s.contains("123456") && !s.contains("123 456"), "secret echoed: {s}");
    }
    let calls = h.mock.calls();
    assert!(calls.contains(&MockCall::Login { apple_id: "me@example.com".into() }));
    assert_eq!(calls.iter().filter(|c| **c == MockCall::SubmitTwoFactor).count(), 2);
    assert_eq!(h.run(&["status", "--json"]).await.json()["registered"], true);

    // logout, then three wrong codes -> 4
    assert_eq!(h.run(&["logout"]).await.code, 0);
    let o = h.run_stdin(&["login", "--apple-id", "me@example.com", "--password-stdin", "--code-stdin"], Some("pw\n1\n2\n3\n")).await;
    assert_eq!(o.code, 4, "{}", o.stderr);
    // code needed but none supplied -> 4; then --code-only finishes it
    let o = h.run_stdin(&["login", "--apple-id", "me@example.com", "--password-stdin", "--code-stdin", "--json"], Some("pw\n")).await;
    assert_eq!(o.code, 4, "{}", o.stderr);
    let o = h.run_stdin(&["login", "--code-only", "--code-stdin", "--json"], Some("654321\n")).await;
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.json()["registered"], true);
    // --password-stdin without --apple-id -> usage
    let o = h.run_stdin(&["login", "--password-stdin"], Some("pw\n")).await;
    assert_eq!(o.code, 2, "{}", o.stderr);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_reports_daemon() {
    let Some(h) = Harness::logged_in().await else { return };
    let o = h.run(&["doctor", "--json"]).await;
    let v = o.json();
    let checks = v["checks"].as_array().unwrap();
    let get = |name: &str| checks.iter().find(|c| c["check"] == name).unwrap_or_else(|| panic!("no {name} check: {v}"));
    assert_eq!(get("daemon")["level"], "ok");
    assert_eq!(get("connection")["level"], "ok");
    assert_eq!(get("registration")["level"], "ok");
    assert!(get("registration")["detail"].as_str().unwrap().contains("me@example.com"));
    // no hwconfig in the temp state dir -> FAIL -> exit 1
    assert_eq!(get("hwconfig")["level"], "FAIL");
    assert_eq!(get("anisette")["level"], "WARN");
    assert_eq!(o.code, 1);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sms_enable_status_and_send() {
    let Some(h) = Harness::logged_in().await else { return };
    h.mock.set_other_handles(vec!["tel:+15555550100".into()]);
    let o = h.run(&["sms", "status"]).await;
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("state   off"), "{}", o.stdout);
    let o = h.run(&["sms", "enable"]).await;
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("requested (via +15555550100)"), "{}", o.stdout);
    assert!(o.stderr.contains("Text Message Forwarding"), "{}", o.stderr);
    let o = h.run(&["status", "--json"]).await;
    assert_eq!(o.json()["sms"]["state"], "requested");
    h.mock.activate_sms("tel:+15555550100");
    let o = h.run(&["send", "--sms", "+1 555 555 0123", "via", "the", "phone"]).await;
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("as a text message"), "{}", o.stdout);
    assert!(h.mock.calls().iter().any(|c| matches!(c, MockCall::SendSms { text: Some(t), .. } if t == "via the phone")));
    let o = h.run(&["messages", "+15555550123"]).await;
    assert!(o.stdout.contains("[SMS]"), "{}", o.stdout);
    let o = h.run(&["sms", "disable", "--json"]).await;
    assert_eq!(o.json()["state"], "off");
    h.shutdown().await;
}

/// `chat --pin/--unpin`, the pin mark in `chats`, `pinned`/`pin_order` in
/// `chats --json`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pin_and_unpin() {
    let Some(h) = Harness::logged_in().await else { return };
    for to in ["bob@example.com", "carol@example.com"] {
        assert_eq!(h.run(&["send", to, "hi"]).await.code, 0);
    }
    let o = h.run(&["chat", "carol@example.com", "--pin"]).await;
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "pinned mailto:carol@example.com");
    let o = h.run(&["chat", "bob@example.com", "--pin", "--position", "1", "--json"]).await;
    assert_eq!((o.json()["pinned"].as_bool(), o.json()["pin_order"].as_u64()), (Some(true), Some(0)));
    let o = h.run(&["chats"]).await;
    assert!(o.stdout.lines().any(|l| l.starts_with("mailto:carol@example.com") && l.contains("📌 carol@example.com")), "{}", o.stdout);
    let v = h.run(&["chats", "--json"]).await.json();
    let carol = v.as_array().unwrap().iter().find(|c| c["id"] == "mailto:carol@example.com").unwrap();
    assert_eq!((carol["pinned"].as_bool(), carol["pin_order"].as_u64()), (Some(true), Some(1)));
    let o = h.run(&["chat", "carol@example.com"]).await;
    assert!(o.stdout.contains("pinned        yes (#2)"), "{}", o.stdout);
    let o = h.run(&["chat", "carol@example.com", "--unpin"]).await;
    assert_eq!(o.stdout.trim(), "unpinned mailto:carol@example.com");
    let o = h.run(&["chat", "carol@example.com", "--json"]).await;
    assert_eq!((o.json()["pinned"].as_bool(), o.json()["pin_order"].is_null()), (Some(false), true));
    h.shutdown().await;
}

/// `sync-mac` end to end: a fake `ssh` on PATH runs the embedded exporter
/// locally against a synthetic Mac home (tools/test_mac_sync_export.py
/// builds it). Skipped without python3.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_mac_with_a_fake_ssh() {
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        return eprintln!("SKIP: python3 not found");
    }
    let Some(h) = Harness::logged_in().await else { return };
    let t = tempfile::tempdir().unwrap();
    let (mac, bin, data, state, cache, home) = (t.path().join("mac"), t.path().join("bin"), t.path().join("data"), t.path().join("state"), t.path().join("cache"), t.path().join("home"));
    for d in [&mac, &bin, &home] {
        std::fs::create_dir_all(d).unwrap();
    }
    let maker = t.path().join("test_mac_sync_export.py");
    std::fs::write(&maker, include_str!("../../../tools/test_mac_sync_export.py")).unwrap();
    std::fs::write(t.path().join("mac-sync-export.py"), include_str!("../../../tools/mac-sync-export.py")).unwrap();
    std::fs::write(t.path().join("mac-backgrounds-export.py"), include_str!("../../../tools/mac-backgrounds-export.py")).unwrap();
    let st = std::process::Command::new("python3").arg("-I").arg(&maker).arg(t.path().join("mac-sync-export.py")).arg("--make-home").arg(&mac).status().unwrap();
    assert!(st.success());
    // ssh [-F x] -o a -o b HOST python3 - ARGS  ->  HOME=<mac> python3 -I - ARGS
    let ssh = bin.join("ssh");
    std::fs::write(&ssh, "#!/bin/sh\nwhile [ $# -gt 0 ] && [ \"$1\" != python3 ]; do shift; done\nshift\nHOME=\"$FAKE_MAC_HOME\" exec python3 -I \"$@\"\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let sync = |args: &[&str]| {
        let mut c = h.command(args);
        c.env("PATH", &path).env("FAKE_MAC_HOME", &mac).env("HOME", &home).env("XDG_DATA_HOME", &data).env("XDG_STATE_HOME", &state).env("XDG_CACHE_HOME", &cache);
        run(c, None)
    };
    for to in ["bob@example.com", "carol@example.com"] {
        assert_eq!(h.run(&["send", to, "hi"]).await.code, 0);
    }
    assert_eq!(h.run(&["chat", "carol@example.com", "--pin"]).await.code, 0);

    // no HOST and no mac_host: usage error
    let o = sync(&["sync-mac"]).await;
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("mac_host"), "{}", o.stderr);

    // dry run: reports, changes nothing
    let o = sync(&["sync-mac", "fakemac", "--dry-run", "--json"]).await;
    assert_eq!(o.code, 0, "{} {}", o.stderr, o.stdout);
    let v = o.json();
    assert_eq!((v["contacts"]["received"].as_u64(), v["contacts"]["written"].as_u64()), (Some(6), Some(6)));
    let results: Vec<(&str, &str)> = v["pins"]["mac"].as_array().unwrap().iter().map(|r| (r["pin"].as_str().unwrap(), r["result"].as_str().unwrap())).collect();
    assert_eq!(
        results,
        [
            ("+12055550100", "not-in-daemon"),
            ("bob@example.com", "would-pin"),
            ("5b0e2c1a-3f4d-4e21-9a55-0c7e1b2d3f40", "not-in-daemon"),
            ("chat987654", "not-in-daemon"),
            ("garbage!", "unknown")
        ]
    );
    assert_eq!(v["pins"]["mac"][3]["chat_id"], "group:ABCD-0000-1111");
    assert_eq!(v["pins"]["order"], serde_json::json!(["mailto:bob@example.com", "mailto:carol@example.com"]));
    assert!(!data.join("kpeoplevcard").exists());
    let bob = h.run(&["chat", "bob@example.com", "--json"]).await.json();
    assert_eq!(bob["pinned"], false);

    // for real: vCards written, bob pinned first, carol (local) kept after
    let o = sync(&["sync-mac", "fakemac"]).await;
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("contacts     6 from the Mac (2 with photos): 6 written, 0 unchanged, 0 removed"), "{}", o.stdout);
    assert!(o.stdout.contains("pins         5 on the Mac: 1 pinned, 4 not found; 1 local pin(s) kept"), "{}", o.stdout);
    let vcf = data.join("kpeoplevcard/mac-AAAA-1111_ABPerson.vcf");
    assert!(std::fs::read_to_string(&vcf).unwrap().contains("FN:Alice Anderson"));
    let chats = h.run(&["chats", "--json"]).await.json();
    let order = |id: &str| chats.as_array().unwrap().iter().find(|c| c["id"] == id).unwrap()["pin_order"].as_u64();
    assert_eq!((order("mailto:bob@example.com"), order("mailto:carol@example.com")), (Some(0), Some(1)));
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(state.join("imsg/mac-pins.json")).unwrap()).unwrap();
    assert_eq!(saved["chats"], serde_json::json!(["mailto:bob@example.com"]));

    // again: nothing to write; the Mac unpins bob -> unpinned here, carol stays
    let o = sync(&["sync-mac", "fakemac", "--contacts", "--json"]).await;
    assert_eq!((o.json()["contacts"]["unchanged"].as_u64(), o.json().get("pins").is_none()), (Some(6), true));
    let plist = mac.join("Library/Preferences/com.apple.messages.pinning.plist");
    std::fs::write(&plist, "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>pD</key><dict><key>pP</key><array/></dict></dict></plist>").unwrap();
    let o = sync(&["sync-mac", "fakemac", "--pins", "--json"]).await;
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.json()["pins"]["unpin"], serde_json::json!(["mailto:bob@example.com"]));
    assert_eq!(h.run(&["chat", "bob@example.com", "--json"]).await.json()["pinned"], false);
    assert_eq!(h.run(&["chat", "carol@example.com", "--json"]).await.json()["pin_order"], 0);

    // backgrounds ride along in the same ssh trip (the fixture's chat is not in the daemon)
    let o = sync(&["sync-mac", "fakemac", "--backgrounds", "--json"]).await;
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.json()["backgrounds"]["chats"][0]["result"], "not-in-daemon");

    // a section failing on the Mac: reported, exit 1, the folder left alone
    std::fs::remove_dir_all(mac.join("Library/Application Support/AddressBook")).unwrap();
    let o = sync(&["sync-mac", "fakemac", "--json"]).await;
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert!(o.json()["contacts"]["error"].as_str().unwrap().contains("no Contacts database"));
    assert!(o.json()["pins"]["order"].is_array());
    assert!(vcf.exists());
    h.shutdown().await;
}
