//! Handing a file or URL to the desktop's default application, and the
//! system clipboard (`imsg-tui`).

/// What [`open_with_default`] handed to the desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opened {
    /// The file or URL itself.
    Target,
    /// The folder holding the file (Windows: a type that is not known to be
    /// a document could run as a program, see [`viewable`]).
    Folder,
}

/// Open `target` (a path or a URL; callers vet URLs) with the default
/// application. Unix: spawns `xdg-open` and does not wait. Windows:
/// `ShellExecuteW` "open" (what `start` does, without cmd.exe's parsing of
/// `&` and `^` in URLs) on a helper thread that initialises COM, waited
/// for. "Opening" a program there runs it, so only files of a known
/// viewable type are opened; for anything else the folder is.
pub fn open_with_default(target: &str) -> Result<Opened, String> {
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open")
            .arg(target)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| Opened::Target)
            .map_err(|e| format!("xdg-open: {e}"))
    }
    #[cfg(windows)]
    {
        let path = std::path::Path::new(target);
        let (what, opened) = match path.parent() {
            Some(dir) if !is_web_url(target) && !viewable(path) && !dir.as_os_str().is_empty() => (dir.as_os_str(), Opened::Folder),
            _ => (std::ffi::OsStr::new(target), Opened::Target),
        };
        let wide: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(what).chain(std::iter::once(0)).collect();
        std::thread::spawn(move || shell_open(&wide)).join().map_err(|_| "ShellExecute panicked".to_string())??;
        Ok(opened)
    }
}

#[cfg(windows)]
fn is_web_url(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.starts_with("https://") || l.starts_with("http://")
}

/// Is `path` a picture, video, sound or plain document that "open" shows
/// rather than runs? An allow-list: everything else (programs, scripts,
/// shortcuts, installers, Office files with macros, unknown types) is
/// shown in its folder instead. The name is read as Win32 does, without
/// trailing dots and spaces (`x.exe.` is `x.exe`).
pub fn viewable(path: &std::path::Path) -> bool {
    const VIEWABLE: &[&str] = &[
        // pictures
        "jpg", "jpeg", "png", "gif", "heic", "heif", "webp", "bmp", "tif", "tiff", "avif", // video
        "mp4", "m4v", "mov", "avi", "mkv", "webm", "3gp", // audio
        "mp3", "m4a", "aac", "wav", "flac", "ogg", "opus", "amr", "caf", // documents
        "pdf", "txt", "vcf",
    ];
    let Some(name) = path.file_name().map(|n| n.to_string_lossy()) else { return false };
    let name = name.trim_end_matches(['.', ' ']);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => VIEWABLE.iter().any(|v| v.eq_ignore_ascii_case(ext)),
        _ => false,
    }
}

#[cfg(windows)]
fn shell_open(target: &[u16]) -> Result<(), String> {
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize};
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let open: Vec<u16> = "open\0".encode_utf16().collect();
    // SAFETY: both strings are NUL-terminated UTF-16 that outlive the call;
    // null HWND / parameters / directory are allowed. ShellExecute wants COM
    // initialised on the calling thread (this short-lived one), balanced by
    // CoUninitialize.
    unsafe {
        let hr = CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32);
        if hr < 0 {
            return Err(format!("CoInitializeEx failed (0x{hr:08x})"));
        }
        let r = ShellExecuteW(std::ptr::null_mut(), open.as_ptr(), target.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
        CoUninitialize();
        // Success is a value greater than 32; below that, an error code.
        let code = r as isize;
        if code > 32 { Ok(()) } else { Err(format!("ShellExecute failed (error {code})")) }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn only_documents_are_opened() {
        for p in ["/d/photo.JPG", "/d/IMG_1.heic", "/d/a.b.pdf", "/d/clip.mov", "/d/memo.m4a", "/d/card.vcf", "/d/x.webp"] {
            assert!(viewable(Path::new(p)), "{p}");
        }
        for p in ["/d/x.exe", "/d/Invoice.PDF.Exe", "/d/evil.exe.", "/d/evil.exe . ", "/d/a.lnk", "/d/s.ps1", "/d/i.msi", "/d/m.docm", "/d/noext", "/d/.pdf", "/d/x.svg"] {
            assert!(!viewable(Path::new(p)), "{p}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn web_urls() {
        assert!(is_web_url("HTTPS://example.com/a.exe") && !is_web_url("file:///c:/x.exe"));
    }
}
