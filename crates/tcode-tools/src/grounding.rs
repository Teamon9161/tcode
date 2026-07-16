use std::path::Path;
use std::process::Command;

/// Build the opening context. The scratch parameter remains in the signature
/// so existing frontends can call it, while the exact path now comes from the
/// stable `${TCODE_SCRATCH_DIR}` system-prompt substitution.
pub fn project_map_with_scratch(cwd: &Path, _scratch: &Path) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Environment\nplatform: {}\ncwd: {}\ndate: {}\n",
        std::env::consts::OS,
        cwd.display(),
        chrono_date(),
    ));

    match git_summary(cwd) {
        Some(git) => out.push_str(&format!("\n# Git\n{git}\n")),
        None => out.push_str("\nNot a git repository.\n"),
    }

    let tree = dir_tree(cwd);
    if !tree.is_empty() {
        out.push_str(&format!(
            "\n# Project layout (2 levels, gitignore-aware)\n{tree}"
        ));
    }

    out.push_str(&tcode_core::MemoryManager::new(cwd).startup_prompt());
    out
}

/// Convenience wrapper for callers that do not own a live `ToolCtx`.
pub fn project_map(cwd: &Path) -> String {
    project_map_with_scratch(cwd, &tcode_core::store::scratchpad_dir(cwd))
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

fn git_summary(cwd: &Path) -> Option<String> {
    let branch = git(cwd, &["branch", "--show-current"])?;
    let status = git(cwd, &["status", "--porcelain"]).unwrap_or_default();
    let last = git(cwd, &["log", "-1", "--format=%h %s"]).unwrap_or_default();
    let dirty = status.lines().count();
    let mut s = format!("branch: {branch}\nlast commit: {last}\n");
    if dirty == 0 {
        s.push_str("working tree: clean");
    } else {
        s.push_str(&format!("working tree: {dirty} changed file(s)\n"));
        for line in status.lines().take(15) {
            s.push_str(&format!("  {line}\n"));
        }
        if dirty > 15 {
            s.push_str(&format!("  … (+{} more)\n", dirty - 15));
        }
    }
    Some(s)
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
