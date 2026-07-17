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
    ActiveModel, Agent, AgentEvent, AgentModels, Approval, ApprovalDecision, Approver,
    ContentBlock, DelegateEvent, Entry, ModelCell, PermissionMode, PermissionRequest,
    PermissionRules, ProviderSafetyClassifier, SafetyClassifier, Session, TaskRunStatus, Tool,
    ToolCtx, ToolOutput,
};

const EXPLORE_SYSTEM: &str = include_str!("../../../prompts/task-explore-system.md");
const PLAN_SYSTEM: &str = include_str!("../../../prompts/task-plan-system.md");
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
        _allows_project: bool,
        _input: &serde_json::Value,
    ) -> Approval {
        Approval::simple(
            ApprovalDecision::No,
            Some("sub-agents cannot prompt the user".into()),
        )
    }
}

/// The sub-agent kinds `task` dispatches to. They are intentionally separate
/// from configurable auxiliary model roles: `auto` configures a classifier and
/// is never a value accepted by `task(agent=...)`.
pub const TASK_AGENT_KINDS: [&str; 3] = ["explore", "plan", "general"];

/// Roles surfaced by `/agents`: task kinds plus the auxiliary models the
/// harness itself runs — the Auto Mode classifier and the next-prompt guess.
/// Both want something small and fast, which is the whole point of pinning.
pub const MODEL_ROLES: [&str; 6] = ["explore", "plan", "general", "auto", "suggest", "vision"];

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

    /// Read-only sub-agents get only side-effect-free tools. `plan` has the
    /// same exploration surface as `explore`, except it cannot submit a plan:
    /// approval and the plan-mode transition remain exclusive to the parent.
    fn sub_tools(&self, agent_kind: &str, cwd: &Path, model: ModelCell) -> Vec<Arc<dyn Tool>> {
        let mut tools = crate::builtin_tools(cwd);
        tools.push(Arc::new(crate::ViewImageTool::new(
            model,
            self.pinned.clone(),
        )));
        tools
            .into_iter()
            .filter(|tool| keeps_sub_tool(agent_kind, tool.as_ref()))
            .collect()
    }
}

fn keeps_sub_tool(agent_kind: &str, tool: &dyn Tool) -> bool {
    !matches!(agent_kind, "explore" | "plan")
        || (!tool.is_mutating() && (agent_kind != "plan" || tool.name() != "exit_plan"))
}

/// Keep legacy/direct task calls useful while the tool schema nudges models to
/// supply an intentional one-line summary.
fn prompt_summary(prompt: &str) -> String {
    const MAX_CHARS: usize = 88;
    let first = prompt
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    if first.chars().count() <= MAX_CHARS {
        return first.to_string();
    }
    let capped: String = first.chars().take(MAX_CHARS - 1).collect();
    format!("{capped}…")
}

fn task_batch_label(inputs: &[&Value]) -> String {
    let count = inputs.len();
    let kinds: Vec<&str> = inputs
        .iter()
        .filter_map(|input| input["agent"].as_str())
        .collect();
    let kind = match kinds.as_slice() {
        ["explore", ..] if kinds.iter().all(|kind| *kind == "explore") => "Explore",
        ["plan", ..] if kinds.iter().all(|kind| *kind == "plan") => "Plan",
        ["general", ..] if kinds.iter().all(|kind| *kind == "general") => "Delegate",
        _ => "Delegate",
    };
    format!(
        "{kind} {count} {}",
        if count == 1 { "task" } else { "tasks" }
    )
}

#[cfg(test)]
mod tests {
    use super::{keeps_sub_tool, task_batch_label, TASK_AGENT_KINDS};
    use serde_json::json;

    #[test]
    fn task_batch_labels_name_the_delegated_work() {
        let explore = json!({"agent": "explore"});
        let plan = json!({"agent": "plan"});
        assert_eq!(task_batch_label(&[&explore]), "Explore 1 task");
        assert_eq!(task_batch_label(&[&explore, &explore]), "Explore 2 tasks");
        assert_eq!(task_batch_label(&[&plan, &explore]), "Delegate 2 tasks");
    }

    #[test]
    fn plan_kind_is_registered_and_has_explore_tools_except_exit_plan() {
        assert!(TASK_AGENT_KINDS.contains(&"plan"));
        let tools = crate::builtin_tools(&std::env::temp_dir());
        let explore: Vec<&str> = tools
            .iter()
            .filter(|tool| keeps_sub_tool("explore", tool.as_ref()))
            .map(|tool| tool.name())
            .collect();
        let plan: Vec<&str> = tools
            .iter()
            .filter(|tool| keeps_sub_tool("plan", tool.as_ref()))
            .map(|tool| tool.name())
            .collect();

        assert!(plan.iter().all(|name| *name != "exit_plan"));
        assert!(tools
            .iter()
            .filter(|tool| keeps_sub_tool("plan", tool.as_ref()))
            .all(|tool| !tool.is_mutating()));
        assert_eq!(
            plan,
            explore
                .into_iter()
                .filter(|name| *name != "exit_plan")
                .collect::<Vec<_>>()
        );
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
         context). Use agent='plan' for a read-only implementation-plan \
         draft that the parent must still review and submit. Use \
         agent='general' for independent multi-step work. Give a complete, \
         self-contained prompt and a very short task summary in the same language as that prompt; \
         the sub-agent sees nothing of this conversation and cannot ask questions."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent": { "type": "string", "enum": ["explore", "plan", "general"] },
                "prompt": { "type": "string" },
                "summary": {
                    "type": "string",
                    "description": "A very short summary of the delegated objective. Use the same language as prompt; it appears in the live agent tree."
                }
            },
            "required": ["agent", "prompt"]
        })
    }

    fn batch_policy_for(&self, input: &Value) -> tcode_core::BatchPolicy {
        match input["agent"].as_str() {
            Some("explore" | "plan") => tcode_core::BatchPolicy::ParallelReadOnly,
            _ => tcode_core::BatchPolicy::Isolated,
        }
    }

    fn batch_label(&self, inputs: &[&Value]) -> String {
        task_batch_label(inputs)
    }

    fn permission(&self, input: &Value) -> PermissionRequest {
        match input["agent"].as_str() {
            // Read-only: never prompts.
            Some("explore" | "plan") => PermissionRequest::None,
            _ => {
                let prompt = input["prompt"].as_str().unwrap_or("?");
                let preview: String = prompt.chars().take(60).collect();
                PermissionRequest::Ask {
                    descriptor: "task(general)".into(),
                    aliases: Vec::new(),
                    summary: format!("delegate to sub-agent: {preview}"),
                    is_edit: false,
                }
            }
        }
    }

    async fn run(&self, input: Value, ctx: &ToolCtx, cancel: &CancellationToken) -> ToolOutput {
        // Only reachable through a caller that bypassed `run_with_call`; the
        // run still works, its trace just cannot be tied to a ledger entry.
        self.run_with_call("", input, ctx, cancel).await
    }

    async fn run_with_call(
        &self,
        call_id: &str,
        input: Value,
        ctx: &ToolCtx,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let (Some(kind), Some(prompt)) = (input["agent"].as_str(), input["prompt"].as_str()) else {
            return ToolOutput::err("missing required parameters: agent, prompt");
        };
        let summary = input["summary"]
            .as_str()
            .map(str::trim)
            .filter(|summary| !summary.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| prompt_summary(prompt));
        let system = match kind {
            "explore" => EXPLORE_SYSTEM,
            "plan" => PLAN_SYSTEM,
            "general" => GENERAL_SYSTEM,
            other => {
                return ToolOutput::err(format!(
                    "unknown agent '{other}'; use 'explore', 'plan', or 'general'"
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
            model: model.clone(),
            // A sub-agent has no input box, so it never suggests; it still
            // carries the pins so its own classifier resolves the same way.
            models: self.pinned.clone(),
            tools: self.sub_tools(kind, &ctx.cwd, model.clone()),
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
            ToolCtx::with_scratch_dir(ctx.cwd.clone(), self.output_budget, ctx.scratch_dir.clone())
                .with_model(model.clone()),
            PermissionMode::Auto,
            PermissionRules::default(),
        )
        .with_cache_scope(format!("task-{kind}-{run}"));

        // Trace: the run gets a stable per-session id, and (when the parent
        // session persists) its own JSONL ledger log for the trace viewer.
        // Nothing here enters the parent's provider ledger.
        let (run_id, trace) = ctx.task_traces.lock().expect("task traces lock").begin(
            call_id,
            kind,
            &model_name,
            prompt,
            &summary,
        );
        if let Some(trace) = &trace {
            session.ledger.attach_sink(Box::new(trace.clone()));
        }
        let delegate = ctx.delegate_reporter();
        if let Some(delegate) = &delegate {
            // Best-effort visual/trace updates; losing them must never
            // interrupt the sub-agent.
            let _ = delegate.send(DelegateEvent::TaskStarted {
                run: run_id.clone(),
                parent_call: call_id.to_string(),
                kind: kind.to_string(),
                model: model_name.clone(),
                prompt: prompt.to_string(),
                summary,
            });
        }

        // Drain sub-agent events: count tool calls for the stats line and
        // forward everything, tagged with the run id, so the parent UI can
        // show live activity and a full trace. Streaming deltas coalesce so a
        // chatty sub-agent does not cross the channel one token at a time.
        let (tx, mut rx) = mpsc::channel(64);
        let tagger = {
            let delegate = delegate.clone();
            let run = run_id.clone();
            tokio::spawn(async move {
                let send = |ev: AgentEvent| {
                    if let Some(delegate) = &delegate {
                        let _ = delegate.send(DelegateEvent::TaskEvent {
                            run: run.clone(),
                            event: Box::new(ev),
                        });
                    }
                };
                let mut tools = 0usize;
                // At most one buffered delta run (text or thinking).
                let mut pending: Option<AgentEvent> = None;
                while let Some(ev) = rx.recv().await {
                    match &ev {
                        AgentEvent::ToolStart { .. } => tools += 1,
                        // Parallel batches emit no per-call ToolStart.
                        AgentEvent::ToolBatchStart { calls, .. } => tools += calls.len(),
                        _ => {}
                    }
                    match (&mut pending, ev) {
                        (Some(AgentEvent::TextDelta(buf)), AgentEvent::TextDelta(t)) => {
                            buf.push_str(&t)
                        }
                        (Some(AgentEvent::ThinkingDelta(buf)), AgentEvent::ThinkingDelta(t)) => {
                            buf.push_str(&t)
                        }
                        (slot, ev @ (AgentEvent::TextDelta(_) | AgentEvent::ThinkingDelta(_))) => {
                            if let Some(prev) = slot.take() {
                                send(prev);
                            }
                            *slot = Some(ev);
                        }
                        (slot, ev) => {
                            if let Some(prev) = slot.take() {
                                send(prev);
                            }
                            send(ev);
                        }
                    }
                    let full = pending.as_ref().is_some_and(|ev| match ev {
                        AgentEvent::TextDelta(t) | AgentEvent::ThinkingDelta(t) => t.len() >= 64,
                        _ => false,
                    });
                    if full {
                        send(pending.take().expect("checked above"));
                    }
                }
                if let Some(prev) = pending.take() {
                    send(prev);
                }
                tools
            })
        };

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
        let tool_calls = tagger.await.unwrap_or(0);

        let status = if result.is_err() {
            TaskRunStatus::Failed
        } else if cancel.is_cancelled() {
            TaskRunStatus::Cancelled
        } else {
            TaskRunStatus::Done
        };
        let usage = session.turn_usage;
        if let Some(trace) = &trace {
            trace.finish(status, tool_calls, usage);
        }
        if let Some(delegate) = &delegate {
            let _ = delegate.send(DelegateEvent::TaskFinished {
                run: run_id,
                status,
                tool_calls,
                usage,
            });
        }

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
