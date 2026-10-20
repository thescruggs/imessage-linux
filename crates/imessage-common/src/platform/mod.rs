//! The OS-specific glue `imessaged`, `imsg` and `imsg-tui` share, so call
//! sites stay free of `cfg`s: owner-only files ([`fs`]), the shell that runs
//! user-configured commands ([`process`]), opening things and the clipboard
//! ([`desktop`]) and, on Windows, DPAPI ([`dpapi`]).
//!
//! Unix behaviour is what the call sites did before this module existed;
//! Windows branches are the port (docs/WINDOWS-PLAN.md, phase W1).
//! Filesystem locations stay in [`crate::paths`].

pub mod desktop;
#[cfg(windows)]
pub mod dpapi;
pub mod fs;
pub mod process;
