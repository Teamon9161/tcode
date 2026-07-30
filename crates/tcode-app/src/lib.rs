//! The tcode desktop backend.
//!
//! `main.rs` is the Tauri shell; everything it does lives here, written
//! against the [`bridge::Emit`] abstraction so the turn-driving path is
//! exercised by tests with no window in sight.

pub mod boot;
pub mod bridge;
pub mod commands;
pub mod openers;
pub mod paths;
pub mod picker;
pub mod projects;
pub mod state;
pub mod workspace;
