//! Optional log file (`log = true` in imsg-app.toml): `logs/imsg-app.log`,
//! truncated at start. Never message text; handles and chat ids go
//! through [`redact`]; error texts through
//! `imessage_common::redact::redact_line` (masks token-like strings).

use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

struct FileLogger {
    file: Mutex<std::fs::File>,
}

impl log::Log for FileLogger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info || m.target().starts_with("imsg")
    }

    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let line = imessage_common::redact::redact_line(&r.args().to_string());
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        if let Ok(mut f) = self.file.lock() {
            let _ = writeln!(f, "{now} {:<5} {}: {line}", r.level(), r.target());
        }
    }

    fn flush(&self) {
        if let Ok(mut f) = self.file.lock() {
            let _ = f.flush();
        }
    }
}

/// Start logging to `dir/imsg-app.log`.
pub fn init(dir: &Path) -> std::io::Result<()> {
    crate::appdir::create_private_dir(dir)?;
    let file = std::fs::File::create(dir.join("imsg-app.log"))?;
    let logger = Box::new(FileLogger { file: Mutex::new(file) });
    log::set_boxed_logger(logger).map_err(std::io::Error::other)?;
    log::set_max_level(log::LevelFilter::Debug);
    Ok(())
}

/// A handle or chat id with the personal part masked:
/// `mailto:bob@example.com` -> `mailto:b***@e***`, `tel:+15551234567` ->
/// `tel:+1********67`, `group:5B0E…` -> `group:5B0E…` (a GUID is not
/// personal), `group-p:` lists each masked.
pub fn redact(id: &str) -> String {
    if let Some(r) = id.strip_prefix("group-p:") {
        return format!("group-p:{}", r.split(',').map(redact).collect::<Vec<_>>().join(","));
    }
    if id.starts_with("group:") {
        return id.to_string();
    }
    if let Some(r) = id.strip_prefix("mailto:") {
        return format!("mailto:{}", mask_email(r));
    }
    if let Some(r) = id.strip_prefix("tel:") {
        return format!("tel:{}", mask_digits(r));
    }
    // An attachment id: <message GUID>:<part>; a GUID is not personal.
    if id.contains('@') { mask_email(id) } else if id.starts_with('+') { mask_digits(id) } else { id.to_string() }
}

fn mask_email(e: &str) -> String {
    let (user, domain) = e.split_once('@').unwrap_or((e, ""));
    let first = |s: &str| s.chars().next().map(|c| c.to_string()).unwrap_or_default();
    format!("{}***@{}***", first(user), first(domain))
}

fn mask_digits(n: &str) -> String {
    let chars: Vec<char> = n.chars().collect();
    if chars.len() <= 4 {
        return "*".repeat(chars.len());
    }
    let keep_head = if chars[0] == '+' { 2 } else { 1 };
    chars.iter().enumerate().map(|(i, c)| if i < keep_head || i >= chars.len() - 2 { *c } else { '*' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks() {
        assert_eq!(redact("mailto:bob@example.com"), "mailto:b***@e***");
        assert_eq!(redact("tel:+15551234567"), "tel:+1********67");
        assert_eq!(redact("tel:46339"), "tel:4**39");
        assert_eq!(redact("group:5B0E2C1A"), "group:5B0E2C1A");
        assert_eq!(redact("group-p:mailto:a@x,tel:+15551234567"), "group-p:mailto:a***@x***,tel:+1********67");
        assert_eq!(redact("0F1E:1"), "0F1E:1");
    }
}
