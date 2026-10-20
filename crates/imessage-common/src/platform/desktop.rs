//! Handing a file or URL to the desktop's default application, and the
//! system clipboard (`imsg-tui`).

/// Open `target` (a path or a URL; callers vet URLs) with the default
/// application, fire and forget. Unix: `xdg-open`. Windows: `ShellExecuteW`
/// "open" on a helper thread (which is what `start` does, without cmd.exe's
/// parsing of `&` and `^` in URLs). "Opening" a program there runs it, so a
/// file that would run ([`runs_as_program`]) gets its folder opened instead.
pub fn open_with_default(target: &str) {
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open")
            .arg(target)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(windows)]
    {
        let path = std::path::Path::new(target);
        let target = match path.parent() {
            Some(dir) if !is_web_url(target) && runs_as_program(path) => dir.as_os_str(),
            _ => std::ffi::OsStr::new(target),
        };
        let target: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(target).chain(std::iter::once(0)).collect();
        std::thread::spawn(move || shell_open(&target));
    }
}

#[cfg(windows)]
fn is_web_url(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.starts_with("https://") || l.starts_with("http://")
}

/// Would ShellExecute "open" run this file rather than show it? Its
/// extension is in `%PATHEXT%` or a known script / installer / shortcut type.
#[cfg(windows)]
fn runs_as_program(path: &std::path::Path) -> bool {
    const RUNNABLE: &[&str] = &[
        "exe", "com", "bat", "cmd", "scr", "pif", "cpl", "msi", "msp", "msc", "hta", "js", "jse", "vbs", "vbe", "wsf", "wsh", "ps1",
        "psm1", "lnk", "url", "reg", "jar", "appref-ms", "application", "appx", "msix", "inf", "scf", "settingcontent-ms",
    ];
    let Some(ext) = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()) else { return false };
    let pathext = std::env::var("PATHEXT").unwrap_or_default().to_ascii_lowercase();
    RUNNABLE.contains(&ext.as_str()) || pathext.split(';').any(|p| p.strip_prefix('.') == Some(ext.as_str()))
}

#[cfg(windows)]
fn shell_open(target: &[u16]) {
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx};
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let open: Vec<u16> = "open\0".encode_utf16().collect();
    // SAFETY: both strings are NUL-terminated UTF-16 that outlive the call;
    // null HWND / parameters / directory are allowed. ShellExecute wants COM
    // initialised on the calling thread (this short-lived one).
    unsafe {
        CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32);
        ShellExecuteW(std::ptr::null_mut(), open.as_ptr(), target.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
    }
}

/// Put `s` on the system clipboard. Returns what was used, `None` if
/// nothing worked (the caller may fall back to OSC 52).
/// Unix: `wl-copy` (Wayland) first, then the X11 tools `xclip` and `xsel`.
/// Windows: the Win32 clipboard as `CF_UNICODETEXT` (`clipboard-win`;
/// `clip.exe` mangles non-ANSI text).
pub fn copy_to_clipboard(s: &str) -> Option<&'static str> {
    #[cfg(not(windows))]
    {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let tools: [(&str, &[&str]); 3] = [("wl-copy", &[]), ("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"])];
        for (tool, args) in tools {
            let Ok(mut c) = Command::new(tool).args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else { continue };
            if let Some(mut i) = c.stdin.take() {
                let _ = i.write_all(s.as_bytes());
            }
            if c.wait().map(|st| st.success()).unwrap_or(false) {
                return Some(tool);
            }
        }
        None
    }
    #[cfg(windows)]
    {
        clipboard_win::set_clipboard_string(s).ok().map(|_| "clipboard")
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn programs_are_not_opened() {
        for p in [r"C:\d\x.exe", r"C:\d\Invoice.PDF.Exe", r"C:\d\a.lnk", r"C:\d\s.ps1", r"C:\d\i.msi"] {
            assert!(runs_as_program(Path::new(p)), "{p}");
        }
        for p in [r"C:\d\photo.jpg", r"C:\d\doc.pdf", r"C:\d\noext", r"C:\d\movie.mov"] {
            assert!(!runs_as_program(Path::new(p)), "{p}");
        }
        assert!(is_web_url("HTTPS://example.com/a.exe") && !is_web_url("file:///c:/x.exe"));
    }
}
