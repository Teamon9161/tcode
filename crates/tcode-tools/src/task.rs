//! Sub-agents: the `task` tool runs a nested agent loop with its own
//! fresh ledger and a restricted tool set. The parent context only pays
//! for the prompt and the final report — the sub-agent's exploration
//! tokens never enter the parent's window.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use tcode_core::config::WatchdogConfig;
use tcode_core::{
    ActiveModel, Agent, AgentModels, Approval, ApprovalDecision, Approver, ContentBlock, Entry,
    ModelCell, PermissionMode, PermissionRequest, PermissionRules, ProviderSafetyClassifier,
    SafetyClassifier, Session, Tool, ToolCtx, ToolOutput,
};

const EXPLORE_SYSTEM: &str = include_str!("../../../prompts/task-explore-system.md");
const GENERAL_SYSTEM: &str = include_str!("../../../prompts/task-general-system.md");

/// Sub-agents run in unsafe mode and must never prompt; this approver is
/// a safety net in case a deny-rule path still asks.
struct NeverAsk;

#[async_trait]
impl Approver for NeverAsk {
    async fn ask(
        &self,
        _tool: &str,
        _summary: &str,
        _descriptor: &str,
        _input: &serde_json::Value,
    ) -> Approval {
        Approval {
            decision: ApprovalDecision::No,
            comment: Some("sub-agents cannot prompt the user".into()),
        }
    }
}

/// The sub-agent kinds `task` dispatches to. They are intentionally separate
/// from configurable auxiliary model roles: `auto` configures a classifier and
/// is never a value accepted by `task(agent=...)`.
pub const TASK_AGENT_KINDS: [&str; 2] = ["explore", "general"];

/// Roles surfaced by `/agents`: task kinds plus the auxiliary models the
/// harness itself runs — the Auto Mode classifier and the next-prompt guess.
/// Both want something small and fast, which is the whole point of pinning.
pub const MODEL_ROLES: [&str; 4] = ["explore", "general", "auto", "suggest"];

pub struct TaskTool {
    /// Shared with the parent agent: sub-agents follow `/model` switches.
    model: ModelCell,
    /// Per-kind pins (`[agents.<kind>]`, or `/agents`). A pinned kind does
    /// *not* follow `/model` — that is the point: "explore always runs on the
    /// cheap model". The registry is shared with the frontend that edits it,
    /// so a pick takes effect on the next sub-agent, not the next process.
    pinned: AgentModels,
    watchdog: WatchdogConfig,
    output_budget: usize,
    auto_policy: String,
}

impl TaskTool {
    pub fn new(
        model: ModelCell,
        watchdog: WatchdogConfig,
        output_budget: usize,
        _cwd: PathBuf,
    ) -> Self {
        Self {
            model,
            pinned: AgentModels::default(),
            watchdog,
            output_budget,
            auto_policy: String::new(),
        }
    }

    /// Share the live pin registry with the frontend that edits it.
    pub fn with_agent_models(mut self, pinned: AgentModels) -> Self {
        self.pinned = pinned;
        self
    }

    /// Supply the parent session's global Auto Mode policy. Project-local
    /// config never reaches this field, even for delegated work.
    pub fn with_auto_policy(mut self, policy: String) -> Self {
        self.auto_policy = policy;
        self
    }

    /// The pinned model for `kind`, else a snapshot of the parent's.
    fn model_for(&self, kind: &str) -> ActiveModel {
        self.pinned
            .get(kind)
            .unwrap_or_else(|| self.model.snapshot())
    }

    /// `explore` sub-agents get only side-effect-free tools. The question to
    /// ask a tool is whether it mutates, not how it batches: `batch_policy`
    /// describes parallelism, and reading it as a safety filter silently
    /// excluded harmless non-parallel tools like `skill`.
    fn sub_tools(&self, agent_kind: &str, cwd: &Path) -> Vec<Arc<dyn Tool>> {
        crate::builtin_tools(cwd)
            .into_iter()
            .filter(|t| agent_kind != "explore" || !t.is_mutating())
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

    async fn run(&self, input: Value, ctx: &ToolCtx, cancel: &CancellationToken) -> ToolOutput {
        let (Some(kind), Some(prompt)) = (input["agent"].as_str(), input["prompt"].as_str()) else {
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

        let model = self.model_for(kind);
        let model_name = model.provider.model().to_string();
        let model = ModelCell::new(model);
        let safety_classifier: Arc<dyn SafetyClassifier> = Arc::new(ProviderSafetyClassifier::new(
            model.clone(),
            self.pinned.clone(),
        ));
        let agent = Agent {
            model,
            // A sub-agent has no input box, so it never suggests; it still
            // carries the pins so its own classifier resolves the same way.
            models: self.pinned.clone(),
            tools: self.sub_tools(kind, &ctx.cwd),
            system: system.to_string(),
            watchdog: self.watchdog.clone(),
            hooks: Default::default(),
            safety_classifier: Some(safety_classifier),
            auto_policy: self.auto_policy.clone(),
            max_steps: tcode_core::DEFAULT_MAX_STEPS,
        };
        // Every sub-agent run is its own conversation on (usually) the parent's
        // provider. Sharing the parent's cache id would interleave two
        // unrelated prefixes on it, so each run names its own scope.
        static RUN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let run = RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut session = Session::new(
            ToolCtx::new(ctx.cwd.clone(), self.output_budget),
            PermissionMode::Auto,
            PermissionRules::default(),
        )
        .with_cache_scope(format!("task-{kind}-{run}"));

        // Drain sub-agent events; count tool calls for the stats line.
        let (tx, mut rx) = mpsc::channel(64);
        let usage_reporter = ctx.usage_reporter();
        let counter = tokio::spawn(async move {
            let mut tools = 0usize;
            while let Some(ev) = rx.recv().await {
                match ev {
                    tcode_core::AgentEvent::ToolStart { .. } => tools += 1,
                    tcode_core::AgentEvent::Usage(usage) => {
                        if let Some(reporter) = &usage_reporter {
                            // It is a best-effort visual/statistical update;
                            // losing it must never interrupt the sub-agent.
                            let _ = reporter.send(usage);
                        }
                    }
                    _ => {}
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
            "[{kind} sub-agent on {model_name}: {tool_calls} tool calls, \
             in {} | out {} tokens]\n{report}",
            u.total_input(),
            u.output_tokens
        ))
    }
}
