//! Sub-agents: the `task` tool runs a nested agent loop with its own
//! fresh ledger and a restricted tool set. The parent context only pays
//! for the prompt and the final report — the sub-agent's exploration
//! tokens never enter the parent's window.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use tcode_core::config::WatchdogConfig;
use tcode_core::{
    Agent, Approval, ApprovalDecision, Approver, ContentBlock, Entry, PermissionMode,
    PermissionRequest, PermissionRules, Provider, Session, Tool, ToolCtx, ToolOutput,
};

const EXPLORE_SYSTEM: &str = "\
You are a read-only exploration sub-agent inside tcode. Investigate the \
codebase with the tools provided and answer the request. You cannot edit \
files, run commands, or ask the user anything. Finish with a compact, \
self-contained report: findings, relevant file paths (with line numbers \
where useful), and conclusions. The report is all the caller will see.";

const GENERAL_SYSTEM: &str = "\
You are a sub-agent inside tcode executing a delegated task. Work \
autonomously: you cannot ask the user anything. When done, summarize \
what you did, what changed, and anything the caller must know. The \
summary is all the caller will see.";

/// Sub-agents run in Auto mode and must never prompt; this approver is
/// a safety net in case a deny-rule path still asks.
struct NeverAsk;

#[async_trait]
impl Approver for NeverAsk {
    async fn ask(&self, _tool: &str, _summary: &str, _descriptor: &str) -> Approval {
        Approval {
            decision: ApprovalDecision::No,
            comment: Some("sub-agents cannot prompt the user".into()),
        }
    }
}

pub struct TaskTool {
    provider: Arc<dyn Provider>,
    watchdog: WatchdogConfig,
    max_tokens: u32,
    context_window: u64,
    output_budget: usize,
    cwd: PathBuf,
}

impl TaskTool {
    pub fn new(
        provider: Arc<dyn Provider>,
        watchdog: WatchdogConfig,
        max_tokens: u32,
        context_window: u64,
        output_budget: usize,
        cwd: PathBuf,
    ) -> Self {
        Self {
            provider,
            watchdog,
            max_tokens,
            context_window,
            output_budget,
            cwd,
        }
    }

    fn sub_tools(&self, agent_kind: &str) -> Vec<Arc<dyn Tool>> {
        let read_only = ["read", "grep", "glob", "read_output"];
        crate::builtin_tools()
            .into_iter()
            .filter(|t| agent_kind != "explore" || read_only.contains(&t.name()))
            .collect()
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "Delegate a bounded subtask to a sub-agent with its own fresh \
         context. Use agent='explore' for read-only reconnaissance that \
         returns a report (cheap: its exploration never enters your \
         context). Use agent='general' for independent multi-step work. \
         Give a complete, self-contained prompt; the sub-agent sees \
         nothing of this conversation and cannot ask questions."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent": { "type": "string", "enum": ["explore", "general"] },
                "prompt": { "type": "string" }
            },
            "required": ["agent", "prompt"]
        })
    }

    fn permission(&self, input: &Value) -> PermissionRequest {
        match input["agent"].as_str() {
            // Read-only: never prompts.
            Some("explore") => PermissionRequest::None,
            _ => {
                let prompt = input["prompt"].as_str().unwrap_or("?");
                let preview: String = prompt.chars().take(60).collect();
                PermissionRequest::Ask {
                    descriptor: "task(general)".into(),
                    summary: format!("delegate to sub-agent: {preview}"),
                    is_edit: false,
                }
            }
        }
    }

    async fn run(&self, input: Value, _ctx: &ToolCtx, cancel: &CancellationToken) -> ToolOutput {
        let (Some(kind), Some(prompt)) = (input["agent"].as_str(), input["prompt"].as_str())
        else {
            return ToolOutput::err("missing required parameters: agent, prompt");
        };
        let system = match kind {
            "explore" => EXPLORE_SYSTEM,
            "general" => GENERAL_SYSTEM,
            other => {
                return ToolOutput::err(format!(
                    "unknown agent '{other}'; use 'explore' or 'general'"
                ))
            }
        };

        let agent = Agent {
            provider: self.provider.clone(),
            tools: self.sub_tools(kind),
            system: system.to_string(),
            max_tokens: self.max_tokens,
            context_window: self.context_window,
            watchdog: self.watchdog.clone(),
            hooks: Default::default(),
        };
        let mut session = Session::new(
            ToolCtx::new(self.cwd.clone(), self.output_budget),
            PermissionMode::Auto,
            PermissionRules::default(),
        );

        // Drain sub-agent events; count tool calls for the stats line.
        let (tx, mut rx) = mpsc::channel(64);
        let counter = tokio::spawn(async move {
            let mut tools = 0usize;
            while let Some(ev) = rx.recv().await {
                if matches!(ev, tcode_core::AgentEvent::ToolStart { .. }) {
                    tools += 1;
                }
            }
            tools
        });

        let result = agent
            .user_turn(
                &mut session,
                vec![ContentBlock::Text {
                    text: prompt.to_string(),
                }],
                &tx,
                &NeverAsk,
                cancel.clone(),
            )
            .await;
        drop(tx);
        let tool_calls = counter.await.unwrap_or(0);

        if let Err(e) = result {
            return ToolOutput::err(format!("sub-agent failed: {e}"));
        }
        if cancel.is_cancelled() {
            return ToolOutput::err("sub-agent cancelled by user");
        }

        // The report = text of the final assistant entry.
        let report: String = session
            .ledger
            .entries()
            .iter()
            .rev()
            .find_map(|e| match e {
                Entry::Assistant(blocks) => {
                    let text: String = blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    (!text.trim().is_empty()).then_some(text)
                }
                _ => None,
            })
            .unwrap_or_else(|| "(sub-agent produced no report)".into());

        let u = session.turn_usage;
        ToolOutput::ok(format!(
            "[{kind} sub-agent: {tool_calls} tool calls, in {} | out {} tokens]\n{report}",
            u.total_input(),
            u.output_tokens
        ))
    }
}
