//! Anisette (Apple's device-attestation headers for GrandSlam login).
//!
//! Two sources:
//!
//! * [`Provider::Remote`]: omnisette's remote anisette v3 client
//!   (`anisette_url`, e.g. ani.sidestore.io). Since 2026-10-13 Apple rejects
//!   logins carrying headers from the Android-based anisette-v3-server
//!   software every public server runs (GSA error -80045) when the client
//!   claims to be a Mac, which we do. Kept for when that changes again.
//! * [`Provider::Command`]: run a command that prints the headers as a JSON
//!   object, normally `ssh -F ~/.ssh/config -o BatchMode=yes -o
//!   LogLevel=ERROR <mac> .local/bin/imessage-anisette`
//!   (`tools/mac-anisette/imessage-anisette.m`), which mints them natively
//!   with the Mac's AOSKit/AuthKit. Those are the same libraries macOS uses,
//!   so Apple accepts them. The Mac is only contacted when the daemon
//!   authenticates (login, silent re-login), never for message traffic.
//!
//! The command supplies the attested pair `X-Apple-I-MD` (one-time password)
//! and `X-Apple-I-MD-M` (machine id) plus `X-Apple-I-MD-LU`,
//! `X-Mme-Device-Id`, `X-Apple-I-MD-RINFO`, `X-Mme-Client-Info` and
//! `X-Apple-Locale`; we add the time-dependent `X-Apple-I-Client-Time` and
//! `X-Apple-I-TimeZone` here, as omnisette's own providers do. omnisette
//! caches the result for 60 s, so a login runs the command a few times.
//!
//! The daemon's systemd unit sandboxes it (`ProtectSystem=strict` etc.),
//! which for a *user* unit implies `PrivateUsers=`: root-owned files appear
//! owned by nobody and OpenSSH then refuses the system `ssh_config.d`
//! includes ("Bad owner or permissions"). Hence the documented command uses
//! `ssh -F ~/.ssh/config` (or `-F /dev/null`) to skip the system config,
//! and `-o LogLevel=ERROR` so banners / known_hosts notices on stderr do
//! not end up in the journal.

// omnisette's error enum is large; its size is not ours to choose.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use imessage_common::platform::process::{SHELL, shell_command};
use imessage_common::redact;
use omnisette::remote_anisette_v3::RemoteAnisetteProviderV3;
use omnisette::{AnisetteError, AnisetteProvider};

/// Headers the command must print; anything else is passed through.
pub const REQUIRED_HEADERS: &[&str] = &["X-Apple-I-MD", "X-Apple-I-MD-M"];
/// Headers we fill in with defaults when the command omits them.
const DEFAULT_RINFO: &str = "17106176";
const DEFAULT_LOCALE: &str = "en_US";
/// A Mac over ssh answers in well under a second; a sleeping laptop may
/// take a few seconds to wake its network. Beyond this, report failure.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs `sh -c <command>` and parses its stdout as a JSON object of headers.
pub struct CommandAnisetteProvider {
    command: String,
}

impl CommandAnisetteProvider {
    pub fn new(command: String) -> CommandAnisetteProvider {
        CommandAnisetteProvider { command }
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    /// Run the command once and return the complete header set.
    pub async fn fetch(&self) -> Result<HashMap<String, String>, AnisetteError> {
        let out = run_command(&self.command).await?;
        let mut headers = parse_headers(&out)?;
        add_time_headers(&mut headers);
        log::debug!(
            "anisette: {} headers from command (client info {:?})",
            headers.len(),
            headers.get("X-Mme-Client-Info").map(String::as_str).unwrap_or("-")
        );
        Ok(headers)
    }
}

/// Run `sh -c command` (Windows: `cmd.exe /c`; [`shell_command`]) with stdin
/// closed, capturing stdout. stderr goes to the log, redacted
/// ([`redact::redact_line`]). Errors read
/// `anisette command failed: <reason>` (the command itself is only logged)
/// and never include stdout (headers are credentials).
pub async fn run_command(command: &str) -> Result<Vec<u8>, AnisetteError> {
    let child = tokio::process::Command::from(shell_command(command))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| fail(command, format!("cannot start {SHELL}: {e}")))?;
    let out = tokio::time::timeout(COMMAND_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| fail(command, format!("no answer within {} s", COMMAND_TIMEOUT.as_secs())))?
        .map_err(|e| fail(command, e.to_string()))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    for l in stderr.lines().filter(|l| !l.trim().is_empty()) {
        log::warn!("anisette command stderr: {}", redact::redact_line(l));
    }
    if !out.status.success() {
        let last = redact::last_line(&stderr).map(|l| format!(": {l}")).unwrap_or_default();
        return Err(fail(command, format!("{}{last}", out.status)));
    }
    Ok(out.stdout)
}

/// Error for a failed run: logs the command, returns
/// `anisette command failed: <reason>` (the prefix
/// [`ANISETTE_COMMAND_FAILED`] the engine and `imsg` key on).
fn fail(command: &str, reason: String) -> AnisetteError {
    log::warn!("anisette command {command:?} failed: {reason}");
    anyhow::anyhow!("{ANISETTE_COMMAND_FAILED}: {reason}").into()
}

/// Prefix of every [`run_command`] error.
pub const ANISETTE_COMMAND_FAILED: &str = "anisette command failed";

/// Parse the command's JSON object; require [`REQUIRED_HEADERS`], default
/// RINFO/locale, drop empty values.
pub fn parse_headers(json: &[u8]) -> Result<HashMap<String, String>, AnisetteError> {
    let v: serde_json::Value = serde_json::from_slice(json)
        .map_err(|e| anyhow::anyhow!("anisette command output is not JSON: {e}"))?;
    let obj = v
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("anisette command output is not a JSON object"))?;
    let mut headers: HashMap<String, String> = obj
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.trim().to_string())))
        .filter(|(_, v)| !v.is_empty())
        .collect();
    for k in REQUIRED_HEADERS {
        if !headers.contains_key(*k) {
            return Err(anyhow::anyhow!("anisette command output lacks {k}").into());
        }
    }
    headers.entry("X-Apple-I-MD-RINFO".into()).or_insert_with(|| DEFAULT_RINFO.into());
    headers.entry("X-Apple-Locale".into()).or_insert_with(|| DEFAULT_LOCALE.into());
    Ok(headers)
}

/// `X-Apple-I-Client-Time` / `X-Apple-I-TimeZone` as omnisette and real
/// clients send them: whole seconds, `YYYY-MM-DDTHH:MM:SSZ`.
pub fn add_time_headers(headers: &mut HashMap<String, String>) {
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    headers.insert("X-Apple-I-Client-Time".into(), now);
    headers.insert("X-Apple-I-TimeZone".into(), "UTC".into());
}

/// The daemon's anisette provider: whichever source the config selects.
pub enum Provider {
    Remote(Box<RemoteAnisetteProviderV3>),
    Command(CommandAnisetteProvider),
}

impl AnisetteProvider for Provider {
    async fn get_anisette_headers(&mut self) -> Result<HashMap<String, String>, AnisetteError> {
        match self {
            Provider::Remote(r) => r.get_anisette_headers().await,
            Provider::Command(c) => c.fetch().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_requires_otp_and_machine_id() {
        let ok = parse_headers(br#"{"X-Apple-I-MD":"a","X-Apple-I-MD-M":"b","X-Mme-Device-Id":"c","empty":""}"#).unwrap();
        assert_eq!(ok["X-Apple-I-MD"], "a");
        assert_eq!(ok["X-Apple-I-MD-RINFO"], DEFAULT_RINFO);
        assert_eq!(ok["X-Apple-Locale"], DEFAULT_LOCALE);
        assert!(!ok.contains_key("empty"));
        assert!(parse_headers(br#"{"X-Apple-I-MD":"a"}"#).is_err());
        assert!(parse_headers(b"not json").is_err());
        assert!(parse_headers(b"[1]").is_err());
    }

    #[test]
    fn time_headers_look_like_omnisette() {
        let mut h = HashMap::new();
        add_time_headers(&mut h);
        let t = &h["X-Apple-I-Client-Time"];
        assert!(t.ends_with('Z') && t.contains('T') && !t.contains('.'), "{t}");
        assert_eq!(t.len(), "2026-10-16T12:34:56Z".len(), "{t}");
        assert_eq!(h["X-Apple-I-TimeZone"], "UTC");
    }

    #[tokio::test]
    async fn command_provider_runs_shell() {
        let p = CommandAnisetteProvider::new(r#"printf '{"X-Apple-I-MD":"otp","X-Apple-I-MD-M":"mid"}'"#.into());
        let h = p.fetch().await.unwrap();
        assert_eq!(h["X-Apple-I-MD-M"], "mid");
        assert!(h.contains_key("X-Apple-I-Client-Time"));

        let bad = CommandAnisetteProvider::new("echo nope >&2; exit 3".into());
        let e = bad.fetch().await.unwrap_err().to_string();
        assert_eq!(e, "anisette command failed: exit status: 3: nope");

        let leaky = CommandAnisetteProvider::new("echo 'X-Apple-I-MD: c2VjcmV0c2VjcmV0c2VjcmV0MTIz' >&2; exit 1".into());
        let e = leaky.fetch().await.unwrap_err().to_string();
        assert!(!e.contains("c2VjcmV0") && e.contains(redact::REDACTED), "{e}");
    }
}
