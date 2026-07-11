//! Per-tool hooks: external commands triggered around tool execution.
//! Semantics follow Claude Code where sensible: exit code 2 blocks a
//! pre_tool_use call and stderr becomes the reason the model sees.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use crate::permission::pattern_match;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// Before a tool runs; may block it.
    PreToolUse,
    /// After a tool ran; stderr on failure is appended to the result.
    PostToolUse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookDef {
    pub event: HookEvent,
    /// Tool-name pattern; `*` wildcard, `|` alternation ("edit|write").
    pub matcher: String,
    pub command: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    30
}

fn matches(matcher: &str, tool: &str) -> bool {
    matcher.split('|').any(|m| pattern_match(m.trim(), tool))
}

/// The outcome the agent loop acts on.
pub struct HookVerdict {
    /// Pre hook demanded a block (exit code 2); reason for the model.
    pub block: Option<String>,
    /// Messages worth showing the model (non-zero exits, stderr).
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Hooks {
    pub defs: Vec<HookDef>,
}

impl Hooks {
    pub fn new(defs: Vec<HookDef>) -> Self {
        Self { defs }
    }

    pub async fn run(
        &self,
        event: HookEvent,
        tool: &str,
        input: &Value,
        output: Option<&str>,
        cwd: &Path,
    ) -> HookVerdict {
        let mut verdict = HookVerdict {
            block: None,
            notes: Vec::new(),
        };
        for def in self
            .defs
            .iter()
            .filter(|d| d.event == event && matches(&d.matcher, tool))
        {
            let payload = json!({
                "event": event,
                "tool": tool,
                "input": input,
                "output": output,
                "cwd": cwd.to_string_lossy(),
            });
            match run_one(def, &payload, cwd).await {
                Ok((code, stderr)) => match code {
                    0 => {}
                    2 if event == HookEvent::PreToolUse => {
                        let reason = if stderr.trim().is_empty() {
                            format!("blocked by hook `{}`", def.command)
                        } else {
                            stderr.trim().to_string()
                        };
                        verdict.block = Some(reason);
                        return verdict;
                    }
                    _ => verdict.notes.push(format!(
                        "hook `{}` exited {code}: {}",
                        def.command,
                        stderr.trim()
                    )),
                },
                Err(e) => verdict
                    .notes
                    .push(format!("hook `{}` failed to run: {e}", def.command)),
            }
        }
        verdict
    }
}

async fn run_one(
    def: &HookDef,
    payload: &Value,
    cwd: &Path,
) -> Result<(i32, String), std::io::Error> {
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(&def.command);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(&def.command);
        c
    };
    let mut child = cmd
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.to_string().as_bytes()).await;
        // Dropping stdin closes it so the hook sees EOF.
    }
    let out = tokio::time::timeout(
        Duration::from_secs(def.timeout_secs),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "hook timed out"))??;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(event: HookEvent, matcher: &str, command: &str) -> HookDef {
        HookDef {
            event,
            matcher: matcher.into(),
            command: command.into(),
            timeout_secs: 10,
        }
    }

    #[test]
    fn matcher_supports_alternation_and_glob() {
        assert!(matches("edit|write", "write"));
        assert!(matches("edit | write", "edit"));
        assert!(matches("*", "anything"));
        assert!(!matches("edit|write", "shell"));
    }

    #[tokio::test]
    async fn exit_2_blocks_pre_tool_use() {
        let cmd = if cfg!(windows) {
            "echo no edits allowed 1>&2 & exit 2"
        } else {
            "echo 'no edits allowed' >&2; exit 2"
        };
        let hooks = Hooks::new(vec![hook(HookEvent::PreToolUse, "edit", cmd)]);
        let v = hooks
            .run(
                HookEvent::PreToolUse,
                "edit",
                &json!({"path": "a.rs"}),
                None,
                Path::new("."),
            )
            .await;
        assert!(v.block.is_some());
        assert!(v.block.unwrap().contains("no edits allowed"));
    }

    #[tokio::test]
    async fn nonzero_post_hook_becomes_note() {
        let cmd = if cfg!(windows) {
            "echo fmt failed 1>&2 & exit 1"
        } else {
            "echo 'fmt failed' >&2; exit 1"
        };
        let hooks = Hooks::new(vec![hook(HookEvent::PostToolUse, "edit|write", cmd)]);
        let v = hooks
            .run(
                HookEvent::PostToolUse,
                "write",
                &json!({}),
                Some("wrote a.rs"),
                Path::new("."),
            )
            .await;
        assert!(v.block.is_none());
        assert_eq!(v.notes.len(), 1);
        assert!(v.notes[0].contains("fmt failed"));
    }

    #[tokio::test]
    async fn passing_hook_is_silent() {
        let hooks = Hooks::new(vec![hook(HookEvent::PostToolUse, "*", "exit 0")]);
        let v = hooks
            .run(
                HookEvent::PostToolUse,
                "edit",
                &json!({}),
                None,
                Path::new("."),
            )
            .await;
        assert!(v.block.is_none());
        assert!(v.notes.is_empty());
    }
}
