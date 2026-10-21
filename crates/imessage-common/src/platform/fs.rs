//! Owner-only files and directories.
//!
//! Unix: created files are mode 0600 and directories 0700, and the mode
//! helpers read and tighten permission bits. Windows has no mode bits: files
//! inherit the ACL of the per-user `%LOCALAPPDATA%` tree they live in, so
//! [`mode`] is `None`, [`set_mode`] does nothing and everything counts as
//! private. TODO(W6): set an explicit owner-only DACL on what we create.

use std::fs::{DirBuilder, File, Metadata, OpenOptions};
use std::io;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

/// `OpenOptions` whose newly created file is owner-only (unix: mode 0600).
/// Chain the access flags as usual:
/// `private_options().write(true).create_new(true).open(p)`.
pub fn private_options() -> OpenOptions {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut o = OpenOptions::new();
    #[cfg(unix)]
    o.mode(0o600);
    o
}

/// `mkdir -p` with mode 0700 for every directory it creates. Existing
/// directories are left as they are.
pub fn create_private_dir_all(dir: &Path) -> io::Result<()> {
    let mut b = DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    b.mode(0o700);
    b.create(dir)
}

/// Permission bits (`mode & 0o777`); `None` where there are none (Windows).
pub fn mode(meta: &Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        Some(meta.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// Neither group nor others have any access (always true without mode bits).
pub fn is_private(meta: &Metadata) -> bool {
    mode(meta).is_none_or(|m| m & 0o077 == 0)
}

/// `chmod path mode`; a no-op where there are no mode bits.
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// Open `path` for reading only if it is a regular file, without ever
/// blocking on a FIFO or device. Unix: opened `O_NONBLOCK`, then checked.
/// Windows: opening a named pipe (`\\.\pipe\...`) or device does not block;
/// the open handle must be a disk file (`GetFileType`), since a pipe can
/// pass `Metadata::is_file`.
pub fn open_regular_file(path: &Path) -> io::Result<File> {
    let not_regular = || io::Error::new(io::ErrorKind::InvalidInput, "not a regular file");
    #[cfg(unix)]
    let f = OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path)?;
    #[cfg(not(unix))]
    let f = File::open(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_DISK, GetFileType};
        // SAFETY: `f` owns a valid handle for the duration of the call.
        if unsafe { GetFileType(f.as_raw_handle()) } != FILE_TYPE_DISK {
            return Err(not_regular());
        }
    }
    if !f.metadata()?.is_file() {
        return Err(not_regular());
    }
    Ok(f)
}

/// Take an exclusive lock on `path` (created owner-only if missing) for as
/// long as the returned file stays open; `Ok(None)` if another process
/// holds it. Unix: `flock(LOCK_EX | LOCK_NB)`. Windows: the file opened
/// with no sharing, so a second open fails with a sharing violation.
pub fn lock_exclusive(path: &Path) -> io::Result<Option<File>> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let f = private_options().read(true).write(true).create(true).truncate(false).open(path)?;
        // SAFETY: `f` owns a valid descriptor for the duration of the call.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(f));
        }
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::WouldBlock { Ok(None) } else { Err(e) }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const ERROR_SHARING_VIOLATION: i32 = 32;
        match private_options().read(true).write(true).create(true).truncate(false).share_mode(0).open(path) {
            Ok(f) => Ok(Some(f)),
            Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Device names Win32 reserves in every directory, with any extension.
const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
    "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// `name` (one path component) as Win32 would create it, made harmless:
/// trailing dots and spaces (which Win32 silently drops, so `a.exe.`
/// becomes `a.exe`) removed, and a reserved device name (`CON`, `nul.txt`,
/// `COM1.png`, ...) prefixed with `_`. Pure, so it is tested everywhere.
pub fn windows_safe_component(name: &str) -> String {
    let name = name.trim_end_matches(['.', ' ']);
    let stem = name.split('.').next().unwrap_or("").trim_end_matches(' ');
    if WINDOWS_RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) { format!("_{name}") } else { name.to_string() }
}

/// [`windows_safe_component`] on Windows; unchanged elsewhere.
pub fn platform_safe_component(name: &str) -> String {
    #[cfg(windows)]
    {
        windows_safe_component(name)
    }
    #[cfg(not(windows))]
    {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn private_file_and_dir() {
        let t = std::env::temp_dir().join(format!("imessage-common-fs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t); // left over from a failed run
        let dir = t.join("a").join("b");
        create_private_dir_all(&dir).unwrap();
        let p = dir.join("f");
        private_options().write(true).create_new(true).open(&p).unwrap().write_all(b"x").unwrap();
        assert!(private_options().write(true).create_new(true).open(&p).is_err(), "create_new");
        let meta = std::fs::metadata(&p).unwrap();
        assert!(is_private(&meta));
        assert!(is_private(&std::fs::metadata(&dir).unwrap()));
        #[cfg(unix)]
        {
            assert_eq!(mode(&meta), Some(0o600));
            assert_eq!(mode(&std::fs::metadata(&dir).unwrap()), Some(0o700));
            set_mode(&p, 0o644).unwrap();
            assert!(!is_private(&std::fs::metadata(&p).unwrap()));
        }
        #[cfg(not(unix))]
        {
            assert_eq!(mode(&meta), None);
            set_mode(&p, 0o644).unwrap();
        }
        let mut s = String::new();
        open_regular_file(&p).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "x");
        // Windows refuses to open a directory as a file (PermissionDenied).
        #[cfg(unix)]
        assert_eq!(open_regular_file(&dir).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert!(open_regular_file(&dir).is_err());
        assert!(open_regular_file(&dir.join("missing")).is_err());
        std::fs::remove_dir_all(&t).unwrap();
    }

    #[test]
    fn exclusive_lock() {
        let t = std::env::temp_dir().join(format!("imessage-common-lock-{}", std::process::id()));
        let _ = std::fs::remove_file(&t);
        let held = lock_exclusive(&t).unwrap().expect("first lock");
        assert!(lock_exclusive(&t).unwrap().is_none(), "second lock while held");
        drop(held);
        assert!(lock_exclusive(&t).unwrap().is_some(), "lock again after release");
        std::fs::remove_file(&t).unwrap();
    }

    #[test]
    fn windows_components() {
        for (i, o) in [
            ("evil.exe.", "evil.exe"),
            ("evil.exe . .", "evil.exe"),
            ("photo.jpg", "photo.jpg"),
            ("CON", "_CON"),
            ("nul.txt", "_nul.txt"),
            ("Com1.png", "_Com1.png"),
            ("lpt9", "_lpt9"),
            ("aux .txt", "_aux .txt"),
            ("conin$", "_conin$"),
            ("console.log", "console.log"),
            ("COM10", "COM10"),
            ("...", ""),
        ] {
            assert_eq!(windows_safe_component(i), o, "{i:?}");
        }
        #[cfg(not(windows))]
        assert_eq!(platform_safe_component("CON."), "CON.");
        #[cfg(windows)]
        assert_eq!(platform_safe_component("CON."), "_CON");
    }

    /// A named pipe or a device is not a regular file (a pipe can pass
    /// `is_file`).
    #[cfg(windows)]
    #[test]
    fn open_regular_file_refuses_pipe_and_device() {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
        use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT};
        let name = format!(r"\\.\pipe\imessage-common-test-{}", std::process::id());
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // One listening instance per client open.
        // SAFETY: NUL-terminated name, no security attributes; closed below.
        let servers = [(); 2].map(|_| unsafe {
            CreateNamedPipeW(wide.as_ptr(), PIPE_ACCESS_DUPLEX, PIPE_TYPE_BYTE | PIPE_WAIT, PIPE_UNLIMITED_INSTANCES, 512, 512, 0, std::ptr::null())
        });
        assert!(servers.iter().all(|&h| h != INVALID_HANDLE_VALUE));
        let client = File::open(&name).expect("the pipe itself opens");
        assert_eq!(open_regular_file(Path::new(&name)).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        drop(client);
        for h in servers {
            // SAFETY: our handles, each closed once.
            unsafe { CloseHandle(h) };
        }
        assert_eq!(open_regular_file(Path::new(r"\\.\NUL")).unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    #[test]
    fn open_regular_file_refuses_fifo() {
        let t = std::env::temp_dir().join(format!("imessage-common-fifo-{}", std::process::id()));
        std::fs::create_dir_all(&t).unwrap();
        let fifo = t.join("fifo");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert_eq!(open_regular_file(&fifo).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        std::fs::remove_dir_all(&t).unwrap();
    }
}
