//! Filesystem locations shared by `imessaged` and `imsg`.

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

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// `$VAR` if it is set to an absolute path (per the XDG spec, relative values
/// are ignored), else `~/<fallback>`.
fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
}

/// Default daemon state directory (keys, registration, hwconfig, store):
/// `$XDG_STATE_HOME/imessaged`, else `~/.local/state/imessaged`.
pub fn default_state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("imessaged")
}

/// Downloaded Apple files (`IMDAppleServices`, `cert-1.0.plist`), written by
/// `tools/bootstrap.sh`: `${XDG_DATA_HOME:-~/.local/share}/imessage-linux`.
pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("imessage-linux")
}

/// Where KPeople (kmessages' contacts) reads vCards from and `imsg
/// sync-mac` writes the Mac's: `${XDG_DATA_HOME:-~/.local/share}/kpeoplevcard`.
pub fn kpeople_vcard_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("kpeoplevcard")
}

/// `imsg`'s own state (the pins the last `imsg sync-mac` applied):
/// `${XDG_STATE_HOME:-~/.local/state}/imsg`.
pub fn imsg_state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("imsg")
}
