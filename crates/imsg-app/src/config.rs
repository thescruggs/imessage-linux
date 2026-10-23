//! `imsg-app.toml`: non-secret settings. The bridge token lives only in
//! `secrets.bin` (see `secrets.rs`).
//!
//! ```toml
//! # Used when pairing with a bare token instead of a full pairing string.
//! bridge = "http://100.64.0.10:7227"
//! # Write logs/imsg-app.log (handles masked, no message text).
//! log = false
//! # "s", "m" (default) or "l".
//! font_size = "m"
//! # "system" (default: follow the Windows light/dark setting), "light", "dark".
//! theme = "system"
//!
//! [notifications]
//! enabled = true
//! # Show the message text in notifications (off: sender name only).
//! show_text = false
//! ```

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bridge: Option<String>,
    pub log: bool,
    pub font_size: FontSize,
    pub theme: ThemeMode,
    pub notifications: Notifications,
}

/// Light or dark colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    /// Follow the system's app light/dark setting (and its changes).
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeMode {
    pub fn index(self) -> i32 {
        match self {
            ThemeMode::System => 0,
            ThemeMode::Light => 1,
            ThemeMode::Dark => 2,
        }
    }

    pub fn from_index(i: i32) -> ThemeMode {
        match i {
            1 => ThemeMode::Light,
            2 => ThemeMode::Dark,
            _ => ThemeMode::System,
        }
    }

    /// `IMSG_APP_DEV_THEME` and friends.
    pub fn parse(s: &str) -> Option<ThemeMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "system" | "auto" | "" => Some(ThemeMode::System),
            "light" => Some(ThemeMode::Light),
            "dark" => Some(ThemeMode::Dark),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Notifications {
    pub enabled: bool,
    pub show_text: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FontSize {
    S,
    #[default]
    M,
    L,
}

impl FontSize {
    /// Base font size in logical px. M is deliberately larger than the
    /// usual 14 px (readability).
    pub fn px(self) -> f32 {
        match self {
            FontSize::S => 14.0,
            FontSize::M => 16.0,
            FontSize::L => 19.0,
        }
    }

    pub fn index(self) -> i32 {
        match self {
            FontSize::S => 0,
            FontSize::M => 1,
            FontSize::L => 2,
        }
    }

    pub fn from_index(i: i32) -> FontSize {
        match i {
            0 => FontSize::S,
            2 => FontSize::L,
            _ => FontSize::M,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config { bridge: None, log: false, font_size: FontSize::M, theme: ThemeMode::System, notifications: Notifications::default() }
    }
}

impl Default for Notifications {
    fn default() -> Self {
        Notifications { enabled: true, show_text: false }
    }
}

impl Config {
    /// The config, or the defaults if there is none. A broken file is
    /// reported and left alone (not overwritten until the user saves).
    pub fn load(path: &Path) -> (Config, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(s) => match toml::from_str::<Config>(&s) {
                Ok(c) => (c, None),
                Err(e) => (Config::default(), Some(format!("{}: {e}", path.display()))),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), None),
            Err(e) => (Config::default(), Some(format!("{}: {e}", path.display()))),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut s = String::from("# imsg-app settings (see README-portable.txt). The token is in secrets.bin.\n");
        s.push_str(&toml::to_string_pretty(self).map_err(std::io::Error::other)?);
        crate::secrets::write_atomic(path, s.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_round_trip() {
        let c: Config = toml::from_str("bridge = \"http://100.64.0.10:7227\"\n[notifications]\nshow_text = true\n").unwrap();
        assert_eq!(c.bridge.as_deref(), Some("http://100.64.0.10:7227"));
        assert!(c.notifications.show_text && c.notifications.enabled);
        assert_eq!(c.font_size, FontSize::M);
        assert_eq!(c.theme, ThemeMode::System);
        let dark: Config = toml::from_str("theme = \"dark\"").unwrap();
        assert_eq!(dark.theme, ThemeMode::Dark);
        assert!(toml::from_str::<Config>("theme = \"purple\"").is_err());
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("imsg-app.toml");
        let mut c2 = c.clone();
        c2.font_size = FontSize::L;
        c2.theme = ThemeMode::Light;
        c2.save(&p).unwrap();
        assert_eq!(Config::load(&p), (c2, None));
        assert_eq!(Config::load(&d.path().join("none.toml")), (Config::default(), None));
        std::fs::write(&p, "font_size = 3").unwrap();
        assert!(Config::load(&p).1.is_some());
        // The shipped example parses.
        let ex = include_str!("../../../packaging/windows/imsg-app.toml.example");
        assert_eq!(toml::from_str::<Config>(ex).unwrap().bridge.as_deref(), Some("http://100.64.0.10:7227"));
    }

    #[test]
    fn theme_mode() {
        for m in [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark] {
            assert_eq!(ThemeMode::from_index(m.index()), m);
        }
        assert_eq!(ThemeMode::parse(" Dark "), Some(ThemeMode::Dark));
        assert_eq!(ThemeMode::parse("light"), Some(ThemeMode::Light));
        assert_eq!(ThemeMode::parse("nope"), None);
    }
}
