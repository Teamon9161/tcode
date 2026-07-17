//! Background task registry. A `shell` call with `run_in_background` parks
//! its process here and returns a task id immediately; its output streams to a
//! log file the model tails with the normal `read` tool, and the agent loop
//! appends a harness note at the next safe boundary when a task finishes (pure
//! append, cache-safe).
//!
//! A monitor (`monitor` tool) is the same task with a different notification
//! contract: instead of one note on exit, every output line is an event the
//! agent loop delivers at safe boundaries — and, when the session is idle, the
//! frontend wakes a turn for (see `monitor_wake_deadline`). One registry, one
//! log pipeline, one `kill_task`; only the `notify` semantics differ.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Event lines are context-bound, so they get the same per-line cap as grep.
const MAX_EVENT_LINE_BYTES: usize = 512;
/// Pending events kept verbatim; beyond this they are counted, not stored —
/// the full text is always in the log file.
const MAX_PENDING_EVENTS: usize = 100;
/// Flood guard: more events than this within [`FLOOD_WINDOW`] auto-stops the
/// monitor so a firehose can never grind the session through wake turns.
const FLOOD_MAX_EVENTS: usize = 120;
const FLOOD_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Exited(i32),
    Killed,
}

impl TaskStatus {
    fn label(&self) -> String {
        match self {
            TaskStatus::Running => "running".into(),
            TaskStatus::Exited(code) => format!("exited with code {code}"),
            TaskStatus::Killed => "killed".into(),
        }
    }
}

/// Monitor-only state: undelivered events and the guards around them.
#[derive(Debug, Default)]
struct MonitorInner {
    pending: Vec<String>,
    /// Events beyond [`MAX_PENDING_EVENTS`] since the last drain.
    dropped: usize,
    /// When the oldest undelivered event arrived; the idle-wake deadline is
    /// this plus `quiet`, so bursts coalesce into one wake with bounded delay.
    first_pending: Option<Instant>,
    /// Arrival times within the flood window.
    recent: VecDeque<Instant>,
    flooded: bool,
    timed_out: bool,
}

#[derive(Debug)]
struct MonitorState {
    description: String,
    quiet: Duration,
    /// Shared with the frontend so an event can wake an idle session.
    signal: Arc<Notify>,
    inner: Mutex<MonitorInner>,
}

/// State shared between the registry and the pipe-reader task that owns
/// the child process.
#[derive(Debug)]
pub struct TaskShared {
    /// Live output streams here; the model tails it with `read`.
    pub log_path: PathBuf,
    /// Append handle created when the task is registered, so the advertised
    /// log path exists even before the child writes its first output line.
    file: Mutex<std::fs::File>,
    /// Line count kept in memory so completion notes don't re-scan the file.
    lines: Mutex<usize>,
    pub status: Mutex<TaskStatus>,
    /// Cancelling this kills the child process.
    pub kill: CancellationToken,
    monitor: Option<MonitorState>,
}

impl TaskShared {
    fn new(log_path: PathBuf, monitor: Option<MonitorState>) -> Result<Self, String> {
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "cannot create background task log directory {}: {e}",
                    parent.display()
                )
            })?;
        }
        std::fs::File::create(&log_path).map_err(|e| {
            format!(
                "cannot create background task log {}: {e}",
                log_path.display()
            )
        })?;
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .map_err(|e| {
                format!(
                    "cannot open background task log {} for append: {e}",
                    log_path.display()
                )
            })?;
        Ok(Self {
            log_path,
            file: Mutex::new(file),
            lines: Mutex::new(0),
            status: Mutex::new(TaskStatus::Running),
            kill: CancellationToken::new(),
            monitor,
        })
    }

    pub fn append_output(&self, chunk: &str) {
        let mut file = self.file.lock().expect("task file lock");
        let _ = file.write_all(chunk.as_bytes());
        *self.lines.lock().expect("task lines lock") += chunk.matches('\n').count();
    }

    /// One produced output line: logged for every task, and additionally
    /// recorded as an undelivered event when this task is a monitor.
    pub fn push_line(&self, line: &str) {
        self.append_output(line);
        self.append_output("\n");
        let Some(monitor) = &self.monitor else {
            return;
        };
        let now = Instant::now();
        {
            let mut inner = monitor.inner.lock().expect("monitor lock");
            if inner.flooded {
                return;
            }
            while inner
                .recent
                .front()
                .is_some_and(|t| now.duration_since(*t) > FLOOD_WINDOW)
            {
                inner.recent.pop_front();
            }
            inner.recent.push_back(now);
            if inner.recent.len() > FLOOD_MAX_EVENTS {
                inner.flooded = true;
                self.kill.cancel();
            } else if inner.pending.len() >= MAX_PENDING_EVENTS {
                inner.dropped += 1;
            } else {
                inner.pending.push(truncate_event_line(line));
            }
            inner.first_pending.get_or_insert(now);
        }
        monitor.signal.notify_one();
    }

    pub fn line_count(&self) -> usize {
        *self.lines.lock().expect("task lines lock")
    }

    pub fn set_status(&self, status: TaskStatus) {
        *self.status.lock().expect("task status lock") = status;
        // A monitor's exit is itself an event: wake an idle session so the
        // completion note (and any last pending lines) reach the model now.
        if let Some(monitor) = &self.monitor {
            monitor.signal.notify_one();
        }
    }

    pub fn status(&self) -> TaskStatus {
        *self.status.lock().expect("task status lock")
    }

    /// Record that the monitor hit its deadline; the caller cancels `kill`.
    pub fn mark_timed_out(&self) {
        if let Some(monitor) = &self.monitor {
            monitor.inner.lock().expect("monitor lock").timed_out = true;
        }
    }
}

fn truncate_event_line(line: &str) -> String {
    if line.len() <= MAX_EVENT_LINE_BYTES {
        return line.to_string();
    }
    let mut end = MAX_EVENT_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

#[derive(Debug)]
struct Task {
    id: String,
    command: String,
    started: Instant,
    shared: Arc<TaskShared>,
    /// Completion already delivered to the model as a harness note.
    notified: bool,
}

#[derive(Debug)]
pub struct BackgroundTasks {
    tasks: Vec<Task>,
    /// Where task logs are written (`<dir>/b1.log`, …).
    dir: PathBuf,
    /// Fired whenever any monitor produces an event or exits. The frontend
    /// holds a clone and re-checks `monitor_wake_deadline` when it fires.
    signal: Arc<Notify>,
}

impl BackgroundTasks {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            tasks: Vec::new(),
            dir,
            signal: Arc::new(Notify::new()),
        }
    }

    /// Shared handle the frontend listens on for monitor activity.
    pub fn monitor_signal(&self) -> Arc<Notify> {
        self.signal.clone()
    }

    /// Register a new task and return its id plus the shared state the
    /// process-owning task writes into.
    pub fn register(&mut self, command: &str) -> Result<(String, Arc<TaskShared>), String> {
        self.register_inner(command, None)
    }

    /// Register a monitor: same task, but each output line is an undelivered
    /// event until the agent loop drains it via `take_notes`.
    pub fn register_monitor(
        &mut self,
        command: &str,
        description: &str,
        quiet: Duration,
    ) -> Result<(String, Arc<TaskShared>), String> {
        self.register_inner(
            command,
            Some(MonitorState {
                description: description.to_string(),
                quiet,
                signal: self.signal.clone(),
                inner: Mutex::new(MonitorInner::default()),
            }),
        )
    }

    fn register_inner(
        &mut self,
        command: &str,
        monitor: Option<MonitorState>,
    ) -> Result<(String, Arc<TaskShared>), String> {
        let prefix = if monitor.is_some() { "m" } else { "b" };
        let id = format!("{prefix}{}", self.tasks.len() + 1);
        let shared = Arc::new(TaskShared::new(
            self.dir.join(format!("{id}.log")),
            monitor,
        )?);
        self.tasks.push(Task {
            id: id.clone(),
            command: command.to_string(),
            started: Instant::now(),
            shared: shared.clone(),
            notified: false,
        });
        Ok((id, shared))
    }

    fn find(&self, id: &str) -> Result<&Task, String> {
        self.tasks.iter().find(|t| t.id == id).ok_or_else(|| {
            format!(
                "unknown background task '{id}'. Tasks: {}",
                if self.tasks.is_empty() {
                    "none".to_string()
                } else {
                    self.tasks
                        .iter()
                        .map(|t| format!("{} ({}, {})", t.id, t.command, t.shared.status().label()))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            )
        })
    }

    /// Kill a running task. Killing an already-finished task is reported,
    /// not an error the model has to think about.
    pub fn kill(&mut self, id: &str) -> Result<String, String> {
        let task = self.find(id)?;
        match task.shared.status() {
            TaskStatus::Running => {
                task.shared.kill.cancel();
                Ok(format!("kill signal sent to {id} ({})", task.command))
            }
            status => Ok(format!("{id} already {}; nothing to kill", status.label())),
        }
    }

    /// When an idle session should wake to deliver monitor activity: the
    /// oldest undelivered event plus that monitor's quiet window (bursts
    /// coalesce, delivery latency stays bounded), or now for a finished
    /// monitor whose completion the model has not heard yet. `None` while
    /// there is nothing to deliver.
    pub fn monitor_wake_deadline(&self) -> Option<Instant> {
        self.tasks
            .iter()
            .filter_map(|task| {
                let monitor = task.shared.monitor.as_ref()?;
                if !task.notified && task.shared.status() != TaskStatus::Running {
                    return Some(Instant::now());
                }
                let inner = monitor.inner.lock().expect("monitor lock");
                (!inner.pending.is_empty() || inner.dropped > 0)
                    .then(|| inner.first_pending.unwrap_or_else(Instant::now) + monitor.quiet)
            })
            .min()
    }

    /// Undelivered notes: monitor events first, then completions the model
    /// has not heard about. Called by the agent loop at safe append
    /// boundaries (and by an idle wake turn).
    pub fn take_notes(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        for task in &mut self.tasks {
            if let Some(event_note) = drain_monitor_events(task) {
                notes.push(event_note);
            }
            if task.notified {
                continue;
            }
            let status = task.shared.status();
            if status == TaskStatus::Running {
                continue;
            }
            task.notified = true;
            notes.push(match &task.shared.monitor {
                Some(monitor) => monitor_completion_note(task, monitor, status),
                None => completion_note(task, status),
            });
        }
        notes
    }

    pub fn running(&self) -> Vec<&str> {
        self.tasks
            .iter()
            .filter(|t| t.shared.status() == TaskStatus::Running)
            .map(|t| t.id.as_str())
            .collect()
    }
}

/// Pending monitor events for one task, drained into a single note.
fn drain_monitor_events(task: &Task) -> Option<String> {
    let monitor = task.shared.monitor.as_ref()?;
    let (pending, dropped) = {
        let mut inner = monitor.inner.lock().expect("monitor lock");
        inner.first_pending = None;
        (
            std::mem::take(&mut inner.pending),
            std::mem::take(&mut inner.dropped),
        )
    };
    if pending.is_empty() && dropped == 0 {
        return None;
    }
    let count = pending.len() + dropped;
    let mut note = format!(
        "Monitor {} ({}): {count} new event {}:\n{}",
        task.id,
        monitor.description,
        if count == 1 { "line" } else { "lines" },
        pending.join("\n"),
    );
    if dropped > 0 {
        note.push_str(&format!("\n(+{dropped} more lines not shown)"));
    }
    note.push_str(&format!("\nFull log: {}", task.shared.log_path.display()));
    Some(note)
}

fn completion_note(task: &Task, status: TaskStatus) -> String {
    let lines = task.shared.line_count();
    let output_hint = if lines == 0 {
        " No output was captured by tcode; command-level redirection may have sent output elsewhere."
    } else {
        " Read that file (with offset) if relevant."
    };
    format!(
        "Background task {} ({}) {} after {}s; {lines} output lines in {}.{output_hint}",
        task.id,
        task.command,
        status.label(),
        task.started.elapsed().as_secs(),
        task.shared.log_path.display(),
    )
}

fn monitor_completion_note(task: &Task, monitor: &MonitorState, status: TaskStatus) -> String {
    let inner = monitor.inner.lock().expect("monitor lock");
    let cause = if inner.flooded {
        format!(
            " It was stopped automatically: more than {FLOOD_MAX_EVENTS} events in \
             {}s. If you still need it, restart it with a more selective filter.",
            FLOOD_WINDOW.as_secs()
        )
    } else if inner.timed_out {
        " It hit its timeout; restart it (or use persistent=true) if the watch \
         should continue."
            .to_string()
    } else {
        String::new()
    };
    format!(
        "Monitor {} ({}) {} after {}s; full log: {}.{cause}",
        task.id,
        monitor.description,
        status.label(),
        task.started.elapsed().as_secs(),
        task.shared.log_path.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> BackgroundTasks {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        // Unique per call so parallel tests don't share a b1.log.
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tcode-bg-{}-{n}", std::process::id()));
        BackgroundTasks::new(dir)
    }

    #[test]
    fn lifecycle_notes_once() {
        let mut reg = reg();
        let (id, shared) = reg.register("cargo watch").unwrap();
        assert_eq!(id, "b1");
        assert!(reg.take_notes().is_empty());
        assert_eq!(reg.running(), vec!["b1"]);

        shared.append_output("line1\nline2\n");
        shared.set_status(TaskStatus::Exited(0));
        let notes = reg.take_notes();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("b1"));
        assert!(notes[0].contains("exited with code 0"));
        assert!(notes[0].contains("2 output lines"));
        assert!(notes[0].contains("b1.log"));
        // Delivered exactly once.
        assert!(reg.take_notes().is_empty());
        assert!(reg.running().is_empty());
        let _ = std::fs::remove_file(&shared.log_path);
    }

    #[test]
    fn output_streams_to_the_log_file() {
        let mut reg = reg();
        let (_id, shared) = reg.register("server").unwrap();
        shared.append_output("a\nb\nc\n");
        assert_eq!(shared.line_count(), 3);
        let logged = std::fs::read_to_string(&shared.log_path).unwrap();
        assert_eq!(logged, "a\nb\nc\n");
        let _ = std::fs::remove_file(&shared.log_path);
    }

    #[test]
    fn registered_log_exists_before_the_task_emits_output() {
        let mut reg = reg();
        let (_id, shared) = reg.register("quiet command").unwrap();
        assert_eq!(std::fs::read_to_string(&shared.log_path).unwrap(), "");

        shared.set_status(TaskStatus::Exited(1));
        let notes = reg.take_notes();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("0 output lines"));
        assert!(notes[0].contains("No output was captured by tcode"));
        assert!(notes[0].contains("redirection may have sent output elsewhere"));
        let _ = std::fs::remove_file(&shared.log_path);
    }

    #[test]
    fn unknown_id_lists_tasks() {
        let mut reg = reg();
        reg.register("x").unwrap();
        let err = reg.kill("b9").unwrap_err();
        assert!(err.contains("b1"));
        assert!(err.contains("x"));
    }

    #[test]
    fn kill_semantics() {
        let mut reg = reg();
        let (id, shared) = reg.register("server").unwrap();
        assert!(reg.kill(&id).unwrap().contains("kill signal sent"));
        assert!(shared.kill.is_cancelled());
        shared.set_status(TaskStatus::Killed);
        assert!(reg.kill(&id).unwrap().contains("already killed"));
    }

    #[test]
    fn monitor_events_deliver_once_and_reference_the_log() {
        let mut reg = reg();
        let (id, shared) = reg
            .register_monitor(
                "tail errors",
                "errors in app.log",
                Duration::from_millis(500),
            )
            .unwrap();
        assert_eq!(id, "m1");
        assert!(reg.monitor_wake_deadline().is_none());

        shared.push_line("ERROR one");
        shared.push_line("ERROR two");
        assert!(reg.monitor_wake_deadline().is_some());
        let notes = reg.take_notes();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("m1"));
        assert!(notes[0].contains("errors in app.log"));
        assert!(notes[0].contains("2 new event lines"));
        assert!(notes[0].contains("ERROR one\nERROR two"));
        assert!(notes[0].contains("m1.log"));
        // Drained: nothing pending, no wake needed.
        assert!(reg.take_notes().is_empty());
        assert!(reg.monitor_wake_deadline().is_none());
        // The full text is in the log regardless of event delivery.
        let logged = std::fs::read_to_string(&shared.log_path).unwrap();
        assert_eq!(logged, "ERROR one\nERROR two\n");
        let _ = std::fs::remove_file(&shared.log_path);
    }

    #[test]
    fn monitor_wake_deadline_is_first_event_plus_quiet() {
        let mut reg = reg();
        let (_id, shared) = reg
            .register_monitor("watch", "d", Duration::from_secs(2))
            .unwrap();
        let before = Instant::now();
        shared.push_line("first");
        std::thread::sleep(Duration::from_millis(20));
        shared.push_line("second");
        let deadline = reg.monitor_wake_deadline().unwrap();
        // Anchored to the first event, not pushed out by the second.
        assert!(deadline >= before + Duration::from_secs(2));
        assert!(deadline <= before + Duration::from_secs(2) + Duration::from_millis(20));
        let _ = std::fs::remove_file(&shared.log_path);
    }

    #[test]
    fn monitor_finished_wakes_immediately_and_notes_completion() {
        let mut reg = reg();
        let (_id, shared) = reg
            .register_monitor("watch", "ci status", Duration::from_secs(30))
            .unwrap();
        shared.push_line("done: success");
        shared.set_status(TaskStatus::Exited(0));
        // Finished monitor: wake now, not after the quiet window.
        let deadline = reg.monitor_wake_deadline().unwrap();
        assert!(deadline <= Instant::now() + Duration::from_millis(50));
        let notes = reg.take_notes();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[0].contains("done: success"));
        assert!(notes[1].contains("Monitor m1 (ci status) exited with code 0"));
        let _ = std::fs::remove_file(&shared.log_path);
    }

    #[test]
    fn monitor_flood_kills_and_explains() {
        let mut reg = reg();
        let (_id, shared) = reg
            .register_monitor("firehose", "noisy", Duration::from_millis(100))
            .unwrap();
        for i in 0..(FLOOD_MAX_EVENTS + 10) {
            shared.push_line(&format!("event {i}"));
        }
        assert!(shared.kill.is_cancelled());
        shared.set_status(TaskStatus::Killed);
        let notes = reg.take_notes();
        // Pending events (capped) still deliver, then the stop explanation.
        assert_eq!(notes.len(), 2, "{}", notes.len());
        assert!(notes[0].contains(&format!(
            "+{} more lines not shown",
            FLOOD_MAX_EVENTS - MAX_PENDING_EVENTS
        )));
        assert!(notes[1].contains("stopped automatically"));
        assert!(notes[1].contains("more selective filter"));
        let _ = std::fs::remove_file(&shared.log_path);
    }

    #[test]
    fn monitor_event_lines_are_truncated() {
        let mut reg = reg();
        let (_id, shared) = reg
            .register_monitor("watch", "d", Duration::from_millis(100))
            .unwrap();
        shared.push_line(&"x".repeat(2 * MAX_EVENT_LINE_BYTES));
        let notes = reg.take_notes();
        assert!(notes[0].contains('…'));
        // The log keeps the full line even though the event was truncated.
        assert!(std::fs::read_to_string(&shared.log_path)
            .unwrap()
            .contains(&"x".repeat(2 * MAX_EVENT_LINE_BYTES)));
        let _ = std::fs::remove_file(&shared.log_path);
    }

    #[test]
    fn timed_out_monitor_note_suggests_persistent() {
        let mut reg = reg();
        let (_id, shared) = reg
            .register_monitor("watch", "d", Duration::from_millis(100))
            .unwrap();
        shared.mark_timed_out();
        shared.set_status(TaskStatus::Killed);
        let notes = reg.take_notes();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("timeout"));
        assert!(notes[0].contains("persistent=true"));
        let _ = std::fs::remove_file(&shared.log_path);
    }
}
