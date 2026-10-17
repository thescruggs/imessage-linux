//! The real `imessaged` binary (rustpush engine, no hwconfig, file key
//! provider) on a private bus: the name is registered, GetStatus answers,
//! SIGTERM shuts it down cleanly and the store files are private.
//! Prints `SKIP:` and passes when `dbus-daemon` is not installed.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use imessaged::dbus::{BUS_NAME, INTERFACE, OBJECT_PATH};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_serves_dbus_and_stops_on_sigterm() {
    let Some(bus) = common::PrivateBus::start() else { return };
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_imessaged"))
        .args(["--state-dir", dir.path().to_str().unwrap(), "--key-provider", "file"])
        .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
        .env_remove("RUST_LOG")
        .env("IMESSAGED_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let conn = zbus::connection::Builder::address(bus.address.as_str()).unwrap().build().await.unwrap();
    common::wait_for_name(&conn, BUS_NAME, 20).await;
    let p: zbus::Proxy<'_> = zbus::Proxy::new(&conn, BUS_NAME, OBJECT_PATH, INTERFACE).await.unwrap();
    let status: String = p.call("GetStatus", &()).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&status).unwrap();
    assert_eq!(v["connection"]["state"], "needs_login", "{status}");
    assert_eq!(v["registered"], false);
    assert_eq!(v["unread"], 0);
    let e = p.call::<_, _, String>("Login", &("me@example.com", "secret-pw")).await.unwrap_err();
    assert!(matches!(&e, zbus::Error::MethodError(n, _, _) if n.as_str() == "org.imessage.Daemon1.Error.NoHardwareConfig"), "{e:?}");

    let st = Command::new("kill").args(["-TERM", &child.id().to_string()]).status().unwrap();
    assert!(st.success());
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "imessaged did not exit after SIGTERM");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let mut stderr = String::new();
    std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut stderr).unwrap();
    assert!(status.success(), "exit {status:?}\n{stderr}");
    assert!(stderr.contains("message store closed"), "{stderr}");
    assert!(!stderr.contains("secret-pw"), "password leaked into the log");
    assert!(!common::has_owner(&conn, BUS_NAME).await);
    for f in ["messages.db", "state.enc", "state.key"] {
        let m = std::fs::metadata(dir.path().join(f)).unwrap_or_else(|e| panic!("{f}: {e}"));
        assert_eq!(m.permissions().mode() & 0o777, 0o600, "{f}");
    }
}
