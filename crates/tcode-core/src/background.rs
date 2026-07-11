//! Background task registry. A `shell` call with `run_in_background` parks
//! its process here and returns a task id immediately; the model pages the
//! live output via `read_output` and the agent loop appends a harness note
//! at the next safe boundary when a task finishes (pure append, cache-safe).

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
    pub output: Mutex<String>,
    pub status: Mutex<TaskStatus>,
    /// Cancelling this kills the child process.
    pub kill: CancellationToken,
}

impl TaskShared {
    fn new() -> Self {
        Self {
            output: Mutex::new(String::new()),
            status: Mutex::new(TaskStatus::Running),
            kill: CancellationToken::new(),
        }
    }

    pub fn append_output(&self, chunk: &str) {
        self.output
            .lock()
            .expect("task output lock")
            .push_str(chunk);
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

#[derive(Debug, Default)]
pub struct BackgroundTasks {
    tasks: Vec<Task>,
}

impl BackgroundTasks {
    /// Register a new task and return its id plus the shared state the
    /// process-owning task writes into.
    pub fn register(&mut self, command: &str) -> (String, Arc<TaskShared>) {
        let id = format!("b{}", self.tasks.len() + 1);
        let shared = Arc::new(TaskShared::new());
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

    /// Page through a task's output so far. 1-based line offset.
    pub fn read(&self, id: &str, offset: usize, limit: usize) -> Result<String, String> {
        let task = self.find(id)?;
        let output = task.shared.output.lock().expect("task output lock");
        let status = task.shared.status().label();
        let lines: Vec<&str> = output.lines().collect();
        let start = offset.saturating_sub(1).min(lines.len());
        let end = start.saturating_add(limit).min(lines.len());
        let mut out = format!(
            "[{id} — {} — {status} — {} output lines]\n",
            task.command,
            lines.len()
        );
        for (i, l) in lines[start..end].iter().enumerate() {
            out.push_str(&format!("{:>6}\t{l}\n", start + i + 1));
        }
        if end < lines.len() {
            out.push_str(&format!(
                "[{} more lines; continue with offset={}]",
                lines.len() - end,
                end + 1
            ));
        }
        Ok(out)
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
            let lines = task
                .shared
                .output
                .lock()
                .expect("task output lock")
                .lines()
                .count();
            notes.push(format!(
                "Background task {} ({}) {} after {}s; {lines} output lines. \
                 Read them with read_output(id=\"{}\") if relevant.",
                task.id,
                task.command,
                status.label(),
                task.started.elapsed().as_secs(),
                task.id,
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

    #[test]
    fn lifecycle_notes_once() {
        let mut reg = BackgroundTasks::default();
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
        // Delivered exactly once.
        assert!(reg.take_completion_notes().is_empty());
        assert!(reg.running().is_empty());
    }

    #[test]
    fn read_pages_live_output() {
        let mut reg = BackgroundTasks::default();
        let (id, shared) = reg.register("server");
        shared.append_output("a\nb\nc\n");
        let page = reg.read(&id, 2, 1).unwrap();
        assert!(page.contains("running"));
        assert!(page.contains("b"));
        assert!(page.contains("continue with offset=3"));
    }

    #[test]
    fn unknown_id_lists_tasks() {
        let mut reg = BackgroundTasks::default();
        reg.register("x");
        let err = reg.read("b9", 1, 10).unwrap_err();
        assert!(err.contains("b1"));
        assert!(err.contains("x"));
    }

    #[test]
    fn kill_semantics() {
        let mut reg = BackgroundTasks::default();
        let (id, shared) = reg.register("server");
        assert!(reg.kill(&id).unwrap().contains("kill signal sent"));
        assert!(shared.kill.is_cancelled());
        shared.set_status(TaskStatus::Killed);
        assert!(reg.kill(&id).unwrap().contains("already killed"));
    }
}
