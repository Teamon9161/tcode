//! Sub-agents: the `task` tool runs a nested agent loop with its own
//! fresh ledger and a restricted tool set. The parent context only pays
//! for the prompt and the final report — the sub-agent's exploration
//! tokens never enter the parent's window.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use tcode_core::config::WatchdogConfig;
use tcode_core::{
    ActiveModel, Agent, AgentEvent, AgentModels, AgentRole, Approval, ApprovalDecision, Approver,
    ContentBlock, DelegateEvent, Entry, ModelCell, PermissionMode, PermissionRequest,
    PermissionRules, ProviderSafetyClassifier, SafetyClassifier, Session, TaskRunStatus, Tool,
    ToolCtx, ToolOutput,
};

use crate::agent_defs::{keeps_tool, AgentDef, AgentRegistry};

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

/// The sub-agent kinds `task` dispatches to. The shared role registry owns
/// the complete `/agents` catalogue; this compatibility view names only task
/// inputs.
pub const TASK_AGENT_KINDS: [&str; 3] = AgentRole::TASK_KEYS;

/// Parked resumable runs per task-tool instance; oldest is evicted beyond
/// this. Each parked run keeps its whole ledger in memory.
const MAX_LIVE_TASKS: usize = 8;

/// A finished delegated run kept alive for follow-up turns. Resuming appends
/// to the same session under the same cache scope, so a follow-up costs only
/// the increment on top of a full prefix cache hit.
struct LiveTask {
    agent: Agent,
    session: Session,
    exchanges_left: u32,
    def_name: String,
    model_name: String,
    /// Park order for oldest-first eviction.
    seq: u64,
}

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
    auto_compact: bool,
    auto_compact_percent: u8,
    trusted_read_hosts: crate::TrustedReadHosts,
    /// Builtin task kinds and discovered custom agents, one registry.
    defs: Arc<AgentRegistry>,
    /// Spawnable subset for a nested instance; `None` = the whole registry.
    allowed: Option<Vec<String>>,
    /// Nesting depth of the *owner* of this tool: the top-level agent holds a
    /// depth-0 instance. At `MAX_TASK_DEPTH` sub-agents get no task tool.
    depth: usize,
    /// Built once at construction: the description enters the cached prompt
    /// prefix and must not change within a session.
    description: String,
    /// Resumable runs, keyed by their task id. Entries are taken out for the
    /// duration of a resumed turn, so concurrent resumes of one id fail with
    /// a self-healing error instead of racing.
    live: Arc<Mutex<HashMap<String, LiveTask>>>,
}

impl TaskTool {
    pub fn new(
        model: ModelCell,
        watchdog: WatchdogConfig,
        output_budget: usize,
        _cwd: PathBuf,
    ) -> Self {
        let defs = Arc::new(AgentRegistry::builtin());
        Self {
            model,
            pinned: AgentModels::default(),
            watchdog,
            output_budget,
            auto_policy: String::new(),
            auto_compact: true,
            auto_compact_percent: 85,
            trusted_read_hosts: crate::trusted_read_hosts(Vec::new()),
            description: task_description(&defs, None),
            defs,
            allowed: None,
            depth: 0,
            live: Arc::default(),
        }
    }

    /// Share the live pin registry with the frontend that edits it.
    pub fn with_agent_models(mut self, pinned: AgentModels) -> Self {
        self.pinned = pinned;
        self
    }

    /// Dispatch to this registry instead of the builtin-only default.
    pub fn with_agent_defs(mut self, defs: Arc<AgentRegistry>) -> Self {
        self.description = task_description(&defs, self.allowed.as_deref());
        self.defs = defs;
        self
    }

    /// Configure this instance for a top-level `--agent` run: the named
    /// definition's spawn list becomes the whole schema, already one level
    /// deep (the process itself is that agent). Call after `with_agent_defs`.
    pub fn scoped_to(mut self, def: &AgentDef) -> Self {
        self.description = task_description(&self.defs, Some(&def.agents));
        self.allowed = Some(def.agents.clone());
        self.depth = 1;
        self
    }

    /// Supply the parent session's global Auto Mode policy. Project-local
    /// config never reaches this field, even for delegated work.
    pub fn with_auto_policy(mut self, policy: String) -> Self {
        self.auto_policy = policy;
        self
    }

    /// Apply the main session's automatic compaction policy to isolated runs.
    pub fn with_auto_compact(mut self, enabled: bool, percent: u8) -> Self {
        self.auto_compact = enabled;
        self.auto_compact_percent = percent;
        self
    }

    /// Carry the global, tool-scoped trusted read hosts into each isolated
    /// sub-agent. Project configuration never reaches this field.
    pub fn with_trusted_read_hosts(mut self, hosts: crate::TrustedReadHosts) -> Self {
        self.trusted_read_hosts = hosts;
        self
    }

    /// The pinned model for `kind`, else a snapshot of the parent's. String
    /// keyed so custom kinds resolve through `[agents.<name>]` for free.
    fn model_for(&self, kind: &str) -> ActiveModel {
        self.pinned
            .get(kind)
            .unwrap_or_else(|| self.model.snapshot())
    }

    /// The definition-derived toolset. Read-only agents get only
    /// side-effect-free tools; builtin `plan` additionally loses `exit_plan`
    /// (approval and the plan-mode transition remain exclusive to the
    /// parent); an allowlist restricts further.
    fn sub_tools(&self, def: &AgentDef, cwd: &Path, model: ModelCell) -> Vec<Arc<dyn Tool>> {
        let mut tools = crate::builtin_tools_with_web_fetch(
            cwd,
            crate::WebFetchTool::new(self.trusted_read_hosts.clone()).with_summarizer(
                crate::FetchSummarizer::new(model.clone(), self.pinned.clone()),
            ),
        );
        tools.push(Arc::new(crate::ViewImageTool::new(
            model.clone(),
            self.pinned.clone(),
        )));
        tools.retain(|tool| keeps_tool(def, tool.as_ref()));
        // Delegation is granted by the `agents` field alone — deliberately
        // outside the allowlist/read-only tiers — and bounded by depth, so
        // definition cycles terminate without graph analysis.
        if !def.agents.is_empty() && self.depth < crate::agent_defs::MAX_TASK_DEPTH {
            tools.push(Arc::new(self.child(def, model)));
        }
        tools
    }

    /// A task tool for a sub-agent that may itself delegate: same registry
    /// and pins, spawn set restricted to the definition's list, one level
    /// deeper. The child's parent handle is the sub-agent's own model cell,
    /// so an unpinned grandchild inherits its spawner, not the top level.
    fn child(&self, def: &AgentDef, model: ModelCell) -> TaskTool {
        TaskTool {
            model,
            pinned: self.pinned.clone(),
            watchdog: self.watchdog.clone(),
            output_budget: self.output_budget,
            auto_policy: self.auto_policy.clone(),
            auto_compact: self.auto_compact,
            auto_compact_percent: self.auto_compact_percent,
            trusted_read_hosts: self.trusted_read_hosts.clone(),
            description: task_description(&self.defs, Some(&def.agents)),
            defs: self.defs.clone(),
            allowed: Some(def.agents.clone()),
            depth: self.depth + 1,
            // Each instance parks its own runs: the child tool lives inside
            // the spawning sub-agent's toolset, so a parked grandchild
            // survives exactly as long as its parker can still resume it.
            live: Arc::default(),
        }
    }

    /// Park a finished run for follow-ups, evicting the oldest beyond cap.
    fn park(&self, id: &str, task: LiveTask) {
        let mut live = self.live.lock().expect("live tasks lock");
        if live.len() >= MAX_LIVE_TASKS {
            if let Some(oldest) = live
                .iter()
                .min_by_key(|(_, task)| task.seq)
                .map(|(id, _)| id.clone())
            {
                live.remove(&oldest);
            }
        }
        live.insert(id.to_string(), task);
    }

    /// The definition for `kind`, honoring this instance's spawn allowlist.
    fn def_for(&self, kind: &str) -> Option<&AgentDef> {
        let allowed = self
            .allowed
            .as_deref()
            .is_none_or(|allow| allow.iter().any(|name| name == kind));
        self.defs.get(kind).filter(|_| allowed)
    }
}

/// Tool description for a given registry view. The unrestricted text keeps
/// the hand-written builtin paragraph verbatim (byte-identical prefix when no
/// custom agents exist); a restricted (nested) view describes exactly the
/// spawnable set instead.
fn task_description(defs: &AgentRegistry, allow: Option<&[String]>) -> String {
    let base = match allow {
        None => {
            "Delegate a bounded subtask to a sub-agent with its own fresh \
             context. Use agent='explore' for read-only reconnaissance that \
             returns a report (cheap: its exploration never enters your \
             context). Use agent='plan' for a read-only implementation-plan \
             draft that the parent must still review and submit. Use \
             agent='general' for independent multi-step work. Give a complete, \
             self-contained prompt and a very short task summary in the same language as that prompt; \
             the sub-agent sees nothing of this conversation and cannot ask questions."
                .to_string()
        }
        Some(allow) => {
            let mut base = String::from(
                "Delegate a bounded subtask to a sub-agent with its own fresh \
                 context. Give a complete, self-contained prompt and a very \
                 short task summary in the same language as that prompt; the \
                 sub-agent sees nothing of this conversation and cannot ask \
                 questions.\n\nAvailable agents:\n",
            );
            for name in defs.names_for(Some(allow)) {
                if let Some(def) = defs.get(name) {
                    if matches!(def.source, crate::AgentSource::Builtin) {
                        base.push_str(&format!("- {}: {}\n", def.name, def.description));
                    }
                }
            }
            base
        }
    };
    format!("{base}{}", defs.custom_listing(allow))
}

/// Input schema for a given registry view; the enum is the spawnable set.
fn task_schema(defs: &AgentRegistry, allow: Option<&[String]>) -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent": { "type": "string", "enum": defs.names_for(allow) },
            "prompt": { "type": "string" },
            "resume": {
                "type": "string",
                "description": "Task id of a resumable previous run (given in its result line). The same sub-agent continues with its context intact — use for follow-up questions or feedback. `agent` must match the original kind."
            },
            "summary": {
                "type": "string",
                "description": "A very short summary of the delegated objective. Use the same language as prompt; it appears in the live agent tree."
            }
        },
        "required": ["agent", "prompt"]
    })
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
    use super::{task_batch_label, task_description, task_schema, TASK_AGENT_KINDS};
    use crate::agent_defs::AgentRegistry;
    use serde_json::json;

    #[test]
    fn task_batch_labels_name_the_delegated_work() {
        let explore = json!({"agent": "explore"});
        let plan = json!({"agent": "plan"});
        assert_eq!(task_batch_label(&[&explore]), "Explore 1 task");
        assert_eq!(task_batch_label(&[&explore, &explore]), "Explore 2 tasks");
        assert_eq!(task_batch_label(&[&plan, &explore]), "Delegate 2 tasks");
    }

    fn registry_with(defs: &[(&str, &str)]) -> AgentRegistry {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".tcode/agents");
        std::fs::create_dir_all(&dir).unwrap();
        for (name, front) in defs {
            std::fs::write(
                dir.join(format!("{name}.md")),
                format!("---\ndescription: {name} agent\n{front}\n---\nSystem for {name}."),
            )
            .unwrap();
        }
        let (registry, warnings) = AgentRegistry::discover(tmp.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        registry
    }

    #[test]
    fn schema_enum_and_description_track_custom_agents() {
        let registry = registry_with(&[("investor", "agents: quant-dev"), ("quant-dev", "")]);
        let schema = task_schema(&registry, None);
        let kinds: Vec<&str> = schema["properties"]["agent"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(
            kinds,
            ["explore", "plan", "general", "investor", "quant-dev"]
        );
        assert!(TASK_AGENT_KINDS.iter().all(|kind| kinds.contains(kind)));
        let description = task_description(&registry, None);
        assert!(description.contains("investor: investor agent"));
    }

    #[test]
    fn a_spawn_allowlist_restricts_schema_and_description() {
        let registry = registry_with(&[("investor", "agents: quant-dev"), ("quant-dev", "")]);
        let allow = vec!["quant-dev".to_string()];
        let schema = task_schema(&registry, Some(&allow));
        assert_eq!(schema["properties"]["agent"]["enum"], json!(["quant-dev"]));
        let description = task_description(&registry, Some(&allow));
        assert!(description.contains("quant-dev:"));
        assert!(!description.contains("investor:"));
        assert!(!description.contains("agent='explore'"));
    }

    #[test]
    fn without_custom_agents_the_description_is_the_static_paragraph() {
        let registry = AgentRegistry::builtin();
        let description = task_description(&registry, None);
        assert!(description.ends_with("cannot ask questions."));
        assert!(!description.contains("Custom agents"));
    }

    /// Never streams: toolset-shape tests construct models without talking.
    struct NullProvider;

    #[async_trait::async_trait]
    impl tcode_core::Provider for NullProvider {
        fn name(&self) -> &str {
            "null"
        }
        fn model(&self) -> &str {
            "null"
        }
        fn cache_strategy(&self) -> tcode_core::CacheStrategy {
            tcode_core::CacheStrategy::ImplicitPrefix
        }
        async fn stream(
            &self,
            _req: tcode_core::Request,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<tcode_core::EventStream, tcode_core::ProviderError> {
            unreachable!("toolset tests never stream")
        }
    }

    fn null_model() -> super::ModelCell {
        super::ModelCell::new(tcode_core::ActiveModel {
            provider: std::sync::Arc::new(NullProvider),
            max_tokens: 1024,
            context_window: 100_000,
            effort: None,
        })
    }

    #[test]
    fn nesting_is_granted_by_the_agents_field_and_bounded_by_depth() {
        use tcode_core::Tool as _;
        let registry = std::sync::Arc::new(registry_with(&[
            ("investor", "agents: quant-dev"),
            ("quant-dev", ""),
        ]));
        let task = super::TaskTool::new(
            null_model(),
            Default::default(),
            2_000,
            std::env::temp_dir(),
        )
        .with_agent_defs(registry.clone());
        let investor = registry.get("investor").unwrap();
        let leaf = registry.get("quant-dev").unwrap();
        let tmp = std::env::temp_dir();

        // A spawner gets a task tool whose schema is exactly its spawn list.
        let tools = task.sub_tools(investor, &tmp, null_model());
        let child = tools
            .iter()
            .find(|tool| tool.name() == "task")
            .expect("spawner receives a task tool");
        assert_eq!(
            child.input_schema()["properties"]["agent"]["enum"],
            json!(["quant-dev"])
        );

        // A definition without `agents` is a leaf.
        assert!(!task
            .sub_tools(leaf, &tmp, null_model())
            .iter()
            .any(|tool| tool.name() == "task"));

        // Depth bound: instances at MAX_TASK_DEPTH stop handing the tool out.
        let d2 = task
            .child(investor, null_model())
            .child(investor, null_model());
        let d3 = d2.child(investor, null_model());
        assert!(d2
            .sub_tools(investor, &tmp, null_model())
            .iter()
            .any(|tool| tool.name() == "task"));
        assert!(!d3
            .sub_tools(investor, &tmp, null_model())
            .iter()
            .any(|tool| tool.name() == "task"));
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        task_schema(&self.defs, self.allowed.as_deref())
    }

    fn batch_policy_for(&self, input: &Value) -> tcode_core::BatchPolicy {
        // Resumed runs mutate parked session state; serialize them.
        let read_only = input["resume"].as_str().is_none()
            && input["agent"]
                .as_str()
                .and_then(|kind| self.def_for(kind))
                .is_some_and(|def| def.read_only);
        if read_only {
            tcode_core::BatchPolicy::ParallelReadOnly
        } else {
            tcode_core::BatchPolicy::Isolated
        }
    }

    fn batch_label(&self, inputs: &[&Value]) -> String {
        task_batch_label(inputs)
    }

    fn permission(&self, input: &Value) -> PermissionRequest {
        match input["agent"].as_str().and_then(|kind| self.def_for(kind)) {
            // Read-only: never prompts.
            Some(def) if def.read_only => PermissionRequest::None,
            Some(def) => {
                let prompt = input["prompt"].as_str().unwrap_or("?");
                let preview: String = prompt.chars().take(60).collect();
                PermissionRequest::Ask {
                    descriptor: format!("task({})", def.name),
                    aliases: Vec::new(),
                    summary: format!("delegate to sub-agent: {preview}"),
                    is_edit: false,
                }
            }
            // Unknown kind: run() fails immediately with a self-healing
            // error before any side effect, so prompting would be noise.
            None => PermissionRequest::None,
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
        let Some(def) = self.def_for(kind) else {
            return ToolOutput::err(format!(
                "unknown agent '{kind}'; available: {}",
                self.defs.names_for(self.allowed.as_deref()).join(", ")
            ));
        };
        if let Some(id) = input["resume"]
            .as_str()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            return self
                .resume_run(id, def, prompt, &summary, call_id, ctx, cancel)
                .await;
        }
        let system = def.system.clone();

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
            tools: self.sub_tools(def, &ctx.cwd, model.clone()),
            system,
            watchdog: self.watchdog.clone(),
            hooks: Default::default(),
            safety_classifier: Some(safety_classifier),
            auto_policy: self.auto_policy.clone(),
            max_steps: def.max_steps.unwrap_or(tcode_core::DEFAULT_MAX_STEPS),
            auto_compact: self.auto_compact,
            auto_compact_percent: self.auto_compact_percent,
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

        let run = match self
            .drive(
                &agent,
                &mut session,
                kind,
                &model_name,
                prompt,
                &summary,
                call_id,
                ctx,
                cancel,
            )
            .await
        {
            Ok(run) => run,
            Err(out) => return out,
        };

        if def.max_exchanges > 0 {
            let id = run.run_id.clone();
            let header = format!(
                "[{kind} sub-agent {id} on {model_name}: {}; resumable — call task with \
                 agent=\"{kind}\", resume=\"{id}\" for up to {} follow-up turns]",
                run.stats, def.max_exchanges
            );
            static PARK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            self.park(
                &id,
                LiveTask {
                    agent,
                    session,
                    exchanges_left: def.max_exchanges,
                    def_name: def.name.clone(),
                    model_name,
                    seq: PARK_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                },
            );
            return ToolOutput::ok(format!("{header}\n{}", run.report));
        }
        ToolOutput::ok(format!(
            "[{kind} sub-agent on {model_name}: {}]\n{}",
            run.stats, run.report
        ))
    }
}

/// What one delegated turn produced, shared by fresh and resumed runs.
struct TaskRunOutcome {
    run_id: String,
    /// `"{tool_calls} tool calls, in X | out Y tokens"`.
    stats: String,
    report: String,
}

impl TaskTool {
    /// Continue a parked run: same session, same cache scope, pure append —
    /// a follow-up costs only the increment on a full prefix cache hit.
    #[allow(clippy::too_many_arguments)]
    async fn resume_run(
        &self,
        id: &str,
        def: &AgentDef,
        prompt: &str,
        summary: &str,
        call_id: &str,
        ctx: &ToolCtx,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let taken = self.live.lock().expect("live tasks lock").remove(id);
        let Some(mut task) = taken else {
            let mut parked: Vec<String> = {
                let live = self.live.lock().expect("live tasks lock");
                live.keys().cloned().collect()
            };
            parked.sort();
            return ToolOutput::err(format!(
                "no resumable task '{id}' — it may have expired, hit its follow-up limit, \
                 or be resuming concurrently. Resumable now: [{}]. Start a fresh task instead.",
                parked.join(", ")
            ));
        };
        if task.def_name != def.name {
            let owner = task.def_name.clone();
            self.live
                .lock()
                .expect("live tasks lock")
                .insert(id.to_string(), task);
            return ToolOutput::err(format!(
                "task '{id}' belongs to agent '{owner}'; call it with agent=\"{owner}\""
            ));
        }
        task.exchanges_left -= 1;
        let outcome = self
            .drive(
                &task.agent,
                &mut task.session,
                &def.name,
                &task.model_name,
                prompt,
                summary,
                call_id,
                ctx,
                cancel,
            )
            .await;
        match outcome {
            // A failed or cancelled follow-up drops the parked run: its
            // session state is no longer trustworthy.
            Err(out) => out,
            Ok(run) => {
                let left = task.exchanges_left;
                let model_name = task.model_name.clone();
                let note = if left > 0 {
                    self.park(id, task);
                    format!("{left} follow-up turns left")
                } else {
                    "follow-up limit reached, task closed".to_string()
                };
                ToolOutput::ok(format!(
                    "[{} sub-agent {id} resumed on {model_name}: {}; {note}]\n{}",
                    def.name, run.stats, run.report
                ))
            }
        }
    }

    /// Run one turn of a delegated agent — trace, live-event forwarding,
    /// report extraction — shared by fresh runs and resumed follow-ups.
    #[allow(clippy::too_many_arguments)]
    async fn drive(
        &self,
        agent: &Agent,
        session: &mut Session,
        kind: &str,
        model_name: &str,
        prompt: &str,
        summary: &str,
        call_id: &str,
        ctx: &ToolCtx,
        cancel: &CancellationToken,
    ) -> Result<TaskRunOutcome, ToolOutput> {
        // Trace: the run gets a stable per-session id, and (when the parent
        // session persists) its own JSONL ledger log for the trace viewer.
        // Nothing here enters the parent's provider ledger. A resumed run
        // swaps in the new trace's sink, so each trace records its own turn.
        let (run_id, trace) = ctx
            .task_traces
            .lock()
            .expect("task traces lock")
            .begin(call_id, kind, model_name, prompt, summary);
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
                model: model_name.to_string(),
                prompt: prompt.to_string(),
                summary: summary.to_string(),
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
                session,
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
                run: run_id.clone(),
                status,
                tool_calls,
                usage,
            });
        }

        if let Err(e) = result {
            return Err(ToolOutput::err(format!("sub-agent failed: {e}")));
        }
        if cancel.is_cancelled() {
            return Err(ToolOutput::err("sub-agent cancelled by user"));
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
        Ok(TaskRunOutcome {
            run_id,
            stats: format!(
                "{tool_calls} tool calls, in {} | out {} tokens",
                u.total_input(),
                u.output_tokens
            ),
            report,
        })
    }
}
