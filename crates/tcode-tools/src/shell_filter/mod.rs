//! Declarative compaction of successful shell output.
//!
//! Command output is mostly repetition: progress counters, per-package
//! "Compiling …" lines, install banners. It is paid for once in tokens and
//! then again in every cached prefix that carries it. This module is the
//! registry that removes it — a chain of [`OutputFilter`]s, of which most are
//! TOML files and one (`cargo test` folding) is Rust because its rule is a
//! state machine rather than a line predicate.
//!
//! Three properties are structural rather than conventional:
//!
//! - **Only successful output is ever filtered.** Failures go to the model
//!   untouched; that is the agent loop's rule (`Agent::gate`), not this
//!   module's, so no filter can opt into rewriting a diagnostic.
//! - **A filter cannot hide what it removed.** The pointer to the full text is
//!   appended by the harness after this returns. A project's `filters.toml`
//!   comes from the repository, which is data, and data must not be able to
//!   make its own removals invisible.
//! - **Filtering never makes output longer.** A filter that grew the text is
//!   treated as not having applied at all.
//!
//! # What belongs here, and what belongs to the output gate
//!
//! Both layers shorten tool output, and the line between them is whether the
//! decision depends on size:
//!
//! - **A filter removes what is never worth keeping**, at any size. A
//!   `Compiling …` line and a `(use "git add …")` hint carry no information
//!   in a ten-line output or a ten-thousand-line one.
//! - **The gate (`BlobStore`) removes what is only a burden when large.** A
//!   diff's context lines explain the change in a small diff and drown it in a
//!   huge one; that is a budget question, and the budget lives there.
//!
//! This is why there is no `git diff` filter. Its only size-independent noise
//! is the `index`/`---`/`+++` headers — about 4% of a real diff — while a big
//! diff is already over budget, so filtering it first would spill the original
//! to one file and the gate would spill the filtered text to a second,
//! handing the model two pointers to two nearly identical files. Cutting the
//! context lines instead would save far more, but that is the size-dependent
//! judgement the gate already makes, with `diff_summary` preserving the file
//! list the truncation would otherwise hide.

mod cargo_test;
mod toml_def;

use std::path::Path;
use std::sync::{Arc, RwLock};

use tcode_core::blobs::approx_tokens;
use tcode_core::cwd_scope::CwdScoped;
use tcode_core::Compacted;

/// Built-in filters: every `builtin/*.toml`, concatenated in name order by
/// `build.rs` and embedded at compile time.
const BUILTIN_TOML: &str = include_str!(concat!(env!("OUT_DIR"), "/builtin_filters.toml"));

/// One rule for shortening a command's successful output.
///
/// `None` means "not mine, or nothing to remove" — the same answer, because
/// the chain treats both the same way and the caller only cares whether some
/// filter produced a shorter text.
pub trait OutputFilter: Send + Sync + std::fmt::Debug {
    /// Identifies the filter for shadowing and for diagnostics. A project
    /// filter replaces the built-in of the same name.
    fn name(&self) -> &str;
    fn apply(&self, command: &str, output: &str) -> Option<String>;
}

/// The filter chain, in priority order: project, then user, then built-in.
///
/// The chain is behind a lock because the project half follows the working
/// directory (see the [`CwdScoped`] implementation). The user and built-in
/// halves are compiled once — they do not depend on where the conversation
/// currently is, and recompiling their regexes on every `/cd` would be waste.
#[derive(Debug)]
pub struct ShellFilters {
    chain: RwLock<Arc<Vec<Arc<dyn OutputFilter>>>>,
    /// User and built-in filters, already compiled, in priority order.
    stable: Vec<Arc<dyn OutputFilter>>,
    enabled: bool,
}

impl ShellFilters {
    /// Every successful output passes through untouched. This is what
    /// `[limits] shell_output_filters = false` selects.
    pub fn disabled() -> Self {
        Self {
            chain: RwLock::new(Arc::new(Vec::new())),
            stable: Vec::new(),
            enabled: false,
        }
    }

    /// Built-in and user filters only, for callers assembling a toolset
    /// without a project directory to read.
    pub fn builtin() -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let stable = stable_chain(&mut warnings);
        let filters = Self {
            chain: RwLock::new(Arc::new(stable.clone())),
            stable,
            enabled: true,
        };
        (filters, warnings)
    }

    /// The full chain for `cwd`, including its `.tcode/filters.toml`.
    /// Warnings are returned rather than printed: this is a library, and the
    /// frontend decides where a startup complaint goes.
    pub fn load(cwd: &Path) -> (Self, Vec<String>) {
        let (filters, mut warnings) = Self::builtin();
        warnings.extend(filters.reload_project(cwd));
        (filters, warnings)
    }

    /// Replace the project half of the chain from `cwd`, keeping the compiled
    /// user and built-in halves.
    fn reload_project(&self, cwd: &Path) -> Vec<String> {
        if !self.enabled {
            return Vec::new();
        }
        let mut warnings = Vec::new();
        let path = cwd.join(".tcode").join("filters.toml");
        let project = read_filters(&path, &mut warnings);

        let shadowed: Vec<&str> = self
            .stable
            .iter()
            .map(|f| f.name())
            .filter(|name| project.iter().any(|p| p.name() == *name))
            .collect();
        if !shadowed.is_empty() {
            warnings.push(format!(
                "{}: replaces filter(s) {}",
                path.display(),
                shadowed.join(", ")
            ));
        }

        let mut chain = project;
        let taken: Vec<&str> = chain.iter().map(|f| f.name()).collect();
        let kept: Vec<Arc<dyn OutputFilter>> = self
            .stable
            .iter()
            .filter(|f| !taken.contains(&f.name()))
            .cloned()
            .collect();
        chain.extend(kept);
        *self.chain.write().expect("filter chain lock") = Arc::new(chain);
        warnings
    }

    /// Shorten `output` if some filter claims it. `None` leaves the original
    /// text alone, and is also the answer when a filter's result would not be
    /// any smaller — spending a spill file and a pointer line to save nothing
    /// is a loss.
    pub fn apply(&self, command: &str, output: &str) -> Option<Compacted> {
        if !self.enabled {
            return None;
        }
        // The harness's own status line is not the command's output and no
        // filter gets a say over it: it is lifted off before the chain runs
        // and put back afterwards. That also lets a filter's rules — and its
        // inline tests — be written against the bare output of the tool.
        let (body, status) = split_status_line(output);
        let chain = self.chain.read().expect("filter chain lock").clone();
        let (by, filtered) = chain
            .iter()
            .find_map(|filter| Some((filter.name(), filter.apply(command, body)?)))?;
        if approx_tokens(&filtered) >= approx_tokens(body) {
            return None;
        }
        Some(Compacted {
            text: match status {
                Some(status) => format!("{filtered}\n{status}"),
                None => filtered,
            },
            by: by.to_string(),
        })
    }

    /// Filter names in priority order, for tests and diagnostics.
    pub fn names(&self) -> Vec<String> {
        self.chain
            .read()
            .expect("filter chain lock")
            .iter()
            .map(|f| f.name().to_string())
            .collect()
    }
}

/// The project's filters follow the conversation's directory. Without this a
/// `/cd` into another repository would keep applying the first repository's
/// rules — silently, since a filter that does not fire looks exactly like a
/// filter that has nothing to remove.
impl CwdScoped for ShellFilters {
    fn rescope(&self, cwd: &Path) -> Vec<String> {
        self.reload_project(cwd)
    }
}

/// User filters, then built-ins, then the one native filter. The native filter
/// sits last so a user or project rule for the same output wins by being
/// earlier, not by being special-cased.
fn stable_chain(warnings: &mut Vec<String>) -> Vec<Arc<dyn OutputFilter>> {
    let mut chain: Vec<Arc<dyn OutputFilter>> = Vec::new();
    if let Ok(dir) = tcode_core::config::Config::global_path() {
        chain.extend(read_filters(&dir.join("filters.toml"), warnings));
    }
    let builtin = toml_def::parse_file(BUILTIN_TOML)
        .expect("built-in filters are validated by the test suite");
    let from_user: Vec<&str> = chain.iter().map(|f| f.name()).collect();
    let builtin: Vec<Arc<dyn OutputFilter>> = builtin
        .into_iter()
        .filter(|(f, _)| !from_user.contains(&f.name()))
        .map(|(f, _)| Arc::new(f) as Arc<dyn OutputFilter>)
        .collect();
    chain.extend(builtin);
    chain.push(Arc::new(cargo_test::CargoTestFilter));
    chain
}

/// Read one filter file. A missing file is the normal case; a broken one costs
/// its own filters and says so, rather than taking down the session over a
/// directory the user merely walked into.
fn read_filters(path: &Path, warnings: &mut Vec<String>) -> Vec<Arc<dyn OutputFilter>> {
    let Ok(source) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    match toml_def::parse_file(&source) {
        Ok(filters) => filters
            .into_iter()
            .map(|(f, _)| Arc::new(f) as Arc<dyn OutputFilter>)
            .collect(),
        Err(error) => {
            warnings.push(format!("{}: {error}", path.display()));
            Vec::new()
        }
    }
}

/// Split the harness's trailing `(exit code N)` line off the output.
fn split_status_line(output: &str) -> (&str, Option<&str>) {
    let Some(start) = output.rfind("\n(exit code ") else {
        return (output, None);
    };
    let status = &output[start + 1..];
    if status.ends_with(')') && !status.contains('\n') {
        (&output[..start], Some(status))
    } else {
        (output, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_filters_path(cwd: &Path) -> std::path::PathBuf {
        cwd.join(".tcode").join("filters.toml")
    }

    /// Most assertions care about the resulting text; the attribution has its
    /// own test.
    fn text(filters: &ShellFilters, command: &str, output: &str) -> Option<String> {
        filters.apply(command, output).map(|c| c.text)
    }

    /// Every shipped filter must parse and compile. `stable_chain` would
    /// panic, but a named failure beats a panic inside an unrelated test.
    #[test]
    fn builtin_filters_all_compile() {
        let filters = toml_def::parse_file(BUILTIN_TOML).expect("builtin filters compile");
        assert!(filters.len() >= 6, "expected the shipped filter set");
    }

    /// A filter without a test is a regex nobody has ever seen run.
    #[test]
    fn every_builtin_filter_has_inline_tests_and_they_pass() {
        for (filter, cases) in toml_def::parse_file(BUILTIN_TOML).unwrap() {
            assert!(
                !cases.is_empty(),
                "builtin filter '{}' has no inline tests",
                filter.name()
            );
            for case in cases {
                assert_eq!(
                    filter.transform(&case.input),
                    case.expected,
                    "builtin filter '{}' test '{}'",
                    filter.name(),
                    case.name
                );
            }
        }
    }

    #[test]
    fn builtin_filter_names_are_unique() {
        let mut names: Vec<String> = toml_def::parse_file(BUILTIN_TOML)
            .unwrap()
            .iter()
            .map(|(f, _)| f.name().to_string())
            .collect();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate builtin filter name");
    }

    #[test]
    fn a_disabled_chain_passes_everything_through() {
        assert_eq!(
            text(
                &ShellFilters::disabled(),
                "cargo build",
                "Compiling x\ndone"
            ),
            None
        );
    }

    #[test]
    fn the_status_line_survives_a_filter_that_would_have_eaten_it() {
        let (filters, _) = ShellFilters::builtin();
        let output = "\
   Compiling tcode-core v0.1.8
   Compiling tcode-tools v0.1.8
    Finished `dev` profile in 5.89s
(exit code 0)";
        let filtered = text(&filters, "cargo build --workspace", output).unwrap();
        assert!(filtered.ends_with("\n(exit code 0)"), "{filtered}");
        assert!(!filtered.contains("Compiling"), "{filtered}");
    }

    /// The reduction is attributed. Naming the rule is what tells a reader
    /// whether the removal was the boring kind — a count cannot — and it stops
    /// a repository-supplied rule from editing a tool's output anonymously.
    #[test]
    fn the_reduction_names_the_rule_that_made_it() {
        let (filters, _) = ShellFilters::builtin();
        let applied = filters
            .apply(
                "cargo build",
                "   Compiling tcode-core v0.1.8\n   Compiling tcode-tools v0.1.8\n    Finished in 5.89s",
            )
            .unwrap();
        assert_eq!(applied.by, "cargo-build");
    }

    /// The pointer line costs tokens too. A "reduction" that does not reduce
    /// is worse than nothing, so it is not treated as one.
    #[test]
    fn a_filter_that_does_not_shrink_the_output_is_not_applied() {
        let (filters, _) = ShellFilters::builtin();
        assert_eq!(
            text(&filters, "cargo build", "    Finished\n(exit code 0)"),
            None
        );
    }

    #[test]
    fn a_project_filter_replaces_the_builtin_of_the_same_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".tcode")).unwrap();
        std::fs::write(
            project_filters_path(dir.path()),
            r#"
[filters.cargo-build]
match_command = "cargo build"
on_empty = "shadowed"
strip_lines_matching = ["."]
"#,
        )
        .unwrap();

        let (filters, warnings) = ShellFilters::load(dir.path());
        assert!(
            warnings.iter().any(|w| w.contains("cargo-build")),
            "{warnings:?}"
        );
        // The shadowed built-in is gone, not merely outranked.
        assert_eq!(
            filters
                .names()
                .iter()
                .filter(|n| *n == "cargo-build")
                .count(),
            1
        );
        assert_eq!(
            text(
                &filters,
                "cargo build",
                "   Compiling x\n    Finished\n(exit code 0)"
            ),
            Some("shadowed\n(exit code 0)".into())
        );
    }

    /// The point of `CwdScoped`: the rules must come from the directory the
    /// conversation is in now, not the one the process started in.
    #[test]
    fn rescoping_swaps_the_project_filters() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        for (dir, message) in [(&first, "from-first"), (&second, "from-second")] {
            std::fs::create_dir_all(dir.path().join(".tcode")).unwrap();
            std::fs::write(
                project_filters_path(dir.path()),
                format!(
                    "[filters.demo]\nmatch_command = \"demo\"\n\
                     strip_lines_matching = [\".\"]\non_empty = \"{message}\"\n"
                ),
            )
            .unwrap();
        }

        let (filters, _) = ShellFilters::load(first.path());
        assert_eq!(
            text(&filters, "demo", "noise noise noise"),
            Some("from-first".into())
        );

        filters.rescope(second.path());
        assert_eq!(
            text(&filters, "demo", "noise noise noise"),
            Some("from-second".into())
        );

        // Leaving both, the project filter is simply gone.
        let empty = tempfile::tempdir().unwrap();
        filters.rescope(empty.path());
        assert_eq!(text(&filters, "demo", "noise noise noise"), None);
    }

    #[test]
    fn a_broken_project_file_costs_only_its_own_filters() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".tcode")).unwrap();
        std::fs::write(project_filters_path(dir.path()), "[filters.oops\n").unwrap();

        let (filters, warnings) = ShellFilters::load(dir.path());
        assert!(
            warnings.iter().any(|w| w.contains("filters.toml")),
            "{warnings:?}"
        );
        assert!(filters.names().iter().any(|n| n == "cargo-test"));
    }

    #[test]
    fn the_status_line_is_only_taken_when_it_really_is_one() {
        assert_eq!(split_status_line("a\nb"), ("a\nb", None));
        assert_eq!(
            split_status_line("a\n(exit code 1)"),
            ("a", Some("(exit code 1)"))
        );
        // A line that merely starts like one stays part of the output.
        assert_eq!(
            split_status_line("a\n(exit code 1) and more\nb"),
            ("a\n(exit code 1) and more\nb", None)
        );
    }
}
