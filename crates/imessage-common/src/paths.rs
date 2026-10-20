//! Filesystem locations shared by `imessaged` and `imsg`.
//!
//! On Windows every directory lives under the per-user `%LOCALAPPDATA%`
//! (fallback `%USERPROFILE%\AppData\Local`) instead of the XDG base
//! directories; `XDG_*` variables are not consulted there.

use std::path::PathBuf;

/// File name of the imported Mac identity inside the state directory.
pub const HWCONFIG_FILE: &str = "hwconfig.plist";

/// File name of Apple's public identity-validation cert in the data dir.
pub const VALIDATION_CERT_FILE: &str = "cert-1.0.plist";

/// sha256 of [`VALIDATION_CERT_FILE`]. Keep in sync with `CERT_SHA256` in
/// `tools/bootstrap.sh`. The local copy is for tests/`imsg doctor` only;
/// rustpush fetches the cert from the IDS bag at runtime.
pub const VALIDATION_CERT_SHA256: &str =
    "6ef3521ed899be820e132a6d8c53cac955267c2c3f862684477524be91dc3a06";

/// The user's home directory: `$HOME` (Windows: `%USERPROFILE%`).
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty());
    home.map(PathBuf::from)
}

#[cfg(not(windows))]
fn home() -> PathBuf {
    home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// `$VAR` if it is set to an absolute path (per the XDG spec, relative values
/// are ignored), else `~/<fallback>`.
#[cfg(not(windows))]
fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
}

/// Windows: `%LOCALAPPDATA%` if set to an absolute path, else
/// `%USERPROFILE%\AppData\Local`. (open-absinthe's `nac::data_dir` repeats
/// this rule; keep them in sync.)
#[cfg(windows)]
fn local_app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home_dir().unwrap_or_else(|| PathBuf::from(".")).join("AppData").join("Local"))
}

/// Base for state (`$XDG_STATE_HOME`, Windows `%LOCALAPPDATA%`).
fn state_home() -> PathBuf {
    #[cfg(not(windows))]
    {
        xdg("XDG_STATE_HOME", ".local/state")
    }
    #[cfg(windows)]
    {
        local_app_data()
    }
}

/// Base for data (`$XDG_DATA_HOME`, Windows `%LOCALAPPDATA%`).
fn data_home() -> PathBuf {
    #[cfg(not(windows))]
    {
        xdg("XDG_DATA_HOME", ".local/share")
    }
    #[cfg(windows)]
    {
        local_app_data()
    }
}

/// Default daemon state directory (keys, registration, hwconfig, store):
/// `$XDG_STATE_HOME/imessaged`, else `~/.local/state/imessaged`
/// (Windows: `%LOCALAPPDATA%\imessaged`).
pub fn default_state_dir() -> PathBuf {
    state_home().join("imessaged")
}

/// Downloaded Apple files (`IMDAppleServices`, `cert-1.0.plist`), written by
/// `tools/bootstrap.sh`: `${XDG_DATA_HOME:-~/.local/share}/imessage-linux`
/// (Windows: `%LOCALAPPDATA%\imessage-linux`).
pub fn data_dir() -> PathBuf {
    data_home().join("imessage-linux")
}

/// Where KPeople (kmessages' contacts) reads vCards from and `imsg
/// sync-mac` writes the Mac's: `${XDG_DATA_HOME:-~/.local/share}/kpeoplevcard`
/// (Windows: `%LOCALAPPDATA%\kpeoplevcard`).
pub fn kpeople_vcard_dir() -> PathBuf {
    data_home().join("kpeoplevcard")
}

/// `imsg`'s own state (the pins the last `imsg sync-mac` applied):
/// `${XDG_STATE_HOME:-~/.local/state}/imsg` (Windows: `%LOCALAPPDATA%\imsg`).
pub fn imsg_state_dir() -> PathBuf {
    state_home().join("imsg")
}

/// `imsg`'s scratch space (staged Mac exports):
/// `${XDG_CACHE_HOME:-~/.cache}/imsg` (Windows: `%LOCALAPPDATA%\imsg\cache`).
pub fn imsg_cache_dir() -> PathBuf {
    #[cfg(not(windows))]
    {
        xdg("XDG_CACHE_HOME", ".cache").join("imsg")
    }
    #[cfg(windows)]
    {
        local_app_data().join("imsg").join("cache")
    }
}
