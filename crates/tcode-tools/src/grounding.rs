use std::path::Path;
use std::process::Command;

/// Opening project map: the facts every session starts by rediscovering
/// (directory layout, git state, project memory), collected once at
/// startup and baked into the cached system-prompt prefix.
pub fn project_map(cwd: &Path) -> String {
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
        out.push_str(&format!("\n# Project layout (2 levels, gitignore-aware)\n{tree}"));
    }

    if let Some((source, text)) = memory_file(cwd) {
        out.push_str(&format!("\n# Project memory (from {source})\n{text}\n"));
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
    let out = Command::new("git").args(args).current_dir(cwd).output().ok()?;
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
    }
    Some(s)
}

fn dir_tree(cwd: &Path) -> String {
    let mut entries: Vec<String> = Vec::new();
    let walker = ignore::WalkBuilder::new(cwd)
        .max_depth(Some(2))
        .hidden(true)
        .build();
    for entry in walker.flatten() {
        let path = entry.path();
        if path == cwd {
            continue;
        }
        let Ok(rel) = path.strip_prefix(cwd) else { continue };
        let depth = rel.components().count();
        let indent = "  ".repeat(depth - 1);
        let name = rel.file_name().unwrap_or_default().to_string_lossy();
        if path.is_dir() {
            entries.push(format!("{indent}{name}/"));
        } else {
            entries.push(format!("{indent}{name}"));
        }
        if entries.len() >= 80 {
            entries.push("… (truncated)".into());
            break;
        }
    }
    entries.join("\n") + "\n"
}

/// Project memory: .tcode/AGENTS.md > AGENTS.md > CLAUDE.md, plus the
/// global ~/.tcode/AGENTS.md if present.
fn memory_file(cwd: &Path) -> Option<(String, String)> {
    let mut text = String::new();
    let mut sources: Vec<String> = Vec::new();
    if let Some(home) = dirs_home() {
        let global = home.join(".tcode").join("AGENTS.md");
        if let Ok(t) = std::fs::read_to_string(&global) {
            sources.push("~/.tcode/AGENTS.md".into());
            text.push_str(&t);
            text.push('\n');
        }
    }
    for candidate in [".tcode/AGENTS.md", "AGENTS.md", "CLAUDE.md"] {
        let p = cwd.join(candidate);
        if let Ok(t) = std::fs::read_to_string(&p) {
            sources.push(candidate.into());
            text.push_str(&t);
            break; // first hit wins among project files
        }
    }
    if text.is_empty() {
        return None;
    }
    const CAP: usize = 16_000;
    if text.len() > CAP {
        let cut = text
            .char_indices()
            .take_while(|(i, _)| *i < CAP)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        text.truncate(cut);
        text.push_str("\n… (memory truncated)");
    }
    Some((sources.join(" + "), text))
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(Into::into)
}
