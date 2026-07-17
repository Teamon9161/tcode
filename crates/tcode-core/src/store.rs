//! Session persistence: a JSONL log of ledger operations. The log is
//! append-only even across rewinds — a rewind is recorded as an event,
//! not by erasing lines — so earlier branches stay recoverable.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::environment::{EnvironmentSnapshot, StartupContext};
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
    /// Byte-stable system-prefix context captured before the first request.
    /// Multiple records are possible only while no model-visible history
    /// exists, e.g. an initial `/cd`; replay takes the last one.
    StartupContext {
        startup: StartupContext,
    },
    /// Historical record: prior versions wrote this together with an immediate
    /// model-facing Note, so it is also treated as delivered during replay.
    EnvironmentChanged {
        environment: EnvironmentSnapshot,
    },
    /// Latest actual harness environment. It may be newer than the environment
    /// the model has seen because a `/cd` can be coalesced before delivery.
    EnvironmentObserved {
        environment: EnvironmentSnapshot,
    },
    /// Latest runtime environment whose explanatory Note was actually appended
    /// to the model-visible ledger. This distinguishes transient `/cd` state
    /// from context the model can safely rely on after resume.
    EnvironmentDelivered {
        environment: EnvironmentSnapshot,
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
    /// First line of a task trace file (see `task_trace.rs`). Never appears
    /// in a session log.
    TaskMeta {
        id: String,
        parent_call: String,
        kind: String,
        model: String,
        prompt: String,
        /// One-line parent-authored description for task lists. Older trace
        /// files omit this; their loader derives a prompt-based fallback.
        #[serde(default)]
        summary: String,
        created_unix: u64,
    },
    /// Last line of a completed task trace file.
    TaskFinished {
        status: crate::task_trace::TaskRunStatus,
        tool_calls: usize,
        usage: crate::types::Usage,
    },
    /// Display label of a concurrent tool batch, recorded at execution time.
    /// `after` is the ledger length when the batch started (its assistant
    /// entry sits at `after - 1`). Only opt-in sinks receive it.
    Batch {
        label: String,
        after: usize,
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

/// Best-effort startup GC of `sessions/`, `checkpoints/` and `tasks/`.
///
/// They expire *together*: a conversation you can still resume must still be
/// rewindable and its task traces still viewable, and a checkpoint or trace
/// without the log that indexes it is just a file nobody can name. So the rule
/// is one rule — a per-session directory exists iff its session is kept —
/// which also collects orphans left by earlier crashes.
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
    for per_session in [&checkpoints_dir, &data_dir.join("tasks")] {
        let Ok(dirs) = fs::read_dir(per_session) else {
            continue;
        };
        for dir in dirs.flatten() {
            let id = dir.file_name().to_string_lossy().into_owned();
            if !kept.contains(&id) {
                let _ = fs::remove_dir_all(dir.path());
            }
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

/// Background tasks and monitors whose process was still running when the
/// session ended: started (per the tool's stable success prefix) but never
/// terminated by a completion note. One note lists them all.
fn lost_background_note(ledger: &Ledger) -> Option<String> {
    let mut open: Vec<String> = Vec::new();
    for entry in ledger.entries() {
        match entry {
            Entry::ToolResults(blocks) => {
                for block in blocks {
                    let crate::types::ContentBlock::ToolResult {
                        content,
                        is_error: false,
                        ..
                    } = block
                    else {
                        continue;
                    };
                    let started = content
                        .strip_prefix("Started monitor ")
                        .or_else(|| content.strip_prefix("Started background task "));
                    if let Some(id) = started.and_then(|rest| rest.split_whitespace().next()) {
                        open.push(id.trim_end_matches(':').to_string());
                    }
                }
            }
            Entry::Note(note) => {
                // Completion notes name the task and a terminal status; event
                // notes ("Monitor m1 (...): N new event lines") do neither.
                let terminated = note.contains("exited with code")
                    || note.contains("killed after")
                    || note.contains("timeout");
                if !terminated {
                    continue;
                }
                let id = note
                    .strip_prefix("Monitor ")
                    .or_else(|| note.strip_prefix("Background task "))
                    .and_then(|rest| rest.split_whitespace().next());
                if let Some(id) = id {
                    open.retain(|o| o != id);
                }
            }
            _ => {}
        }
    }
    (!open.is_empty()).then(|| {
        format!(
            "Resumed session: background task(s) {} did not survive the restart \
             — their processes are gone, though their log files remain readable. \
             Restart any watch that is still needed.",
            open.join(", ")
        )
    })
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
    /// Missing for sessions created before startup contexts were persisted.
    pub startup: Option<StartupContext>,
    /// The last environment observed before tcode stopped.
    pub environment: Option<EnvironmentSnapshot>,
    /// The last runtime environment explicitly delivered into the model's
    /// append-only context. Sessions with a startup snapshot always have this
    /// baseline; older logs without one may omit it.
    pub delivered_environment: Option<EnvironmentSnapshot>,
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
        let mut startup = None;
        let mut environment = None;
        let mut delivered_environment = None;
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
                LogEvent::StartupContext { startup: context } => {
                    environment = Some(context.environment.clone());
                    delivered_environment = Some(context.environment.clone());
                    startup = Some(context);
                }
                LogEvent::EnvironmentChanged {
                    environment: snapshot,
                } => {
                    // Pre-delivery versions emitted this event with the
                    // matching Note, so legacy snapshots are model-known.
                    delivered_environment = Some(snapshot.clone());
                    environment = Some(snapshot);
                }
                LogEvent::EnvironmentObserved {
                    environment: snapshot,
                } => {
                    environment = Some(snapshot);
                }
                LogEvent::EnvironmentDelivered {
                    environment: snapshot,
                } => {
                    delivered_environment = Some(snapshot);
                }
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
                // Trace-file lines; a session log never contains them.
                LogEvent::TaskMeta { .. }
                | LogEvent::TaskFinished { .. }
                | LogEvent::Batch { .. } => {}
            }
        }
        // Background processes don't survive a restart. Zero-guessing: tell
        // the model which watches are gone instead of letting it discover a
        // dead task id. Derived from the replayed ledger (not persisted), so
        // repeating a resume repeats the same single note.
        if let Some(note) = lost_background_note(&ledger) {
            ledger.append(Entry::Note(note));
        }
        let file = OpenOptions::new().append(true).open(&path)?;
        Ok(Resumed {
            store: Self {
                id,
                writer: BufWriter::new(file),
            },
            ledger,
            checkpoints,
            startup,
            environment,
            delivered_environment,
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

    fn environment(cwd: &str, changed_files: usize) -> EnvironmentSnapshot {
        EnvironmentSnapshot {
            cwd: cwd.into(),
            platform: "test".into(),
            os_version: Some("1".into()),
            command_shells: vec!["test shell".into()],
            git: crate::GitSnapshot {
                repository: true,
                branch: Some("main".into()),
                head: Some("abc initial".into()),
                changed_files,
                status_preview: Vec::new(),
            },
            date: "2026-07-17".into(),
        }
    }

    #[test]
    fn resume_recovers_the_last_startup_context_and_environment_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), dir.path()).unwrap();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        let startup = StartupContext {
            text: "stable prefix\n# Environment\nworking directory: /old".into(),
            environment: environment("/old", 0),
        };
        ledger.record_aux(&LogEvent::StartupContext {
            startup: startup.clone(),
        });
        ledger.record_aux(&LogEvent::EnvironmentChanged {
            environment: environment("/new", 2),
        });
        ledger.append(text("keep the prefix"));

        let resumed = SessionStore::resume(dir.path(), None).unwrap();
        assert_eq!(resumed.startup, Some(startup));
        assert_eq!(resumed.environment, Some(environment("/new", 2)));
        assert_eq!(resumed.delivered_environment, Some(environment("/new", 2)));
        assert_eq!(resumed.ledger.entries().len(), 1);
    }

    #[test]
    fn resume_keeps_unobserved_environment_separate_from_delivered_context() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), dir.path()).unwrap();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        let startup = StartupContext {
            text: "stable prefix".into(),
            environment: environment("/old", 0),
        };
        ledger.record_aux(&LogEvent::StartupContext {
            startup: startup.clone(),
        });
        ledger.record_aux(&LogEvent::EnvironmentObserved {
            environment: environment("/temporary", 1),
        });
        ledger.append(text("continue later"));

        let resumed = SessionStore::resume(dir.path(), None).unwrap();
        assert_eq!(resumed.startup, Some(startup));
        assert_eq!(resumed.environment, Some(environment("/temporary", 1)));
        assert_eq!(resumed.delivered_environment, Some(environment("/old", 0)));
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

    /// A session's log, its checkpoints and its task traces live and die
    /// together, empty logs are not conversations and never occupy a slot,
    /// and a per-session directory with no session left is garbage.
    #[test]
    fn the_sweep_keeps_conversations_and_their_checkpoints_together() {
        let dir = std::env::temp_dir().join(format!("tcode-sweep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let sessions = dir.join("sessions");
        let checkpoints = dir.join("checkpoints");
        let tasks = dir.join("tasks");
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
            fs::create_dir_all(tasks.join(id)).unwrap();
            fs::write(tasks.join(id).join("t1.jsonl"), "x").unwrap();
        }

        sweep_old_sessions(&dir);

        assert!(sessions.join(format!("{real_id}.jsonl")).exists());
        assert!(checkpoints.join(&real_id).exists(), "kept with its session");
        assert!(tasks.join(&real_id).exists(), "traces kept with it too");
        assert!(!empty_log.exists(), "a launch nobody spoke into");
        assert!(!checkpoints.join(&empty_id).exists());
        assert!(!tasks.join(&empty_id).exists());
        assert!(!checkpoints.join("deadbeef").exists(), "orphan collected");
        assert!(!tasks.join("deadbeef").exists(), "orphan trace collected");

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

    /// A monitor still running when the session ended is reported as lost on
    /// resume; one that already completed is not.
    #[test]
    fn resume_notes_monitors_that_did_not_survive_the_restart() {
        let tool_result = |content: &str| {
            Entry::ToolResults(vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: content.into(),
                is_error: false,
                images: vec![],
            }])
        };
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::new();
        let store = SessionStore::create(dir.path(), dir.path()).unwrap();
        ledger.attach_sink(Box::new(store));
        ledger.append(text("watch things"));
        ledger.append(tool_result(
            "Started monitor m1 (ci status): every line the script prints…",
        ));
        ledger.append(tool_result(
            "Started monitor m2 (log errors): every line the script prints…",
        ));
        ledger.append(tool_result(
            "Started background task b3: cargo watch\nIt keeps running…",
        ));
        // m1 finished before the session ended; m2 and b3 did not.
        ledger.append(Entry::Note(
            "Monitor m1 (ci status) exited with code 0 after 9s; full log: m1.log.".into(),
        ));
        drop(ledger);

        let resumed = SessionStore::resume(dir.path(), None).unwrap();
        let last = resumed.ledger.entries().last().unwrap();
        let Entry::Note(note) = last else {
            panic!("expected a lost-background note, got {last:?}");
        };
        assert!(note.contains("m2, b3"), "{note}");
        assert!(!note.contains("m1,"), "{note}");
        assert!(note.contains("did not survive"), "{note}");
    }
}
