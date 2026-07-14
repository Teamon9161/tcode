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

/// Scratch space for this project the model and harness can write to:
/// `<project-data>/scratchpad/`. Overflowed tool output and background task
/// logs live under `scratchpad/tool-output/`. Falls back to a temp dir when
/// there is no home directory. The directory is created lazily by writers.
pub fn scratchpad_dir(cwd: &Path) -> PathBuf {
    project_data_dir(cwd)
        .unwrap_or_else(|| std::env::temp_dir().join("tcode"))
        .join("scratchpad")
}

/// Where oversized tool output and background logs are parked so `read`/`grep`
/// can reach them — no separate paging tool needed.
pub fn tool_output_dir(cwd: &Path) -> PathBuf {
    scratchpad_dir(cwd).join("tool-output")
}

/// Best-effort: delete tool-output files older than a week. Called once at
/// startup, and only touches the directory if it already exists (so a session
/// with no overflow never creates it).
pub fn sweep_old_tool_output(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(7 * 24 * 3600))
        .unwrap_or(UNIX_EPOCH);
    for entry in entries.flatten() {
        let too_old = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|modified| modified < cutoff)
            .unwrap_or(false);
        if too_old {
            let _ = fs::remove_file(entry.path());
        }
    }
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
        let sessions = data_dir.join("sessions");
        fs::create_dir_all(&sessions)?;
        // Millisecond timestamp: unique per machine, sorts newest-last.
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let id = format!("{millis:013x}");
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(sessions.join(format!("{id}.jsonl")))?;
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
                LogEvent::Append { entry } => ledger.append(entry),
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
