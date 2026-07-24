//! What the launchpad reads: every folder tcode has ever held a conversation
//! in, and the conversations inside each one.
//!
//! There is no registry of projects to consult — `~/.tcode/projects/<id>/` is
//! named after a *lossy* transform of the path (`store::project_id` folds every
//! non-alphanumeric character to `-`), so the directory name cannot be turned
//! back into a folder. The path is recovered from the only place it is recorded
//! verbatim: the `Meta` line every session log opens with.
//!
//! That makes listing cheap by construction. Enumerating projects reads one
//! line per project; the expensive full replay that produces conversation
//! previews only happens for the project the user actually opens.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use tcode_core::store::{LogEvent, SessionStore};

/// A folder with at least one conversation in it.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ProjectInfo {
    /// The folder itself, as it was recorded when the session was created.
    pub path: String,
    /// Last path component — what the launchpad shows as the title.
    pub name: String,
    pub session_count: usize,
    /// Newest session log's mtime, as unix seconds. Sorts the launchpad.
    pub last_active: Option<u64>,
    /// False when the recorded folder is gone (moved, deleted, unmounted).
    /// Listed anyway — a project that vanished is information, and silently
    /// dropping it looks identical to a bug in this scan.
    pub exists: bool,
}

/// One resumable conversation, for the picker inside a project.
#[derive(Debug, Serialize)]
pub struct StoredSession {
    pub id: String,
    pub preview: String,
    pub modified: Option<u64>,
}

/// Every project with a session log, newest first.
///
/// Best-effort throughout: a project directory that cannot be read is skipped
/// rather than failing the whole list, because one damaged log must not empty
/// the launchpad.
pub fn list(home: &Path) -> Vec<ProjectInfo> {
    let projects = home.join(".tcode").join("projects");
    let Ok(entries) = fs::read_dir(&projects) else {
        return Vec::new();
    };

    let mut found: Vec<ProjectInfo> = entries
        .flatten()
        .filter_map(|entry| describe(&entry.path()))
        .collect();
    // Newest first, and a stable tiebreak so the list does not shuffle between
    // launches when two projects share a timestamp.
    found.sort_by(|a, b| {
        b.last_active
            .cmp(&a.last_active)
            .then_with(|| a.path.cmp(&b.path))
    });
    found
}

/// Read one `~/.tcode/projects/<id>/` directory into a [`ProjectInfo`].
fn describe(dir: &Path) -> Option<ProjectInfo> {
    let mut logs: Vec<PathBuf> = fs::read_dir(dir.join("sessions"))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    if logs.is_empty() {
        return None;
    }
    // Session ids are hex timestamps, so lexical order is chronological.
    logs.sort();

    // Walk newest-first for the folder: the last log is the freshest record of
    // where this project lives, and an older one may predate a `/cd`.
    let path = logs.iter().rev().find_map(|log| recorded_cwd(log))?;
    let last_active = logs.last().and_then(|log| modified_unix(log));
    let folder = PathBuf::from(&path);

    Some(ProjectInfo {
        name: folder
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone()),
        exists: folder.is_dir(),
        path,
        session_count: logs.len(),
        last_active,
    })
}

/// The `cwd` from a log's opening `Meta` record.
fn recorded_cwd(log: &Path) -> Option<String> {
    let file = fs::File::open(log).ok()?;
    let mut first = String::new();
    BufReader::new(file).read_line(&mut first).ok()?;
    match serde_json::from_str::<LogEvent>(&first).ok()? {
        LogEvent::Meta { cwd, .. } => Some(cwd),
        // Every log opens with `Meta`. Anything else is a log from a format
        // this build does not know; it is not this function's job to guess.
        _ => None,
    }
}

fn modified_unix(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_secs())
}

/// The conversations inside one project, newest first.
///
/// Unlike [`list`], this replays each log — that is what produces a preview
/// worth showing — so it runs only for a project the user has opened.
pub fn sessions(cwd: &Path) -> Vec<StoredSession> {
    let Some(data_dir) = tcode_core::store::project_data_dir(cwd) else {
        return Vec::new();
    };
    SessionStore::list(&data_dir)
        .unwrap_or_default()
        .into_iter()
        .map(|info| StoredSession {
            id: info.id,
            preview: info.last_user_preview,
            modified: info
                .modified
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|since| since.as_secs()),
        })
        .collect()
}

/// Unix seconds now, for the frontend's relative timestamps. Sent with the
/// project list so "2 hours ago" is computed against the backend's clock
/// rather than the webview's, which can differ across a suspend.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a session log whose `Meta` names `cwd`.
    fn log(home: &Path, project: &str, id: &str, cwd: &str) {
        let sessions = home.join(".tcode/projects").join(project).join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let meta = serde_json::json!({ "ev": "meta", "id": id, "cwd": cwd, "created_unix": 0 });
        fs::write(sessions.join(format!("{id}.jsonl")), format!("{meta}\n")).unwrap();
    }

    #[test]
    fn recovers_folders_from_the_meta_line() {
        let home = tempfile::tempdir().unwrap();
        let cwd = home.path().join("code").join("tcode");
        fs::create_dir_all(&cwd).unwrap();
        log(home.path(), "proj-a", "0000000000001", &cwd.to_string_lossy());

        let found = list(home.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "tcode");
        assert_eq!(found[0].path, cwd.to_string_lossy());
        assert!(found[0].exists);
    }

    #[test]
    fn a_project_whose_folder_is_gone_is_still_listed() {
        let home = tempfile::tempdir().unwrap();
        log(home.path(), "proj-a", "0000000000001", "/gone/elsewhere");

        let found = list(home.path());
        assert_eq!(found.len(), 1);
        assert!(!found[0].exists, "a missing folder must be reported, not hidden");
    }

    #[test]
    fn counts_logs_and_prefers_the_newest_recorded_folder() {
        let home = tempfile::tempdir().unwrap();
        log(home.path(), "proj-a", "0000000000001", "/old/place");
        log(home.path(), "proj-a", "0000000000002", "/new/place");

        let found = list(home.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_count, 2);
        assert_eq!(found[0].path, "/new/place");
    }

    #[test]
    fn skips_directories_with_no_readable_log() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".tcode/projects/empty/sessions")).unwrap();
        fs::write(
            home.path().join(".tcode/projects/empty/sessions/bad.jsonl"),
            "not json\n",
        )
        .unwrap();
        log(home.path(), "proj-a", "0000000000001", "/real/place");

        let found = list(home.path());
        assert_eq!(found.len(), 1, "one bad project must not empty the list");
        assert_eq!(found[0].path, "/real/place");
    }

    #[test]
    fn no_projects_directory_is_an_empty_list_not_an_error() {
        let home = tempfile::tempdir().unwrap();
        assert!(list(home.path()).is_empty());
    }
}
