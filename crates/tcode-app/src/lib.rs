//! The tcode desktop backend.
//!
//! One shell drives it: `bin/sidecar.rs` is the backend the Electron main
//! process spawns (`electron/main.js` on the other end of its pipe). Everything
//! below is written against the [`bridge::Emit`] abstraction so the turn-driving
//! path is exercised by tests with no window in sight.

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
