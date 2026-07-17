//! Stable environment facts captured by the harness, never inferred by the model.
//!
//! A session's startup copy belongs in its cached system prefix. Later changes
//! are compared as data and reported as append-only harness notes instead of
//! rewriting that prefix.

use serde::{Deserialize, Serialize};

/// Git facts small enough to include in the startup environment block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitSnapshot {
    pub repository: bool,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub changed_files: usize,
    /// Bounded preview retained for the initial Git block only. Runtime diffs
    /// report the count, not an ever-changing list of paths.
    pub status_preview: Vec<String>,
}

/// Facts about the actual harness runtime, not facts the model must discover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentSnapshot {
    pub cwd: String,
    pub platform: String,
    pub os_version: Option<String>,
    /// Shells tcode's registered command tools can actually execute.
    pub command_shells: Vec<String>,
    pub git: GitSnapshot,
    /// Calendar date when this snapshot was captured, in YYYY-MM-DD form.
    pub date: String,
}

impl EnvironmentSnapshot {
    /// Human-readable changes that affect how the model should operate.
    ///
    /// The directory tree is intentionally absent: it is large, volatile, and
    /// should be inspected on demand instead of invalidating every resume.
    pub fn diff_lines(&self, current: &Self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.cwd != current.cwd {
            lines.push(format!("Working directory: {} → {}", self.cwd, current.cwd));
        }
        if self.platform != current.platform || self.os_version != current.os_version {
            let old = display_os(&self.platform, self.os_version.as_deref());
            let new = display_os(&current.platform, current.os_version.as_deref());
            lines.push(format!("Platform: {old} → {new}"));
        }
        if self.command_shells != current.command_shells {
            lines.push(format!(
                "Command shells: {} → {}",
                display_shells(&self.command_shells),
                display_shells(&current.command_shells)
            ));
        }
        if self.git.repository != current.git.repository {
            lines.push(format!(
                "Git repository: {} → {}",
                yes_no(self.git.repository),
                yes_no(current.git.repository)
            ));
        }
        if self.git.branch != current.git.branch {
            lines.push(format!(
                "Git branch: {} → {}",
                display_optional(self.git.branch.as_deref()),
                display_optional(current.git.branch.as_deref())
            ));
        }
        if self.git.head != current.git.head {
            lines.push(format!(
                "Git HEAD: {} → {}",
                display_optional(self.git.head.as_deref()),
                display_optional(current.git.head.as_deref())
            ));
        }
        if self.git.changed_files != current.git.changed_files {
            lines.push(format!(
                "Git working tree: {} → {} changed file(s)",
                self.git.changed_files, current.git.changed_files
            ));
        }
        if self.date != current.date {
            lines.push(format!("Date: {} → {}", self.date, current.date));
        }
        lines
    }
}

/// The byte-stable, persisted portion of a conversation's system prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupContext {
    pub text: String,
    pub environment: EnvironmentSnapshot,
}

fn display_os(platform: &str, version: Option<&str>) -> String {
    match version.filter(|version| !version.is_empty()) {
        Some(version) => format!("{platform} {version}"),
        None => platform.to_string(),
    }
}

fn display_shells(shells: &[String]) -> String {
    if shells.is_empty() {
        "none".into()
    } else {
        shells.join(", ")
    }
}

fn display_optional(value: Option<&str>) -> &str {
    value.filter(|value| !value.is_empty()).unwrap_or("none")
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> EnvironmentSnapshot {
        EnvironmentSnapshot {
            cwd: "/repo".into(),
            platform: "linux".into(),
            os_version: Some("1.0".into()),
            command_shells: vec!["bash".into()],
            git: GitSnapshot {
                repository: true,
                branch: Some("main".into()),
                head: Some("abc first".into()),
                changed_files: 0,
                status_preview: Vec::new(),
            },
            date: "2026-07-17".into(),
        }
    }

    #[test]
    fn environment_diff_omits_unchanged_facts_and_bounded_status_preview() {
        let old = snapshot();
        assert!(old.diff_lines(&old).is_empty());

        let mut current = old.clone();
        current.cwd = "/other".into();
        current.git.changed_files = 3;
        current.git.status_preview = vec!["M noisy-path".into()];
        assert_eq!(
            old.diff_lines(&current),
            vec![
                "Working directory: /repo → /other",
                "Git working tree: 0 → 3 changed file(s)",
            ]
        );
    }
}
