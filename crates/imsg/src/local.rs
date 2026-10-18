//! Commands that work without the daemon (`hwconfig import`) and `doctor`,
//! which checks the local setup and then the daemon.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use imessage_common::hwconfig::{self, Level, Report};
use imessage_common::model::ConnectionState;
use imessage_common::paths;
use sha1::{Digest, Sha1};
use sha2::Sha256;

use crate::client::{BUS_NAME, Bus, Client, exit};
use crate::format::{self, Style};

// ---------------------------------------------------------------------------
// hwconfig import
// ---------------------------------------------------------------------------

fn print_report(report: &Report) {
    for c in &report.checks {
        println!("  {:>4}  {}: {}", c.level.to_string(), c.field, c.detail);
    }
    if let Some(e) = &report.parse_error {
        println!("  FAIL  (parse): {e}");
    }
}

pub fn hwconfig_import(file: &Path, state_dir: &Path, force: bool) -> anyhow::Result<u8> {
    let bytes = fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let report = hwconfig::inspect_bytes(&bytes).with_context(|| format!("{} is not a plist", file.display()))?;
    println!("hwconfig {}:", file.display());
    print_report(&report);

    let worst = report.worst();
    let Some(cfg) = report.config.as_ref().filter(|_| worst != Level::Fail) else {
        eprintln!("imsg: hwconfig has FAIL checks; not importing. Re-run tools/mac-hwinfo.sh on the Mac.");
        return Ok(exit::ERROR);
    };
    println!(
        "identity: {} ({}), macOS {} build {}",
        cfg.inner.product_name, cfg.inner.board_id, cfg.version, cfg.inner.os_build_num
    );

    let dest = state_dir.join(paths::HWCONFIG_FILE);
    if let Ok(existing) = fs::read(&dest) {
        if existing == bytes {
            println!("already imported: {}", dest.display());
            fs::set_permissions(&dest, fs::Permissions::from_mode(0o600))?;
            return Ok(exit::OK);
        }
        if !force {
            bail!(
                "{} already holds a different hwconfig; pass --force to replace it \
                 (changing identity forces re-registration)",
                dest.display()
            );
        }
    }
    install_private(&bytes, state_dir, &dest)?;
    if worst == Level::Warn {
        println!("imported with warnings: {}", dest.display());
    } else {
        println!("imported: {}", dest.display());
    }
    Ok(exit::OK)
}

/// Atomically write `bytes` to `dest` with mode 0600 (dir 0700).
fn install_private(bytes: &[u8], dir: &Path, dest: &Path) -> anyhow::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".{}.tmp.{}", paths::HWCONFIG_FILE, std::process::id()));
    let write = || -> anyhow::Result<()> {
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, dest)?;
        Ok(())
    };
    write()
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
        .with_context(|| format!("writing {}", dest.display()))
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

pub struct DoctorLine {
    pub check: &'static str,
    pub level: Level,
    pub detail: String,
}

fn line(check: &'static str, level: Level, detail: impl Into<String>) -> DoctorLine {
    DoctorLine { check, level, detail: detail.into() }
}

/// Lowercase hex digest of a file's contents.
fn hash_file<D: Digest>(path: &Path) -> std::io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut h = D::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Checks that need no daemon: hwconfig, Apple binary, cert.
pub fn local_checks(state_dir: &Path) -> Vec<DoctorLine> {
    let mut lines = Vec::new();

    let hw = state_dir.join(paths::HWCONFIG_FILE);
    let (level, detail) = if !hw.exists() {
        (Level::Fail, format!("{} missing; run `imsg hwconfig import <file>`", hw.display()))
    } else {
        match hwconfig::inspect_path(&hw) {
            Ok(r) => {
                let worst = r.worst();
                let bad: Vec<String> =
                    r.checks.iter().filter(|c| c.level != Level::Ok).map(|c| format!("{} ({})", c.field, c.level)).collect();
                let mode = fs::metadata(&hw).map(|m| m.permissions().mode() & 0o777).unwrap_or(0);
                let mut d = format!("{}", hw.display());
                if let Some(c) = &r.config {
                    d.push_str(&format!(": {} macOS {}", c.inner.product_name, c.version));
                }
                if !bad.is_empty() {
                    d.push_str(&format!("; issues: {}", bad.join(", ")));
                }
                if mode & 0o077 != 0 {
                    d.push_str(&format!("; mode {mode:o} (should be 600)"));
                    (worst.max(Level::Warn), d)
                } else {
                    (worst, d)
                }
            }
            Err(e) => (Level::Fail, format!("{}: {e}", hw.display())),
        }
    };
    lines.push(line("hwconfig", level, detail));

    let bin = open_absinthe::nac::binary_path();
    let (level, detail) = match hash_file::<Sha1>(&bin) {
        Ok(sum) if sum == open_absinthe::nac::BINARY_SHA1 => (Level::Ok, format!("{} (sha1 ok)", bin.display())),
        Ok(sum) => (
            Level::Fail,
            format!("{}: sha1 {sum} != {}; re-run {}", bin.display(), open_absinthe::nac::BINARY_SHA1, open_absinthe::nac::BOOTSTRAP_HINT),
        ),
        Err(e) => (Level::Fail, format!("{}: {e}; run {}", bin.display(), open_absinthe::nac::BOOTSTRAP_HINT)),
    };
    lines.push(line("IMDAppleServices", level, detail));

    // cert-1.0.plist: test/doctor-only copy (rustpush fetches the cert from
    // the IDS bag at runtime), so problems are WARN, never FAIL.
    let cert = paths::data_dir().join(paths::VALIDATION_CERT_FILE);
    let want = paths::VALIDATION_CERT_SHA256;
    let (level, detail) = match hash_file::<Sha256>(&cert) {
        Ok(sum) if sum == want => (Level::Ok, format!("{} (sha256 ok)", cert.display())),
        Ok(sum) => (Level::Warn, format!("{}: sha256 {sum} != {want}; re-run {}", cert.display(), open_absinthe::nac::BOOTSTRAP_HINT)),
        Err(e) => (Level::Warn, format!("{}: {e}; run {}", cert.display(), open_absinthe::nac::BOOTSTRAP_HINT)),
    };
    lines.push(line("cert-1.0.plist", level, detail));
    lines
}

/// Directories searched for the D-Bus activation file, most specific first
/// (`$XDG_DATA_HOME`, then `$XDG_DATA_DIRS`, each + `dbus-1/services`).
pub fn activation_dirs(xdg_data_home: Option<&str>, home: Option<&str>, xdg_data_dirs: Option<&str>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    match xdg_data_home.filter(|v| v.starts_with('/')) {
        Some(d) => dirs.push(PathBuf::from(d)),
        None => {
            if let Some(h) = home {
                dirs.push(Path::new(h).join(".local/share"));
            }
        }
    }
    let sys = xdg_data_dirs.filter(|v| !v.is_empty()).unwrap_or("/usr/local/share:/usr/share");
    dirs.extend(sys.split(':').filter(|d| d.starts_with('/')).map(PathBuf::from));
    dirs.into_iter().map(|d| d.join("dbus-1/services")).collect()
}

fn activation_check() -> DoctorLine {
    let env = |k: &str| std::env::var(k).ok();
    let dirs = activation_dirs(env("XDG_DATA_HOME").as_deref(), env("HOME").as_deref(), env("XDG_DATA_DIRS").as_deref());
    let file = format!("{BUS_NAME}.service");
    match dirs.iter().map(|d| d.join(&file)).find(|p| p.is_file()) {
        Some(p) => line("D-Bus activation", Level::Ok, p.display().to_string()),
        None => line(
            "D-Bus activation",
            Level::Warn,
            format!(
                "{file} not installed (no on-demand start); install packaging/dbus/{file} into ~/.local/share/dbus-1/services/"
            ),
        ),
    }
}

/// Default anisette server (keep in sync with imessaged's config).
const DEFAULT_ANISETTE_URL: &str = "https://ani.sidestore.io";

/// A daemon setting as `imsg` can see it: the `IMESSAGED_*` environment
/// variable, else `key` in `state_dir/config.toml`. (A flag given to the
/// daemon itself is not visible here.)
fn daemon_setting(state_dir: &Path, env: &str, key: &str) -> Option<String> {
    if let Some(v) = std::env::var(env).ok().filter(|v| !v.is_empty()) {
        return Some(v);
    }
    fs::read_to_string(state_dir.join("config.toml"))
        .ok()
        .and_then(|s| s.parse::<toml::Table>().ok())
        .and_then(|t| t.get(key).and_then(|v| v.as_str()).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string))
}

/// The anisette URL the daemon will use (`IMESSAGED_ANISETTE_URL`, config
/// `anisette_url`, else the default).
fn anisette_url(state_dir: &Path) -> String {
    daemon_setting(state_dir, "IMESSAGED_ANISETTE_URL", "anisette_url").unwrap_or_else(|| DEFAULT_ANISETTE_URL.to_string())
}

/// The anisette command, if the daemon is configured to use one
/// (`IMESSAGED_ANISETTE_COMMAND`, config `anisette_command`).
pub(crate) fn anisette_command(state_dir: &Path) -> Option<String> {
    daemon_setting(state_dir, "IMESSAGED_ANISETTE_COMMAND", "anisette_command")
}

/// `(host, port)` of an http(s) URL.
pub fn url_host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let default = match scheme.to_ascii_lowercase().as_str() {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    if authority.is_empty() {
        return None;
    }
    if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6.split_once(']')?;
        let port = after.strip_prefix(':').map_or(Some(default), |p| p.parse().ok())?;
        return Some((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) => Some((h.to_string(), p.parse().ok()?)),
        None => Some((authority.to_string(), default)),
    }
}

/// Anisette source works: the command answers with the attested headers,
/// or (remote v3 server) the host accepts a TCP connection. Remote servers
/// are flagged: Apple rejects their headers since 2026-10-13 (GSA -80045).
/// An ssh command also gets the `anisette ssh` checks ([`ssh_lines`]).
async fn anisette_check(state_dir: &Path) -> Vec<DoctorLine> {
    if let Some(cmd) = anisette_command(state_dir) {
        let hw_model = hwconfig::inspect_path(state_dir.join(paths::HWCONFIG_FILE))
            .ok()
            .and_then(|r| r.config)
            .map(|c| c.inner.product_name);
        let mut lines = vec![anisette_command_check(&cmd, hw_model.as_deref()).await];
        if command_uses_ssh(&cmd) {
            let env = std::process::Command::new("systemctl")
                .args(["--user", "show-environment"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
            let ssh_config = std::env::var_os("HOME")
                .map(|h| Path::new(&h).join(".ssh/config"))
                .and_then(|p| std::fs::read_to_string(p).ok());
            lines.extend(ssh_lines(&cmd, env.as_deref(), ssh_config.as_deref()));
        }
        return lines;
    }
    let url = anisette_url(state_dir);
    let Some((host, port)) = url_host_port(&url) else {
        return vec![line("anisette", Level::Fail, format!("{url}: not an http(s) URL"))];
    };
    let connect = tokio::net::TcpStream::connect((host.as_str(), port));
    let hint = "; Apple has rejected anisette-v3-server logins (-80045) since 2026-10-13, set anisette_command (README: Anisette)";
    vec![match tokio::time::timeout(Duration::from_secs(5), connect).await {
        Ok(Ok(_)) => line("anisette", Level::Warn, format!("remote v3 server {url} reachable{hint}")),
        Ok(Err(e)) => line("anisette", Level::Warn, format!("{url}: {e}{hint}")),
        Err(_) => line("anisette", Level::Warn, format!("{url}: no TCP connection within 5 s{hint}")),
    }]
}

/// Does the command run ssh (a word `ssh` or `.../ssh`)?
pub fn command_uses_ssh(cmd: &str) -> bool {
    cmd.split_whitespace().any(|w| w == "ssh" || w.ends_with("/ssh"))
}

/// Warnings for an ssh anisette command (pure; unit tested). `systemd_env`:
/// `systemctl --user show-environment` output, `None` if unavailable.
/// `ssh_config`: the user's `~/.ssh/config`; when it names an
/// `IdentityAgent` or `IdentityFile`, ssh needs no SSH_AUTH_SOCK.
pub fn ssh_lines(cmd: &str, systemd_env: Option<&str>, ssh_config: Option<&str>) -> Vec<DoctorLine> {
    let mut lines = Vec::new();
    if !cmd.split_whitespace().any(|w| w.starts_with("-F")) {
        lines.push(line(
            "anisette ssh",
            Level::Warn,
            "command runs ssh without -F: the sandboxed imessaged unit cannot read the root-owned system ssh config              (\"Bad owner or permissions\"); use `ssh -F ~/.ssh/config -o BatchMode=yes -o LogLevel=ERROR ...` (README: Anisette)",
        ));
    }
    let config_names_key = ssh_config.is_some_and(|c| {
        c.lines().any(|l| {
            let l = l.trim_start();
            !l.starts_with('#') && l.split_whitespace().next().is_some_and(|k| k.eq_ignore_ascii_case("IdentityAgent") || k.eq_ignore_ascii_case("IdentityFile"))
        })
    });
    if let Some(env) = systemd_env
        && !config_names_key
        && !env.lines().any(|l| l.starts_with("SSH_AUTH_SOCK="))
    {
        lines.push(line(
            "anisette ssh",
            Level::Warn,
            "the systemd user manager has no SSH_AUTH_SOCK, so the daemon's ssh sees no agent; \
             fine if ~/.ssh/config names one (IdentityAgent) or a key file, \
             else `systemctl --user import-environment SSH_AUTH_SOCK`",
        ));
    }
    lines
}

/// Run the configured command the way the daemon does and check it prints
/// a JSON object with the attested pair. Values are never shown.
/// `hw_model`: hwconfig.plist's product name, compared with the model the
/// command's Mac reports.
async fn anisette_command_check(cmd: &str, hw_model: Option<&str>) -> DoctorLine {
    let run = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(Duration::from_secs(30), run).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return line("anisette", Level::Fail, format!("command {cmd:?}: cannot run: {e}")),
        Err(_) => return line("anisette", Level::Fail, format!("command {cmd:?}: no answer within 30 s")),
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = imessage_common::redact::last_line(&err).unwrap_or_default();
        return line("anisette", Level::Fail, format!("command {cmd:?} failed ({}): {last}", out.status));
    }
    match anisette_headers_summary(&out.stdout) {
        Ok((summary, model)) => match (model.as_deref(), hw_model) {
            (Some(m), Some(hw)) if m != hw => line(
                "anisette",
                Level::Warn,
                format!(
                    "command ok: {summary}, but hwconfig.plist is a {hw}; anisette_command must reach the Mac \
                     the hwconfig was extracted from"
                ),
            ),
            _ => line("anisette", Level::Ok, format!("command ok: {summary}")),
        },
        Err(e) => line("anisette", Level::Fail, format!("command {cmd:?}: {e}")),
    }
}

/// The hardware model in `X-Mme-Client-Info` (`<Mac14,2> <macOS;...> <...>`).
pub fn client_info_model(info: &str) -> Option<String> {
    let rest = info.trim().strip_prefix('<')?;
    let model = rest.split('>').next()?.trim();
    (!model.is_empty()).then(|| model.to_string())
}

/// ("<n> headers, Mac model <m>", model) for a JSON object with the
/// attested pair; an error naming what is missing otherwise.
pub fn anisette_headers_summary(stdout: &[u8]) -> Result<(String, Option<String>), String> {
    let v: serde_json::Value = serde_json::from_slice(stdout).map_err(|e| format!("output is not JSON: {e}"))?;
    let obj = v.as_object().ok_or("output is not a JSON object")?;
    let present = |k: &str| obj.get(k).and_then(|v| v.as_str()).is_some_and(|s| !s.trim().is_empty());
    for k in ["X-Apple-I-MD", "X-Apple-I-MD-M"] {
        if !present(k) {
            return Err(format!("output lacks {k}"));
        }
    }
    let model = obj.get("X-Mme-Client-Info").and_then(|v| v.as_str()).and_then(client_info_model);
    Ok((format!("{} headers, Mac model {}", obj.len(), model.as_deref().unwrap_or("unknown")), model))
}

fn systemd_check() -> Option<DoctorLine> {
    let out = std::process::Command::new("systemctl").args(["--user", "is-active", "imessaged"]).output().ok()?;
    let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let state = if state.is_empty() { String::from_utf8_lossy(&out.stderr).trim().to_string() } else { state };
    Some(match state.as_str() {
        "active" => line("systemd unit", Level::Ok, "imessaged.service active"),
        "activating" | "reloading" => line("systemd unit", Level::Warn, format!("imessaged.service {state}")),
        "failed" => line("systemd unit", Level::Fail, "imessaged.service failed; see `journalctl --user -u imessaged`"),
        other => line(
            "systemd unit",
            Level::Warn,
            format!("imessaged.service {other}; `systemctl --user enable --now imessaged` to start it at login"),
        ),
    })
}

/// Checks against the running daemon.
/// `imsg sync-mac`'s default host: the daemon's `mac_host`, or (unset) a
/// suggestion from the host the ssh anisette command reaches (pure).
pub fn mac_host_line(mac_host: Option<&str>, anisette_host: Option<&str>, config: &Path) -> Option<DoctorLine> {
    match (mac_host, anisette_host) {
        (Some(h), _) => Some(line("mac host", Level::Ok, format!("{h} (`imsg sync-mac` syncs contacts and pins from it)"))),
        (None, Some(a)) => Some(line(
            "mac host",
            Level::Warn,
            format!(
                "mac_host is not set, so `imsg sync-mac` needs the Mac's host every time; your anisette command reaches {a}: \
                 add `mac_host = \"{a}\"` to {} and restart imessaged",
                config.display()
            ),
        )),
        (None, None) => None,
    }
}

async fn daemon_checks(bus: &Bus, state_dir: &Path) -> Vec<DoctorLine> {
    let mut lines = Vec::new();
    let client = match Client::connect(bus).await {
        Ok(c) => c,
        Err(e) => {
            lines.push(line("daemon", Level::Fail, e.message));
            return lines;
        }
    };
    let was_running = client.daemon_present().await;
    // Version may trigger D-Bus activation; give it the bus's own timeout.
    let version = tokio::time::timeout(Duration::from_secs(30), client.version()).await;
    match version {
        Ok(Ok(v)) => lines.push(line("daemon", Level::Ok, format!("imessaged {v} owns {BUS_NAME}{}", if was_running { "" } else { " (just started by D-Bus activation)" }))),
        Ok(Err(e)) => {
            lines.push(line("daemon", Level::Fail, e.message));
            return lines;
        }
        Err(_) => {
            lines.push(line("daemon", Level::Fail, "no reply from imessaged within 30 s"));
            return lines;
        }
    }
    match client.status().await {
        Ok(s) => {
            let s = s.value;
            let (level, detail) = match &s.connection {
                ConnectionState::Connected => (Level::Ok, "connected".to_string()),
                ConnectionState::NeedsLogin => (Level::Warn, "needs_login; run `imsg login`".to_string()),
                ConnectionState::NeedsTwoFactor => (Level::Warn, "needs_two_factor; run `imsg login --code-only`".to_string()),
                ConnectionState::Error(d) => (Level::Fail, format!("error: {d}")),
                other => (Level::Warn, format::connection_name(other).to_string()),
            };
            lines.push(line("connection", level, detail));
            let handles: Vec<String> = s.registration.handles.iter().map(format::short_handle).collect();
            let reg = if s.registered {
                let mut d = format!("registered: {}", handles.join(", "));
                if let Some(t) = s.registration.expires_at {
                    d.push_str(&format!("; renews {}", format::abs_time_secs(t)));
                }
                line("registration", Level::Ok, d)
            } else {
                line("registration", Level::Warn, "not registered (no iMessage handles)")
            };
            lines.push(reg);
            if let Some(p) = &s.profiles {
                use imessage_common::model::ProfilesState as P;
                let level = match p.state {
                    P::Ready => Level::Ok,
                    P::Pending | P::NeedsLogin | P::Unavailable => Level::Warn,
                };
                let mut d = format!("shared names and photos: {}", format::profiles_text(p));
                if p.state == P::NeedsLogin {
                    d.push_str("; run `imsg login` once (password, and a 2FA code if Apple asks) to enable them");
                }
                lines.push(line("profiles", level, d));
            }
            let anisette_host = anisette_command(state_dir).and_then(|c| imessage_common::ssh::host_of_command(&c));
            lines.extend(mac_host_line(s.mac_host.as_deref(), anisette_host.as_deref(), &state_dir.join("config.toml")));
        }
        Err(e) => lines.push(line("connection", Level::Fail, e.message)),
    }
    lines
}

pub async fn doctor(state_dir: &Path, bus: &Bus, json: bool, st: Style) -> anyhow::Result<u8> {
    let mut lines = local_checks(state_dir);
    lines.extend(anisette_check(state_dir).await);
    lines.push(activation_check());
    if let Some(l) = systemd_check() {
        lines.push(l);
    }
    lines.extend(daemon_checks(bus, state_dir).await);

    let worst = lines.iter().map(|l| l.level).max().unwrap_or(Level::Ok);
    if json {
        let v: Vec<serde_json::Value> = lines
            .iter()
            .map(|l| serde_json::json!({ "check": l.check, "level": l.level.to_string(), "detail": l.detail }))
            .collect();
        println!("{}", serde_json::to_string(&serde_json::json!({ "checks": v, "ok": worst != Level::Fail }))?);
    } else {
        for l in &lines {
            let lvl = format!("{:>4}", l.level.to_string());
            let lvl = match l.level {
                Level::Ok => st.green(&lvl),
                Level::Warn => st.yellow(&lvl),
                Level::Fail => st.red(&lvl),
            };
            println!("{lvl}  {}: {}", l.check, l.detail);
        }
    }
    Ok(if worst == Level::Fail { exit::ERROR } else { exit::OK })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parsing() {
        assert_eq!(url_host_port("https://ani.sidestore.io"), Some(("ani.sidestore.io".into(), 443)));
        assert_eq!(url_host_port("http://localhost:6969/v3"), Some(("localhost".into(), 6969)));
        assert_eq!(url_host_port("HTTP://u:p@h.example/x"), Some(("h.example".into(), 80)));
        assert_eq!(url_host_port("https://[::1]:8443/"), Some(("::1".into(), 8443)));
        assert_eq!(url_host_port("https://[::1]"), Some(("::1".into(), 443)));
        for bad in ["ftp://x", "nope", "https://", "https://h:notaport"] {
            assert_eq!(url_host_port(bad), None, "{bad}");
        }
    }

    #[test]
    fn anisette_headers_summary_checks_attested_pair() {
        let ok = anisette_headers_summary(
            br#"{"X-Apple-I-MD":"a","X-Apple-I-MD-M":"b","X-Mme-Client-Info":"<Mac14,2> <macOS;15.6;24G84> <com.apple.AuthKit/1 ((null)/1.0)>"}"#,
        )
        .unwrap();
        assert_eq!(ok, ("3 headers, Mac model Mac14,2".to_string(), Some("Mac14,2".to_string())));
        let none = anisette_headers_summary(br#"{"X-Apple-I-MD":"a","X-Apple-I-MD-M":"b"}"#).unwrap();
        assert_eq!(none, ("2 headers, Mac model unknown".to_string(), None));
        assert_eq!(client_info_model("garbage"), None);
        assert_eq!(client_info_model("<> x"), None);
        assert_eq!(anisette_headers_summary(br#"{"X-Apple-I-MD":"a"}"#).unwrap_err(), "output lacks X-Apple-I-MD-M");
        assert!(anisette_headers_summary(b"nope").unwrap_err().starts_with("output is not JSON"));
    }

    #[test]
    fn ssh_command_warnings() {
        assert!(command_uses_ssh("ssh -F ~/.ssh/config mac x"));
        assert!(command_uses_ssh("/usr/bin/ssh mac x"));
        assert!(!command_uses_ssh("printf '{}' # sshd"));
        let good = "ssh -F ~/.ssh/config -o BatchMode=yes -o LogLevel=ERROR mac .local/bin/imessage-anisette";
        let env_ok = "HOME=/home/u\nSSH_AUTH_SOCK=/run/user/1000/ssh-agent\n";
        assert!(ssh_lines(good, Some(env_ok), None).is_empty());
        assert!(ssh_lines(good, None, None).is_empty(), "no systemd: no agent warning");
        assert!(ssh_lines("ssh -F/dev/null mac x", Some(env_ok), None).is_empty());
        let no_f = ssh_lines("ssh -o BatchMode=yes mac x", Some(env_ok), None);
        assert_eq!(no_f.len(), 1);
        assert!(no_f[0].level == Level::Warn && no_f[0].detail.contains("-F"), "{}", no_f[0].detail);
        let no_agent = ssh_lines(good, Some("HOME=/home/u\nXSSH_AUTH_SOCK=x\n"), None);
        assert_eq!(no_agent.len(), 1);
        assert!(no_agent[0].level == Level::Warn && no_agent[0].detail.contains("IdentityAgent"), "{}", no_agent[0].detail);
        let agent_in_config = "Host *\n  IdentityAgent ~/.1password/agent.sock\n";
        assert!(ssh_lines(good, Some("HOME=/home/u\n"), Some(agent_in_config)).is_empty(), "IdentityAgent in ssh config: no warning");
        assert!(ssh_lines(good, Some("HOME=/home/u\n"), Some("Host m\n  identityfile ~/.ssh/id\n")).is_empty());
        assert_eq!(ssh_lines(good, Some("HOME=/home/u\n"), Some("# IdentityAgent commented out\n")).len(), 1);
    }

    #[test]
    fn mac_host_suggestion() {
        let cfg = Path::new("/s/config.toml");
        let l = mac_host_line(Some("bebop.local"), Some("other"), cfg).unwrap();
        assert!(l.level == Level::Ok && l.detail.starts_with("bebop.local"), "{}", l.detail);
        let l = mac_host_line(None, Some("bebop.local"), cfg).unwrap();
        assert!(l.level == Level::Warn && l.detail.contains("mac_host = \"bebop.local\"") && l.detail.contains("/s/config.toml"), "{}", l.detail);
        assert!(mac_host_line(None, None, cfg).is_none());
    }

    #[test]
    fn anisette_command_from_config() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        assert_eq!(anisette_command(d), None);
        fs::write(d.join("config.toml"), "anisette_command = \"ssh mac imessage-anisette\"\n").unwrap();
        assert_eq!(anisette_command(d).as_deref(), Some("ssh mac imessage-anisette"));
    }

    #[test]
    fn anisette_url_from_config() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        assert_eq!(anisette_url(d), DEFAULT_ANISETTE_URL);
        fs::write(d.join("config.toml"), "anisette_url = \"http://localhost:6969\"\n").unwrap();
        if std::env::var_os("IMESSAGED_ANISETTE_URL").is_none() {
            assert_eq!(anisette_url(d), "http://localhost:6969");
        }
    }

    #[test]
    fn activation_search_path() {
        let d = activation_dirs(None, Some("/home/u"), None);
        assert_eq!(
            d,
            vec![
                PathBuf::from("/home/u/.local/share/dbus-1/services"),
                PathBuf::from("/usr/local/share/dbus-1/services"),
                PathBuf::from("/usr/share/dbus-1/services"),
            ]
        );
        let d = activation_dirs(Some("/x"), Some("/home/u"), Some("/a:relative:/b"));
        assert_eq!(d, vec![PathBuf::from("/x/dbus-1/services"), PathBuf::from("/a/dbus-1/services"), PathBuf::from("/b/dbus-1/services")]);
        // relative XDG_DATA_HOME is ignored (XDG spec)
        assert_eq!(activation_dirs(Some("rel"), Some("/h"), Some("/s"))[0], PathBuf::from("/h/.local/share/dbus-1/services"));
    }
}
