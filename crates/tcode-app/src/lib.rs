//! The tcode desktop backend.
//!
//! Two shells drive it and neither is in here. `main.rs` is the Tauri one and
//! `bin/sidecar.rs` is the Electron one (`electron/main.js` on the other end of
//! its pipe); both reach the same [`dispatch::Registry`], and everything below
//! that is written against the [`bridge::Emit`] abstraction so the turn-driving
//! path is exercised by tests with no window in sight. See
//! `MIGRATION-ELECTRON.md`.

pub mod address;
pub mod boot;
pub mod bridge;
pub mod browser;
pub mod commands;
pub mod dispatch;
pub mod openers;
pub mod paths;
pub mod picker;
pub mod projects;
pub mod serve;
pub mod sidecar;
pub mod state;
pub mod terminal;
pub mod workspace;
