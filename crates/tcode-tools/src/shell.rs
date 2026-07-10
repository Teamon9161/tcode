use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use tcode_core::{PermissionRequest, Tool, ToolCtx, ToolOutput};

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
                let mut cmd = tokio::process::Command::new(
                    which_bash().unwrap_or_else(|| "bash".into()),
                );
                cmd.args(["-c", script]);
                cmd.current_dir(cwd);
                cmd
            }
        }
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        match self.kind {
            ShellKind::PowerShell => "shell",
            ShellKind::Bash => "bash",
        }
    }

    fn description(&self) -> &str {
        match self.kind {
            ShellKind::PowerShell => {
                "Run a PowerShell command. Output is captured; interactive \
                 commands will hang and must be avoided. Use timeout_ms for \
                 long-running commands (default 120s, max 600s)."
            }
            ShellKind::Bash => {
                "Run a bash (Git Bash) command with POSIX syntax. Same rules \
                 as shell: non-interactive only, timeout_ms configurable."
            }
        }
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "timeout_ms": { "type": "integer", "description": "Kill after this many ms (default 120000, max 600000)" },
                "cwd": { "type": "string", "description": "Working directory (default: project cwd)" }
            },
            "required": ["command"]
        })
    }

    fn permission(&self, input: &Value) -> PermissionRequest {
        let command = input["command"].as_str().unwrap_or("?");
        PermissionRequest::Ask {
            descriptor: format!("{}({command})", self.name()),
            summary: format!("run: {command}"),
            is_edit: false,
        }
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
