//! Private D-Bus bus for integration tests: a `dbus-daemon` with a minimal
//! config (no service activation, listens in a temp dir). Tests print
//! `SKIP:` and return when `dbus-daemon` is not installed.

#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

pub struct PrivateBus {
    child: Child,
    pub address: String,
    _dir: tempfile::TempDir,
}

impl PrivateBus {
    pub fn start() -> Option<PrivateBus> {
        if Command::new("dbus-daemon").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() {
            eprintln!("SKIP: dbus-daemon not installed");
            return None;
        }
        // Unix socket paths are limited to ~108 bytes; a long $TMPDIR makes
        // dbus-daemon fail silently. Use a short base dir instead.
        let base = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(d) if !d.is_empty() && d.len() < 60 => std::path::PathBuf::from(d),
            _ => std::path::PathBuf::from("/tmp"),
        };
        let dir = tempfile::Builder::new().prefix("imsg-bus").tempdir_in(&base).unwrap();
        let conf = dir.path().join("bus.conf");
        std::fs::write(
            &conf,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                dir.path().display()
            ),
        )
        .unwrap();
        let mut child = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", conf.display()))
            .args(["--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn dbus-daemon");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let address = line.trim().to_string();
        assert!(
            address.starts_with("unix:"),
            "dbus-daemon failed to start (socket path too long?): got bus address {address:?}"
        );
        Some(PrivateBus { child, address, _dir: dir })
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Wait until `name` has an owner on the bus (or panic after `secs`).
pub async fn wait_for_name(conn: &zbus::Connection, name: &str, secs: u64) {
    let dbus = zbus::fdo::DBusProxy::new(conn).await.unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let n = zbus::names::BusName::try_from(name).unwrap();
        if dbus.name_has_owner(n).await.unwrap_or(false) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "{name} never appeared on the bus");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

pub async fn has_owner(conn: &zbus::Connection, name: &str) -> bool {
    let dbus = zbus::fdo::DBusProxy::new(conn).await.unwrap();
    dbus.name_has_owner(zbus::names::BusName::try_from(name).unwrap()).await.unwrap_or(false)
}
