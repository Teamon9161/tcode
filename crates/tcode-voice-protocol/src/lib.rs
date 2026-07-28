//! Compatibility metadata shared by the tcode TUI and its voice sidecar.
//!
//! Bump this package when a voice-sidecar release is required. Ordinary tcode
//! releases keep using this version and therefore reuse the installed binary.

/// Release version of the sidecar the TUI installs.
pub const SIDECAR_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Tag that publishes the sidecar assets, separate from the main tcode tag.
pub const RELEASE_TAG: &str = concat!("voice-v", env!("CARGO_PKG_VERSION"));
