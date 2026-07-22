//! Where the harness's own state lives.
//!
//! Everything tcode owns — `~/.tcode/config.toml`, `~/.tcode/projects/*`,
//! provider credentials, imported logs — resolves through [`home_dir`], so
//! one variable relocates all of it at once.

use std::path::PathBuf;

/// Overrides the user's home directory for everything tcode stores.
pub const TCODE_HOME: &str = "TCODE_HOME";

/// The directory tcode keeps its state under: `TCODE_HOME` when set, the
/// user's home otherwise. Resolved per call rather than once, so a process
/// that redirects its state at startup (a portable install, a sandboxed run,
/// a test) is not racing a cached value.
pub fn home_dir() -> Option<PathBuf> {
    match std::env::var_os(TCODE_HOME) {
        Some(home) if !home.is_empty() => Some(PathBuf::from(home)),
        _ => dirs::home_dir(),
    }
}

/// Test isolation. Not `#[cfg(test)]`: the tests that need it live in other
/// crates, which link this one as an ordinary dependency.
#[doc(hidden)]
pub mod testing {
    use super::TCODE_HOME;
    use std::path::PathBuf;
    use std::sync::OnceLock;

    /// Redirect this process's harness state into a private temporary
    /// directory, once, and return it.
    ///
    /// Anything that reads or writes `~/.tcode` — `ToolCtx`, `MemoryManager`,
    /// `Config` — must be constructed after this call in a test, or it writes
    /// into the developer's real home: a run of the suite otherwise leaves
    /// one project directory per temporary working directory behind, and
    /// those accumulate in the thousands.
    pub fn temp_home() -> PathBuf {
        static HOME: OnceLock<PathBuf> = OnceLock::new();
        HOME.get_or_init(|| {
            // One parent for every test process, so the whole lot is a single
            // directory to delete; one child per process, so parallel test
            // binaries cannot write each other's config.
            let home = std::env::temp_dir()
                .join("tcode-test-home")
                .join(std::process::id().to_string());
            let _ = std::fs::create_dir_all(&home);
            std::env::set_var(TCODE_HOME, &home);
            home
        })
        .clone()
    }
}
