use std::path::Path;
use std::process::Command;

use tcode_core::{EnvironmentSnapshot, GitSnapshot, StartupContext};

use crate::shell;

/// Build the byte-stable startup context for a new conversation. The scratch
/// parameter remains in the signature so existing frontends can call it, while
/// the exact path comes from the stable `${TCODE_SCRATCH_DIR}` substitution.
pub fn startup_context_with_scratch(cwd: &Path, _scratch: &Path) -> StartupContext {
    let environment = environment_snapshot(cwd);
    let mut out = String::new();

    let tree = dir_tree(cwd);
    if !tree.is_empty() {
        out.push_str(&format!(
            "# Project layout (2 levels, gitignore-aware)\n{tree}"
        ));
    }

    out.push_str(&tcode_core::MemoryManager::new(cwd).startup_prompt());
    out.push_str(&render_environment(&environment));
    out.push_str(&render_git(&environment.git));
    StartupContext {
        text: out,
        environment,
    }
}

/// The live runtime facts used to compare an inactive session with the current
/// process. Keep this separate from the project tree: the tree is startup
/// context, not a volatile fact to rescan and diff on every resume.
pub fn environment_snapshot(cwd: &Path) -> EnvironmentSnapshot {
    let info = os_info::get();
    EnvironmentSnapshot {
        cwd: cwd.display().to_string(),
        platform: info.os_type().to_string(),
        os_version: Some(info.version().to_string()).filter(|version| !version.is_empty()),
        command_shells: command_shells(),
        git: git_snapshot(cwd),
        date: chrono_date(),
    }
}

/// Compatibility wrapper for callers that only need the text portion.
pub fn project_map_with_scratch(cwd: &Path, scratch: &Path) -> String {
    startup_context_with_scratch(cwd, scratch).text
}

/// Convenience wrapper for callers that do not own a live `ToolCtx`.
pub fn project_map(cwd: &Path) -> String {
    project_map_with_scratch(cwd, &tcode_core::store::scratchpad_dir(cwd))
}

fn command_shells() -> Vec<String> {
    if cfg!(windows) {
        let mut shells = vec!["PowerShell".into()];
        if shell::bash_available() {
            shells.push("Git Bash".into());
        }
        shells
    } else {
        vec!["bash".into()]
    }
}

fn render_environment(environment: &EnvironmentSnapshot) -> String {
    let version = environment
        .os_version
        .as_deref()
        .filter(|version| !version.is_empty())
        .map(|version| format!(" {version}"))
        .unwrap_or_default();
    format!(
        "\n# Environment\nworking directory: {}\nplatform: {}{}\ncommand shells: {}\ndate: {}\n",
        environment.cwd,
        environment.platform,
        version,
        environment.command_shells.join(", "),
        environment.date,
    )
}

fn render_git(git: &GitSnapshot) -> String {
    if !git.repository {
        return "\n# Git\nNot a git repository.\n".into();
    }
    let mut out = format!(
        "\n# Git\nbranch: {}\nlast commit: {}\n",
        git.branch.as_deref().unwrap_or("(detached HEAD)"),
        git.head.as_deref().unwrap_or("unknown")
    );
    if git.changed_files == 0 {
        out.push_str("working tree: clean\n");
    } else {
        out.push_str(&format!(
            "working tree: {} changed file(s)\n",
            git.changed_files
        ));
        for line in &git.status_preview {
            out.push_str(&format!("  {line}\n"));
        }
        if git.changed_files > git.status_preview.len() {
            out.push_str(&format!(
                "  … (+{} more)\n",
                git.changed_files - git.status_preview.len()
            ));
        }
    }
    out
}

fn chrono_date() -> String {
    // Date without pulling in chrono: seconds since epoch → civil date.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    civil_from_days(days as i64)
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(z: i64) -> String {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_snapshot(cwd: &Path) -> GitSnapshot {
    let Some(branch) = git(cwd, &["branch", "--show-current"]) else {
        return GitSnapshot::default();
    };
    let status = git(cwd, &["status", "--porcelain"]).unwrap_or_default();
    let changed_files = status.lines().count();
    GitSnapshot {
        repository: true,
        branch: (!branch.is_empty()).then_some(branch),
        head: git(cwd, &["log", "-1", "--format=%h %s"]).filter(|head| !head.is_empty()),
        changed_files,
        status_preview: status.lines().take(15).map(str::to_owned).collect(),
    }
}

/// Overall budget for the layout section of the system prompt.
const TREE_MAX_ENTRIES: usize = 80;
/// One crowded directory (generated assets, fixtures…) must not spend the
/// whole budget before its siblings appear, so children are capped per dir.
const TREE_MAX_PER_DIR: usize = 20;

fn dir_tree(cwd: &Path) -> String {
    use std::collections::BTreeMap;

    let walker = ignore::WalkBuilder::new(cwd)
        .max_depth(Some(2))
        .hidden(true)
        .sort_by_file_name(std::cmp::Ord::cmp)
        .build();
    // parent (relative, "" = root) → child display names.
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in walker.flatten() {
        let path = entry.path();
        if path == cwd {
            continue;
        }
        let Ok(rel) = path.strip_prefix(cwd) else {
            continue;
        };
        let parent = rel
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = rel.file_name().unwrap_or_default().to_string_lossy();
        let name = if path.is_dir() {
            format!("{name}/")
        } else {
            name.into_owned()
        };
        children.entry(parent).or_default().push(name);
    }

    let mut entries: Vec<String> = Vec::new();
    let emit = |names: &[String], indent: &str, entries: &mut Vec<String>| {
        for name in names.iter().take(TREE_MAX_PER_DIR) {
            entries.push(format!("{indent}{name}"));
        }
        if names.len() > TREE_MAX_PER_DIR {
            entries.push(format!(
                "{indent}… (+{} more)",
                names.len() - TREE_MAX_PER_DIR
            ));
        }
    };
    let top = children.remove("").unwrap_or_default();
    for name in top.iter().take(TREE_MAX_PER_DIR) {
        entries.push(name.clone());
        if let Some(sub) = name.strip_suffix('/').and_then(|d| children.get(d)) {
            emit(sub, "  ", &mut entries);
        }
        if entries.len() >= TREE_MAX_ENTRIES {
            entries.push("… (truncated)".into());
            break;
        }
    }
    if top.len() > TREE_MAX_PER_DIR {
        entries.push(format!(
            "… (+{} more top-level entries)",
            top.len() - TREE_MAX_PER_DIR
        ));
    }
    if entries.is_empty() {
        return String::new();
    }
    entries.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_context_ends_with_environment_and_git_blocks() {
        let context = startup_context_with_scratch(Path::new("."), Path::new("/scratch"));
        let environment = context.text.rfind("# Environment").unwrap();
        let git = context.text.rfind("# Git").unwrap();
        assert!(environment < git);
        assert!(context.text[environment..].contains("command shells:"));
    }
}
