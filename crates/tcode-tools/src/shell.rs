use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::shell_filter::ShellFilters;

use tcode_core::{
    AutoSafety, BatchPolicy, PermissionRequest, TaskStatus, Tool, ToolCtx, ToolOutput,
};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const FINAL_TAIL_LINES: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Full,
    Final,
}

impl OutputMode {
    fn parse(input: &Value) -> Result<Self, ToolOutput> {
        match input["output_mode"].as_str().unwrap_or("full") {
            "full" => Ok(Self::Full),
            "final" => Ok(Self::Final),
            other => Err(ToolOutput::err(format!(
                "invalid output_mode: {other:?}; use \"full\" (default) or \"final\""
            ))),
        }
    }
}

fn final_output_summary(output: &str, log_path: &std::path::Path) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let omitted = lines.len().saturating_sub(FINAL_TAIL_LINES);
    let tail = lines
        .iter()
        .skip(omitted)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let prefix = if omitted == 0 {
        "Command output was captured in final mode".to_string()
    } else {
        format!("Command output was captured in final mode; {omitted} earlier lines omitted")
    };
    format!("{prefix}. Full output: {}\n\n{tail}", log_path.display())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    PowerShell,
    Bash,
}

impl ShellKind {
    pub const fn tool_name(self) -> &'static str {
        match self {
            Self::PowerShell => "shell",
            Self::Bash => "bash",
        }
    }
}

pub fn bash_available() -> bool {
    which_bash().is_some()
}

fn which_bash() -> Option<std::path::PathBuf> {
    // Prefer Git Bash over WSL's bash.exe shim in System32.
    if cfg!(windows) {
        for candidate in [
            "C:\\Program Files\\Git\\bin\\bash.exe",
            "C:\\Program Files (x86)\\Git\\bin\\bash.exe",
        ] {
            let p = std::path::PathBuf::from(candidate);
            if p.exists() {
                return Some(p);
            }
        }
        None
    } else {
        Some(std::path::PathBuf::from("bash"))
    }
}

pub struct ShellTool {
    kind: ShellKind,
    /// Shared with every other consumer of the chain, so `/cd` re-derives the
    /// project's rules once for all of them.
    filters: Arc<ShellFilters>,
}

impl ShellTool {
    /// Without a chain nothing is filtered — the honest default for a tool
    /// built outside the composition root.
    #[cfg(test)]
    pub fn new(kind: ShellKind) -> Self {
        Self::with_filters(kind, Arc::new(ShellFilters::disabled()))
    }

    pub fn with_filters(kind: ShellKind, filters: Arc<ShellFilters>) -> Self {
        Self { kind, filters }
    }

    fn command(&self, script: &str, cwd: &std::path::Path) -> tokio::process::Command {
        shell_command(self.kind, script, cwd)
    }
}

/// Build the interpreter invocation for a script. Shared with the `monitor`
/// tool so both spawn commands identically.
pub(crate) fn shell_command(
    kind: ShellKind,
    script: &str,
    cwd: &std::path::Path,
) -> tokio::process::Command {
    match kind {
        ShellKind::PowerShell => {
            let mut cmd = tokio::process::Command::new("powershell.exe");
            // Force UTF-8 so output survives non-English codepages.
            let wrapped = format!(
                "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; \
                 $OutputEncoding=[System.Text.Encoding]::UTF8; {script}"
            );
            cmd.args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &wrapped,
            ]);
            cmd.current_dir(cwd);
            cmd
        }
        ShellKind::Bash => {
            let mut cmd =
                tokio::process::Command::new(which_bash().unwrap_or_else(|| "bash".into()));
            cmd.args(["-c", script]);
            cmd.current_dir(cwd);
            cmd
        }
    }
}

impl ShellTool {
    /// Detach a long-running command: the process is owned by a supervisor
    /// task, its output streams into the background registry, and the model
    /// gets a task id back immediately. No timeout — that is the point;
    /// `kill_task` stops it, and process exit is reported via harness note.
    fn spawn_background(&self, script: &str, cwd: &std::path::Path, ctx: &ToolCtx) -> ToolOutput {
        let mut cmd = self.command(script, cwd);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ToolOutput::err(format!("failed to start background task: {e}")),
        };
        let (id, shared) = match ctx
            .background
            .lock()
            .expect("background lock")
            .register(script)
        {
            Ok(task) => task,
            Err(error) => return ToolOutput::err(error),
        };

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let readers = [
            spawn_line_reader(stdout, shared.clone()),
            spawn_line_reader(stderr, shared.clone()),
        ];
        let supervisor_shared = shared.clone();
        tokio::spawn(async move {
            tokio::select! {
                status = child.wait() => {
                    // Drain what the pipes still hold before declaring done.
                    for r in readers {
                        let _ = r.await;
                    }
                    let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
                    supervisor_shared.set_status(TaskStatus::Exited(code));
                }
                _ = supervisor_shared.kill.cancelled() => {
                    let _ = child.kill().await;
                    for r in readers {
                        let _ = r.await;
                    }
                    supervisor_shared.set_status(TaskStatus::Killed);
                }
            }
        });
        let log = shared.log_path.display();
        ToolOutput::ok(format!(
            "Started background task {id}: {script}\nIt keeps running while you \
             continue working. Its output streams to {log} — read that file (with \
             offset to follow new lines); you will get a note when it finishes. \
             Stop it with kill_task(id=\"{id}\")."
        ))
    }
}

/// Stops a background task started by shell/bash with run_in_background.
pub struct KillTaskTool;

#[async_trait]
impl Tool for KillTaskTool {
    fn name(&self) -> &str {
        "kill_task"
    }

    fn description(&self) -> &str {
        "Stop a background task or monitor by id (e.g. b1, m2). Killing an \
         already-finished task is a no-op. Its captured output stays readable \
         in the task's log file via read."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Background task or monitor id, e.g. b1 or m2" }
            },
            "required": ["id"]
        })
    }

    fn permission(&self, _input: &Value) -> PermissionRequest {
        // Only reaches processes the model itself started.
        PermissionRequest::None
    }

    async fn run(&self, input: Value, ctx: &ToolCtx, _cancel: &CancellationToken) -> ToolOutput {
        let Some(id) = input["id"].as_str() else {
            return ToolOutput::err("missing required parameter: id");
        };
        match ctx.background.lock().expect("background lock").kill(id) {
            Ok(msg) => ToolOutput::ok(msg),
            Err(e) => ToolOutput::err(e),
        }
    }
}

/// Append a pipe's lines to the shared task output until EOF. For monitor
/// tasks each line also becomes an undelivered event (see `TaskShared`).
pub(crate) fn spawn_line_reader(
    pipe: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    shared: std::sync::Arc<tcode_core::TaskShared>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(pipe).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            shared.push_line(&line);
        }
    })
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        match self.kind {
            ShellKind::PowerShell => "shell",
            ShellKind::Bash => "bash",
        }
    }

    fn display_name(&self) -> String {
        // "Run" reads clearer than the shell's raw name; the command content
        // itself shows which interpreter it targets.
        "Run".to_string()
    }

    fn batch_label(&self, inputs: &[&Value]) -> String {
        let count = inputs.len();
        format!(
            "Run {count} {}",
            if count == 1 { "command" } else { "commands" }
        )
    }

    fn description(&self) -> &str {
        match self.kind {
            ShellKind::PowerShell => {
                "Run a PowerShell command. Output is captured; interactive \
                 commands will hang and must be avoided. Use timeout_ms for \
                 long-running commands (default 120s, max 600s). Several \
                 shell calls in one message share a single approval and run \
                 in order — batch related commands. Set \
                 run_in_background=true ONLY for commands that run long and \
                 whose intermediate output you don't need to wait for (dev \
                 server, watcher, long build/test): you get a task id and a log \
                 file path back immediately, can keep working, read the log to \
                 follow output, and are notified when it finishes."
            }
            ShellKind::Bash => {
                "Run a bash (Git Bash) command with POSIX syntax. Same rules \
                 as shell: non-interactive only, timeout_ms configurable, \
                 batched calls share one approval and run in order, \
                 run_in_background for long-running commands."
            }
        }
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "timeout_ms": { "type": "integer", "description": "Kill after this many ms (default 120000, max 600000); ignored for background tasks" },
                "cwd": { "type": "string", "description": "Working directory (default: project cwd)" },
                "run_in_background": { "type": "boolean", "description": "Run detached and return a task id immediately (default false)" },
                "output_mode": { "type": "string", "enum": ["full", "final"], "description": "full returns all output (default). final saves complete output in session scratch and returns only the final 80 lines; use for watch/polling commands such as gh run watch." }
            },
            "required": ["command"]
        })
    }

    fn auto_safety(&self, _input: &Value) -> AutoSafety {
        // A declared `cwd` is not a containment boundary: the command inside it
        // may name absolute paths, reach the network, or spawn anything. File
        // tools can be fast-pathed on their target because the target *is* the
        // whole effect; for a shell command it is only where it starts, so the
        // classifier stays in the loop no matter where it is rooted.
        AutoSafety::Classify
    }

    fn permission(&self, input: &Value) -> PermissionRequest {
        let command = input["command"].as_str().unwrap_or("?");
        PermissionRequest::Ask {
            descriptor: format!("run({command})"),
            aliases: vec![format!("{}({command})", self.name())],
            summary: format!("run: {command}"),
            is_edit: false,
        }
    }

    fn context_paths(&self, input: &Value) -> Vec<String> {
        vec![input["cwd"].as_str().unwrap_or(".").to_string()]
    }

    fn is_mutating(&self) -> bool {
        true
    }

    fn batch_policy(&self) -> BatchPolicy {
        BatchPolicy::SequentialBatch
    }

    /// Invocations that already park their full output elsewhere are left
    /// alone: `final` mode returns a deliberate tail of a saved log, and a
    /// background run returns a task id rather than output. Filtering either
    /// would spill a second copy of the same text for nothing.
    fn compact_success_output(&self, input: &Value, output: &str) -> Option<tcode_core::Compacted> {
        let diverted = input["run_in_background"].as_bool().unwrap_or(false)
            || input["output_mode"].as_str() == Some("final");
        if diverted {
            return None;
        }
        self.filters.apply(input["command"].as_str()?, output)
    }

    async fn run(&self, input: Value, ctx: &ToolCtx, cancel: &CancellationToken) -> ToolOutput {
        let Some(script) = input["command"].as_str() else {
            return ToolOutput::err("missing required parameter: command");
        };
        let output_mode = match OutputMode::parse(&input) {
            Ok(mode) => mode,
            Err(error) => return error,
        };
        let cwd = input["cwd"]
            .as_str()
            .map(|c| ctx.resolve(c))
            .unwrap_or_else(|| ctx.cwd.clone());
        if !cwd.is_dir() {
            return ToolOutput::err(format!("cwd does not exist: {}", cwd.display()));
        }
        let timeout = Duration::from_millis(
            input["timeout_ms"]
                .as_u64()
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS),
        );
        if input["run_in_background"].as_bool().unwrap_or(false) {
            return self.spawn_background(script, &cwd, ctx);
        }

        let mut cmd = self.command(script, &cwd);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return ToolOutput::err(format!(
                    "failed to start {}: {e}",
                    match self.kind {
                        ShellKind::PowerShell => "powershell.exe",
                        ShellKind::Bash => "bash",
                    }
                ));
            }
        };

        let mut stdout_pipe = child.stdout.take().expect("piped stdout");
        let mut stderr_pipe = child.stderr.take().expect("piped stderr");
        let reader = async {
            let (mut out_buf, mut err_buf) = (Vec::new(), Vec::new());
            let _ = tokio::join!(
                stdout_pipe.read_to_end(&mut out_buf),
                stderr_pipe.read_to_end(&mut err_buf)
            );
            (out_buf, err_buf)
        };

        tokio::select! {
            ((out_buf, err_buf), status) = async {
                let bufs = reader.await;
                let status = child.wait().await;
                (bufs, status)
            } => {
                let mut out = String::from_utf8_lossy(&out_buf).into_owned();
                let err = String::from_utf8_lossy(&err_buf);
                if !err.trim().is_empty() {
                    out.push_str("\n--- stderr ---\n");
                    out.push_str(err.trim_end());
                }
                let code = status.as_ref().ok().and_then(|s| s.code()).unwrap_or(-1);
                if out.trim().is_empty() {
                    out = "(no output)".into();
                }
                if code != 0 {
                    out.push_str(&format!("\n(exit code {code})"));
                } else {
                    out.push_str("\n(exit code 0)");
                }
                if output_mode == OutputMode::Final {
                    static NEXT_FINAL_LOG: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(1);
                    let dir = ctx.scratch_dir.join("tool-output");
                    let id = NEXT_FINAL_LOG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let log_path = dir.join(format!("shell-final-{id:04}.log"));
                    let saved = async {
                        tokio::fs::create_dir_all(&dir).await?;
                        tokio::fs::write(&log_path, &out).await
                    }
                    .await;
                    if let Err(error) = saved {
                        return ToolOutput::err(format!(
                            "final output mode could not save complete output: {error}\n\n{}",
                            final_output_summary(&out, std::path::Path::new("(unavailable)"))
                        ));
                    }
                    out = final_output_summary(&out, &log_path);
                }
                if code != 0 {
                    ToolOutput::err(out)
                } else {
                    ToolOutput::ok(out)
                }
            }
            _ = tokio::time::sleep(timeout) => {
                let _ = child.kill().await;
                ToolOutput::err(format!(
                    "command timed out after {}s and was killed. If it \
                     legitimately needs longer, re-run with timeout_ms up to {}.",
                    timeout.as_secs(),
                    MAX_TIMEOUT_MS
                ))
            }
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                ToolOutput::err("command cancelled by user and killed".to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_mode_keeps_tail_and_points_to_complete_log() {
        let output = (0..(FINAL_TAIL_LINES + 3))
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let summary = final_output_summary(&output, std::path::Path::new("/scratch/full.log"));
        assert!(summary.contains("3 earlier lines omitted"));
        assert!(summary.contains("/scratch/full.log"));
        assert!(!summary.contains("line 0"));
        assert!(summary.contains(&format!("line {}", FINAL_TAIL_LINES + 2)));
    }

    #[test]
    fn output_mode_rejects_unknown_values() {
        let error = OutputMode::parse(&json!({ "output_mode": "changes" })).unwrap_err();
        assert!(error.is_error);
        assert!(error.content.contains("full"));
        assert!(error.content.contains("final"));
    }

    #[tokio::test]
    async fn final_mode_saves_full_output_and_returns_a_tail() {
        let root = std::env::temp_dir().join(format!("tcode-shell-final-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        tcode_core::home::testing::temp_home();
        let ctx = ToolCtx::with_scratch_dir(root.clone(), 10_000, root.join("scratch"));
        let (kind, command) = if cfg!(windows) {
            (
                ShellKind::PowerShell,
                "0..84 | ForEach-Object { \"line $_\" }",
            )
        } else {
            (
                ShellKind::Bash,
                "for i in $(seq 0 84); do echo line $i; done",
            )
        };
        let output = ShellTool::new(kind)
            .run(
                json!({ "command": command, "output_mode": "final" }),
                &ctx,
                &CancellationToken::new(),
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("earlier lines omitted"));
        assert!(!output.content.contains("line 0"));
        assert!(output.content.contains("line 84"));
        let logs = std::fs::read_dir(root.join("scratch/tool-output")).unwrap();
        let path = logs.into_iter().next().unwrap().unwrap().path();
        let raw = std::fs::read_to_string(path).unwrap();
        assert!(raw.contains("line 0"));
        assert!(raw.contains("line 84"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
