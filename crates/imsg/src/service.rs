//! `imsg service`: start the daemon at login and control it.
//!
//! Linux: the packaged systemd user unit (`systemctl --user ... imessaged`).
//! Windows: a per-user Task Scheduler task `imessaged` (not a Windows
//! service: the daemon's secrets are per-user) that starts imessaged.exe at
//! log on, runs it without a time limit, and starts it again every 5
//! minutes if it is not running (a crash; Task Scheduler's own
//! "restart on failure" does not fire for a program that exits with an
//! error). `stop` also disables the task, so the daemon stays stopped
//! until `start`.

// The Task Scheduler helpers are pure so their tests run everywhere; only
// Windows calls them.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::{Path, PathBuf};

use crate::ServiceCommand;
use crate::client::exit;
#[cfg(windows)]
use crate::local::DoctorLine;

/// The task name (Windows) / unit name (Linux).
pub const NAME: &str = "imessaged";

pub fn run(cmd: &ServiceCommand, state_dir: &Path) -> anyhow::Result<u8> {
    imp::run(cmd, state_dir)
}

/// `imsg doctor`'s view of the autostart setup (Linux: local.rs checks
/// D-Bus activation and the systemd unit).
#[cfg(windows)]
pub fn doctor_lines(state_dir: &Path) -> Vec<DoctorLine> {
    imp::doctor_lines(state_dir)
}

/// XML special characters escaped (element text).
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// Task Scheduler definition (schema 1.2, Windows 7+; elements in the
/// order Task Scheduler exports them) for `exe`, run as
/// `user` (`DOMAIN\name`) at their log on, with their normal (unelevated)
/// rights and only while they are logged on (so DPAPI and their ssh agent
/// work). Pure, so it is tested everywhere.
pub fn task_xml(exe: &Path, user: &str) -> String {
    let exe_s = exe.to_string_lossy();
    // By hand: `Path::parent` splits at `\` only on Windows, and the tests run everywhere.
    let dir = exe_s.rfind(['\\', '/']).map(|i| &exe_s[..i]).unwrap_or_default();
    let user = xml_escape(user);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>iMessage daemon (imessaged) for imsg and imsg-tui. Created by `imsg service install`.</Description>
    <URI>\{NAME}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Repetition>
        <Interval>PT5M</Interval>
        <StopAtDurationEnd>false</StopAtDurationEnd>
      </Repetition>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>"{exe}"</Command>
      <WorkingDirectory>{dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        exe = xml_escape(&exe_s),
        dir = xml_escape(dir),
    )
}

/// `<Settings><Enabled>` of a task's XML (`schtasks /Query /XML`); `None`
/// if there is none. The triggers have their own `<Enabled>`, so only the
/// `Settings` element counts.
pub fn settings_enabled(xml: &str) -> Option<bool> {
    let settings = &xml[xml.find("<Settings>")?..];
    let settings = &settings[..settings.find("</Settings>").unwrap_or(settings.len())];
    // Nested elements (IdleSettings) have no <Enabled>, so the first one is it.
    let v = settings.split("<Enabled>").nth(1)?.split("</Enabled>").next()?.trim();
    Some(v.eq_ignore_ascii_case("true"))
}

/// The daemon's peer-to-peer port from a `bus-address` file's text.
pub fn address_port(text: &str) -> Option<u16> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    line.split_once(':')?.1.split(',').find_map(|kv| kv.strip_prefix("port="))?.parse().ok()
}

/// Is a peer-to-peer daemon listening at the address in `state_dir`? (A
/// plain TCP connect: the daemon drops it for lack of a nonce.)
pub fn peer_daemon_listening(state_dir: &Path) -> Option<u16> {
    use imessage_common::platform::ipc::BUS_ADDRESS_FILE;
    let port = address_port(&std::fs::read_to_string(state_dir.join(BUS_ADDRESS_FILE)).ok()?)?;
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(1)).ok().map(|_| port)
}

/// imessaged next to this imsg (how the installer lays them out).
pub fn default_daemon_exe() -> Option<PathBuf> {
    let name = if cfg!(windows) { "imessaged.exe" } else { "imessaged" };
    std::env::current_exe().ok()?.parent().map(|d| d.join(name))
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    use std::process::Command;

    fn systemctl(args: &[&str]) -> anyhow::Result<u8> {
        let status = Command::new("systemctl")
            .arg("--user")
            .args(args)
            .status()
            .map_err(|e| anyhow::anyhow!("cannot run systemctl: {e}"))?;
        Ok(match status.code() {
            Some(0) => exit::OK,
            Some(c) => u8::try_from(c).unwrap_or(exit::ERROR),
            None => exit::ERROR,
        })
    }

    /// The packaged unit does the work; these are `systemctl --user`.
    pub fn run(cmd: &ServiceCommand, _state_dir: &Path) -> anyhow::Result<u8> {
        match cmd {
            ServiceCommand::Install { exe: Some(_), .. } => {
                anyhow::bail!("--exe is for Windows; on Linux the systemd unit (packaging/systemd/imessaged.service) names the binary")
            }
            ServiceCommand::Install { no_start: true, .. } => systemctl(&["enable", NAME]),
            ServiceCommand::Install { .. } => systemctl(&["enable", "--now", NAME]),
            ServiceCommand::Uninstall => systemctl(&["disable", "--now", NAME]),
            ServiceCommand::Start => systemctl(&["start", NAME]),
            ServiceCommand::Stop => systemctl(&["stop", NAME]),
            ServiceCommand::Status => systemctl(&["status", "--no-pager", NAME]),
        }
    }

}

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::local::line;
    use imessage_common::hwconfig::Level;
    use std::process::{Command, Output};

    fn schtasks_exe() -> PathBuf {
        let root = std::env::var_os("SystemRoot").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        root.join("System32").join("schtasks.exe")
    }

    fn schtasks(args: &[&str]) -> anyhow::Result<Output> {
        Command::new(schtasks_exe()).args(args).output().map_err(|e| anyhow::anyhow!("cannot run schtasks.exe: {e}"))
    }

    /// stdout + stderr of a failed schtasks run, for the error message.
    fn text(o: &Output) -> String {
        let s = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        s.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ")
    }

    fn check(o: Output, what: &str) -> anyhow::Result<()> {
        if o.status.success() { Ok(()) } else { anyhow::bail!("{what}: schtasks failed: {}", text(&o)) }
    }

    /// The task's XML, `None` if it does not exist.
    fn query_xml() -> Option<String> {
        let o = schtasks(&["/Query", "/TN", NAME, "/XML"]).ok()?;
        let xml = String::from_utf8_lossy(&o.stdout).into_owned();
        (o.status.success() && xml.contains("<Task")).then_some(xml)
    }

    /// `DOMAIN\user` of whoever runs imsg.
    fn current_user() -> anyhow::Result<String> {
        let user = std::env::var("USERNAME").ok().filter(|v| !v.is_empty()).ok_or_else(|| anyhow::anyhow!("%USERNAME% is not set"))?;
        Ok(match std::env::var("USERDOMAIN").ok().filter(|v| !v.is_empty()) {
            Some(d) => format!("{d}\\{user}"),
            None => user,
        })
    }

    fn install(exe: Option<&Path>, start: bool) -> anyhow::Result<u8> {
        let exe = match exe {
            Some(e) => e.to_path_buf(),
            None => default_daemon_exe().ok_or_else(|| anyhow::anyhow!("cannot tell where imsg.exe is; pass --exe <path to imessaged.exe>"))?,
        };
        let exe = std::path::absolute(&exe).unwrap_or(exe);
        if !exe.is_file() {
            anyhow::bail!("{} does not exist; pass --exe <path to imessaged.exe>", exe.display());
        }
        let xml = task_xml(&exe, &current_user()?);
        // schtasks wants the file in UTF-16 (with BOM) to match the
        // declaration.
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
        let dir = imessage_common::paths::imsg_cache_dir();
        imessage_common::platform::fs::create_private_dir_all(&dir)?;
        let file = dir.join(format!("task-{}.xml", std::process::id()));
        std::fs::write(&file, bytes)?;
        let r = schtasks(&["/Create", "/TN", NAME, "/XML", &file.to_string_lossy(), "/F"]);
        let _ = std::fs::remove_file(&file);
        check(r?, "creating the task")?;
        println!("task {NAME} installed: starts {} at log on", exe.display());
        if start {
            check(schtasks(&["/Run", "/TN", NAME])?, "starting the task")?;
            println!("started");
        }
        Ok(exit::OK)
    }

    pub fn run(cmd: &ServiceCommand, state_dir: &Path) -> anyhow::Result<u8> {
        match cmd {
            ServiceCommand::Install { exe, no_start } => install(exe.as_deref(), !no_start),
            ServiceCommand::Uninstall => {
                if query_xml().is_none() {
                    println!("task {NAME} is not installed");
                    return Ok(exit::OK);
                }
                let _ = schtasks(&["/End", "/TN", NAME]);
                check(schtasks(&["/Delete", "/TN", NAME, "/F"])?, "deleting the task")?;
                println!("task {NAME} removed (imessaged stopped)");
                Ok(exit::OK)
            }
            ServiceCommand::Start => {
                if query_xml().is_none() {
                    anyhow::bail!("task {NAME} is not installed; run `imsg service install`");
                }
                check(schtasks(&["/Change", "/TN", NAME, "/ENABLE"])?, "enabling the task")?;
                check(schtasks(&["/Run", "/TN", NAME])?, "starting the task")?;
                println!("started");
                Ok(exit::OK)
            }
            ServiceCommand::Stop => {
                if query_xml().is_none() {
                    anyhow::bail!("task {NAME} is not installed");
                }
                // Disabled first, or the 5-minute repetition starts it again.
                check(schtasks(&["/Change", "/TN", NAME, "/DISABLE"])?, "disabling the task")?;
                let _ = schtasks(&["/End", "/TN", NAME]);
                println!("stopped; it stays stopped (also at log on) until `imsg service start`");
                Ok(exit::OK)
            }
            ServiceCommand::Status => {
                let lines = doctor_lines(state_dir);
                for l in &lines {
                    println!("{}: {}", l.check, l.detail);
                }
                Ok(if peer_daemon_listening(state_dir).is_some() { exit::OK } else { exit::NO_DAEMON })
            }
        }
    }

    pub fn doctor_lines(state_dir: &Path) -> Vec<DoctorLine> {
        let mut lines = Vec::new();
        lines.push(match query_xml() {
            None => line("autostart", Level::Warn, format!("no logon task {NAME}; run `imsg service install` (the installer does this)")),
            Some(xml) => {
                let cmd = xml.split("<Command>").nth(1).and_then(|r| r.split("</Command>").next()).unwrap_or("?").trim().to_string();
                match settings_enabled(&xml) {
                    Some(false) => line("autostart", Level::Warn, format!("logon task {NAME} is disabled (`imsg service stop`); `imsg service start` turns it on")),
                    _ => line("autostart", Level::Ok, format!("logon task {NAME} runs {cmd}")),
                }
            }
        });
        lines.push(match peer_daemon_listening(state_dir) {
            Some(port) => line("daemon process", Level::Ok, format!("imessaged listening on 127.0.0.1:{port}")),
            None => line("daemon process", Level::Warn, "imessaged is not running; `imsg service start`"),
        });
        let log = state_dir.join("logs").join("imessaged.log");
        lines.push(line("log", Level::Ok, log.display().to_string()));
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_definition() {
        let xml = task_xml(Path::new(r"C:\Program Files\iMessage & Co\imessaged.exe"), r"PC\Jo <x>");
        assert!(xml.contains(r#"<Command>"C:\Program Files\iMessage &amp; Co\imessaged.exe"</Command>"#), "{xml}");
        assert!(xml.contains(r"<WorkingDirectory>C:\Program Files\iMessage &amp; Co</WorkingDirectory>"), "{xml}");
        assert_eq!(xml.matches(r"<UserId>PC\Jo &lt;x&gt;</UserId>").count(), 2, "{xml}");
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>") && xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>") && xml.contains("<Interval>PT5M</Interval>"));
        assert_eq!(settings_enabled(&xml), Some(true));
        let disabled = xml.replace("<Enabled>true</Enabled>\n    <Hidden>", "<Enabled>false</Enabled>\n    <Hidden>");
        assert_eq!(settings_enabled(&disabled), Some(false), "only Settings/Enabled counts");
        assert_eq!(settings_enabled("<Task/>"), None);
    }

    #[test]
    fn port_from_address() {
        assert_eq!(address_port("nonce-tcp:host=127.0.0.1,port=50123,noncefile=C%3a%5cx,guid=ab\r\n"), Some(50123));
        assert_eq!(address_port("\n nonce-tcp:port=1,host=x\n"), Some(1));
        assert_eq!(address_port("unix:path=/x"), None);
        assert_eq!(address_port(""), None);
    }
}
