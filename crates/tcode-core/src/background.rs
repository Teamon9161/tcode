//! Background task registry. A `shell` call with `run_in_background` parks
//! its process here and returns a task id immediately; its output streams to a
//! log file the model tails with the normal `read` tool, and the agent loop
//! appends a harness note at the next safe boundary when a task finishes (pure
//! append, cache-safe).

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio_util::sync::CancellationToken;

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

/// State shared between the registry and the pipe-reader task that owns
/// the child process.
#[derive(Debug)]
pub struct TaskShared {
    /// Live output streams here; the model tails it with `read`.
    pub log_path: PathBuf,
    /// Append handle, opened lazily on first output.
    file: Mutex<Option<std::fs::File>>,
    /// Line count kept in memory so completion notes don't re-scan the file.
    lines: Mutex<usize>,
    pub status: Mutex<TaskStatus>,
    /// Cancelling this kills the child process.
    pub kill: CancellationToken,
}

impl TaskShared {
    fn new(log_path: PathBuf) -> Self {
        Self {
            log_path,
            file: Mutex::new(None),
            lines: Mutex::new(0),
            status: Mutex::new(TaskStatus::Running),
            kill: CancellationToken::new(),
        }
    }

    pub fn append_output(&self, chunk: &str) {
        let mut guard = self.file.lock().expect("task file lock");
        if guard.is_none() {
            if let Some(parent) = self.log_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            *guard = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.log_path)
                .ok();
        }
        if let Some(file) = guard.as_mut() {
            let _ = file.write_all(chunk.as_bytes());
        }
        *self.lines.lock().expect("task lines lock") += chunk.matches('\n').count();
    }

    pub fn line_count(&self) -> usize {
        *self.lines.lock().expect("task lines lock")
    }

    pub fn set_status(&self, status: TaskStatus) {
        *self.status.lock().expect("task status lock") = status;
    }

    pub fn status(&self) -> TaskStatus {
        *self.status.lock().expect("task status lock")
    }
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
}

impl BackgroundTasks {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            tasks: Vec::new(),
            dir,
        }
    }

    /// Register a new task and return its id plus the shared state the
    /// process-owning task writes into.
    pub fn register(&mut self, command: &str) -> (String, Arc<TaskShared>) {
        let id = format!("b{}", self.tasks.len() + 1);
        let shared = Arc::new(TaskShared::new(self.dir.join(format!("{id}.log"))));
        self.tasks.push(Task {
            id: id.clone(),
            command: command.to_string(),
            started: Instant::now(),
            shared: shared.clone(),
            notified: false,
        });
        (id, shared)
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

    /// Finished tasks that the model has not heard about yet. Called by the
    /// agent loop at safe append boundaries.
    pub fn take_completion_notes(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        for task in &mut self.tasks {
            if task.notified {
                continue;
            }
            let status = task.shared.status();
            if status == TaskStatus::Running {
                continue;
            }
            task.notified = true;
            let lines = task.shared.line_count();
            notes.push(format!(
                "Background task {} ({}) {} after {}s; {lines} output lines in {}. \
                 Read that file (with offset) if relevant.",
                task.id,
                task.command,
                status.label(),
                task.started.elapsed().as_secs(),
                task.shared.log_path.display(),
            ));
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
        let (id, shared) = reg.register("cargo watch");
        assert_eq!(id, "b1");
        assert!(reg.take_completion_notes().is_empty());
        assert_eq!(reg.running(), vec!["b1"]);

        shared.append_output("line1\nline2\n");
        shared.set_status(TaskStatus::Exited(0));
        let notes = reg.take_completion_notes();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("b1"));
        assert!(notes[0].contains("exited with code 0"));
        assert!(notes[0].contains("2 output lines"));
        assert!(notes[0].contains("b1.log"));
        // Delivered exactly once.
        assert!(reg.take_completion_notes().is_empty());
        assert!(reg.running().is_empty());
        let _ = std::fs::remove_file(&shared.log_path);
    }

    #[test]
    fn output_streams_to_the_log_file() {
        let mut reg = reg();
        let (_id, shared) = reg.register("server");
        shared.append_output("a\nb\nc\n");
        assert_eq!(shared.line_count(), 3);
        let logged = std::fs::read_to_string(&shared.log_path).unwrap();
        assert_eq!(logged, "a\nb\nc\n");
        let _ = std::fs::remove_file(&shared.log_path);
    }

    #[test]
    fn unknown_id_lists_tasks() {
        let mut reg = reg();
        reg.register("x");
        let err = reg.kill("b9").unwrap_err();
        assert!(err.contains("b1"));
        assert!(err.contains("x"));
    }

    #[test]
    fn kill_semantics() {
        let mut reg = reg();
        let (id, shared) = reg.register("server");
        assert!(reg.kill(&id).unwrap().contains("kill signal sent"));
        assert!(shared.kill.is_cancelled());
        shared.set_status(TaskStatus::Killed);
        assert!(reg.kill(&id).unwrap().contains("already killed"));
    }
}
