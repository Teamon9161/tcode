//! One way to turn a folder the user picked into the path the app keys on.
//!
//! `std::fs::canonicalize` is the right call — two spellings of one folder must
//! not become two projects — but on Windows it returns the verbatim
//! `\\?\C:\code\rust\tcode` form, and that prefix is not merely ugly in a title
//! bar. `store::project_id` folds every non-alphanumeric character into `-`, so
//! a verbatim path lands the desktop app's sessions and auto memory in
//! `----c--code-rust-tcode` while the terminal's live in `c--code-rust-tcode`:
//! the same folder, two histories, neither aware of the other.
//!
//! Stripping it once, here, is what keeps the two frontends looking at the same
//! project. `tcode-core` does the same thing when the model runs `cd`.

use std::path::{Path, PathBuf};

/// Canonicalize `path`, in the spelling the rest of the system uses.
pub fn canonical_dir(path: &Path) -> std::io::Result<PathBuf> {
    Ok(simplify(path.canonicalize()?))
}

/// Drop Windows' verbatim `\\?\` prefix. A no-op everywhere else, and on paths
/// long enough to genuinely need it the prefix stays — dropping it there would
/// hand out a path Windows cannot open.
pub fn simplify(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        const MAX_PATH: usize = 260;
        let text = path.to_string_lossy();
        // UNC verbatim (`\\?\UNC\server\share`) is left alone: the short form
        // needs the prefix rewritten rather than removed, and a wrong guess
        // there is a path that does not resolve.
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            if !rest.starts_with("UNC\\") && rest.len() < MAX_PATH {
                return PathBuf::from(rest);
            }
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_dir_has_no_verbatim_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = canonical_dir(dir.path()).unwrap();
        assert!(
            !resolved.to_string_lossy().starts_with(r"\\?\"),
            "canonicalized {} still carries the verbatim prefix",
            resolved.display()
        );
        // Still the same folder — stripping the prefix must not break opening.
        assert!(resolved.is_dir());
    }

    #[test]
    fn canonical_dir_rejects_what_is_not_there() {
        let dir = tempfile::tempdir().unwrap();
        assert!(canonical_dir(&dir.path().join("absent")).is_err());
    }
}
