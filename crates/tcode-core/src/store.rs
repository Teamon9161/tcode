//! Session persistence: a JSONL log of ledger operations. The log is
//! append-only even across rewinds — a rewind is recorded as an event,
//! not by erasing lines — so earlier branches stay recoverable.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::environment::{EnvironmentSnapshot, RuntimeCapabilities, StartupContext};
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
    /// Latest frontend/tool capability set observed by this process. It may be
    /// newer than what the model has seen because frontend switches are
    /// coalesced until a legal append boundary.
    RuntimeCapabilitiesObserved {
        capabilities: RuntimeCapabilities,
    },
    /// Latest frontend/tool capability set whose explanatory Note was actually
    /// appended, or the startup baseline for a new session.
    RuntimeCapabilitiesDelivered {
        capabilities: RuntimeCapabilities,
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
        /// The run this turn continues, when it is a follow-up. Each turn gets
        /// its own trace holding only its own appends, so this link is what
        /// lets a run be rebuilt whole from disk instead of amnesiac.
        #[serde(default)]
        resume_of: Option<String>,
        created_unix: u64,
    },
    /// Last line of a completed task trace file.
    TaskFinished {
        status: crate::task_trace::TaskRunStatus,
        tool_calls: usize,
        usage: crate::types::Usage,
    },
    /// The progress file this conversation took over. Recorded so a resumed
    /// session finds its way back to the same plan — the file itself is the
    /// truth and is re-read on resume, since the user may have edited it.
    ProgressAdopted {
        path: String,
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
/// `~/.tcode/projects/<id>/{sessions,checkpoints,blobs}/`.
pub fn project_data_dir(cwd: &Path) -> Option<PathBuf> {
    Some(project_dir_in(
        &crate::home_dir()?,
        &cwd.to_string_lossy().to_lowercase(),
    ))
}

/// The project directory for an already-normalized path key, under an explicit
/// home. Callers differ in what identifies "the project" — session state keys
/// on the cwd, auto memory on the project root, so that a worktree shares its
/// parent's memories — but the naming and the legacy migration are one rule,
/// kept here.
pub fn project_dir_in(home: &Path, key: &str) -> PathBuf {
    let projects = home.join(".tcode").join("projects");
    let dir = projects.join(project_id(key));
    let hashed = |key: &str| projects.join(format!("{:016x}", fnv1a(key.as_bytes())));
    adopt_dir(&hashed(key), &dir);
    // The same directory once hashed under the Windows extended-path spelling
    // `canonicalize` returns, before that prefix was stripped from path keys.
    // A project whose state is split across both spellings is exactly how a
    // conversation goes missing from `/resume`, so fold them together.
    adopt_dir(&hashed(&format!(r"\\?\{key}")), &dir);
    // And the readable form of that same spelling: `\\?\c:\code\rust\tcode`
    // folds to `----c--code-rust-tcode`, which is a real directory on any
    // machine where the desktop app opened a folder before it learned to strip
    // the prefix. Same split, same cost — the terminal and the app looking at
    // one folder through two separate histories.
    adopt_dir(&projects.join(project_id(&format!(r"\\?\{key}"))), &dir);
    dir
}

/// A path's directory-name form: `c:\code\rust\tcode` → `c--code-rust-tcode`.
///
/// Readable beats compact for this one name: it is what a person reads when
/// they go looking for a session log, a saved plan or a project's memories,
/// and an opaque hash makes every one of those directories unidentifiable.
/// Folding separators to `-` is not injective, so two paths can in principle
/// share a directory (`C:\code\rust-tcode` and `C:\code\rust\tcode`); that
/// merges two projects' state, which is a real but remote cost, and it is the
/// same trade every readable-id agent harness makes.
pub fn project_id(key: &str) -> String {
    let id: String = key
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    // This name prefixes every session log, checkpoint and scratch file
    // beneath it, and Windows still enforces MAX_PATH in plenty of places.
    // Keep the tail — the deepest components are the ones that name the
    // project; the drive and the shared parent directories are the noise.
    let length = id.chars().count();
    match length > PROJECT_ID_MAX {
        true => id.chars().skip(length - PROJECT_ID_MAX).collect(),
        false => id,
    }
}

const PROJECT_ID_MAX: usize = 80;

/// Adopt state a superseded key left behind. Sessions, checkpoints, plans,
/// task traces and memories all live inside one project directory, so moving
/// the directory is the entire migration.
///
/// Best-effort and idempotent: it runs on every lookup and costs one failed
/// stat once nothing is left to adopt. Delete it — and the `fnv1a` it is the
/// last caller of — when that is true of everyone's machine.
pub(crate) fn adopt_dir(legacy: &Path, dir: &Path) {
    if legacy == dir || !legacy.is_dir() {
        return;
    }
    if !dir.exists() && fs::rename(legacy, dir).is_ok() {
        return;
    }
    merge_into(legacy, dir);
}

/// Move `from`'s contents into `into`, recursing wherever both hold the same
/// directory — several superseded keys can map onto one destination, and each
/// of them has its own `sessions/`, `checkpoints/` and `tasks/`. Nothing
/// already at the destination is replaced: session ids are timestamps and
/// checkpoint names are content hashes, so a name that collides is either the
/// same file or the newer conversation, and neither wants overwriting. What
/// cannot be moved stays where it is — the superseded directory survives
/// holding exactly the files this refused to clobber, rather than being
/// emptied into one of them.
fn merge_into(from: &Path, into: &Path) {
    let Ok(entries) = fs::read_dir(from) else {
        return;
    };
    for entry in entries.flatten() {
        let source = entry.path();
        let target = into.join(entry.file_name());
        if !target.exists() {
            let _ = fs::rename(&source, &target);
        } else if source.is_dir() && target.is_dir() {
            merge_into(&source, &target);
        }
    }
    let _ = fs::remove_dir(from);
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

/// Where the window's browser saves files a page hands it — for the user's own
/// manual downloads and the `browser` tool's alike.
///
/// **Window-level, not per-project**, and that is the whole reason it lives here
/// beside `home_dir()` rather than under `project_data_dir`: the browser is one
/// shared instance, a person's manual download belongs to no session, and the
/// working folder can change mid-download, so there is no project to key on.
/// Kept under `~/.tcode` so a download never lands in the workspace as git
/// noise, and stable across folder switches so an in-flight file does not lose
/// its home. `None` only when there is no home directory at all, the same
/// condition every other `~/.tcode` path answers `None` to.
pub fn downloads_dir() -> Option<PathBuf> {
    Some(crate::home_dir()?.join(".tcode").join("downloads"))
}

/// Where this project's progress files live: `<project-data>/progress/`.
/// Runtime state, not part of the user's repository — an agent writing into a
/// repo is git noise and accidental commits; anyone who wants a plan tracked
/// exports it there explicitly. Falls back to a temp dir when there is no home
/// directory.
///
/// Adopts the superseded `plans/` directory on first use, so drafts written
/// before progress files existed keep showing up in `/plan list`.
pub fn progress_dir(cwd: &Path) -> PathBuf {
    let root = project_data_dir(cwd).unwrap_or_else(|| std::env::temp_dir().join("tcode"));
    let dir = root.join("progress");
    adopt_dir(&root.join("plans"), &dir);
    dir
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
    // Recover conversations that older `/clear` hid inside another session's
    // log as truncated segments. Must run before the retention scan, so the
    // extracted files are counted toward the keep limit.
    extract_cleared_segments(&sessions_dir);
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
    logs.sort_by_key(|(modified, ..)| std::cmp::Reverse(*modified)); // newest first

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

/// One-time migration: older `/clear` recorded `truncate_tail(0)` inside the
/// same session file, hiding earlier conversations from the picker. This scans
/// each log for full-clear events and writes the earlier segments as separate
/// session files so they become independently resumable.
///
/// Idempotent: extracted files get a deterministic id derived from the
/// original session, so a second run finds them already present and skips.
fn extract_cleared_segments(sessions_dir: &Path) {
    let Ok(entries) = fs::read_dir(sessions_dir) else {
        return;
    };
    let paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    for path in paths {
        let _ = extract_segments_from(&path, sessions_dir);
    }
}

/// Extract pre-clear segments from one session file. Returns the number of
/// segments written (0 when there is nothing to extract).
fn extract_segments_from(path: &Path, sessions_dir: &Path) -> Option<usize> {
    let file = File::open(path).ok()?;
    let lines: Vec<String> = BufReader::new(file)
        .lines()
        .collect::<Result<_, _>>()
        .ok()?;

    // Quick scan: does this file even have a full clear?
    if !lines
        .iter()
        .any(|line| line.contains("\"truncate_tail\"") && line.contains("\"len\":0"))
    {
        return Some(0);
    }

    // Parse just enough: collect meta/startup header, then split on
    // truncate_tail(0) boundaries.
    let mut header_lines: Vec<&str> = Vec::new(); // meta + startup_context
    let mut segments: Vec<Vec<&str>> = Vec::new(); // segments before clear
    let mut current: Vec<&str> = Vec::new();
    let mut meta_cwd = String::new();
    let mut meta_created: u64 = 0;
    for line in &lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Cheap shape check without full deserialization.
        let shape: LineShape = serde_json::from_str(trimmed).ok()?;
        match shape.ev.as_str() {
            "meta" => {
                header_lines.push(line);
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                    meta_cwd = v["cwd"].as_str().unwrap_or_default().to_string();
                    meta_created = v["created_unix"].as_u64().unwrap_or(0);
                }
            }
            "startup_context"
            | "environment_observed"
            | "environment_delivered"
            | "environment_changed" => {
                header_lines.push(line);
            }
            "truncate_tail" if shape.len == Some(0) => {
                if current.iter().any(|l| l.contains("\"ev\":\"append\"")) {
                    segments.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
            }
            _ => {
                current.push(line);
            }
        }
    }
    // `current` holds the tail segment (the one that survived all clears);
    // leave it in the original file.

    if segments.is_empty() {
        return Some(0);
    }

    let mut written = 0;
    for (i, segment) in segments.iter().enumerate() {
        // Deterministic id: original session timestamp + offset, so a re-run
        // of the migration finds the file already present.
        let segment_millis = (meta_created as u128) * 1000 + i as u128 + 1;
        let segment_id = format!("{segment_millis:013x}");
        let segment_path = sessions_dir.join(format!("{segment_id}.jsonl"));
        // Skip if already extracted.
        let Ok(mut file) = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&segment_path)
        else {
            continue;
        };
        // Write header.
        let meta = serde_json::json!({
            "ev": "meta",
            "id": segment_id,
            "cwd": meta_cwd,
            "created_unix": meta_created + i as u64,
        });
        let _ = writeln!(file, "{}", meta);
        for header in &header_lines {
            if header.contains("\"startup_context\"") {
                let _ = writeln!(file, "{header}");
            }
        }
        // Write segment events.
        for event_line in segment {
            let _ = writeln!(file, "{event_line}");
        }
        written += 1;
    }
    Some(written)
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
/// The literal prefix makes this deliberately narrow: ordinary harness and
/// user notes retain their original meaning on resume.
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

/// Close an interrupted tool batch before replay admits another entry. This
/// preserves the provider-required adjacency between an assistant's tool calls
/// and their results even when a later prompt was persisted after the failure.
fn close_replayed_tool_batch(ledger: &mut Ledger) {
    let cut_off = ledger.close_dangling_tool_calls(
        "No result: tcode stopped while this call was in flight. Whether it \
         took effect is unknown — verify before assuming either way.",
    );
    if !cut_off.is_empty() {
        ledger.append(Entry::Note(format!(
            "The previous tool batch ended before {} had a recorded result. Its state is \
             unknown; re-check what it may have changed before continuing.",
            cut_off.join(", ")
        )));
    }
}

/// The run id in a background sub-agent's dispatch line or completion note:
/// `[background <kind> sub-agent <id> …]`.
fn background_agent_run_id(text: &str) -> Option<&str> {
    text.strip_prefix("[background ")?
        .split_once(" sub-agent ")?
        .1
        .split_whitespace()
        .next()
}

/// Background tasks, monitors, and background sub-agent runs that were still in
/// flight when the session ended: started (per the tool's stable success
/// prefix, or a `[background … dispatched on …]` line) but never terminated by
/// a completion note. One note lists them all — in-memory processes and
/// spawned runs alike are gone after a restart.
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
                    // A background sub-agent's dispatch line opens a run whose
                    // completion note (if any) closes it below.
                    if content.contains(" dispatched on ") {
                        if let Some(id) = background_agent_run_id(content) {
                            open.push(id.to_string());
                        }
                    }
                }
            }
            Entry::Note(note) => {
                // A background sub-agent completion note closes its run.
                if note.contains(" finished on ") || note.contains(" failed:") {
                    if let Some(id) = background_agent_run_id(note) {
                        open.retain(|o| o != id);
                    }
                    continue;
                }
                // Task/monitor completion notes name the task and a terminal
                // status; event notes ("Monitor m1 (...): N new event lines") do
                // neither.
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
            "Resumed session: background work {} did not survive the restart — the \
             processes and runs are gone, though any log files remain readable. \
             Re-run or re-dispatch anything still needed.",
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
    /// The last frontend/tool capability set observed before tcode stopped.
    pub capabilities: Option<RuntimeCapabilities>,
    /// The last frontend/tool capability set explicitly delivered into the
    /// model's append-only context.
    pub delivered_capabilities: Option<RuntimeCapabilities>,
    /// The progress file this conversation was last working through. Only the
    /// path survives; the plan is re-read from disk on resume.
    pub progress: Option<PathBuf>,
}

/// A resumable conversation in one project, suitable for a UI picker.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub last_user_preview: String,
    pub modified: Option<SystemTime>,
}

fn summary_preview(summary: &str) -> String {
    summary
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .unwrap_or_else(|| "Compacted conversation".into())
}

/// The line a picker shows for one `Entry::User`, if it has one at all.
fn prompt_preview(blocks: &[crate::types::ContentBlock]) -> Option<String> {
    blocks.iter().find_map(|b| match b {
        crate::types::ContentBlock::Text { text } if !text.starts_with("<tcode-status>") => {
            text.lines().next().map(str::to_owned)
        }
        _ => None,
    })
}

/// One log line, read for its shape and nothing else.
///
/// `LogEvent` is internally tagged, so deserializing one buffers the whole line
/// into a generic tree before re-reading it as a variant — every prompt, every
/// tool result, every pasted image, twice. That is the right price to resume
/// one conversation and the wrong one for a picker, which asks the same
/// question of every log in the project. A plain struct has no buffering step:
/// serde skips the fields it does not name without allocating them.
#[derive(Deserialize)]
struct LineShape {
    ev: String,
    /// `Meta`: the log's own name for itself.
    id: Option<String>,
    /// `Append`: the entry's kind, its payload skipped.
    entry: Option<EntryShape>,
    /// `TruncateTail`.
    len: Option<usize>,
    /// `Compact`.
    summary: Option<String>,
    upto: Option<usize>,
}

#[derive(Deserialize)]
struct EntryShape {
    kind: String,
}

/// A replayed entry, reduced to the only thing a preview can come from.
enum Mark {
    User(String),
    Summary(String),
    Other,
}

/// What the picker shows for one log, without materializing its entries.
///
/// A replay and not a scan: `/clear` and rewind are recorded as events, so
/// reading the appends backwards would resurrect a conversation that was
/// deliberately cleared. It replays `append` / `truncate_tail` / `compact` and
/// ignores every other event, which is safe by construction rather than by
/// enumeration — those three are the only mutations a [`Ledger`] has, so
/// nothing this build fails to recognize can move the answer. What it keeps
/// per entry is one `Mark`, so a log full of megabyte tool results costs the
/// same as one full of one-line notes.
///
/// A line that is not valid JSON still drops the whole log, as the full replay
/// did: a preview taken from half a file is a confident claim about a
/// conversation that will not open. The one behaviour that changed is the
/// narrower case of a log carrying an *event type* from a newer format — it is
/// now listed rather than hidden, and selecting it still reports the real
/// replay error, which is the recovery path `list` already promises.
fn preview(path: &Path) -> Option<SessionInfo> {
    let modified = fs::metadata(path).and_then(|m| m.modified()).ok();
    let mut id = path.file_stem()?.to_string_lossy().into_owned();
    let mut entries: Vec<Mark> = Vec::new();

    for line in BufReader::new(File::open(path).ok()?).lines() {
        let line = line.ok()?;
        if line.trim().is_empty() {
            continue;
        }
        let shape: LineShape = serde_json::from_str(&line).ok()?;
        match shape.ev.as_str() {
            "meta" => id = shape.id?,
            "append" => entries.push(match shape.entry?.kind.as_str() {
                // Two entry kinds can carry a preview and both are small, so
                // those lines — and only those — are read a second time with
                // the real types instead of a re-guessed shape.
                "user" | "summary" => {
                    let LogEvent::Append { entry } = serde_json::from_str(&line).ok()? else {
                        return None;
                    };
                    match entry {
                        Entry::User(blocks) => {
                            prompt_preview(&blocks).map_or(Mark::Other, Mark::User)
                        }
                        Entry::Summary(summary) => Mark::Summary(summary_preview(&summary)),
                        _ => Mark::Other,
                    }
                }
                _ => Mark::Other,
            }),
            "truncate_tail" => entries.truncate(shape.len?),
            "compact" => {
                let tail = entries.split_off(shape.upto?.min(entries.len()));
                entries.clear();
                entries.push(Mark::Summary(summary_preview(&shape.summary?)));
                entries.extend(tail);
            }
            _ => {}
        }
    }

    let last_user_preview = entries
        .iter()
        .rev()
        .find_map(|mark| match mark {
            Mark::User(text) => Some(text.clone()),
            _ => None,
        })
        // Compaction intentionally removes the historical User entries. The
        // replayed summary is then the only honest preview source; reading raw
        // append events would revive cleared history.
        .or_else(|| {
            entries.iter().find_map(|mark| match mark {
                Mark::Summary(text) => Some(text.clone()),
                _ => None,
            })
        })?;
    Some(SessionInfo {
        id,
        last_user_preview,
        modified,
    })
}

/// A bounded slice of resumable conversations.
#[derive(Debug)]
pub struct SessionPage {
    pub sessions: Vec<SessionInfo>,
    pub has_more: bool,
}

fn session_logs(data_dir: &Path) -> Result<Vec<PathBuf>, StoreError> {
    let sessions = data_dir.join("sessions");
    let mut files: Vec<PathBuf> = fs::read_dir(&sessions)
        .map_err(|_| StoreError::NoSession)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect();
    files.sort();
    files.reverse();
    Ok(files)
}

impl SessionStore {
    /// List recent non-empty sessions in newest-first order. This is kept
    /// separate from `resume`: starting tcode creates a fresh log first, and
    /// that empty log must not hide the conversations a user can restore.
    ///
    /// It reads every log in the project, so it reads them the cheap way
    /// ([`preview`]) rather than through `resume_path`. A damaged log is
    /// skipped rather than failing the list: one broken conversation must not
    /// empty the picker, and selecting it still reports the replay error.
    pub fn list(data_dir: &Path) -> Result<Vec<SessionInfo>, StoreError> {
        Ok(session_logs(data_dir)?
            .iter()
            .filter_map(|path| preview(path))
            .collect())
    }

    /// List at most `limit` conversations older than `before`.
    ///
    /// The cursor is a session id rather than an offset. New conversations may
    /// be created while a picker is open; an offset would then shift every older
    /// row and either duplicate or skip one on the next request. Session ids are
    /// the chronologically sortable log stems, so filtering before the last id
    /// returned keeps a walk stable while new logs arrive.
    pub fn list_page(
        data_dir: &Path,
        before: Option<&str>,
        limit: usize,
    ) -> Result<SessionPage, StoreError> {
        let files = session_logs(data_dir)?;
        let mut sessions: Vec<SessionInfo> = files
            .iter()
            .filter(|path| {
                let Some(cursor) = before else {
                    return true;
                };
                path.file_stem()
                    .is_some_and(|stem| stem.to_string_lossy().as_ref() < cursor)
            })
            // Read one valid preview past the requested page. Empty or damaged
            // logs are skipped and therefore never create a false "more" row.
            .filter_map(|path| preview(path))
            .take(limit.saturating_add(1))
            .collect();
        let has_more = sessions.len() > limit;
        sessions.truncate(limit);
        Ok(SessionPage { sessions, has_more })
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
        Self::resume_path(&path)
    }

    /// Replay an already-selected JSONL log. Keeping selection separate lets
    /// the session picker reuse its directory scan for every candidate.
    fn resume_path(path: &Path) -> Result<Resumed, StoreError> {
        let mut ledger = Ledger::new();
        let mut checkpoints = Vec::new();
        let mut startup = None;
        let mut environment = None;
        let mut delivered_environment = None;
        let mut capabilities = None;
        let mut delivered_capabilities = None;
        let mut progress = None;
        let mut id = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        for line in BufReader::new(File::open(path)?).lines() {
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
                LogEvent::RuntimeCapabilitiesObserved {
                    capabilities: snapshot,
                } => {
                    capabilities = Some(snapshot);
                }
                LogEvent::RuntimeCapabilitiesDelivered {
                    capabilities: snapshot,
                } => {
                    delivered_capabilities = Some(snapshot);
                }
                // Before `Entry::UserNote` existed, approval annotations were
                // persisted as a pre-formatted machine note. Upgrade that
                // unambiguous legacy shape while replaying so resumed
                // transcripts show the person's own words, just like live
                // annotations and newly-created sessions.
                LogEvent::Append { entry } => {
                    // A failed live batch can leave the assistant's tool calls
                    // durable while a later user entry still reaches the log.
                    // Close it before that ordinary entry, never at replay end,
                    // so OpenAI receives the tool results immediately after
                    // the assistant message that requested them.
                    if !matches!(&entry, Entry::ToolResults(_)) {
                        close_replayed_tool_batch(&mut ledger);
                    }
                    ledger.append(upgrade_legacy_entry(entry));
                }
                LogEvent::TruncateTail { len } => ledger.truncate_tail(len),
                LogEvent::Compact { summary, upto } => ledger.compact(summary, upto),
                LogEvent::Checkpoint {
                    ledger_len,
                    path,
                    saved,
                } => checkpoints.push((ledger_len, path, saved)),
                // The empty path is how a finished plan is recorded: the
                // conversation moved on, and resume must not reopen it.
                LogEvent::ProgressAdopted { path } => {
                    progress = Some(path)
                        .filter(|path| !path.is_empty())
                        .map(PathBuf::from)
                }
                // Trace-file lines; a session log never contains them.
                LogEvent::TaskMeta { .. }
                | LogEvent::TaskFinished { .. }
                | LogEvent::Batch { .. } => {}
            }
        }
        // A log that truly ends mid-batch has no later entry to trigger the
        // boundary repair above, so close it here too.
        close_replayed_tool_batch(&mut ledger);
        // Background processes don't survive a restart. Zero-guessing: tell
        // the model which watches are gone instead of letting it discover a
        // dead task id. Derived from the replayed ledger (not persisted), so
        // repeating a resume repeats the same single note.
        if let Some(note) = lost_background_note(&ledger) {
            ledger.append(Entry::Note(note));
        }
        let file = OpenOptions::new().append(true).open(path)?;
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
            capabilities,
            delivered_capabilities,
            progress,
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

    #[test]
    fn resume_old_logs_without_capability_events() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), dir.path()).unwrap();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        ledger.append(text("legacy prompt"));

        let resumed = SessionStore::resume(dir.path(), None).unwrap();
        assert_eq!(resumed.capabilities, None);
        assert_eq!(resumed.delivered_capabilities, None);
        assert_eq!(resumed.ledger.entries().len(), 1);
    }

    #[test]
    fn resume_replays_capability_observed_and_delivered_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), dir.path()).unwrap();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        let delivered = RuntimeCapabilities::new("tui", ["bash", "read"]);
        let observed = RuntimeCapabilities::new("app", ["bash", "browser", "read", "show"]);
        ledger.record_aux(&LogEvent::RuntimeCapabilitiesDelivered {
            capabilities: delivered.clone(),
        });
        ledger.record_aux(&LogEvent::RuntimeCapabilitiesObserved {
            capabilities: observed.clone(),
        });
        ledger.append(text("continue later"));

        let resumed = SessionStore::resume(dir.path(), None).unwrap();
        assert_eq!(resumed.capabilities, Some(observed));
        assert_eq!(resumed.delivered_capabilities, Some(delivered));
        assert_eq!(resumed.ledger.entries().len(), 1);
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
    fn project_id_reads_as_the_path_it_came_from() {
        assert_eq!(project_id("c:\\code\\rust\\tcode"), "c--code-rust-tcode");
        assert_eq!(project_id("/home/me/code/tcode"), "-home-me-code-tcode");
        // The tail names the project; a deep path drops its shared prefix
        // rather than the part that identifies it.
        let deep = format!("c:\\{}\\tcode", "nested\\".repeat(30));
        let id = project_id(&deep);
        assert_eq!(id.chars().count(), PROJECT_ID_MAX);
        assert!(id.ends_with("-tcode"), "{id}");
    }

    /// Sessions, memories and plans written under the old hashed name stay
    /// reachable: the directory is renamed, not abandoned.
    #[test]
    fn a_hashed_project_directory_is_adopted_under_its_readable_name() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join(".tcode").join("projects");
        let key = "c:\\code\\rust\\tcode";
        let legacy = projects.join(format!("{:016x}", fnv1a(key.as_bytes())));
        fs::create_dir_all(legacy.join("sessions")).unwrap();
        fs::write(legacy.join("sessions").join("old.jsonl"), "{}").unwrap();

        let dir = project_dir_in(home.path(), key);

        assert_eq!(dir, projects.join("c--code-rust-tcode"));
        assert!(dir.join("sessions").join("old.jsonl").exists());
        assert!(!legacy.exists());
        // Idempotent: the second lookup has nothing left to adopt.
        assert_eq!(project_dir_in(home.path(), key), dir);
        assert!(dir.join("sessions").join("old.jsonl").exists());
    }

    /// One location, two spellings: sessions recorded before the Windows
    /// extended-path prefix was stripped from path keys landed in a directory
    /// of their own, which is how a conversation disappears from `/resume`.
    #[test]
    fn the_extended_path_spelling_of_a_project_is_the_same_project() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join(".tcode").join("projects");
        let key = "c:\\code\\rust\\tcode";
        let extended = projects.join(format!("{:016x}", fnv1a(format!(r"\\?\{key}").as_bytes())));
        fs::create_dir_all(extended.join("sessions")).unwrap();
        fs::write(extended.join("sessions").join("lost.jsonl"), "{}").unwrap();

        let dir = project_dir_in(home.path(), key);

        assert!(dir.join("sessions").join("lost.jsonl").exists());
        assert!(!extended.exists());
    }

    /// The desktop app keyed on the extended-path spelling directly, so the
    /// split also exists under the *readable* id — four leading dashes where
    /// the prefix folded.
    #[test]
    fn the_readable_extended_path_directory_is_the_same_project() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join(".tcode").join("projects");
        let key = "c:\\code\\rust\\tcode";
        let extended = projects.join("----c--code-rust-tcode");
        fs::create_dir_all(extended.join("sessions")).unwrap();
        fs::write(extended.join("sessions").join("desktop.jsonl"), "{}").unwrap();
        assert_eq!(project_id(&format!(r"\\?\{key}")), "----c--code-rust-tcode");

        let dir = project_dir_in(home.path(), key);

        assert!(dir.join("sessions").join("desktop.jsonl").exists());
        assert!(!extended.exists());
    }

    /// Several superseded keys can map onto one readable name, and each of
    /// them has its own `sessions/`. Folding must reach inside those, or the
    /// conversations in the second directory stay just as invisible as before.
    #[test]
    fn a_second_hashed_directory_folds_into_the_one_already_adopted() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join(".tcode").join("projects");
        let dir = projects.join("c--code-rust-tcode");
        fs::create_dir_all(dir.join("sessions")).unwrap();
        fs::write(dir.join("sessions").join("kept.jsonl"), "current").unwrap();
        let legacy = projects.join("deadbeefdeadbeef");
        fs::create_dir_all(legacy.join("sessions")).unwrap();
        fs::write(legacy.join("sessions").join("older.jsonl"), "older").unwrap();
        fs::write(legacy.join("sessions").join("kept.jsonl"), "shadowed").unwrap();
        fs::create_dir_all(legacy.join("memory")).unwrap();
        fs::write(legacy.join("memory").join("MEMORY.md"), "remembered").unwrap();

        adopt_dir(&legacy, &dir);

        assert_eq!(
            fs::read_to_string(dir.join("sessions").join("older.jsonl")).unwrap(),
            "older",
            "a conversation from the superseded directory becomes resumable"
        );
        assert_eq!(
            fs::read_to_string(dir.join("sessions").join("kept.jsonl")).unwrap(),
            "current",
            "a name that collides keeps the destination's file"
        );
        assert_eq!(
            fs::read_to_string(dir.join("memory").join("MEMORY.md")).unwrap(),
            "remembered"
        );
        assert!(!legacy.join("memory").exists(), "adopted whole");
        assert!(
            legacy.join("sessions").join("kept.jsonl").exists(),
            "the file that was not adopted stays where it is rather than \
             being deleted or clobbering the destination"
        );
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

    /// Compaction shrinks the model's context, not the user's transcript: a
    /// resumed session still shows what was said before the summary. The log
    /// always held those appends; replaying them into the ledger's archive is
    /// what makes them visible again.
    #[test]
    fn resume_restores_the_conversation_from_before_a_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), dir.path()).unwrap();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        ledger.append(text("the original request"));
        ledger.append(Entry::Assistant(vec![ContentBlock::Text {
            text: "the original answer".into(),
        }]));
        ledger.compact("what happened earlier".into(), ledger.len());
        ledger.append(text("the follow-up"));
        drop(ledger);

        let resumed = SessionStore::resume(dir.path(), None).unwrap();
        assert!(
            matches!(
                resumed.ledger.archived(),
                [Entry::User(_), Entry::Assistant(_)]
            ),
            "{:?}",
            resumed.ledger.archived()
        );
        assert!(
            matches!(&resumed.ledger.entries()[0], Entry::Summary(s) if s == "what happened earlier")
        );
        // ...and the model still resumes on the summary alone.
        let sent = format!("{:?}", resumed.ledger.as_messages());
        assert!(!sent.contains("the original request"), "{sent}");
    }

    /// `/clear` persists as `truncate_tail(0)`; replaying it must not bring
    /// the cleared (and previously compacted) conversation back.
    #[test]
    fn resume_does_not_resurrect_a_cleared_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), dir.path()).unwrap();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        ledger.append(text("private conversation"));
        ledger.compact("summary of it".into(), 1);
        ledger.truncate_tail(0);
        drop(ledger);

        let resumed = SessionStore::resume(dir.path(), None).unwrap();
        assert!(resumed.ledger.is_empty());
        assert!(resumed.ledger.archived().is_empty());
    }

    #[test]
    fn instructions_keep_their_distinct_disk_encoding() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), dir.path()).unwrap();
        let id = store.id.clone();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        ledger.append(Entry::Instruction("current-format instruction".into()));
        drop(ledger);

        let log =
            fs::read_to_string(dir.path().join("sessions").join(format!("{id}.jsonl"))).unwrap();
        assert!(log.contains("\"kind\":\"instruction\""));

        let resumed = SessionStore::resume(dir.path(), Some(&id)).unwrap();
        assert!(matches!(
            resumed.ledger.entries(),
            [Entry::Instruction(text)] if text == "current-format instruction"
        ));
    }

    #[test]
    fn resume_closes_interrupted_tool_calls_before_a_later_user_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), dir.path()).unwrap();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        ledger.append(text("inspect the project"));
        ledger.append(Entry::Assistant(vec![ContentBlock::ToolUse {
            id: "call-1".into(),
            name: "grep".into(),
            input: serde_json::json!({"pattern": "TODO"}),
        }]));
        // This is the shape produced if the live executor fails after the
        // assistant response is durable but before its results are committed.
        ledger.append(text("continue with the next task"));
        drop(ledger);

        let resumed = SessionStore::resume(dir.path(), None).unwrap();
        assert!(matches!(
            resumed.ledger.entries(),
            [
                Entry::User(_),
                Entry::Assistant(_),
                Entry::ToolResults(results),
                Entry::Note(_),
                Entry::User(_),
            ] if matches!(&results[..], [ContentBlock::ToolResult { tool_use_id, is_error: true, .. }] if tool_use_id == "call-1")
        ));

        // `as_messages` keeps the repair in the user turn immediately after
        // the assistant call, so provider adapters flatten tool results before
        // the later human input.
        let messages = resumed.ledger.as_messages();
        assert_eq!(messages.len(), 3);
        assert!(matches!(
            &messages[2].content[0],
            ContentBlock::ToolResult { tool_use_id, is_error: true, .. }
                if tool_use_id == "call-1"
        ));
        assert!(messages[2].content.iter().any(
            |block| matches!(block, ContentBlock::Text { text } if text.contains("continue with the next task"))
        ));
    }

    #[test]
    fn legacy_notes_remain_notes_on_resume() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), dir.path()).unwrap();
        let id = store.id.clone();
        let note =
            "New directory-scoped instructions were discovered for the requested tool paths.\n\
                    tcode-memory-project: C:/proj\n\
                    tcode-memory-source: C:/proj/AGENTS.md\n\
                    rule";
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        ledger.append(Entry::Note(note.into()));
        drop(ledger);

        let resumed = SessionStore::resume(dir.path(), Some(&id)).unwrap();
        assert!(matches!(
            resumed.ledger.entries(),
            [Entry::Note(text)] if text == note
        ));
    }

    #[test]
    fn list_skips_a_corrupt_log_and_keeps_other_conversations() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), dir.path()).unwrap();
        let id = store.id.clone();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        ledger.append(text("recoverable conversation"));
        drop(ledger);

        fs::write(
            dir.path().join("sessions").join("corrupt.jsonl"),
            r#"{\"ev\":\"append\",\"entry\":{\"kind\":\"future_entry\"}}"#,
        )
        .unwrap();

        let sessions = SessionStore::list(dir.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, id);
    }

    #[test]
    fn list_page_uses_a_stable_cursor_and_reads_only_the_requested_slice() {
        let dir = tempfile::tempdir().unwrap();
        for millis in 1..=5 {
            let store = SessionStore::create_at_millis(dir.path(), dir.path(), millis).unwrap();
            let mut ledger = Ledger::new();
            ledger.attach_sink(Box::new(store));
            ledger.append(text(&format!("conversation {millis}")));
        }

        let first = SessionStore::list_page(dir.path(), None, 2).unwrap();
        assert_eq!(
            first
                .sessions
                .iter()
                .map(|session| session.last_user_preview.as_str())
                .collect::<Vec<_>>(),
            ["conversation 5", "conversation 4"]
        );
        assert!(first.has_more);

        // A newer log arriving between requests must not move the next page.
        let store = SessionStore::create_at_millis(dir.path(), dir.path(), 6).unwrap();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        ledger.append(text("conversation 6"));
        let cursor = first.sessions.last().unwrap().id.clone();

        let second = SessionStore::list_page(dir.path(), Some(&cursor), 2).unwrap();
        assert_eq!(
            second
                .sessions
                .iter()
                .map(|session| session.last_user_preview.as_str())
                .collect::<Vec<_>>(),
            ["conversation 3", "conversation 2"]
        );
        assert!(second.has_more);

        let cursor = second.sessions.last().unwrap().id.clone();
        let last = SessionStore::list_page(dir.path(), Some(&cursor), 2).unwrap();
        assert_eq!(last.sessions[0].last_user_preview, "conversation 1");
        assert!(!last.has_more);
    }

    #[test]
    fn list_includes_a_fully_compacted_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), dir.path()).unwrap();
        let id = store.id.clone();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        ledger.append(text("original user request"));
        ledger.compact(
            "## Task\n\nRestore today's compacted conversation in /resume.".into(),
            ledger.len(),
        );
        drop(ledger);

        let sessions = SessionStore::list(dir.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, id);
        assert_eq!(
            sessions[0].last_user_preview,
            "Restore today's compacted conversation in /resume."
        );
    }

    /// The picker replays the log rather than scanning it, so the two events
    /// that *remove* history have to mean the same thing here as in `Ledger`.
    /// Scanning appends backwards would answer "still here" to both.
    #[test]
    fn list_respects_rewind_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), dir.path()).unwrap();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        ledger.append(text("the prompt that stays"));
        ledger.append(text("the prompt that was rewound away"));
        ledger.truncate_tail(1);
        drop(ledger);

        let sessions = SessionStore::list(dir.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].last_user_preview, "the prompt that stays");

        let cleared = SessionStore::create(dir.path(), dir.path()).unwrap();
        let id = cleared.id.clone();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(cleared));
        ledger.append(text("private conversation"));
        ledger.compact("summary of it".into(), 1);
        ledger.truncate_tail(0);
        drop(ledger);

        let sessions = SessionStore::list(dir.path()).unwrap();
        assert!(
            !sessions.iter().any(|s| s.id == id),
            "a cleared conversation must not come back in the picker: {sessions:?}"
        );
    }

    /// The preview comes from the last prompt the *person* typed. Everything
    /// after it — the model's reply, the tool results it produced — is skipped
    /// without being parsed, and that shortcut must not change the answer.
    #[test]
    fn list_previews_the_last_prompt_not_the_last_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), dir.path()).unwrap();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        ledger.append(text("first question"));
        ledger.append(text("second question\nwith a second line"));
        ledger.append(Entry::Assistant(vec![ContentBlock::Text {
            text: "an answer".into(),
        }]));
        ledger.append(Entry::ToolResults(vec![ContentBlock::ToolResult {
            tool_use_id: "1".into(),
            content: "a megabyte of file, in principle".into(),
            is_error: false,
            images: Vec::new(),
        }]));
        // A status block is harness bookkeeping wearing a user role; the
        // picker must look past it to the words someone wrote.
        ledger.append(text("<tcode-status>not a prompt"));
        drop(ledger);

        let sessions = SessionStore::list(dir.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].last_user_preview, "second question");
    }

    #[test]
    fn list_does_not_treat_harness_notes_as_conversations() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), dir.path()).unwrap();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        ledger.append(Entry::Note("harness context only".into()));
        drop(ledger);

        assert!(SessionStore::list(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn leaves_legacy_instruction_notes_as_notes() {
        let entry = upgrade_legacy_entry(Entry::Note(
            "New directory-scoped instructions were discovered for the requested tool paths.\n\
             tcode-memory-source: C:/proj/AGENTS.md\n\
             rule"
                .into(),
        ));
        assert!(matches!(entry, Entry::Note(text) if text.ends_with("rule")));
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

    /// A background sub-agent dispatched but not yet finished is lost on resume;
    /// one whose completion note already landed is not.
    #[test]
    fn resume_notes_background_agents_that_did_not_finish() {
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
        ledger.append(text("explore in the background"));
        ledger.append(tool_result(
            "[background explore sub-agent t1 dispatched on scout-1: survey. It runs on its own…]",
        ));
        ledger.append(tool_result(
            "[background general sub-agent t2 dispatched on scout-1: build. It runs on its own…]",
        ));
        // t1 delivered its completion note; t2 never did.
        ledger.append(Entry::Note(
            "[background explore sub-agent t1 finished on scout-1: 2 tool calls]\n<background-report run=\"t1\" agent=\"explore\">\ndone\n</background-report>".into(),
        ));
        drop(ledger);

        let resumed = SessionStore::resume(dir.path(), None).unwrap();
        let Entry::Note(note) = resumed.ledger.entries().last().unwrap() else {
            panic!("expected a lost-background note");
        };
        assert!(note.contains("t2"), "{note}");
        assert!(!note.contains("t1"), "{note}");
    }

    #[test]
    fn extract_segments_recovers_cleared_conversations() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        // Simulate old /clear behavior: two conversations in one file,
        // separated by truncate_tail(0).
        let store = SessionStore::create(dir.path(), dir.path()).unwrap();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        ledger.append(text("first conversation prompt"));
        ledger.append(Entry::Assistant(vec![ContentBlock::Text {
            text: "first answer".into(),
        }]));
        // Old /clear: truncate to 0
        ledger.truncate_tail(0);
        // Second conversation in same session
        ledger.append(text("second conversation prompt"));
        drop(ledger);

        // Before migration: only the second conversation is visible.
        let before = SessionStore::list(dir.path()).unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].last_user_preview, "second conversation prompt");

        // Run extraction.
        extract_cleared_segments(&sessions);

        // After migration: both conversations are visible.
        let after = SessionStore::list(dir.path()).unwrap();
        assert_eq!(after.len(), 2);
        let previews: Vec<&str> = after.iter().map(|s| s.last_user_preview.as_str()).collect();
        assert!(previews.contains(&"first conversation prompt"));
        assert!(previews.contains(&"second conversation prompt"));

        // The extracted session is independently resumable.
        let extracted = after
            .iter()
            .find(|s| s.last_user_preview == "first conversation prompt")
            .unwrap();
        let resumed = SessionStore::resume(dir.path(), Some(&extracted.id)).unwrap();
        assert!(resumed.ledger.entries().iter().any(
            |e| matches!(e, Entry::User(blocks) if blocks.iter().any(
                |b| matches!(b, ContentBlock::Text { text } if text == "first conversation prompt")
            ))
        ));

        // Idempotent: running again does not create duplicates.
        extract_cleared_segments(&sessions);
        let again = SessionStore::list(dir.path()).unwrap();
        assert_eq!(again.len(), 2);
    }
}
