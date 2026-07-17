//! `monitor` — watch something in the background and receive each output
//! line as an event, without blocking the conversation.
//!
//! A monitor is a background task (same registry, same log pipeline, same
//! `kill_task`) whose notification contract is per-line instead of on-exit:
//! the agent loop delivers pending events at safe append boundaries, and the
//! frontend wakes an idle session after the monitor's quiet window.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::{AutoSafety, PermissionRequest, TaskStatus, Tool, ToolCtx, ToolOutput};

use crate::shell::{shell_command, spawn_line_reader, ShellKind};

const DEFAULT_QUIET_MS: u64 = 1_500;
const QUIET_RANGE_MS: std::ops::RangeInclusive<u64> = 100..=30_000;
const DEFAULT_TIMEOUT_MS: u64 = 300_000;
const MAX_TIMEOUT_MS: u64 = 3_600_000;

pub struct MonitorTool {
    kind: ShellKind,
}

impl MonitorTool {
    pub fn new(kind: ShellKind) -> Self {
        Self { kind }
    }
}

#[async_trait]
impl Tool for MonitorTool {
    fn name(&self) -> &str {
        "monitor"
    }

    fn description(&self) -> &str {
        match self.kind {
            ShellKind::PowerShell => {
                "Watch something in the background: runs a PowerShell script whose \
                 stdout is an event stream — every line it prints reaches you as a \
                 harness note, even between your turns. Use it when you expect \
                 repeated events (tail a log for errors, poll CI/PR status and print \
                 one line per change, watch a directory). For a single \"tell me when \
                 X finishes\", use shell with run_in_background instead. Examples: \
                 `Get-Content app.log -Wait -Tail 0 | Select-String 'ERROR|FATAL'`; a \
                 poll loop with `Start-Sleep 30` printing one line per status change. \
                 Script rules: print only lines worth acting on (a monitor producing \
                 too many events is stopped automatically); cover failure states, not \
                 just success — a filter that only matches the success marker stays \
                 silent through a crash, and silence looks identical to still-running; \
                 in poll loops don't let one failed request end the loop. The monitor \
                 ends when the script exits, times out (default 5min — set persistent \
                 for session-length watches), or kill_task stops it. Each event line \
                 is capped at 512 bytes; the complete stream is always in the \
                 monitor's log file."
            }
            ShellKind::Bash => {
                "Watch something in the background: runs a bash script whose stdout \
                 is an event stream — every line it prints reaches you as a harness \
                 note, even between your turns. Use it when you expect repeated \
                 events (tail a log for errors, poll CI/PR status and print one line \
                 per change, watch a directory). For a single \"tell me when X \
                 finishes\", use shell with run_in_background instead. Examples: \
                 `tail -f app.log | grep --line-buffered -E 'ERROR|FATAL'`; a poll \
                 loop with `sleep 30` printing one line per status change. Script \
                 rules: every pipe stage must flush per line (grep needs \
                 --line-buffered, awk needs fflush()); print only lines worth acting \
                 on (a monitor producing too many events is stopped automatically); \
                 cover failure states, not just success — a filter that only matches \
                 the success marker stays silent through a crash, and silence looks \
                 identical to still-running; in poll loops don't let one failed \
                 request end the loop (`curl ... || true`). The monitor ends when the \
                 script exits, times out (default 5min — set persistent for \
                 session-length watches), or kill_task stops it. Each event line is \
                 capped at 512 bytes; the complete stream is always in the monitor's \
                 log file."
            }
        }
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Script to run; each stdout line is an event, exit ends the watch" },
                "description": { "type": "string", "description": "Short label for what is being watched, shown with every event (e.g. \"errors in deploy.log\")" },
                "quiet_ms": { "type": "integer", "description": "Idle-wake coalescing window: after the first undelivered event, further events within this window join the same wake (default 1500, clamped 100-30000). Delivery at turn boundaries is immediate regardless." },
                "timeout_ms": { "type": "integer", "description": "Kill the monitor after this deadline (default 300000, max 3600000); ignored when persistent" },
                "persistent": { "type": "boolean", "description": "Run until the session ends or kill_task stops it (default false)" }
            },
            "required": ["command", "description"]
        })
    }

    fn auto_safety(&self, _input: &Value) -> AutoSafety {
        AutoSafety::AllowInScratch
    }

    fn safety_target(&self, _input: &Value) -> Option<String> {
        Some(".".to_string())
    }

    fn permission(&self, input: &Value) -> PermissionRequest {
        let command = input["command"].as_str().unwrap_or("?");
        // Same rule domain as the shell tool: an allow/deny written for
        // run(...)/shell(...) applies to a monitor running the same command.
        PermissionRequest::Ask {
            descriptor: format!("run({command})"),
            aliases: vec![
                format!("monitor({command})"),
                format!(
                    "{}({command})",
                    match self.kind {
                        ShellKind::PowerShell => "shell",
                        ShellKind::Bash => "bash",
                    }
                ),
            ],
            summary: format!("monitor: {command}"),
            is_edit: false,
        }
    }

    fn is_mutating(&self) -> bool {
        true
    }

    async fn run(&self, input: Value, ctx: &ToolCtx, _cancel: &CancellationToken) -> ToolOutput {
        let Some(script) = input["command"].as_str() else {
            return ToolOutput::err("missing required parameter: command");
        };
        let Some(description) = input["description"]
            .as_str()
            .filter(|d| !d.trim().is_empty())
        else {
            return ToolOutput::err(
                "missing required parameter: description — a short label for what \
                 is being watched, shown with every event",
            );
        };
        let quiet = Duration::from_millis(
            input["quiet_ms"]
                .as_u64()
                .unwrap_or(DEFAULT_QUIET_MS)
                .clamp(*QUIET_RANGE_MS.start(), *QUIET_RANGE_MS.end()),
        );
        let persistent = input["persistent"].as_bool().unwrap_or(false);
        let timeout = (!persistent).then(|| {
            Duration::from_millis(
                input["timeout_ms"]
                    .as_u64()
                    .unwrap_or(DEFAULT_TIMEOUT_MS)
                    .min(MAX_TIMEOUT_MS),
            )
        });

        let mut cmd = shell_command(self.kind, script, &ctx.cwd);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ToolOutput::err(format!("failed to start monitor: {e}")),
        };
        let (id, shared) = match ctx
            .background
            .lock()
            .expect("background lock")
            .register_monitor(script, description, quiet)
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
            let deadline = async {
                match timeout {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                status = child.wait() => {
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
                _ = deadline => {
                    supervisor_shared.mark_timed_out();
                    let _ = child.kill().await;
                    for r in readers {
                        let _ = r.await;
                    }
                    supervisor_shared.set_status(TaskStatus::Killed);
                }
            }
        });

        let lifetime = match timeout {
            Some(d) => format!("it exits or after {}s", d.as_secs()),
            None => "it exits or the session ends".to_string(),
        };
        ToolOutput::ok(format!(
            "Started monitor {id} ({description}): every line the script prints \
             will reach you as an event note. It runs until {lifetime}; stop it \
             early with kill_task(id=\"{id}\"). Full output accumulates in {}.",
            shared.log_path.display()
        ))
    }
}
