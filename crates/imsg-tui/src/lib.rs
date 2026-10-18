//! `imsg-tui`: your iMessages as bubbles in a terminal, backed by the
//! `imessaged` daemon over D-Bus (see docs/TUI.md).
//!
//! The library exists so integration tests can drive [`backend::Backend`]
//! and [`app::App`] against an in-process daemon; the binary is `main.rs`.

pub mod app;
pub mod backend;
pub mod contacts;
pub mod ui;
pub mod view;
