use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio_util::sync::CancellationToken;

use tcode_core::{
    AutoSafety, BatchPolicy, PermissionRequest, TaskStatus, Tool, ToolCtx, ToolOutput,
};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    PowerShell,
    Bash,
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
}

impl ShellTool {
    pub fn new(kind: ShellKind) -> Self {
        Self { kind }
    }

    fn command(&self, script: &str, cwd: &std::path::Path) -> tokio::process::Command {
        match self.kind {
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
        let (id, shared) = ctx
            .background
            .lock()
            .expect("background lock")
            .register(script);

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
        "Stop a background task by id (e.g. b1). Killing an already-finished \
         task is a no-op. Its captured output stays readable in the task's log \
         file via read."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Background task id, e.g. b1" }
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

/// Append a pipe's lines to the shared task output until EOF.
fn spawn_line_reader(
    pipe: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    shared: std::sync::Arc<tcode_core::TaskShared>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(pipe).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            shared.append_output(&line);
            shared.append_output("\n");
        }
    })
}

/// Successful test runs often contain several nearly-identical target blocks
/// (especially doctests and crates with zero tests). Keep one result for every
/// target that actually ran tests, plus the shell's final status, while avoiding
/// needless context use. Any error-like marker leaves the original output
/// untouched for diagnosis.
fn compact_successful_test_output(output: String) -> String {
    if !(output.contains("test result: ok.")
        && output.contains("running ")
        && !output.contains("test result: FAILED")
        && !output.contains("error:")
        && !output.contains("failures:"))
    {
        return output;
    }

    let lines: Vec<&str> = output.lines().collect();
    let mut passed = Vec::new();
    for (index, result) in lines.iter().enumerate() {
        if !result.trim_start().starts_with("test result: ok.") || result.contains("0 passed") {
            continue;
        }
        let running = lines[..index]
            .iter()
            .rev()
            .find(|line| {
                let trimmed = line.trim_start();
                trimmed.starts_with("running ")
                    && trimmed.contains(" tests")
                    && !trimmed.contains("running 0 tests")
            })
            .copied();
        if let Some(running) = running {
            passed.push(format!("{running}\n{result}"));
        }
    }
    if passed.is_empty() {
        return output;
    }

    let status = lines
        .iter()
        .rev()
        .find(|line| line.starts_with("(exit code "))
        .copied()
        .unwrap_or("test result: ok.");
    format!(
        "{}\n… successful test output folded …\n{status}",
        passed.join("\n")
    )
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
                "run_in_background": { "type": "boolean", "description": "Run detached and return a task id immediately (default false)" }
            },
            "required": ["command"]
        })
    }

    fn auto_safety(&self, _input: &Value) -> AutoSafety {
        AutoSafety::AllowInScratch
    }

    fn safety_target(&self, input: &Value) -> Option<String> {
        Some(input["cwd"].as_str().unwrap_or(".").to_string())
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

    fn compact_success_output(&self, output: String) -> String {
        compact_successful_test_output(output)
    }

    async fn run(&self, input: Value, ctx: &ToolCtx, cancel: &CancellationToken) -> ToolOutput {
        let Some(script) = input["command"].as_str() else {
            return ToolOutput::err("missing required parameter: command");
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
                    ToolOutput::err(out)
                } else {
                    out.push_str("\n(exit code 0)");
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
