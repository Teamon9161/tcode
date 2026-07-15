//! Session persistence: a JSONL log of ledger operations. The log is
//! append-only even across rewinds — a rewind is recorded as an event,
//! not by erasing lines — so earlier branches stay recoverable.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::ledger::{Entry, Ledger, LedgerSink};

/// One line in the session log. `Append`/`TruncateTail`/`Compact`
/// mirror the three legal ledger mutations; replaying them rebuilds
/// the conversation exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum LogEvent {
    Meta {
        id: String,
        cwd: String,
        created_unix: u64,
    },
    Append {
        entry: Entry,
    },
    TruncateTail {
        len: usize,
    },
    Compact {
        summary: String,
        upto: usize,
    },
    /// Original file content saved before a mutating tool ran.
    /// `saved` is the checkpoint file name; None = file did not exist.
    Checkpoint {
        ledger_len: usize,
        path: String,
        saved: Option<String>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("session io: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt session line: {0}")]
    Corrupt(#[from] serde_json::Error),
    #[error("no session found to resume")]
    NoSession,
    #[error("external session: {0}")]
    External(String),
}

/// Where all per-project state lives:
/// `~/.tcode/projects/<hash>/{sessions,checkpoints,blobs}/`.
pub fn project_data_dir(cwd: &Path) -> Option<PathBuf> {
    let key = cwd.to_string_lossy().to_lowercase();
    Some(
        dirs::home_dir()?
            .join(".tcode")
            .join("projects")
            .join(format!("{:016x}", fnv1a(key.as_bytes()))),
    )
}

/// Project-wide parent for ephemeral session scratch directories. Writers must
/// use [`session_scratchpad_dir`] rather than placing new artifacts directly
/// here, so one conversation cannot clean up another's temporary work.
pub fn scratchpad_dir(cwd: &Path) -> PathBuf {
    project_data_dir(cwd)
        .unwrap_or_else(|| std::env::temp_dir().join("tcode"))
        .join("scratchpad")
}

/// Scratch root owned by exactly one conversation. Persistent sessions use the
/// session-log id; ephemeral sessions receive a unique process-local id. The
/// directory is created lazily by writers.
pub fn session_scratchpad_dir(cwd: &Path, session_id: &str) -> PathBuf {
    scratchpad_dir(cwd).join("runs").join(session_id)
}

/// Legacy location for project-wide overflow logs. New `ToolCtx` instances use
/// their session root's `tool-output/` directory instead.
pub fn tool_output_dir(cwd: &Path) -> PathBuf {
    scratchpad_dir(cwd).join("tool-output")
}

/// Approved plans land here as a human-readable mirror of the plan the model
/// holds in its ledger: `<project-data>/plans/`. Runtime state, not part of the
/// user's repository — anyone who wants a plan in the repo copies it there.
/// Falls back to a temp dir when there is no home directory.
pub fn plans_dir(cwd: &Path) -> PathBuf {
    project_data_dir(cwd)
        .unwrap_or_else(|| std::env::temp_dir().join("tcode"))
        .join("plans")
}

/// Nothing in the scratchpad is meant to survive this long.
const SCRATCH_FOR: Duration = Duration::from_secs(7 * 24 * 3600);

/// Best-effort: delete everything in the project's scratchpad that has not been
/// touched for a week, and prune the directories that empty out. Called once at
/// startup; if the scratchpad does not exist, nothing is created.
///
/// One rule for the whole tree, deliberately: the harness's overflowed tool
/// output and the model's own throwaway scripts, repro programs and build
/// directories are all scratch, and exempting a subdirectory is how a stale
/// 3 GB `target/` sits there forever. Age is per file — a file is dead when
/// nobody has read or written it in a week, regardless of what its neighbours
/// have been doing.
pub fn sweep_scratchpad(dir: &Path) {
    let cutoff = SystemTime::now()
        .checked_sub(SCRATCH_FOR)
        .unwrap_or(UNIX_EPOCH);
    sweep_stale(dir, cutoff);
}

/// Returns true when the directory is left empty, so its parent can prune it.
/// Symlinks are removed as links, never followed — a scratch symlink into the
/// project must not become a path for this sweep to delete the user's files.
fn sweep_stale(dir: &Path, cutoff: SystemTime) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    let mut empty = true;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            empty = false;
            continue;
        };
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| modified < cutoff);
        let removed = if kind.is_dir() {
            sweep_stale(&path, cutoff) && fs::remove_dir(&path).is_ok()
        } else if stale {
            fs::remove_file(&path).is_ok()
        } else {
            false
        };
        empty &= removed;
    }
    empty
}

/// How many conversations stay resumable, and for how long. Whichever limit
/// bites first wins.
const KEEP_SESSIONS: usize = 100;
const KEEP_FOR: Duration = Duration::from_secs(30 * 24 * 3600);
/// An empty log younger than this may belong to a tcode that is running right
/// now in this project and has simply not been spoken to yet.
const EMPTY_GRACE: Duration = Duration::from_secs(3600);

/// Best-effort startup GC of `sessions/` and `checkpoints/`.
///
/// The two expire *together*: a conversation you can still resume must still be
/// rewindable, and a checkpoint without the log that indexes it is just a file
/// nobody can name. So the rule is one rule — a checkpoint directory exists iff
/// its session is kept — which also collects orphans left by earlier crashes.
///
/// Logs nobody spoke into (starting tcode and typing nothing leaves one) are
/// not conversations: they are deleted outright and never occupy a slot, so a
/// hundred aborted launches cannot evict a real conversation. Call this *before*
/// creating this run's log, which is empty at that moment by definition.
pub fn sweep_old_sessions(data_dir: &Path) {
    let sessions_dir = data_dir.join("sessions");
    let checkpoints_dir = data_dir.join("checkpoints");
    let Ok(entries) = fs::read_dir(&sessions_dir) else {
        return;
    };
    let mut logs: Vec<(SystemTime, String, PathBuf)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|path| {
            let modified = fs::metadata(&path).and_then(|m| m.modified()).ok()?;
            let id = path.file_stem()?.to_string_lossy().into_owned();
            Some((modified, id, path))
        })
        .collect();
    logs.sort_by(|a, b| b.0.cmp(&a.0)); // newest first

    let now = SystemTime::now();
    let cutoff = now.checked_sub(KEEP_FOR).unwrap_or(UNIX_EPOCH);
    let settled = now.checked_sub(EMPTY_GRACE).unwrap_or(UNIX_EPOCH);
    let mut kept: Vec<String> = Vec::new();
    for (modified, id, path) in logs {
        if !has_conversation(&path) {
            // A launch, not a conversation. Delete it — unless it is minutes
            // old, in which case a second tcode may be running in this project
            // right now with its log still empty. Never occupies a slot.
            if modified < settled {
                let _ = fs::remove_file(&path);
            }
            continue;
        }
        if modified >= cutoff && kept.len() < KEEP_SESSIONS {
            kept.push(id);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
    let Ok(dirs) = fs::read_dir(&checkpoints_dir) else {
        return;
    };
    for dir in dirs.flatten() {
        let id = dir.file_name().to_string_lossy().into_owned();
        if !kept.contains(&id) {
            let _ = fs::remove_dir_all(dir.path());
        }
    }
}

/// Did anyone say anything in this session? Stops at the first entry, so the
/// scan costs one line for a real conversation and a whole (tiny) file only for
/// an empty one.
fn has_conversation(log: &Path) -> bool {
    let Ok(file) = File::open(log) else {
        return false;
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .any(|line| line.contains("\"ev\":\"append\""))
}

/// Deterministic across runs and Rust versions (unlike DefaultHasher).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Convert approval comments written by versions before `Entry::UserNote`.
/// The literal prefix makes this deliberately narrow: ordinary harness notes
/// must retain their original meaning on resume.
fn upgrade_legacy_entry(entry: Entry) -> Entry {
    let Entry::Note(note) = entry else {
        return entry;
    };
    let Some(rest) = note.strip_prefix("Note from the user when approving ") else {
        return Entry::Note(note);
    };
    let Some((about, text)) = rest.split_once(": ") else {
        return Entry::Note(note);
    };
    if about.is_empty() {
        return Entry::Note(note);
    }
    Entry::UserNote {
        about: about.into(),
        answer: false,
        text: text.into(),
    }
}

pub struct SessionStore {
    pub id: String,
    writer: BufWriter<File>,
}

/// A session loaded from disk, ready to continue.
pub struct Resumed {
    pub store: SessionStore,
    pub ledger: Ledger,
    pub checkpoints: Vec<(usize, String, Option<String>)>,
}

/// A resumable conversation in one project, suitable for a UI picker.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub last_user_preview: String,
    pub modified: Option<SystemTime>,
}

impl SessionStore {
    /// List recent non-empty sessions in newest-first order. This is kept
    /// separate from `resume`: starting tcode creates a fresh log first, and
    /// that empty log must not hide the conversations a user can restore.
    pub fn list(data_dir: &Path) -> Result<Vec<SessionInfo>, StoreError> {
        let sessions = data_dir.join("sessions");
        let mut files: Vec<PathBuf> = fs::read_dir(&sessions)
            .map_err(|_| StoreError::NoSession)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect();
        files.sort();
        files.reverse();

        let mut result = Vec::new();
        for path in files {
            let modified = fs::metadata(&path).and_then(|m| m.modified()).ok();
            let id = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Reuse the normal replay path so `/clear` and rewind events
            // are respected; scanning raw append events would resurrect
            // conversations that were deliberately cleared.
            let resumed = Self::resume(data_dir, Some(&id))?;
            let last_user_preview = resumed.ledger.entries().iter().rev().find_map(|entry| {
                let Entry::User(blocks) = entry else {
                    return None;
                };
                blocks.iter().find_map(|b| match b {
                    crate::types::ContentBlock::Text { text }
                        if !text.starts_with("<tcode-status>") =>
                    {
                        text.lines().next().map(str::to_owned)
                    }
                    _ => None,
                })
            });
            if let Some(last_user_preview) = last_user_preview {
                result.push(SessionInfo {
                    id: resumed.store.id,
                    last_user_preview,
                    modified,
                });
            }
        }
        Ok(result)
    }

    /// Start a new session log under `data_dir/sessions/`.
    pub fn create(data_dir: &Path, cwd: &Path) -> Result<Self, StoreError> {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Self::create_at_millis(data_dir, cwd, millis)
    }

    /// Create a chronologically-sortable log name. `create_new` is the
    /// cross-process arbiter: if another tcode claimed this millisecond first,
    /// advance until an unused id is atomically claimed instead of treating a
    /// normal rapid launch as a persistence failure.
    fn create_at_millis(data_dir: &Path, cwd: &Path, mut millis: u128) -> Result<Self, StoreError> {
        let sessions = data_dir.join("sessions");
        fs::create_dir_all(&sessions)?;
        let (id, file) = loop {
            let id = format!("{millis:013x}");
            match OpenOptions::new()
                .create_new(true)
                .append(true)
                .open(sessions.join(format!("{id}.jsonl")))
            {
                Ok(file) => break (id, file),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    millis = millis.checked_add(1).ok_or_else(|| {
                        std::io::Error::other("exhausted session timestamp namespace")
                    })?;
                }
                Err(e) => return Err(e.into()),
            }
        };
        let mut store = Self {
            id: id.clone(),
            writer: BufWriter::new(file),
        };
        store.record(&LogEvent::Meta {
            id,
            cwd: cwd.to_string_lossy().into_owned(),
            created_unix: now_unix(),
        });
        Ok(store)
    }

    /// Resume the most recent session, or one matching an id prefix.
    pub fn resume(data_dir: &Path, id_prefix: Option<&str>) -> Result<Resumed, StoreError> {
        let sessions = data_dir.join("sessions");
        let mut files: Vec<PathBuf> = fs::read_dir(&sessions)
            .map_err(|_| StoreError::NoSession)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .filter(|p| match id_prefix {
                Some(prefix) => p
                    .file_stem()
                    .is_some_and(|s| s.to_string_lossy().starts_with(prefix)),
                None => true,
            })
            .collect();
        files.sort();
        let path = files.pop().ok_or(StoreError::NoSession)?;

        let mut ledger = Ledger::new();
        let mut checkpoints = Vec::new();
        let mut id = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        for line in BufReader::new(File::open(&path)?).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<LogEvent>(&line)? {
                LogEvent::Meta { id: meta_id, .. } => id = meta_id,
                // Before `Entry::UserNote` existed, approval annotations were
                // persisted as a pre-formatted machine note. Upgrade that
                // unambiguous legacy shape while replaying so resumed
                // transcripts show the person's own words, just like live
                // annotations and newly-created sessions.
                LogEvent::Append { entry } => ledger.append(upgrade_legacy_entry(entry)),
                LogEvent::TruncateTail { len } => ledger.truncate_tail(len),
                LogEvent::Compact { summary, upto } => ledger.compact(summary, upto),
                LogEvent::Checkpoint {
                    ledger_len,
                    path,
                    saved,
                } => checkpoints.push((ledger_len, path, saved)),
            }
        }
        let file = OpenOptions::new().append(true).open(&path)?;
        Ok(Resumed {
            store: Self {
                id,
                writer: BufWriter::new(file),
            },
            ledger,
            checkpoints,
        })
    }

    /// Write one event and flush: a crash must not lose accepted turns.
    pub fn record(&mut self, ev: &LogEvent) {
        // Persistence must never break the conversation itself; errors
        // here degrade to an unrecorded session, not a failed turn.
        let line = match serde_json::to_string(ev) {
            Ok(line) => line,
            Err(e) => {
                debug_assert!(false, "unserializable log event: {e}");
                return;
            }
        };
        let _ = writeln!(self.writer, "{line}");
        let _ = self.writer.flush();
    }
}

impl LedgerSink for SessionStore {
    fn record(&mut self, ev: &LogEvent) {
        SessionStore::record(self, ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ContentBlock;

    fn text(s: &str) -> Entry {
        Entry::User(vec![ContentBlock::Text { text: s.into() }])
    }

    /// One clock for the whole scratchpad: the harness's overflow files and the
    /// model's abandoned build tree age the same way, and a directory left
    /// empty by the sweep goes with them. What is still in use stays.
    #[test]
    fn the_scratchpad_sweep_collects_stale_files_and_the_dirs_they_leave_empty() {
        let root = std::env::temp_dir().join(format!("tcode-scratch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let old = SystemTime::now() - Duration::from_secs(8 * 24 * 3600);
        let age = |path: &Path| {
            let handle = OpenOptions::new().write(true).open(path).unwrap();
            handle
                .set_times(fs::FileTimes::new().set_modified(old))
                .unwrap();
        };

        // A week-old build tree the model left behind, an overflow file from
        // the same era, and two things still in use.
        fs::create_dir_all(root.join("auto-smoke-target/debug/deps")).unwrap();
        fs::write(root.join("auto-smoke-target/debug/deps/lib.rlib"), "x").unwrap();
        age(&root.join("auto-smoke-target/debug/deps/lib.rlib"));
        fs::create_dir_all(root.join("tool-output")).unwrap();
        fs::write(root.join("tool-output/old.txt"), "x").unwrap();
        age(&root.join("tool-output/old.txt"));
        fs::write(root.join("tool-output/fresh.txt"), "x").unwrap();
        fs::create_dir_all(root.join("repro")).unwrap();
        fs::write(root.join("repro/main.rs"), "fn main() {}").unwrap();

        sweep_scratchpad(&root);

        assert!(!root.join("auto-smoke-target").exists(), "tree and all");
        assert!(!root.join("tool-output/old.txt").exists());
        assert!(root.join("tool-output/fresh.txt").exists(), "still in use");
        assert!(root.join("repro/main.rs").exists(), "still in use");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rapid_session_creates_claim_distinct_sortable_ids() {
        let dir = std::env::temp_dir().join(format!("tcode-store-ids-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let first = SessionStore::create_at_millis(&dir, Path::new("C:/proj"), 42).unwrap();
        let second = SessionStore::create_at_millis(&dir, Path::new("C:/proj"), 42).unwrap();

        assert_eq!(first.id, format!("{:013x}", 42));
        assert_eq!(second.id, format!("{:013x}", 43));
        assert_ne!(first.id, second.id);
        assert!(dir
            .join("sessions")
            .join(format!("{}.jsonl", first.id))
            .exists());
        assert!(dir
            .join("sessions")
            .join(format!("{}.jsonl", second.id))
            .exists());

        drop(first);
        drop(second);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A session's log and its checkpoints live and die together, empty logs
    /// are not conversations and never occupy a slot, and a checkpoint
    /// directory with no session left is garbage.
    #[test]
    fn the_sweep_keeps_conversations_and_their_checkpoints_together() {
        let dir = std::env::temp_dir().join(format!("tcode-sweep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let sessions = dir.join("sessions");
        let checkpoints = dir.join("checkpoints");
        fs::create_dir_all(&sessions).unwrap();

        // A real conversation, an abandoned launch, and an orphan checkpoint
        // directory whose session is already gone.
        let real = SessionStore::create(&dir, Path::new("C:/proj")).unwrap();
        let real_id = real.id.clone();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(real));
        ledger.append(text("hello"));

        let empty = SessionStore::create(&dir, Path::new("C:/proj")).unwrap();
        let empty_id = empty.id.clone();
        drop(empty);
        // Backdate it past the "another tcode may be starting" grace period.
        let old = SystemTime::now() - Duration::from_secs(2 * 3600);
        let empty_log = sessions.join(format!("{empty_id}.jsonl"));
        let handle = OpenOptions::new().write(true).open(&empty_log).unwrap();
        handle
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
        drop(handle);

        for id in [&real_id, &empty_id, &"deadbeef".to_string()] {
            fs::create_dir_all(checkpoints.join(id)).unwrap();
            fs::write(checkpoints.join(id).join("aa.orig"), "x").unwrap();
        }

        sweep_old_sessions(&dir);

        assert!(sessions.join(format!("{real_id}.jsonl")).exists());
        assert!(checkpoints.join(&real_id).exists(), "kept with its session");
        assert!(!empty_log.exists(), "a launch nobody spoke into");
        assert!(!checkpoints.join(&empty_id).exists());
        assert!(!checkpoints.join("deadbeef").exists(), "orphan collected");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn roundtrip_including_rewind_and_compact() {
        let dir = std::env::temp_dir().join(format!("tcode-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let store = SessionStore::create(&dir, Path::new("C:/proj")).unwrap();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        ledger.append(text("one"));
        ledger.append(text("two"));
        ledger.append(text("three"));
        ledger.truncate_tail(2);
        ledger.compact("sum".into(), 1);
        assert_eq!(ledger.len(), 2);

        let resumed = SessionStore::resume(&dir, None).unwrap();
        assert_eq!(resumed.ledger.len(), 2);
        assert!(matches!(&resumed.ledger.entries()[0], Entry::Summary(s) if s == "sum"));
        assert!(matches!(&resumed.ledger.entries()[1], Entry::User(_)));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_upgrades_legacy_approval_notes() {
        let dir =
            std::env::temp_dir().join(format!("tcode-store-legacy-note-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let store = SessionStore::create(&dir, Path::new("C:/proj")).unwrap();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        ledger.append(Entry::Note(
            "Note from the user when approving bash: use 4 spaces".into(),
        ));

        let resumed = SessionStore::resume(&dir, None).unwrap();
        assert!(matches!(
            &resumed.ledger.entries()[0],
            Entry::UserNote { about, answer, text }
                if about == "bash" && !answer && text == "use 4 spaces"
        ));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn roundtrips_incomplete_assistant_without_prompt_replay() {
        let dir =
            std::env::temp_dir().join(format!("tcode-store-incomplete-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let store = SessionStore::create(&dir, Path::new("C:/proj")).unwrap();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        ledger.append(Entry::IncompleteAssistant {
            text: "partial answer".into(),
            error: "network error".into(),
        });

        let resumed = SessionStore::resume(&dir, None).unwrap();
        assert!(matches!(
            &resumed.ledger.entries()[0],
            Entry::IncompleteAssistant { text, error }
                if text == "partial answer" && error == "network error"
        ));
        assert!(resumed.ledger.as_messages().is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn roundtrips_large_tool_output_with_windows_paths() {
        let dir = std::env::temp_dir().join(format!("tcode-store-large-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let store = SessionStore::create(&dir, Path::new("C:/proj")).unwrap();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        let content = "C:\\code\\rust\\tcode\\plan.md\n".repeat(1_000);
        ledger.append(Entry::ToolResults(vec![ContentBlock::ToolResult {
            tool_use_id: "read-plan".into(),
            content: content.clone(),
            is_error: false,
            images: vec![],
        }]));

        let resumed = SessionStore::resume(&dir, None).unwrap();
        assert!(matches!(
            &resumed.ledger.entries()[0],
            Entry::ToolResults(blocks)
                if matches!(&blocks[0], ContentBlock::ToolResult { content: saved, .. } if saved == &content)
        ));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_without_sessions_errors() {
        let dir = std::env::temp_dir().join("tcode-store-missing");
        assert!(matches!(
            SessionStore::resume(&dir, None),
            Err(StoreError::NoSession)
        ));
    }
}
