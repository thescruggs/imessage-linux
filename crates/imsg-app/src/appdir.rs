//! Where the app keeps its files (docs/PORTABLE-WINDOWS.md, section 3).
//!
//! * `IMSG_APP_DIR` set: that directory (tests, development).
//! * Portable: a file `imsg-app.toml` or `portable` next to the exe: the
//!   exe's directory (the USB stick). Nothing is written anywhere else.
//! * Otherwise `%LOCALAPPDATA%\imsg-app` (Windows) or
//!   `$XDG_CONFIG_HOME/imsg-app` / `~/.config/imsg-app`.
//!
//! Layout: `imsg-app.toml`, `secrets.bin`, `cache\` (`cache\open\` = copies
//! handed to other apps, wiped at start and exit), `logs\`.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDirs {
    pub root: PathBuf,
    pub portable: bool,
}

pub const CONFIG_FILE: &str = "imsg-app.toml";
pub const PORTABLE_MARKER: &str = "portable";

impl AppDirs {
    pub fn detect() -> AppDirs {
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
        Self::resolve(std::env::var_os("IMSG_APP_DIR").map(PathBuf::from), exe_dir.as_deref(), platform_dir())
    }

    /// The rules above, without touching the environment.
    pub fn resolve(env_dir: Option<PathBuf>, exe_dir: Option<&Path>, platform: PathBuf) -> AppDirs {
        if let Some(d) = env_dir.filter(|d| !d.as_os_str().is_empty()) {
            return AppDirs { root: d, portable: true };
        }
        if let Some(d) = exe_dir
            && (d.join(CONFIG_FILE).is_file() || d.join(PORTABLE_MARKER).exists())
        {
            return AppDirs { root: d.to_path_buf(), portable: true };
        }
        AppDirs { root: platform, portable: false }
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join(CONFIG_FILE)
    }

    pub fn secrets_file(&self) -> PathBuf {
        self.root.join("secrets.bin")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// Files downloaded to be opened with another app (and temp files):
    /// never `%TEMP%`.
    pub fn open_dir(&self) -> PathBuf {
        self.cache_dir().join("open")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// Create the root (owner-only on Unix).
    pub fn ensure(&self) -> std::io::Result<()> {
        create_private_dir(&self.root)
    }

    /// Delete `cache/open` (start and exit).
    pub fn wipe_open_dir(&self) {
        let d = self.open_dir();
        if d.exists()
            && let Err(e) = std::fs::remove_dir_all(&d)
        {
            log::warn!("could not clear the open-with cache: {e}");
        }
    }

    /// Delete everything under `cache/`.
    pub fn clear_cache(&self) -> std::io::Result<()> {
        let d = self.cache_dir();
        if d.exists() {
            std::fs::remove_dir_all(&d)?;
        }
        Ok(())
    }
}

pub fn create_private_dir(d: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(d)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(d)
    }
}

fn platform_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(d) = std::env::var_os("LOCALAPPDATA").filter(|d| !d.is_empty()) {
            return PathBuf::from(d).join("imsg-app");
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(d) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
            return PathBuf::from(d).join("imsg-app");
        }
    }
    imessage_common::paths::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".config").join("imsg-app")
}

/// A file name that is safe on Windows and Unix (attachment names come
/// from other people's devices).
pub fn safe_file_name(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    // Win32 drops trailing dots and spaces (`x.exe.` is `x.exe`).
    while s.ends_with(['.', ' ']) {
        s.pop();
    }
    let s = s.trim_start_matches(['.', ' ']).to_string();
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4 && stem.as_bytes()[3].is_ascii_digit());
    let s = if s.is_empty() { "file".to_string() } else if reserved { format!("_{s}") } else { s };
    // Keep it short (MAX_PATH on a deep stick path).
    if s.chars().count() > 120 {
        let ext = Path::new(&s).extension().map(|e| e.to_string_lossy().into_owned()).filter(|e| e.len() <= 10);
        let head: String = s.chars().take(100).collect();
        return match ext {
            Some(e) => format!("{head}.{e}"),
            None => head,
        };
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_rules() {
        let exe = tempfile::tempdir().unwrap();
        let platform = PathBuf::from("/home/u/.config/imsg-app");
        // Nothing next to the exe: the platform dir.
        assert_eq!(AppDirs::resolve(None, Some(exe.path()), platform.clone()), AppDirs { root: platform.clone(), portable: false });
        // A marker: next to the exe.
        std::fs::write(exe.path().join("portable"), "").unwrap();
        assert_eq!(AppDirs::resolve(None, Some(exe.path()), platform.clone()), AppDirs { root: exe.path().to_path_buf(), portable: true });
        std::fs::remove_file(exe.path().join("portable")).unwrap();
        std::fs::write(exe.path().join("imsg-app.toml"), "").unwrap();
        assert!(AppDirs::resolve(None, Some(exe.path()), platform.clone()).portable);
        // The env override wins.
        let d = AppDirs::resolve(Some("/tmp/x".into()), Some(exe.path()), platform);
        assert_eq!(d.root, PathBuf::from("/tmp/x"));
        assert_eq!(d.open_dir(), PathBuf::from("/tmp/x/cache/open"));
    }

    #[test]
    fn file_names() {
        assert_eq!(safe_file_name("IMG_0001.jpeg"), "IMG_0001.jpeg");
        assert_eq!(safe_file_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_file_name("a:b*c?.txt"), "a_b_c_.txt");
        assert_eq!(safe_file_name("evil.exe. . "), "evil.exe");
        assert_eq!(safe_file_name("CON.txt"), "_CON.txt");
        assert_eq!(safe_file_name("com1"), "_com1");
        assert_eq!(safe_file_name("COMMA.txt"), "COMMA.txt");
        assert_eq!(safe_file_name(""), "file");
        assert_eq!(safe_file_name("..."), "file");
        let long = format!("{}.pdf", "x".repeat(300));
        assert_eq!(safe_file_name(&long), format!("{}.pdf", "x".repeat(100)));
    }
}
