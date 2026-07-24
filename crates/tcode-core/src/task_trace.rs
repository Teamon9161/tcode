//! Task run traces: each `task` sub-agent run persists its own ledger as a
//! JSONL file next to the parent session (`<data>/tasks/<session-id>/tN.jsonl`),
//! reusing the session log's `LogEvent` format. A trace exists purely for the
//! UI and for post-hoc inspection — it never enters the parent's provider
//! ledger, so recording it costs the conversation nothing.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::ledger::{Ledger, LedgerSink};
use crate::store::{LogEvent, StoreError};
use crate::types::Usage;

/// How a sub-agent run ended. `Interrupted` is never written: it is what
/// `discover` reports for a trace whose process died before the finish line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskRunStatus {
    Running,
    Done,
    Failed,
    Cancelled,
    Interrupted,
}

impl TaskRunStatus {
    pub fn label(&self) -> &'static str {
        match self {
            TaskRunStatus::Running => "running",
            TaskRunStatus::Done => "done",
            TaskRunStatus::Failed => "failed",
            TaskRunStatus::Cancelled => "cancelled",
            TaskRunStatus::Interrupted => "interrupted",
        }
    }
}

/// Everything a UI needs to list a run without replaying its ledger.
#[derive(Debug, Clone)]
pub struct TaskRunMeta {
    /// Stable per-session run id: `t1`, `t2`, …
    pub id: String,
    /// The provider-issued tool_use id of the parent `task` call, tying the
    /// run to the exact ledger entry that spawned it.
    pub parent_call: String,
    pub kind: String,
    pub model: String,
    pub prompt: String,
    /// One-line parent-authored description, with a prompt-derived fallback
    /// for traces created before task summaries existed.
    pub summary: String,
    /// Set when this trace records a follow-up turn of an earlier run: each
    /// turn's trace holds only its own appends, so the chain is what rebuilds
    /// the run whole (`resume_chain`).
    pub resume_of: Option<String>,
    pub created_unix: u64,
    pub status: TaskRunStatus,
    pub tool_calls: usize,
    pub usage: Usage,
    /// Trace file, when the run was persisted.
    pub path: Option<PathBuf>,
}

/// A fully loaded trace, ready for transcript replay.
pub struct TaskRunLoad {
    pub meta: TaskRunMeta,
    pub ledger: Ledger,
    /// Batch display labels the sub-agent's loop recorded at execution time,
    /// keyed by ledger length when the batch started (the batch's assistant
    /// entry is at `after - 1`). Replay reads these instead of re-deriving
    /// the grouping rule.
    pub batch_labels: Vec<(usize, String)>,
}

/// Per-session run-id allocator and trace-file factory. Lives in `ToolCtx`
/// so the `task` tool can allocate ids without knowing about persistence:
/// without a bound root (ephemeral session, sub-agent context) ids are still
/// issued but nothing is written.
pub struct TaskTraces {
    root: Option<PathBuf>,
    next_seq: u64,
}

impl Default for TaskTraces {
    fn default() -> Self {
        Self {
            root: None,
            next_seq: 1,
        }
    }
}

impl TaskTraces {
    /// Bind (or unbind) the directory traces persist into. Continues the id
    /// sequence past any runs already recorded there, so a resumed session
    /// never reissues an existing id.
    pub fn bind_root(&mut self, root: Option<PathBuf>) {
        self.next_seq = root.as_deref().map_or(1, next_seq_in);
        self.root = root;
    }

    /// Where this session records `id`, whether or not it exists yet. `None`
    /// without a bound root (an ephemeral session persists nothing) and for
    /// anything that is not an issued id shape: **run ids reach this from the
    /// model**, so the only accepted form is `t<digits>` — a raw join would
    /// turn `resume` into an arbitrary-file read through a tool that needs no
    /// approval.
    pub fn trace_path(&self, id: &str) -> Option<PathBuf> {
        let digits = id.strip_prefix('t')?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(self.root.as_ref()?.join(format!("{id}.jsonl")))
    }

    /// Where a cohort's append-only channel log lives, alongside the run
    /// traces in this session's directory. Like `trace_path`, the id reaches
    /// this from the model, so the only accepted shape is `c<digits>` — a raw
    /// join would turn a cohort id into an arbitrary-file write. `None` without
    /// a bound root (an ephemeral session persists nothing) or on a bad id.
    pub fn cohort_channel_path(&self, id: &str) -> Option<PathBuf> {
        let digits = id.strip_prefix('c')?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(self.root.as_ref()?.join(format!("cohort-{id}.jsonl")))
    }

    /// Where a cohort's small resumable meta state lives (per-member run
    /// ids/cursors/state, round, budget), rewritten whole at each scheduler
    /// exit. Same `c<digits>` guard as `cohort_channel_path`: the id reaches
    /// this from the model, so a raw join would turn a cohort id into an
    /// arbitrary-file write. `None` without a bound root or on a bad id.
    pub fn cohort_meta_path(&self, id: &str) -> Option<PathBuf> {
        let digits = id.strip_prefix('c')?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(self.root.as_ref()?.join(format!("cohort-{id}.meta.json")))
    }

    /// Allocate the next run id and, when a root is bound, open its trace
    /// file with the meta line already written. Persistence is best-effort:
    /// an IO failure yields a valid id with no store, never a failed run.
    pub fn begin(
        &mut self,
        parent_call: &str,
        kind: &str,
        model: &str,
        prompt: &str,
        summary: &str,
        resume_of: Option<&str>,
    ) -> (String, Option<TraceStore>) {
        let id = format!("t{}", self.next_seq);
        self.next_seq += 1;
        let store = self.root.as_ref().and_then(|root| {
            fs::create_dir_all(root).ok()?;
            let file = OpenOptions::new()
                .create_new(true)
                .append(true)
                .open(root.join(format!("{id}.jsonl")))
                .ok()?;
            let store = TraceStore {
                writer: Arc::new(Mutex::new(BufWriter::new(file))),
            };
            store.record(&LogEvent::TaskMeta {
                id: id.clone(),
                parent_call: parent_call.to_string(),
                kind: kind.to_string(),
                model: model.to_string(),
                prompt: prompt.to_string(),
                summary: summary.to_string(),
                resume_of: resume_of.map(ToOwned::to_owned),
                created_unix: now_unix(),
            });
            Some(store)
        });
        (id, store)
    }

    /// List the runs recorded under `root`, oldest first. Reads only the meta
    /// and finish lines; a trace without a finish line was cut off by its
    /// process dying and reports `Interrupted`.
    pub fn discover(root: &Path) -> Vec<TaskRunMeta> {
        let Ok(entries) = fs::read_dir(root) else {
            return Vec::new();
        };
        let mut paths: Vec<(u64, PathBuf)> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter_map(|path| Some((trace_seq(&path)?, path)))
            .collect();
        paths.sort_by_key(|(seq, _)| *seq);
        paths
            .into_iter()
            .filter_map(|(_, path)| read_meta(&path))
            .collect()
    }

    /// Rebuild a run's whole conversation from disk: its own trace plus every
    /// follow-up turn recorded against it, replayed in id order into one
    /// ledger. Each turn's trace holds only the events that turn produced —
    /// and `TruncateTail`/`Compact` in it index the *session*, not the file —
    /// so replaying the chain into a single ledger is what reproduces the
    /// session the run had in memory. `None` when this session records no such
    /// run, or when `id` is not a shape it could have issued.
    pub fn restore(&self, id: &str) -> Option<(TaskRunMeta, Ledger)> {
        let path = self.trace_path(id)?;
        let root = self.root.as_ref()?;
        let mut ledger = Ledger::new();
        let mut labels = Vec::new();
        let meta = replay_into(&path, &mut ledger, &mut labels).ok()?;
        for follow in Self::discover(root)
            .into_iter()
            .filter(|meta| meta.resume_of.as_deref() == Some(id))
        {
            let Some(path) = follow.path else { continue };
            // A follow-up whose file is unreadable stops the chain: replaying
            // a later turn over a missing earlier one would silently invent a
            // conversation that never happened.
            if replay_into(&path, &mut ledger, &mut labels).is_err() {
                break;
            }
        }
        Some((meta, ledger))
    }

    /// Load one trace for replay.
    pub fn load(path: &Path) -> Result<TaskRunLoad, StoreError> {
        let mut ledger = Ledger::new();
        let mut batch_labels = Vec::new();
        let meta = replay_into(path, &mut ledger, &mut batch_labels)?;
        Ok(TaskRunLoad {
            meta,
            ledger,
            batch_labels,
        })
    }
}

/// Replay one trace file's events into `ledger`, returning its meta line.
fn replay_into(
    path: &Path,
    ledger: &mut Ledger,
    batch_labels: &mut Vec<(usize, String)>,
) -> Result<TaskRunMeta, StoreError> {
    let mut meta: Option<TaskRunMeta> = None;
    {
        for line in BufReader::new(File::open(path)?).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<LogEvent>(&line)? {
                LogEvent::TaskMeta {
                    id,
                    parent_call,
                    kind,
                    model,
                    prompt,
                    summary,
                    resume_of,
                    created_unix,
                } => {
                    let summary = trace_summary(&summary, &prompt);
                    meta = Some(TaskRunMeta {
                        id,
                        parent_call,
                        kind,
                        model,
                        prompt,
                        summary,
                        resume_of,
                        created_unix,
                        status: TaskRunStatus::Interrupted,
                        tool_calls: 0,
                        usage: Usage::default(),
                        path: Some(path.to_path_buf()),
                    });
                }
                LogEvent::TaskFinished {
                    status,
                    tool_calls,
                    usage,
                } => {
                    if let Some(meta) = &mut meta {
                        meta.status = status;
                        meta.tool_calls = tool_calls;
                        meta.usage = usage;
                    }
                }
                LogEvent::Batch { label, after } => batch_labels.push((after, label)),
                LogEvent::Append { entry } => ledger.append(entry),
                LogEvent::TruncateTail { len } => ledger.truncate_tail(len),
                LogEvent::Compact { summary, upto } => ledger.compact(summary, upto),
                LogEvent::Meta { .. }
                | LogEvent::StartupContext { .. }
                | LogEvent::EnvironmentChanged { .. }
                | LogEvent::EnvironmentObserved { .. }
                | LogEvent::EnvironmentDelivered { .. }
                | LogEvent::Checkpoint { .. } => {}
            }
        }
    }
    meta.ok_or_else(|| StoreError::External("trace has no meta line".into()))
}

/// One run's persisted trace. Cloneable so the `task` tool can hand the
/// sub-agent's ledger a sink and still write the finish line itself.
#[derive(Clone)]
pub struct TraceStore {
    writer: Arc<Mutex<BufWriter<File>>>,
}

impl TraceStore {
    /// Write one event and flush. Best-effort, like the session log: a full
    /// disk degrades to an incomplete trace, never a failed sub-agent.
    fn record(&self, ev: &LogEvent) {
        let line = match serde_json::to_string(ev) {
            Ok(line) => line,
            Err(e) => {
                debug_assert!(false, "unserializable trace event: {e}");
                return;
            }
        };
        let mut writer = self.writer.lock().expect("trace writer lock");
        let _ = writeln!(writer, "{line}");
        let _ = writer.flush();
    }

    pub fn finish(&self, status: TaskRunStatus, tool_calls: usize, usage: Usage) {
        self.record(&LogEvent::TaskFinished {
            status,
            tool_calls,
            usage,
        });
    }
}

impl LedgerSink for TraceStore {
    fn record(&mut self, ev: &LogEvent) {
        TraceStore::record(self, ev);
    }

    /// Traces keep the loop's batch grouping so replay shows the run exactly
    /// as it executed. The main session log stays format-stable by default.
    fn wants_batch_labels(&self) -> bool {
        true
    }
}

/// `tN.jsonl` → N.
fn trace_seq(path: &Path) -> Option<u64> {
    if path.extension()? != "jsonl" {
        return None;
    }
    path.file_stem()?.to_str()?.strip_prefix('t')?.parse().ok()
}

fn next_seq_in(root: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(root) else {
        return 1;
    };
    entries
        .flatten()
        .filter_map(|entry| trace_seq(&entry.path()))
        .max()
        .map_or(1, |max| max + 1)
}

/// Meta + finish lines only, matched by their serde tags before parsing so
/// listing runs never deserializes a trace's full appended history.
fn read_meta(path: &Path) -> Option<TaskRunMeta> {
    let file = File::open(path).ok()?;
    let mut meta: Option<TaskRunMeta> = None;
    for line in BufReader::new(file).lines() {
        let line = line.ok()?;
        if line.contains("\"ev\":\"task_meta\"") {
            if let Ok(LogEvent::TaskMeta {
                id,
                parent_call,
                kind,
                model,
                prompt,
                summary,
                resume_of,
                created_unix,
            }) = serde_json::from_str::<LogEvent>(&line)
            {
                let summary = trace_summary(&summary, &prompt);
                meta = Some(TaskRunMeta {
                    id,
                    parent_call,
                    kind,
                    model,
                    prompt,
                    summary,
                    resume_of,
                    created_unix,
                    status: TaskRunStatus::Interrupted,
                    tool_calls: 0,
                    usage: Usage::default(),
                    path: Some(path.to_path_buf()),
                });
            }
        } else if line.contains("\"ev\":\"task_finished\"") {
            if let (
                Some(meta),
                Ok(LogEvent::TaskFinished {
                    status,
                    tool_calls,
                    usage,
                }),
            ) = (&mut meta, serde_json::from_str::<LogEvent>(&line))
            {
                meta.status = status;
                meta.tool_calls = tool_calls;
                meta.usage = usage;
            }
        }
    }
    meta
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Trace metadata created before task summaries existed carries an empty serde
/// default. Keep it listable by deriving one compact line from its prompt.
fn trace_summary(summary: &str, prompt: &str) -> String {
    const MAX_CHARS: usize = 88;
    let summary = summary.trim();
    if !summary.is_empty() {
        return summary.chars().take(MAX_CHARS).collect();
    }
    let first = prompt
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    if first.chars().count() <= MAX_CHARS {
        return first.to_string();
    }
    let capped: String = first.chars().take(MAX_CHARS - 1).collect();
    format!("{capped}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Entry;
    use crate::types::ContentBlock;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("tcode-trace-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    fn text_entry(s: &str) -> Entry {
        Entry::User(vec![ContentBlock::Text { text: s.into() }])
    }

    #[test]
    fn a_run_id_only_ever_names_a_trace_inside_its_own_session_directory() {
        let root = temp_root("ids");
        let mut traces = TaskTraces::default();
        traces.bind_root(Some(root.clone()));

        assert_eq!(traces.trace_path("t12"), Some(root.join("t12.jsonl")));
        // Ids reach this straight from the model. Anything but `t<digits>` is
        // refused, so `resume` can never name a file outside the session dir.
        for hostile in [
            "../t1",
            "t1/../../t1",
            "../../../../etc/passwd",
            "t",
            "t1x",
            "",
            "t1 ",
        ] {
            assert_eq!(traces.trace_path(hostile), None, "accepted {hostile:?}");
        }

        // Without a bound root nothing is addressable at all.
        assert_eq!(TaskTraces::default().trace_path("t1"), None);
    }

    #[test]
    fn restore_replays_a_run_and_every_follow_up_recorded_against_it() {
        let root = temp_root("chain");
        let mut traces = TaskTraces::default();
        traces.bind_root(Some(root.clone()));

        let (base, store) = traces.begin("call-1", "explore", "m", "survey", "survey", None);
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store.unwrap()));
        ledger.append(text_entry("survey"));
        ledger.append(Entry::Assistant(vec![ContentBlock::Text {
            text: "first pass".into(),
        }]));

        // A follow-up turn records only its own appends, against the same run.
        let (_, store) = traces.begin("call-2", "explore", "m", "more", "more", Some(&base));
        let mut follow = Ledger::new();
        follow.attach_sink(Box::new(store.unwrap()));
        follow.append(text_entry("more"));
        follow.append(Entry::Assistant(vec![ContentBlock::Text {
            text: "second pass".into(),
        }]));

        let (meta, restored) = traces.restore(&base).expect("restorable");
        assert_eq!(meta.kind, "explore");
        assert_eq!(meta.resume_of, None);
        let transcript = format!("{:?}", restored.entries());
        assert!(
            transcript.contains("first pass") && transcript.contains("second pass"),
            "the chain must replay into one conversation: {transcript}"
        );
        assert_eq!(restored.len(), 4);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_trace_roundtrips_ledger_meta_batches_and_finish() {
        let root = temp_root("roundtrip");
        let mut traces = TaskTraces::default();
        traces.bind_root(Some(root.clone()));

        let (id, store) = traces.begin(
            "toolu_01",
            "explore",
            "gpt-test",
            "find the thing",
            "inspect task tracing",
            None,
        );
        assert_eq!(id, "t1");
        let store = store.expect("trace store");

        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store.clone()));
        ledger.append(text_entry("find the thing"));
        ledger.record_batch_label("Read 2 files");
        ledger.append(text_entry("report"));
        store.finish(
            TaskRunStatus::Done,
            3,
            Usage {
                input_tokens: 10,
                output_tokens: 20,
                ..Default::default()
            },
        );

        let load = TaskTraces::load(&root.join("t1.jsonl")).unwrap();
        assert_eq!(load.meta.id, "t1");
        assert_eq!(load.meta.parent_call, "toolu_01");
        assert_eq!(load.meta.kind, "explore");
        assert_eq!(load.meta.prompt, "find the thing");
        assert_eq!(load.meta.summary, "inspect task tracing");
        assert_eq!(load.meta.status, TaskRunStatus::Done);
        assert_eq!(load.meta.tool_calls, 3);
        assert_eq!(load.meta.usage.output_tokens, 20);
        assert_eq!(load.ledger.len(), 2);
        assert_eq!(load.batch_labels, vec![(1, "Read 2 files".to_string())]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn discover_lists_runs_in_order_and_flags_missing_finish_as_interrupted() {
        let root = temp_root("discover");
        let mut traces = TaskTraces::default();
        traces.bind_root(Some(root.clone()));

        let (_, first) = traces.begin("call-a", "explore", "m", "a", "first task", None);
        first
            .expect("store")
            .finish(TaskRunStatus::Done, 1, Usage::default());
        let (_, second) = traces.begin("call-b", "general", "m", "b", "second task", None);
        drop(second); // no finish line: the process "died"

        let runs = TaskTraces::discover(&root);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, "t1");
        assert_eq!(runs[0].status, TaskRunStatus::Done);
        assert_eq!(runs[1].id, "t2");
        assert_eq!(runs[1].status, TaskRunStatus::Interrupted);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rebound_root_continues_the_id_sequence() {
        let root = temp_root("seq");
        let mut traces = TaskTraces::default();
        traces.bind_root(Some(root.clone()));
        let (first, _) = traces.begin("c1", "explore", "m", "p", "first", None);
        let (second, _) = traces.begin("c2", "explore", "m", "p", "second", None);
        assert_eq!((first.as_str(), second.as_str()), ("t1", "t2"));

        // A new process resumes the same session.
        let mut resumed = TaskTraces::default();
        resumed.bind_root(Some(root.clone()));
        let (third, _) = resumed.begin("c3", "explore", "m", "p", "third", None);
        assert_eq!(third, "t3");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn without_a_root_ids_are_issued_but_nothing_persists() {
        let mut traces = TaskTraces::default();
        let (id, store) = traces.begin("c", "explore", "m", "p", "task", None);
        assert_eq!(id, "t1");
        assert!(store.is_none());
    }

    #[test]
    fn legacy_trace_without_summary_derives_one_from_the_prompt() {
        let root = temp_root("legacy-summary");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("t1.jsonl"),
            r#"{"ev":"task_meta","id":"t1","parent_call":"call","kind":"explore","model":"m","prompt":"inspect the old task trace","created_unix":1}"#,
        )
        .unwrap();

        let load = TaskTraces::load(&root.join("t1.jsonl")).unwrap();
        assert_eq!(load.meta.summary, "inspect the old task trace");
        assert_eq!(
            TaskTraces::discover(&root)[0].summary,
            "inspect the old task trace"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
