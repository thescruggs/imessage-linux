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
/// Windows has no FIFOs: the path is checked before opening (named pipes
/// and devices live under `\\.\`, which is not a regular file) and the open
/// handle again.
pub fn open_regular_file(path: &Path) -> io::Result<File> {
    let not_regular = || io::Error::new(io::ErrorKind::InvalidInput, "not a regular file");
    #[cfg(unix)]
    let f = OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path)?;
    #[cfg(not(unix))]
    let f = {
        if !std::fs::metadata(path)?.is_file() {
            return Err(not_regular());
        }
        File::open(path)?
    };
    if !f.metadata()?.is_file() {
        return Err(not_regular());
    }
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn private_file_and_dir() {
        let t = std::env::temp_dir().join(format!("imessage-common-fs-{}", std::process::id()));
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
        assert_eq!(open_regular_file(&dir).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert!(open_regular_file(&dir.join("missing")).is_err());
        std::fs::remove_dir_all(&t).unwrap();
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
