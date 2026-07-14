use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::ledger::Entry;

const INSTRUCTION_CAP: usize = 16_000;
const AUTO_MEMORY_CAP: usize = 25 * 1024;
const AUTO_MEMORY_LINES: usize = 200;
const MAINTENANCE_TURNS: u32 = 20;
const MAINTENANCE_INTERVAL_SECS: u64 = 7 * 24 * 60 * 60;
const SOURCE_MARKER: &str = "tcode-memory-source: ";
const PROJECT_MARKER: &str = "tcode-memory-project: ";
const AUTO_MEMORY_SYSTEM: &str = include_str!("../../../prompts/memory-system.md");

#[derive(Debug, Clone)]
struct Project {
    root: PathBuf,
    identity: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct MaintenanceState {
    #[serde(default = "enabled_by_default")]
    enabled: bool,
    #[serde(default)]
    turns_since_maintenance: u32,
    #[serde(default)]
    last_reminder_unix: u64,
}

fn enabled_by_default() -> bool {
    true
}

#[derive(Debug)]
pub struct MemoryUpdate {
    pub note: String,
}

/// Per-session memory discovery and maintenance state. Instruction files are
/// keyed by canonical path so aliases and symlinks cannot inject duplicates.
#[derive(Debug)]
pub struct MemoryManager {
    home: Option<PathBuf>,
    project: Project,
    auto_dir: Option<PathBuf>,
    initial_sources: Vec<PathBuf>,
    loaded_sources: Vec<PathBuf>,
    loaded_keys: HashSet<String>,
    known_projects: HashSet<String>,
    maintenance: MaintenanceState,
}

impl MemoryManager {
    pub fn new(cwd: &Path) -> Self {
        Self::new_with_home(cwd, dirs::home_dir())
    }

    fn new_with_home(cwd: &Path, home: Option<PathBuf>) -> Self {
        let cwd = canonical_or_normal(cwd);
        let project = locate_project(&cwd, home.as_deref(), true).unwrap_or_else(|| Project {
            root: cwd.clone(),
            identity: cwd.clone(),
        });
        let auto_dir = home.as_ref().map(|home| {
            home.join(".tcode")
                .join("projects")
                .join(format!(
                    "{:016x}",
                    fnv1a(path_key(&project.identity).as_bytes())
                ))
                .join("memory")
        });
        let maintenance = auto_dir
            .as_ref()
            .and_then(|dir| read_state(&dir.join("state.json")))
            .unwrap_or_else(|| MaintenanceState {
                enabled: true,
                turns_since_maintenance: 0,
                last_reminder_unix: now_unix(),
            });

        let mut initial_sources = Vec::new();
        if let Some(home) = &home {
            let global = home.join(".tcode").join("AGENTS.md");
            if global.is_file() {
                initial_sources.push(canonical_or_normal(&global));
            }
        }
        initial_sources.extend(instruction_sources(&project.root, &cwd));
        if maintenance.enabled {
            if let Some(memory) = auto_dir.as_ref().map(|dir| dir.join("MEMORY.md")) {
                if memory.is_file() {
                    initial_sources.push(canonical_or_normal(&memory));
                }
            }
        }
        dedup_paths(&mut initial_sources);

        let loaded_sources = initial_sources.clone();
        let loaded_keys = loaded_sources.iter().map(|path| path_key(path)).collect();
        let known_projects = HashSet::from([path_key(&project.identity)]);
        Self {
            home,
            project,
            auto_dir,
            initial_sources,
            loaded_sources,
            loaded_keys,
            known_projects,
            maintenance,
        }
    }

    pub fn startup_prompt(&self) -> String {
        let mut out = String::new();
        let instruction_sources: Vec<&PathBuf> = self
            .initial_sources
            .iter()
            .filter(|path| path.file_name().is_some_and(|name| name != "MEMORY.md"))
            .collect();
        if !instruction_sources.is_empty() {
            out.push_str("\n# Persistent instructions\n");
            append_sources(&mut out, instruction_sources.into_iter(), INSTRUCTION_CAP);
        }

        out.push_str("\n# Auto memory\n");
        match &self.auto_dir {
            Some(dir) if self.maintenance.enabled => {
                out.push_str(&format!("directory: {}\n", dir.display()));
                let memory = dir.join("MEMORY.md");
                if memory.is_file() {
                    out.push_str(&format!("source: {}\n", memory.display()));
                    out.push_str(&read_auto_memory(&memory));
                    out.push('\n');
                } else {
                    out.push_str("MEMORY.md does not exist yet.\n");
                }
                out.push_str(AUTO_MEMORY_SYSTEM);
            }
            Some(dir) => out.push_str(&format!(
                "disabled for this project (directory: {}). Do not write auto memory.\n",
                dir.display()
            )),
            None => out.push_str("unavailable: no user home directory.\n"),
        }
        out
    }

    /// Human-maintained instructions currently in force, for the Auto Mode
    /// classifier. Deliberately excludes `MEMORY.md` and the auto-memory
    /// maintenance prompt: classifier policy must not be steered by model-
    /// maintained content.
    pub fn classifier_instructions(&self) -> String {
        let mut out = String::new();
        append_sources(
            &mut out,
            self.loaded_sources
                .iter()
                .filter(|path| path.file_name().is_some_and(|name| name != "MEMORY.md")),
            INSTRUCTION_CAP,
        );
        out
    }

    /// Rebuild the loaded set from the immutable startup sources plus durable
    /// ledger markers. This makes resume and rewind agree with visible history.
    pub fn restore_from_entries(&mut self, entries: &[Entry]) {
        self.loaded_sources = self.initial_sources.clone();
        self.loaded_keys = self
            .initial_sources
            .iter()
            .map(|path| path_key(path))
            .collect();
        self.known_projects = HashSet::from([path_key(&self.project.identity)]);
        for entry in entries {
            let text = match entry {
                Entry::Note(text) | Entry::Summary(text) => text,
                _ => continue,
            };
            for line in text.lines() {
                if let Some(raw) = line.strip_prefix(SOURCE_MARKER) {
                    let path = PathBuf::from(raw);
                    let key = path_key(&path);
                    if self.loaded_keys.insert(key) {
                        self.loaded_sources.push(path);
                    }
                } else if let Some(key) = line.strip_prefix(PROJECT_MARKER) {
                    self.known_projects.insert(key.to_string());
                }
            }
        }
    }

    pub fn discover_for_paths(&mut self, paths: &[PathBuf]) -> Option<MemoryUpdate> {
        let mut new_sources = Vec::new();
        let mut new_projects = Vec::new();
        for path in paths {
            let target = canonical_target(path);
            let project = if target.starts_with(&self.project.root) {
                Some(self.project.clone())
            } else {
                locate_project(&target, self.home.as_deref(), false)
            };
            let Some(project) = project else {
                continue;
            };
            let project_key = path_key(&project.identity);
            let first_project_access = self.known_projects.insert(project_key.clone());
            if first_project_access {
                let auto_enabled = self.auto_enabled_for(&project);
                new_projects.push((project.clone(), project_key, auto_enabled));
            }
            let target_dir = target_directory(&target);
            for source in instruction_sources(&project.root, &target_dir) {
                let key = path_key(&source);
                if self.loaded_keys.insert(key) {
                    self.loaded_sources.push(source.clone());
                    new_sources.push(source);
                }
            }
            if first_project_access && self.auto_enabled_for(&project) {
                if let Some(auto_dir) = self.auto_dir_for(&project) {
                    let memory = auto_dir.join("MEMORY.md");
                    if memory.is_file() {
                        let memory = canonical_or_normal(&memory);
                        let key = path_key(&memory);
                        if self.loaded_keys.insert(key) {
                            self.loaded_sources.push(memory.clone());
                            new_sources.push(memory);
                        }
                    }
                }
            }
        }
        if new_sources.is_empty() && new_projects.is_empty() {
            return None;
        }

        let mut note = String::from(
            "New directory-scoped instructions were discovered for the requested tool paths. Apply them before any mutation is retried.\n",
        );
        for (project, key, auto_enabled) in &new_projects {
            note.push_str(PROJECT_MARKER);
            note.push_str(key);
            note.push('\n');
            note.push_str(&format!("project root: {}\n", project.root.display()));
            if let Some(dir) = self.auto_dir_for(project) {
                note.push_str(&format!(
                    "project auto memory: {} ({})\n",
                    dir.display(),
                    if *auto_enabled { "on" } else { "off" }
                ));
            }
        }
        append_source_markers(&mut note, new_sources.iter());
        append_sources(&mut note, new_sources.iter(), INSTRUCTION_CAP);
        Some(MemoryUpdate { note })
    }

    pub fn maintenance_reminder(&mut self) -> Option<String> {
        if !self.maintenance.enabled || self.auto_dir.is_none() {
            return None;
        }
        self.maintenance.turns_since_maintenance =
            self.maintenance.turns_since_maintenance.saturating_add(1);
        let now = now_unix();
        let elapsed = now.saturating_sub(self.maintenance.last_reminder_unix);
        let due = self.maintenance.turns_since_maintenance >= MAINTENANCE_TURNS
            || (self.maintenance.turns_since_maintenance > 1
                && elapsed >= MAINTENANCE_INTERVAL_SECS);
        if due {
            self.maintenance.turns_since_maintenance = 0;
            self.maintenance.last_reminder_unix = now;
        }
        self.save_state();
        due.then(|| {
            let dir = self.auto_dir.as_ref().expect("checked above");
            format!(
                "<memory-maintenance>\nThis actively used project's auto memory is due for maintenance. After completing the user's current task, review {}. Record durable important decisions from recent work, merge duplicates, remove or correct stale facts, keep MEMORY.md concise, and move detail into topic files. Do not record secrets or transient task state.\n</memory-maintenance>",
                dir.display()
            )
        })
    }

    pub fn mark_written(&mut self, path: &Path) {
        let Some(auto_dir) = &self.auto_dir else {
            return;
        };
        if canonical_or_normal(path).starts_with(canonical_or_normal(auto_dir)) {
            self.maintenance.turns_since_maintenance = 0;
            self.maintenance.last_reminder_unix = now_unix();
            self.save_state();
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) -> String {
        self.maintenance.enabled = enabled;
        self.maintenance.turns_since_maintenance = 0;
        self.maintenance.last_reminder_unix = now_unix();
        self.save_state();
        let Some(dir) = &self.auto_dir else {
            return "Auto memory is unavailable because no user home directory was found.".into();
        };
        if !enabled {
            return format!(
                "Auto memory was disabled for this project. Do not read or write {} unless the user explicitly asks.",
                dir.display()
            );
        }
        let memory = dir.join("MEMORY.md");
        let mut note = format!(
            "Auto memory was enabled for this project. Maintain machine-local memory under {}. Keep MEMORY.md concise, update existing facts instead of duplicating them, record durable important decisions, and never store secrets.\n",
            dir.display()
        );
        if memory.is_file() {
            let memory = canonical_or_normal(&memory);
            let key = path_key(&memory);
            if self.loaded_keys.insert(key) {
                self.loaded_sources.push(memory.clone());
            }
            note.push_str(SOURCE_MARKER);
            note.push_str(&memory.to_string_lossy());
            note.push('\n');
            note.push_str(&read_auto_memory(&memory));
        }
        note
    }

    pub fn status(&self) -> String {
        let mut lines = vec![format!(
            "auto memory: {}",
            if self.maintenance.enabled {
                "on"
            } else {
                "off"
            }
        )];
        if let Some(dir) = &self.auto_dir {
            lines.push(format!("directory: {}", dir.display()));
        }
        lines.push(format!("project root: {}", self.project.root.display()));
        if self.loaded_sources.is_empty() {
            lines.push("loaded instructions: none".into());
        } else {
            lines.push("loaded sources:".into());
            lines.extend(
                self.loaded_sources
                    .iter()
                    .map(|path| format!("  {}", path.display())),
            );
        }
        lines.join("\n")
    }

    /// Re-inject dynamically loaded files after compacting away their notes.
    pub fn post_compact_note(&self) -> Option<String> {
        let dynamic: Vec<&PathBuf> = self
            .loaded_sources
            .iter()
            .filter(|path| {
                !self
                    .initial_sources
                    .iter()
                    .any(|p| path_key(p) == path_key(path))
            })
            .collect();
        if dynamic.is_empty() && self.known_projects.len() <= 1 {
            return None;
        }
        let mut note =
            String::from("Directory-scoped instructions still active after compaction:\n");
        for key in &self.known_projects {
            note.push_str(PROJECT_MARKER);
            note.push_str(key);
            note.push('\n');
        }
        append_source_markers(&mut note, dynamic.iter().copied());
        append_sources(&mut note, dynamic.into_iter(), INSTRUCTION_CAP);
        Some(note)
    }

    fn auto_dir_for(&self, project: &Project) -> Option<PathBuf> {
        self.home.as_ref().map(|home| {
            home.join(".tcode")
                .join("projects")
                .join(format!(
                    "{:016x}",
                    fnv1a(path_key(&project.identity).as_bytes())
                ))
                .join("memory")
        })
    }

    fn auto_enabled_for(&self, project: &Project) -> bool {
        if path_key(&project.identity) == path_key(&self.project.identity) {
            return self.maintenance.enabled;
        }
        self.auto_dir_for(project)
            .and_then(|dir| read_state(&dir.join("state.json")))
            .map(|state| state.enabled)
            .unwrap_or(true)
    }

    fn save_state(&self) {
        let Some(dir) = &self.auto_dir else {
            return;
        };
        if fs::create_dir_all(dir).is_err() {
            return;
        }
        let Ok(json) = serde_json::to_vec_pretty(&self.maintenance) else {
            return;
        };
        let _ = fs::write(dir.join("state.json"), json);
    }
}

fn append_sources<'a>(out: &mut String, sources: impl Iterator<Item = &'a PathBuf>, cap: usize) {
    let start = out.len();
    for source in sources {
        if out.len().saturating_sub(start) >= cap {
            out.push_str("... (instruction budget exhausted)\n");
            break;
        }
        let Ok(text) = fs::read_to_string(source) else {
            continue;
        };
        out.push_str(&format!("## {}\n", source.display()));
        let remaining = cap.saturating_sub(out.len().saturating_sub(start));
        out.push_str(&truncate_utf8(&text, remaining));
        out.push('\n');
    }
}

fn append_source_markers<'a>(out: &mut String, sources: impl Iterator<Item = &'a PathBuf>) {
    for source in sources {
        out.push_str(SOURCE_MARKER);
        out.push_str(&source.to_string_lossy());
        out.push('\n');
    }
}

fn read_auto_memory(path: &Path) -> String {
    let Ok(text) = fs::read_to_string(path) else {
        return String::new();
    };
    let by_lines = text
        .lines()
        .take(AUTO_MEMORY_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    let truncated = truncate_utf8(&by_lines, AUTO_MEMORY_CAP);
    if truncated.len() < text.len() {
        format!("{truncated}\n... (auto memory startup view truncated)")
    } else {
        truncated
    }
}

fn instruction_sources(root: &Path, target: &Path) -> Vec<PathBuf> {
    if !target.starts_with(root) {
        return Vec::new();
    }
    let mut dirs: Vec<PathBuf> = target
        .ancestors()
        .take_while(|dir| dir.starts_with(root))
        .map(Path::to_path_buf)
        .collect();
    dirs.reverse();
    let mut sources = Vec::new();
    for dir in dirs {
        for relative in [".tcode/AGENTS.md", "AGENTS.md", "CLAUDE.md"] {
            let candidate = dir.join(relative);
            if candidate.is_file() {
                sources.push(canonical_or_normal(&candidate));
                break;
            }
        }
    }
    sources
}

fn locate_project(path: &Path, home: Option<&Path>, implicit: bool) -> Option<Project> {
    let start = target_directory(path);
    for dir in start.ancestors() {
        if is_root(dir)
            || home.is_some_and(|home| canonical_or_normal(home) == canonical_or_normal(dir))
        {
            break;
        }
        let git = dir.join(".git");
        if git.exists() {
            return Some(Project {
                root: canonical_or_normal(dir),
                identity: git_identity(&git),
            });
        }
        if dir.join(".tcode").join("config.toml").is_file() {
            let root = canonical_or_normal(dir);
            return Some(Project {
                root: root.clone(),
                identity: root,
            });
        }
    }
    implicit.then(|| {
        let root = canonical_or_normal(&start);
        Project {
            root: root.clone(),
            identity: root,
        }
    })
}

fn git_identity(dot_git: &Path) -> PathBuf {
    if dot_git.is_dir() {
        return canonical_or_normal(dot_git);
    }
    let Some(git_dir) = fs::read_to_string(dot_git).ok().and_then(|text| {
        text.strip_prefix("gitdir:")
            .map(str::trim)
            .map(PathBuf::from)
    }) else {
        return canonical_or_normal(dot_git);
    };
    let git_dir = if git_dir.is_absolute() {
        git_dir
    } else {
        dot_git.parent().unwrap_or(Path::new("")).join(git_dir)
    };
    let git_dir = canonical_or_normal(&git_dir);
    let common = fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .map(PathBuf::from);
    match common {
        Some(path) if path.is_absolute() => canonical_or_normal(&path),
        Some(path) => canonical_or_normal(&git_dir.join(path)),
        None => git_dir,
    }
}

fn target_directory(path: &Path) -> PathBuf {
    if path.is_dir() {
        canonical_or_normal(path)
    } else {
        canonical_or_normal(path.parent().unwrap_or(path))
    }
}

fn canonical_target(path: &Path) -> PathBuf {
    if path.exists() {
        return canonical_or_normal(path);
    }
    let mut missing = Vec::new();
    let mut existing = path;
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            break;
        };
        missing.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            break;
        };
        existing = parent;
    }
    let mut out = canonical_or_normal(existing);
    for part in missing.into_iter().rev() {
        out.push(part);
    }
    normalize(out)
}

fn canonical_or_normal(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| normalize(path.to_path_buf()))
}

fn normalize(path: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

fn path_key(path: &Path) -> String {
    let key = canonical_or_normal(path).to_string_lossy().into_owned();
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

fn dedup_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = HashSet::new();
    paths.retain(|path| seen.insert(path_key(path)));
}

fn truncate_utf8(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n... (truncated)", &text[..end])
}

fn read_state(path: &Path) -> Option<MaintenanceState> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn is_root(path: &Path) -> bool {
    path.parent().is_none()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tcode-memory-{name}-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn startup_layers_instructions_root_to_cwd_with_same_level_priority() {
        let base = temp("layers");
        let home = base.join("home");
        let root = base.join("repo");
        let cwd = root.join("crates/core");
        fs::create_dir_all(home.join(".tcode")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(cwd.join(".tcode")).unwrap();
        fs::write(home.join(".tcode/AGENTS.md"), "user").unwrap();
        fs::write(root.join("AGENTS.md"), "root").unwrap();
        fs::write(root.join("CLAUDE.md"), "ignored").unwrap();
        fs::write(cwd.join(".tcode/AGENTS.md"), "nested").unwrap();

        let manager = MemoryManager::new_with_home(&cwd, Some(home));
        let prompt = manager.startup_prompt();
        assert!(prompt.find("user").unwrap() < prompt.find("root").unwrap());
        assert!(prompt.find("root").unwrap() < prompt.find("nested").unwrap());
        assert!(!prompt.contains("ignored"));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn external_path_needs_an_explicit_project_marker() {
        let base = temp("external");
        let home = base.clone();
        let current = home.join("current");
        let plain = home.join("plain");
        let marked = home.join("marked");
        for dir in [&current, &plain, &marked] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::create_dir_all(current.join(".git")).unwrap();
        fs::create_dir_all(marked.join(".git")).unwrap();
        fs::write(plain.join("AGENTS.md"), "must not load").unwrap();
        fs::write(marked.join("AGENTS.md"), "load me").unwrap();
        let mut manager = MemoryManager::new_with_home(&current, Some(home));

        let plain_update = manager.discover_for_paths(&[plain.join("x.rs")]);
        assert!(plain_update.is_none(), "{plain_update:?}");
        let update = manager.discover_for_paths(&[marked.join("x.rs")]).unwrap();
        assert!(update.note.contains("load me"));
        assert!(!update.note.contains("must not load"));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn home_is_not_inferred_as_an_external_project() {
        let base = temp("home-boundary");
        let home = base.clone();
        let current = home.join("repo");
        fs::create_dir_all(current.join(".git")).unwrap();
        fs::write(home.join("AGENTS.md"), "home project").unwrap();
        let mut manager = MemoryManager::new_with_home(&current, Some(home.clone()));
        let home_update = manager.discover_for_paths(&[home.join("notes.txt")]);
        assert!(home_update.is_none(), "{home_update:?}");
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn restore_markers_prevents_duplicate_dynamic_loads() {
        let base = temp("restore");
        let home = base.clone();
        let current = home.join("current");
        let external = home.join("external");
        fs::create_dir_all(current.join(".git")).unwrap();
        fs::create_dir_all(external.join(".git")).unwrap();
        fs::write(external.join("AGENTS.md"), "external rule").unwrap();
        let target = external.join("file.rs");
        let mut first = MemoryManager::new_with_home(&current, Some(home.clone()));
        let note = first
            .discover_for_paths(std::slice::from_ref(&target))
            .unwrap()
            .note;
        let mut resumed = MemoryManager::new_with_home(&current, Some(home));
        resumed.restore_from_entries(&[Entry::Note(note)]);
        assert!(resumed.discover_for_paths(&[target]).is_none());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn truncation_keeps_utf8_valid() {
        let text = "中".repeat(20);
        let cut = truncate_utf8(&text, 17);
        assert!(cut.starts_with("中中中中中"));
    }

    #[test]
    fn git_worktrees_share_auto_memory_identity() {
        let base = temp("worktree");
        let home = base.clone();
        let main = home.join("main");
        let worktree = home.join("worktree");
        let worktree_git_dir = main.join(".git/worktrees/w1");
        fs::create_dir_all(&worktree_git_dir).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        fs::write(worktree_git_dir.join("commondir"), "../..").unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}", worktree_git_dir.display()),
        )
        .unwrap();

        let main_memory = MemoryManager::new_with_home(&main, Some(home.clone()));
        let worktree_memory = MemoryManager::new_with_home(&worktree, Some(home));
        assert_eq!(main_memory.auto_dir, worktree_memory.auto_dir);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn active_projects_receive_periodic_maintenance_reminders() {
        let base = temp("maintenance");
        let home = base.clone();
        let project = home.join("repo");
        fs::create_dir_all(project.join(".git")).unwrap();
        let mut manager = MemoryManager::new_with_home(&project, Some(home));

        for _ in 0..(MAINTENANCE_TURNS - 1) {
            assert!(manager.maintenance_reminder().is_none());
        }
        let reminder = manager.maintenance_reminder().unwrap();
        assert!(reminder.contains("important decisions"));
        assert!(manager.maintenance_reminder().is_none());
        let _ = manager.set_enabled(false);
        for _ in 0..MAINTENANCE_TURNS {
            assert!(manager.maintenance_reminder().is_none());
        }
        let reloaded = MemoryManager::new_with_home(&project, Some(base.clone()));
        assert!(!reloaded.maintenance.enabled, "off state must persist");
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn elapsed_week_triggers_only_when_project_is_used_again() {
        let base = temp("maintenance-time");
        let home = base.clone();
        let project = home.join("repo");
        fs::create_dir_all(project.join(".git")).unwrap();
        let mut manager = MemoryManager::new_with_home(&project, Some(home));
        manager.maintenance.turns_since_maintenance = 1;
        manager.maintenance.last_reminder_unix = now_unix() - MAINTENANCE_INTERVAL_SECS;

        let reminder = manager.maintenance_reminder();
        assert!(reminder.is_some());
        let _ = fs::remove_dir_all(base);
    }
}
