//! The daemon's log file. Linux logs to stderr, which systemd puts in the
//! journal; Windows has no journal and the logon task runs the daemon
//! without a console, so there the log goes to
//! `<state dir>\logs\imessaged.log` (`%LOCALAPPDATA%\imessaged\logs`), also
//! to stderr when that is a terminal. Same records and filter as the
//! journal (the redaction is in the log calls themselves). One generation
//! is kept: past [`MAX_BYTES`] at start, the file moves to
//! `imessaged.log.1`.

use std::fs::File;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use imessage_common::platform::fs as pfs;

/// Directory under the state directory.
pub const LOG_DIR: &str = "logs";
/// The current log file.
pub const LOG_FILE: &str = "imessaged.log";
/// Size at which the log is rotated (checked at start only).
pub const MAX_BYTES: u64 = 10 * 1024 * 1024;

/// `<state dir>/logs/imessaged.log`.
pub fn log_path(state_dir: &Path) -> PathBuf {
    state_dir.join(LOG_DIR).join(LOG_FILE)
}

/// Writes every record to the file and, if stderr is a terminal, there too.
pub struct LogWriter {
    file: File,
    tee: bool,
}

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.tee {
            let _ = io::stderr().write_all(buf);
        }
        self.file.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Open (append) the log file, rotating it first if it is too large.
pub fn open(state_dir: &Path) -> io::Result<LogWriter> {
    let dir = state_dir.join(LOG_DIR);
    crate::state::ensure_private_dir(&dir)?;
    let path = dir.join(LOG_FILE);
    rotate(&path, MAX_BYTES)?;
    let file = pfs::private_options().create(true).append(true).open(&path)?;
    Ok(LogWriter { file, tee: io::stderr().is_terminal() })
}

/// Move `path` to `path.1` (replacing it) if it is larger than `max`.
fn rotate(path: &Path, max: u64) -> io::Result<()> {
    match std::fs::metadata(path) {
        Ok(m) if m.len() > max => {
            let mut old = path.as_os_str().to_owned();
            old.push(".1");
            std::fs::rename(path, old)
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_and_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = open(dir.path()).unwrap();
        w.write_all(b"one\n").unwrap();
        drop(w);
        let mut w = open(dir.path()).unwrap();
        w.write_all(b"two\n").unwrap();
        drop(w);
        let p = log_path(dir.path());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one\ntwo\n");
        rotate(&p, 4).unwrap();
        assert!(!p.exists());
        assert_eq!(std::fs::read_to_string(dir.path().join("logs/imessaged.log.1")).unwrap(), "one\ntwo\n");
        rotate(&p, 4).unwrap(); // nothing there: fine
        let mut w = open(dir.path()).unwrap();
        w.write_all(b"three\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "three\n");
    }
}
