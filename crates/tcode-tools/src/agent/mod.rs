//! Sub-agents: the `agent` tool runs a nested agent loop with its own
//! fresh ledger and a restricted tool set. The parent context only pays
//! for the prompt and the final report — the sub-agent's exploration
//! tokens never enter the parent's window.

pub(crate) mod cohort;
pub(crate) mod defs;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use tcode_core::config::WatchdogConfig;
use tcode_core::{
    ActiveModel, Agent, AgentEvent, AgentModels, Approval, ApprovalDecision, Approver,
    CohortMemberRun, ContentBlock, DelegateEvent, DelegatedApprovalRequest, Entry, ModelCell,
    PermissionMode, PermissionRequest, PermissionRules, ProviderSafetyClassifier, SafetyClassifier,
    Session, TaskRunStatus, Tool, ToolCtx, ToolOutput,
};

use crate::agent::defs::{keeps_tool, AgentDef, AgentRegistry, QuestionPolicy};

/// Resolves a caller-supplied per-call model/effort override into a ready
/// `ActiveModel`. Injected by the composition root because it needs the profile
/// catalogue the tool itself does not hold. `model = None` keeps the base model
/// and changes only effort; an error string is surfaced to the model verbatim.
pub type ModelResolver =
    Arc<dyn Fn(Option<&str>, Option<&str>) -> Result<ActiveModel, String> + Send + Sync>;

/// Fallback for a run with no parent conversation to escalate to — a direct or
/// orphaned run. Declining is the only safe answer: there is nobody to ask.
struct NeverAsk;

#[async_trait]
impl Approver for NeverAsk {
    async fn ask(
        &self,
        _tool: &str,
        _summary: &str,
        _descriptor: &str,
        _is_edit: bool,
        _allows_project: bool,
        _input: &serde_json::Value,
    ) -> Approval {
        Approval::simple(
            ApprovalDecision::No,
            Some("sub-agents cannot prompt the user".into()),
        )
    }
}

/// Forward a delegated run's approvals and permitted questions to the parent
/// conversation's existing approver, so an inherited mode that asks reaches the
/// same human the parent would have asked. The receiver is installed only while
/// the parent executes this `agent` call, so a direct or orphaned run stays
/// safely non-interactive.
struct ParentUserBridge {
    requests: mpsc::UnboundedSender<DelegatedApprovalRequest>,
}

#[async_trait]
impl Approver for ParentUserBridge {
    async fn ask(
        &self,
        tool: &str,
        summary: &str,
        descriptor: &str,
        is_edit: bool,
        allows_project: bool,
        input: &Value,
    ) -> Approval {
        let (reply, response) = oneshot::channel();
        let request = DelegatedApprovalRequest {
            tool: tool.to_string(),
            summary: summary.to_string(),
            descriptor: descriptor.to_string(),
            is_edit,
            allows_project,
            input: input.clone(),
            reply,
        };
        if self.requests.send(request).is_err() {
            return Approval::simple(
                ApprovalDecision::No,
                Some("the parent conversation is no longer available for questions".into()),
            );
        }
        response.await.unwrap_or_else(|_| {
            Approval::simple(
                ApprovalDecision::No,
                Some("the parent conversation stopped before answering".into()),
            )
        })
    }
}

/// Parked resumable runs per task-tool instance; oldest is evicted beyond
/// this. Each parked run keeps its whole ledger in memory — a few MB at the
/// worst — so the cap is generous: eviction should lose a resumable run only
/// well after its follow-up budget has realistically lapsed.
const MAX_LIVE_TASKS: usize = 32;

/// Reports kept for `attach`, independent of parked sessions: a one-shot or
/// evicted run's report stays attachable. Text only, so the cap is cheap.
const MAX_STORED_REPORTS: usize = 32;

/// A finished run's final report, attachable to a later delegation by run id
/// so the caller hands it over verbatim instead of re-typing it.
struct StoredReport {
    agent: String,
    text: String,
    /// The conversation this report belongs to; see `LiveTask::scope`.
    scope: PathBuf,
    /// Insertion order for oldest-first eviction.
    seq: u64,
}

const ATTACH_FENCE_END: &str = "</attached-report>";

/// Follow-up turns granted to a run whose turn ended in an error or an
/// interrupt, when its definition is otherwise one-shot. One is enough to say
/// "continue from where you stopped" — and the alternative is discarding a
/// conversation the user has already paid for in full.
const SALVAGE_EXCHANGES: u32 = 1;

/// A finished delegated run kept alive for follow-up turns. Resuming appends
/// to the same session under the same cache scope, so a follow-up costs only
/// the increment on top of a full prefix cache hit.
struct LiveTask {
    agent: Agent,
    session: Session,
    exchanges_left: u32,
    def_name: String,
    model_name: String,
    /// The conversation that spawned this run, identified by its private
    /// scratch directory. Run ids restart at `t1` per conversation while this
    /// tool instance outlives them all (`/resume` and `/clear` swap the
    /// conversation in place), so without this an id from the previous
    /// conversation resolves to *its* session and leaks that context here.
    scope: PathBuf,
    /// Park order for oldest-first eviction.
    seq: u64,
}

/// Park order, shared by the success and failure paths so eviction sees one
/// sequence.
fn next_park_seq() -> u64 {
    static PARK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    PARK_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub struct AgentTool {
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
    auto_classifier_config: tcode_core::config::AutoClassifierConfig,
    auto_compact: bool,
    auto_compact_percent: u8,
    trusted_read_hosts: crate::TrustedReadHosts,
    /// The parent's already-compiled filter chain. Shared rather than
    /// re-loaded so a sub-agent's `shell` filters exactly like the parent's,
    /// and so `/cd` re-derives the project rules once for both.
    shell_filters: Arc<crate::ShellFilters>,
    /// Extra tools assembled by the composition root (currently MCP). They are
    /// injected into both selector validation and each delegated toolset.
    extension_tools: Vec<Arc<dyn Tool>>,
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
    /// Recent runs' final reports for `attach`, keyed by run id.
    reports: Arc<Mutex<HashMap<String, StoredReport>>>,
    /// Resolves a caller's `model`/`effort` override for one delegation. `None`
    /// leaves overrides unavailable — the run uses its pinned/inherited model.
    resolver: Option<ModelResolver>,
}

impl AgentTool {
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
            auto_classifier_config: Default::default(),
            auto_compact: true,
            auto_compact_percent: 85,
            trusted_read_hosts: crate::trusted_read_hosts(Vec::new()),
            shell_filters: Arc::new(crate::ShellFilters::disabled()),
            extension_tools: Vec::new(),
            description: agent_description(&defs, None),
            defs,
            allowed: None,
            depth: 0,
            live: Arc::default(),
            reports: Arc::default(),
            resolver: None,
        }
    }

    /// Share the live pin registry with the frontend that edits it.
    pub fn with_agent_models(mut self, pinned: AgentModels) -> Self {
        self.pinned = pinned;
        self
    }

    /// Enable per-call `model`/`effort` overrides on the `agent` tool. The
    /// resolver captures the composition root's profile catalogue; without it
    /// an override request is refused and the run keeps its pinned model.
    pub fn with_model_resolver(mut self, resolver: ModelResolver) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// Supply extensions such as already-connected MCP tools. The same set is
    /// used by every delegated agent, then filtered by its ToolPolicy.
    pub fn with_extension_tools(mut self, tools: Vec<Arc<dyn Tool>>) -> Self {
        self.extension_tools = tools;
        self
    }

    /// Dispatch to this registry instead of the builtin-only default.
    pub fn with_agent_defs(mut self, defs: Arc<AgentRegistry>) -> Self {
        self.description = agent_description(&defs, self.allowed.as_deref());
        self.defs = defs;
        self
    }

    /// Configure this instance for a top-level `--agent` run: the named
    /// definition's spawn list becomes the whole schema, already one level
    /// deep (the process itself is that agent). Call after `with_agent_defs`.
    pub fn scoped_to(mut self, def: &AgentDef) -> Self {
        let spawn = self.defs.spawn_list(def);
        self.description = agent_description(&self.defs, Some(&spawn));
        self.allowed = Some(spawn);
        self.depth = 1;
        self
    }

    /// Supply the parent session's global Auto Mode policy. Project-local
    /// config never reaches this field, even for delegated work.
    pub fn with_auto_policy(mut self, policy: String) -> Self {
        self.auto_policy = policy;
        self
    }

    /// Supply the parent session's user-global classifier timing policy to each
    /// isolated sub-agent. Project-local configuration never reaches it.
    pub fn with_auto_classifier_config(
        mut self,
        config: tcode_core::config::AutoClassifierConfig,
    ) -> Self {
        self.auto_classifier_config = config;
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

    /// Share the parent process's compiled filter chain with every isolated
    /// sub-agent, instead of each one re-reading and re-compiling the files.
    pub fn with_shell_filters(mut self, filters: Arc<crate::ShellFilters>) -> Self {
        self.shell_filters = filters;
        self
    }

    /// The pinned model for `kind`, else a snapshot of the parent's. String
    /// keyed so custom kinds resolve through `[agents.<name>]` for free.
    fn model_for(&self, kind: &str) -> ActiveModel {
        self.pinned
            .get(kind)
            .unwrap_or_else(|| self.model.snapshot())
    }

    /// Build the common delegated inventory before an agent-specific policy is
    /// applied. Validation uses this same inventory, so MCP selectors can only
    /// advertise tools a delegated agent will actually receive.
    fn base_tools(&self, cwd: &Path, model: ModelCell) -> Vec<Arc<dyn Tool>> {
        let mut tools = crate::builtin_tools_with_skills_and_web_fetch(
            crate::discover_skills(cwd),
            crate::WebFetchTool::new(self.trusted_read_hosts.clone()).with_summarizer(
                crate::FetchSummarizer::new(model.clone(), self.pinned.clone()),
            ),
            self.shell_filters.clone(),
        );
        tools.extend(self.extension_tools.iter().cloned());
        tools.push(Arc::new(crate::ViewImageTool::new(
            model,
            self.pinned.clone(),
        )));
        tools
    }

    /// Warn about, and remove, custom definitions whose tool policies cannot
    /// produce a usable delegated toolset in this environment.
    pub fn validate_definitions(&self, defs: &mut AgentRegistry, cwd: &Path) -> Vec<String> {
        defs.validate_for_tools(&self.base_tools(cwd, self.model.clone()))
    }

    /// The definition-derived toolset. Read-only agents get only
    /// side-effect-free tools; a definition that explicitly allows user
    /// questions receives `ask_user` through the parent bridge.
    #[cfg(test)]
    fn sub_tools(&self, def: &AgentDef, cwd: &Path, model: ModelCell) -> Vec<Arc<dyn Tool>> {
        self.sub_tools_with(def, cwd, model, &[])
    }

    /// Like `sub_tools`, but appends caller-injected per-run tools that a
    /// definition cannot describe — currently the cohort `channel` tool, whose
    /// instance holds a handle to one member's shared channel. Normal
    /// delegation passes an empty slice. Injected tools bypass the `readonly`
    /// ceiling and tool policy on purpose: they are granted by the cohort
    /// scheduler, not selectable from a definition, exactly like the spawn tool.
    fn sub_tools_with(
        &self,
        def: &AgentDef,
        cwd: &Path,
        model: ModelCell,
        extra: &[Arc<dyn Tool>],
    ) -> Vec<Arc<dyn Tool>> {
        let mut tools = self.base_tools(cwd, model.clone());
        // Submitting a plan for review carries a permission-mode transition on
        // the *parent* conversation, so it is structurally not a sub-agent's to
        // make — no delegated run gets one, whatever its definition says. The
        // discriminator is the request type rather than the tool's name: a tool
        // that asks for plan review is exactly the tool that cannot be
        // delegated. (The main agent keeps it in every mode on purpose: the
        // toolset is part of the cached prefix, so it stays put and
        // `PermissionRules::decide` self-heals the out-of-plan-mode call.)
        tools.retain(|tool| {
            !matches!(
                tool.permission(&json!({})),
                PermissionRequest::PlanReview { .. }
            )
        });
        tools.retain(|tool| keeps_tool(def, tool.as_ref()));
        if def.question_policy == QuestionPolicy::User && def.tool_policy.keeps("ask_user") {
            tools.push(Arc::new(crate::AskUserTool));
        }
        // Delegation is granted by the spawn policy alone — deliberately
        // outside the allowlist/read-only tiers — and bounded by depth, so
        // definition cycles terminate without graph analysis.
        let spawn = self.defs.spawn_list(def);
        if !spawn.is_empty() && self.depth < crate::agent::defs::MAX_TASK_DEPTH {
            tools.push(Arc::new(self.child(spawn, model)));
        }
        tools.extend(extra.iter().cloned());
        tools
    }

    /// A task tool for a sub-agent that may itself delegate: same registry
    /// and pins, spawn set restricted to the definition's resolved list, one
    /// level deeper. The child's parent handle is the sub-agent's own model
    /// cell, so an unpinned grandchild inherits its spawner, not the top level.
    fn child(&self, spawn: Vec<String>, model: ModelCell) -> AgentTool {
        AgentTool {
            model,
            pinned: self.pinned.clone(),
            watchdog: self.watchdog.clone(),
            output_budget: self.output_budget,
            auto_policy: self.auto_policy.clone(),
            auto_classifier_config: self.auto_classifier_config,
            auto_compact: self.auto_compact,
            auto_compact_percent: self.auto_compact_percent,
            trusted_read_hosts: self.trusted_read_hosts.clone(),
            shell_filters: self.shell_filters.clone(),
            extension_tools: self.extension_tools.clone(),
            description: agent_description(&self.defs, Some(&spawn)),
            defs: self.defs.clone(),
            allowed: Some(spawn),
            depth: self.depth + 1,
            // Each instance parks its own runs: the child tool lives inside
            // the spawning sub-agent's toolset, so a parked grandchild
            // survives exactly as long as its parker can still resume it.
            // Reports scope the same way: attach ids are the ids the same
            // caller saw in its own earlier result lines.
            live: Arc::default(),
            reports: Arc::default(),
            resolver: self.resolver.clone(),
        }
    }

    /// Park a finished run for follow-ups, evicting the oldest beyond cap.
    /// Runs from a conversation this tool has since been moved off (`/resume`,
    /// `/clear`) go first: they are unreachable by construction, so holding
    /// their sessions only spends memory and cap on ids that must never match.
    fn park(&self, id: &str, task: LiveTask) {
        let mut live = self.live.lock().expect("live tasks lock");
        live.retain(|_, parked| parked.scope == task.scope);
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

    /// Record a finished run's report for later `attach` use. A resumed run
    /// overwrites its earlier entry: the latest report is the attachable one.
    fn remember_report(&self, id: &str, agent: &str, text: &str, scope: &Path) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut reports = self.reports.lock().expect("attached reports lock");
        // Same scoping rule as `park`: an id only ever means something inside
        // the conversation that issued it.
        reports.retain(|_, report| report.scope == scope);
        if !reports.contains_key(id) && reports.len() >= MAX_STORED_REPORTS {
            if let Some(oldest) = reports
                .iter()
                .min_by_key(|(_, report)| report.seq)
                .map(|(id, _)| id.clone())
            {
                reports.remove(&oldest);
            }
        }
        reports.insert(
            id.to_string(),
            StoredReport {
                agent: agent.to_string(),
                text: text.to_string(),
                scope: scope.to_path_buf(),
                seq: SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            },
        );
    }

    /// Splice the named runs' reports verbatim ahead of the caller's prompt,
    /// so chaining agents costs the caller no output tokens and loses no
    /// fidelity to paraphrase. Report text is another agent's output — data,
    /// not instructions — so the fence closer is neutralized here at the
    /// emitter, like `fence_page`.
    fn attach_reports(
        &self,
        input: &Value,
        prompt: &str,
        scope: &Path,
    ) -> Result<String, ToolOutput> {
        let ids = match input.get("attach") {
            None | Some(Value::Null) => return Ok(prompt.to_string()),
            Some(Value::Array(ids)) => ids,
            Some(_) => {
                return Err(ToolOutput::err(
                    "`attach` must be an array of run-id strings",
                ))
            }
        };
        if ids.is_empty() {
            return Ok(prompt.to_string());
        }
        let reports = self.reports.lock().expect("attached reports lock");
        let mut out = String::new();
        for id in ids {
            let Some(id) = id.as_str() else {
                return Err(ToolOutput::err(
                    "`attach` must be an array of run-id strings",
                ));
            };
            let Some(report) = reports.get(id).filter(|report| report.scope == scope) else {
                let mut known: Vec<&str> = reports
                    .iter()
                    .filter(|(_, report)| report.scope == scope)
                    .map(|(id, _)| id.as_str())
                    .collect();
                known.sort();
                return Err(ToolOutput::err(format!(
                    "no attachable report for run '{id}'; attachable now: [{}]. Reports are \
                     kept for this conversation's {MAX_STORED_REPORTS} most recent runs — \
                     re-run the work or restate the needed findings in the prompt instead.",
                    known.join(", ")
                )));
            };
            let body = report
                .text
                .replace(ATTACH_FENCE_END, "<\\/attached-report>");
            out.push_str(&format!(
                "<attached-report run=\"{id}\" agent=\"{}\">\n{body}\n{ATTACH_FENCE_END}\n\n",
                report.agent
            ));
        }
        out.push_str(prompt);
        Ok(out)
    }

    /// Resolve a caller's `model`/`effort` override for a fresh delegation.
    /// `Ok(None)` when neither is given. A refused override (no resolver, or an
    /// unresolvable model) is a self-healing tool error, not a silent fallback:
    /// the model asked for a specific model and must know its request was not
    /// honored rather than believe a run happened on it.
    fn resolve_override(&self, input: &Value) -> Result<Option<ActiveModel>, ToolOutput> {
        let cleaned = |key: &str| {
            input[key]
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
        };
        let (model, effort) = (cleaned("model"), cleaned("effort"));
        if model.is_none() && effort.is_none() {
            return Ok(None);
        }
        let Some(resolver) = &self.resolver else {
            return Err(ToolOutput::err(
                "per-call model/effort override is unavailable in this session; \
                 this run uses the agent's pinned or inherited model. Ask the user to \
                 pin it with `/agents` or `/model` instead.",
            ));
        };
        resolver(model, effort).map(Some).map_err(|error| {
            ToolOutput::err(format!(
                "cannot run this agent on the requested model: {error}"
            ))
        })
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

/// The model-facing catalogue and the input schema both read the same registry,
/// so an agent definition can never advertise a name or description different
/// from the one AgentTool dispatches.
fn agent_description(defs: &AgentRegistry, allow: Option<&[String]>) -> String {
    format!(
        "Delegate a bounded subtask to an isolated agent with its own fresh context. \
         Give a complete, self-contained prompt and a very short task summary in the \
         same language as that prompt; the agent sees none of this conversation. Most agents cannot ask questions; agents with `questionPolicy: user` may use ask_user to ask the human through this parent conversation. To build on earlier runs' findings, pass their run ids in `attach` instead of re-typing their reports.\n\n{}",
        defs.catalogue(allow)
    )
}

/// Input schema for a given registry view; the enum is the spawnable set.
fn agent_schema(defs: &AgentRegistry, allow: Option<&[String]>) -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent": { "type": "string", "enum": defs.names_for(allow) },
            "prompt": { "type": "string" },
            "resume": {
                "type": "string",
                "description": "Task id of a resumable previous run (given in its result line). The same sub-agent continues with its context intact — use for follow-up questions or feedback, and to pick a run that failed or was interrupted back up from where it stopped instead of paying for its work again. Any run this conversation recorded stays resumable, including after a restart. `agent` must match the original kind."
            },
            "attach": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Run ids of earlier runs (from their result lines). Each run's final report is spliced verbatim into this agent's prompt ahead of your text — reference the attached reports there instead of re-typing them."
            },
            "summary": {
                "type": "string",
                "description": "A very short summary of the delegated objective. Use the same language as prompt; it appears in the live agent tree."
            },
            "model": {
                "type": "string",
                "description": "Optional: run this one delegation on a specific model id from the configured profiles instead of the agent's pinned or inherited model. Use only when the user names a model for the sub-agent (e.g. \"explore this with deepseek-v4-flash\"). An uncatalogued id is passed to the provider verbatim. Ignored on `resume`."
            },
            "effort": {
                "type": "string",
                "description": "Optional reasoning effort for this one delegation (e.g. low, medium, high), overriding the model's default. May be combined with `model` or used alone. Ignored on `resume`."
            }
        },
        "required": ["agent", "prompt"]
    })
}

/// Keep legacy/direct agent calls useful while the tool schema nudges models to
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

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &str {
        "agent"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        agent_schema(&self.defs, self.allowed.as_deref())
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
        let count = inputs.len();
        format!(
            "Delegate {count} {}",
            if count == 1 { "agent" } else { "agents" }
        )
    }

    fn gates_output_for(&self, input: &Value) -> bool {
        input["agent"]
            .as_str()
            .and_then(|kind| self.def_for(kind))
            .is_none_or(|def| def.gates_output)
    }

    fn permission(&self, _input: &Value) -> PermissionRequest {
        // Delegation only creates an isolated agent session. The delegated agent
        // inherits the parent's mode and rules, so each actual side-effecting
        // tool call reaches the same approval boundary as a direct call.
        PermissionRequest::None
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
        let prompt = match self.attach_reports(&input, prompt, &ctx.scratch_dir) {
            Ok(prompt) => prompt,
            Err(out) => return out,
        };
        if let Some(id) = input["resume"]
            .as_str()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            return self
                .resume_run(id, def, &prompt, &summary, call_id, ctx, cancel)
                .await;
        }
        // A per-call model/effort override applies to fresh runs only; a resumed
        // run keeps the model its parked session was built on.
        let model_override = match self.resolve_override(&input) {
            Ok(model_override) => model_override,
            Err(out) => return out,
        };
        let (agent, mut session, model_name) = self.build_run(def, ctx, model_override);

        let run = match self
            .drive(
                &agent,
                &mut session,
                kind,
                &model_name,
                &prompt,
                &summary,
                call_id,
                None,
                None,
                ctx,
                cancel,
            )
            .await
        {
            Ok(run) => run,
            // The run died mid-flight, but everything it spent is sitting in
            // this session. Park it — even for a one-shot definition — so the
            // caller resumes instead of paying for the same work twice.
            Err(fail) => {
                return self.park_failed(
                    &fail.run_id.clone(),
                    &fail,
                    agent,
                    session,
                    def,
                    &model_name,
                    def.max_exchanges.max(SALVAGE_EXCHANGES),
                    &ctx.scratch_dir,
                )
            }
        };
        self.remember_report(&run.run_id, kind, &run.report, &ctx.scratch_dir);

        if def.max_exchanges > 0 {
            let id = run.run_id.clone();
            let header = format!(
                "[{kind} sub-agent {id} on {model_name}: {}; resumable — call agent with \
                 agent=\"{kind}\", resume=\"{id}\" for up to {} follow-up turns]",
                run.stats, def.max_exchanges
            );
            self.park(
                &id,
                LiveTask {
                    agent,
                    session,
                    exchanges_left: def.max_exchanges,
                    def_name: def.name.clone(),
                    model_name,
                    scope: ctx.scratch_dir.clone(),
                    seq: next_park_seq(),
                },
            );
            return ToolOutput::ok(format!("{header}\n{}", run.report));
        }
        // The id is shown even for one-shot runs: their reports are still
        // attachable to a later delegation.
        ToolOutput::ok(format!(
            "[{kind} sub-agent {} on {model_name}: {}]\n{}",
            run.run_id, run.stats, run.report
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

/// A delegated turn that ended without a report. The caller still owns the
/// agent and its session, so this is a parking decision, not a discard.
struct TaskRunFailure {
    run_id: String,
    reason: String,
}

impl AgentTool {
    /// Everything one delegated run needs before its first turn: the agent, an
    /// empty session under its own cache scope, and the resolved model's name.
    /// A restored run is built here too, so a run recovered from a trace is
    /// configured exactly like a fresh one instead of drifting from it.
    fn build_run(
        &self,
        def: &AgentDef,
        ctx: &ToolCtx,
        model_override: Option<ActiveModel>,
    ) -> (Agent, Session, String) {
        self.build_run_with(def, ctx, model_override, &[])
    }

    /// Like `build_run`, but injects extra per-run tools into the agent's
    /// toolset (see `sub_tools_with`). Used by the cohort scheduler to hand each
    /// member its own `channel` tool instance.
    fn build_run_with(
        &self,
        def: &AgentDef,
        ctx: &ToolCtx,
        model_override: Option<ActiveModel>,
        extra: &[Arc<dyn Tool>],
    ) -> (Agent, Session, String) {
        let kind = &def.name;
        let model = model_override.unwrap_or_else(|| self.model_for(kind));
        let model_name = model.provider.model().to_string();
        let model = ModelCell::new(model);
        let safety_classifier: Arc<dyn SafetyClassifier> = Arc::new(
            ProviderSafetyClassifier::new(model.clone(), self.pinned.clone())
                .with_config(self.auto_classifier_config),
        );
        let agent = Agent {
            model: model.clone(),
            // A sub-agent has no input box, so it never suggests; it still
            // carries the pins so its own classifier resolves the same way.
            models: self.pinned.clone(),
            tools: self.sub_tools_with(def, &ctx.cwd, model.clone(), extra),
            system: def.system.clone(),
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
        // Delegated work is still the caller's work: it runs under the mode and
        // rules the user chose for this conversation, not a stance of its own.
        // The definition's `readonly` ceiling is the orthogonal knob — it has
        // already removed mutating tools from `sub_tools`, so a permissive
        // inherited mode cannot hand an explore agent something to mutate with.
        let inherited = ctx.delegated_permissions();
        let (mode, rules) = inherited
            .map(|permissions| (permissions.mode, permissions.rules))
            .unwrap_or_else(|| (PermissionMode::Auto, PermissionRules::default()));
        let session = Session::new(
            ToolCtx::with_scratch_dir(ctx.cwd.clone(), self.output_budget, ctx.scratch_dir.clone())
                .with_model(model),
            mode,
            rules,
        )
        .with_cache_scope(format!("agent-{kind}-{run}"));
        (agent, session, model_name)
    }

    /// Rebuild a resumable run from its persisted trace, so a run outlives the
    /// process that spawned it: after a restart or a `/resume`, `live` is empty
    /// but `tasks/<session>/tN.jsonl` still holds the run's whole ledger. What
    /// this buys is not the provider cache (that prefix is long gone) but the
    /// work itself — every tool call and every conclusion the run already paid
    /// for. `None` when this conversation records no such run, when the id is
    /// not one it could have issued, or when the trace names an agent this
    /// instance may not spawn.
    fn restore_run(&self, id: &str, ctx: &ToolCtx) -> Option<LiveTask> {
        let (meta, ledger) = ctx
            .task_traces
            .lock()
            .expect("task traces lock")
            .restore(id)?;
        // The trace's own kind decides what is rebuilt; the caller's `agent`
        // argument is then checked against it like any other parked run, so a
        // mismatch is reported rather than silently honored.
        let def = self.def_for(&meta.kind)?;
        let (agent, mut session, model_name) = self.build_run(def, ctx, None);
        session.ledger = ledger;
        // A trace can stop anywhere — its process may have been killed
        // mid-batch, which no live path can produce.
        session.ledger.close_dangling_tool_calls(
            "No result: the sub-agent's process exited while this call was in flight. Whether \
             it took effect is unknown — verify before assuming either way.",
        );
        if session.ledger.is_empty() {
            return None;
        }
        Some(LiveTask {
            agent,
            session,
            exchanges_left: def.max_exchanges.max(SALVAGE_EXCHANGES),
            def_name: def.name.clone(),
            model_name,
            scope: ctx.scratch_dir.clone(),
            seq: next_park_seq(),
        })
    }

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
        // An id is only ever valid inside the conversation that issued it: a
        // parked run belonging to a conversation this tool has been moved off
        // is not "the same id", it is a different agent's session.
        let taken = {
            let mut live = self.live.lock().expect("live tasks lock");
            live.get(id)
                .is_some_and(|task| task.scope == ctx.scratch_dir)
                .then(|| live.remove(id))
                .flatten()
        };
        // Nothing parked in memory: this conversation may still have the run on
        // disk, from before a restart or a `/resume`.
        let restored = taken.is_none();
        let Some(mut task) = taken.or_else(|| self.restore_run(id, ctx)) else {
            let mut parked: Vec<String> = {
                let live = self.live.lock().expect("live tasks lock");
                live.iter()
                    .filter(|(_, task)| task.scope == ctx.scratch_dir)
                    .map(|(id, _)| id.clone())
                    .collect()
            };
            parked.sort();
            return ToolOutput::err(format!(
                "no resumable agent run '{id}' — it may have expired, hit its follow-up limit, \
                 be resuming concurrently, or belong to a conversation that has since been \
                 replaced (/clear and a new conversation keep no run alive). Resumable now: \
                 [{}], plus any run recorded earlier in this same conversation. Start a fresh \
                 agent run instead.",
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
                "agent run '{id}' belongs to agent '{owner}'; call agent with agent=\"{owner}\""
            ));
        }
        task.exchanges_left -= 1;
        // A follow-up is a fresh delegation of the caller's current stance, not
        // a replay of the one in force when this run was first parked: the user
        // may have changed mode or rules in between.
        if let Some(permissions) = ctx.delegated_permissions() {
            task.session.mode = permissions.mode;
            task.session.rules = permissions.rules;
        }
        let outcome = self
            .drive(
                &task.agent,
                &mut task.session,
                &def.name,
                &task.model_name,
                prompt,
                summary,
                call_id,
                Some(id),
                None,
                ctx,
                cancel,
            )
            .await;
        match outcome {
            // The follow-up failed, not the run: one API blip must not destroy
            // a conversation this caller has been building across turns. The
            // failed turn is re-parked under its original id, and a run that
            // had spent its budget still gets a salvage exchange to answer with.
            Err(fail) => {
                let LiveTask {
                    agent,
                    session,
                    exchanges_left,
                    model_name,
                    ..
                } = task;
                self.park_failed(
                    id,
                    &fail,
                    agent,
                    session,
                    def,
                    &model_name,
                    exchanges_left.max(SALVAGE_EXCHANGES),
                    &ctx.scratch_dir,
                )
            }
            Ok(run) => {
                self.remember_report(id, &def.name, &run.report, &ctx.scratch_dir);
                let left = task.exchanges_left;
                let model_name = task.model_name.clone();
                let note = if left > 0 {
                    self.park(id, task);
                    format!("{left} follow-up turns left")
                } else {
                    "follow-up limit reached, agent run closed".to_string()
                };
                // Say when the run came back from disk: it means the model
                // reading this may not be the one that produced the history,
                // and that no provider cache backed this turn.
                let how = if restored { "restored" } else { "resumed" };
                ToolOutput::ok(format!(
                    "[{} sub-agent {id} {how} on {model_name}: {}; {note}]\n{}",
                    def.name, run.stats, run.report
                ))
            }
        }
    }

    /// Keep a run whose turn ended in an error or an interrupt, and tell the
    /// caller how to pick it back up. Discarding here is the expensive default:
    /// the session holds every tool result and every thought the run already
    /// paid for, and its ledger is still a legal conversation, so the only
    /// thing a fresh run would buy is the same tokens a second time.
    #[allow(clippy::too_many_arguments)]
    fn park_failed(
        &self,
        park_id: &str,
        fail: &TaskRunFailure,
        agent: Agent,
        session: Session,
        def: &AgentDef,
        model_name: &str,
        exchanges_left: u32,
        scope: &Path,
    ) -> ToolOutput {
        self.park(
            park_id,
            LiveTask {
                agent,
                session,
                exchanges_left,
                def_name: def.name.clone(),
                model_name: model_name.to_string(),
                scope: scope.to_path_buf(),
                seq: next_park_seq(),
            },
        );
        ToolOutput::err(format!(
            "{}\n[run {park_id} kept alive with everything it did before the failure — call \
             agent with agent=\"{}\", resume=\"{park_id}\" to continue from there; a fresh run \
             repeats all of it. {exchanges_left} follow-up turns left]",
            fail.reason, def.name
        ))
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
        // The parked run this turn continues, so the trace chain can rebuild
        // the whole conversation from disk later.
        resume_of: Option<&str>,
        // Cohort identity for UI-only membership cards. Ordinary delegation
        // stays `None` and keeps its existing generic task-card behavior.
        cohort_member: Option<CohortMemberRun>,
        ctx: &ToolCtx,
        cancel: &CancellationToken,
    ) -> Result<TaskRunOutcome, TaskRunFailure> {
        // Trace: the run gets a stable per-session id, and (when the parent
        // session persists) its own JSONL ledger log for the trace viewer.
        // Nothing here enters the parent's provider ledger. A resumed run
        // swaps in the new trace's sink, so each trace records its own turn.
        let (run_id, trace) = ctx
            .task_traces
            .lock()
            .expect("task traces lock")
            .begin(call_id, kind, model_name, prompt, summary, resume_of);
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
                cohort_member,
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

        // Permission approvals always reach the parent. `questionPolicy` gates
        // the `ask_user` *tool* — whether this agent may ask open-ended
        // questions — which is a different thing from whether the human gets to
        // decide about an action it is about to take. Without this, inheriting
        // a mode that asks would silently become a mode that refuses.
        let bridge = ctx
            .delegated_approver()
            .map(|requests| ParentUserBridge { requests });
        let never_ask = NeverAsk;
        let approver: &dyn Approver = bridge
            .as_ref()
            .map(|bridge| bridge as &dyn Approver)
            .unwrap_or(&never_ask);
        let result = agent
            .user_turn(
                session,
                vec![ContentBlock::Text {
                    text: prompt.to_string(),
                }],
                &tx,
                approver,
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

        // A failed or interrupted turn leaves the ledger a legal conversation:
        // a request that never landed appends nothing, a mid-stream failure
        // appends only `Entry::IncompleteAssistant`, and an interrupt commits
        // its own note plus results for every started call. So the session is
        // handed back to the caller to park, not dropped.
        if let Err(e) = result {
            return Err(TaskRunFailure {
                run_id,
                reason: format!("sub-agent failed: {e}"),
            });
        }
        if cancel.is_cancelled() {
            return Err(TaskRunFailure {
                run_id,
                reason: "sub-agent cancelled by user".to_string(),
            });
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

#[cfg(test)]
mod tests {
    use super::{agent_description, agent_schema};
    use crate::agent::defs::AgentRegistry;
    use serde_json::json;

    fn registry_with(defs: &[(&str, &str)]) -> AgentRegistry {
        // Isolate the home root `discover` scans so real `~/.tcode/agents`
        // installs cannot leak into exact schema/enum assertions.
        tcode_core::home::testing::temp_home();
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

    use tcode_core::Tool;

    #[test]
    fn agent_definition_can_bypass_only_its_parent_report_gate() {
        let registry = std::sync::Arc::new(registry_with(&[("verbose", "gatesOutput: false")]));
        let task = super::AgentTool::new(
            null_model(),
            Default::default(),
            2_000,
            std::env::temp_dir(),
        )
        .with_agent_defs(registry);

        assert!(!task.gates_output_for(&json!({"agent": "verbose"})));
        assert!(!task.gates_output_for(&json!({"agent": "explore"})));
        assert!(!task.gates_output_for(&json!({"agent": "plan"})));
        assert!(task.gates_output_for(&json!({"agent": "general"})));
        assert!(task.gates_output_for(&json!({"agent": "missing"})));
    }

    #[test]
    fn delegating_any_builtin_agent_needs_no_approval() {
        let task = super::AgentTool::new(
            null_model(),
            Default::default(),
            2_000,
            std::env::temp_dir(),
        );

        for kind in ["explore", "plan", "general"] {
            assert!(matches!(
                task.permission(&json!({"agent": kind, "prompt": "do work"})),
                tcode_core::PermissionRequest::None
            ));
        }
    }

    #[test]
    fn schema_enum_and_description_track_custom_agents() {
        let registry = registry_with(&[("investor", "agents: quant-dev"), ("quant-dev", "")]);
        let schema = agent_schema(&registry, None);
        let kinds: Vec<&str> = schema["properties"]["agent"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        let builtins = AgentRegistry::builtin();
        let mut expected: Vec<&str> = builtins.names_for(None);
        expected.extend(["investor", "quant-dev"]);
        assert_eq!(kinds, expected);
        let description = agent_description(&registry, None);
        assert!(description.contains("investor: investor agent"));
    }

    #[test]
    fn a_spawn_allowlist_restricts_schema_and_description() {
        let registry = registry_with(&[("investor", "agents: quant-dev"), ("quant-dev", "")]);
        let allow = vec!["quant-dev".to_string()];
        let schema = agent_schema(&registry, Some(&allow));
        assert_eq!(schema["properties"]["agent"]["enum"], json!(["quant-dev"]));
        let description = agent_description(&registry, Some(&allow));
        assert!(description.contains("quant-dev:"));
        assert!(!description.contains("investor:"));
        assert!(!description.contains("agent='explore'"));
    }

    #[test]
    fn without_custom_agents_the_description_lists_embedded_definitions() {
        let registry = AgentRegistry::builtin();
        let description = agent_description(&registry, None);
        assert!(description.contains("Available agents:"));
        for def in registry.visible_defs(None) {
            assert!(description.contains(&format!(
                "{}{}:",
                def.name,
                if def.read_only { " [read-only]" } else { "" }
            )));
        }
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

    /// A provider whose `model()` echoes a chosen id, so a resolver test can
    /// prove the override — not the pin — reached `build_run`.
    struct NamedProvider(&'static str);

    #[async_trait::async_trait]
    impl tcode_core::Provider for NamedProvider {
        fn name(&self) -> &str {
            "named"
        }
        fn model(&self) -> &str {
            self.0
        }
        fn cache_strategy(&self) -> tcode_core::CacheStrategy {
            tcode_core::CacheStrategy::ImplicitPrefix
        }
        async fn stream(
            &self,
            _req: tcode_core::Request,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<tcode_core::EventStream, tcode_core::ProviderError> {
            unreachable!("resolver tests never stream")
        }
    }

    fn named_model(id: &'static str, effort: Option<&str>) -> tcode_core::ActiveModel {
        tcode_core::ActiveModel {
            provider: std::sync::Arc::new(NamedProvider(id)),
            max_tokens: 1024,
            context_window: 100_000,
            effort: effort.map(str::to_string),
        }
    }

    #[test]
    fn resolve_override_is_none_without_model_or_effort() {
        let task = super::AgentTool::new(
            null_model(),
            Default::default(),
            2_000,
            std::env::temp_dir(),
        );
        assert!(task
            .resolve_override(&json!({"agent": "explore", "prompt": "x"}))
            .unwrap()
            .is_none());
    }

    #[test]
    fn resolve_override_refuses_when_no_resolver_is_installed() {
        let task = super::AgentTool::new(
            null_model(),
            Default::default(),
            2_000,
            std::env::temp_dir(),
        );
        let err = task
            .resolve_override(&json!({"model": "deepseek-v4-flash"}))
            .err()
            .expect("override refused without a resolver");
        assert!(err.is_error);
        assert!(err.content.contains("unavailable"), "{}", err.content);
    }

    #[test]
    fn resolve_override_passes_model_and_effort_to_the_resolver() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured = seen.clone();
        let resolver: super::ModelResolver = std::sync::Arc::new(move |model, effort| {
            *captured.lock().unwrap() =
                Some((model.map(str::to_string), effort.map(str::to_string)));
            Ok(named_model("resolved-model", effort))
        });
        let task = super::AgentTool::new(
            null_model(),
            Default::default(),
            2_000,
            std::env::temp_dir(),
        )
        .with_model_resolver(resolver);
        let active = task
            .resolve_override(&json!({"model": "deepseek-v4-flash", "effort": "high"}))
            .unwrap()
            .expect("override resolved to a model");
        assert_eq!(active.provider.model(), "resolved-model");
        assert_eq!(active.effort.as_deref(), Some("high"));
        assert_eq!(
            *seen.lock().unwrap(),
            Some((
                Some("deepseek-v4-flash".to_string()),
                Some("high".to_string())
            ))
        );
    }

    #[test]
    fn resolver_failure_becomes_a_self_healing_tool_error() {
        let resolver: super::ModelResolver =
            std::sync::Arc::new(|_, _| Err("no profile offers 'nope'".to_string()));
        let task = super::AgentTool::new(
            null_model(),
            Default::default(),
            2_000,
            std::env::temp_dir(),
        )
        .with_model_resolver(resolver);
        let err = task
            .resolve_override(&json!({"model": "nope"}))
            .err()
            .expect("resolver failure surfaces as a tool error");
        assert!(err.is_error);
        assert!(
            err.content.contains("no profile offers 'nope'"),
            "{}",
            err.content
        );
    }

    #[test]
    fn agent_schema_advertises_the_model_and_effort_overrides() {
        let schema = agent_schema(&AgentRegistry::builtin(), None);
        let props = &schema["properties"];
        assert!(props["model"].is_object());
        assert!(props["effort"].is_object());
        // The overrides are optional; only agent and prompt are required.
        assert_eq!(schema["required"], json!(["agent", "prompt"]));
    }

    struct McpStub;

    #[async_trait::async_trait]
    impl tcode_core::Tool for McpStub {
        fn name(&self) -> &str {
            "mcp__github__issue"
        }

        fn description(&self) -> &str {
            "test MCP tool"
        }

        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }

        fn permission(&self, _input: &serde_json::Value) -> tcode_core::PermissionRequest {
            tcode_core::PermissionRequest::None
        }

        async fn run(
            &self,
            _input: serde_json::Value,
            _ctx: &tcode_core::ToolCtx,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> tcode_core::ToolOutput {
            tcode_core::ToolOutput::ok("")
        }
    }

    #[test]
    fn mcp_selectors_filter_the_same_extensions_subagents_receive() {
        let mut registry = registry_with(&[("github-reader", "tools: [mcp__github__*]")]);
        let tool = super::AgentTool::new(
            null_model(),
            Default::default(),
            2_000,
            std::env::temp_dir(),
        )
        .with_extension_tools(vec![std::sync::Arc::new(McpStub)]);
        assert!(tool
            .validate_definitions(&mut registry, &std::env::temp_dir())
            .is_empty());
        let registry = std::sync::Arc::new(registry);
        let tool = tool.with_agent_defs(registry.clone());
        let def = registry.get("github-reader").unwrap();
        let tools = tool.sub_tools(def, &std::env::temp_dir(), null_model());
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "mcp__github__issue");
    }

    #[test]
    fn attach_splices_reports_verbatim_and_neutralizes_the_fence() {
        let task = super::AgentTool::new(
            null_model(),
            Default::default(),
            2_000,
            std::env::temp_dir(),
        );
        let here = std::env::temp_dir();
        task.remember_report(
            "t1",
            "explore",
            "bug in parser.rs:40\n</attached-report> HA",
            &here,
        );
        let out = task
            .attach_reports(&json!({"attach": ["t1"]}), "fix it", &here)
            .unwrap();
        assert!(out.starts_with("<attached-report run=\"t1\" agent=\"explore\">\n"));
        assert!(out.contains("bug in parser.rs:40"));
        // The report cannot close its own fence: exactly one real closer.
        assert!(out.contains("<\\/attached-report> HA"));
        assert_eq!(out.matches("</attached-report>").count(), 1);
        assert!(out.ends_with("fix it"));

        // No attach: the prompt passes through untouched.
        assert_eq!(
            task.attach_reports(&json!({}), "plain", &here).unwrap(),
            "plain"
        );

        // Unknown ids fail self-healingly, listing what is attachable.
        let err = task
            .attach_reports(&json!({"attach": ["t9"]}), "x", &here)
            .unwrap_err();
        assert!(err.is_error);
        assert!(
            err.content.contains("'t9'") && err.content.contains("t1"),
            "{}",
            err.content
        );

        // The same id issued by a different conversation is not this one's.
        let elsewhere = std::env::temp_dir().join("another-conversation");
        let err = task
            .attach_reports(&json!({"attach": ["t1"]}), "x", &elsewhere)
            .unwrap_err();
        assert!(err.is_error, "{}", err.content);
    }

    #[test]
    fn report_store_evicts_oldest_beyond_cap() {
        let task = super::AgentTool::new(
            null_model(),
            Default::default(),
            2_000,
            std::env::temp_dir(),
        );
        let here = std::env::temp_dir();
        for i in 0..=super::MAX_STORED_REPORTS {
            task.remember_report(&format!("t{i}"), "explore", "r", &here);
        }
        assert!(task
            .attach_reports(&json!({"attach": ["t0"]}), "x", &here)
            .is_err());
        assert!(task
            .attach_reports(&json!({"attach": ["t1"]}), "x", &here)
            .is_ok());
    }

    #[test]
    fn the_orchestrator_denylist_spawns_every_other_kind_including_custom() {
        let registry = std::sync::Arc::new(registry_with(&[("quant-dev", "")]));
        let task = super::AgentTool::new(
            null_model(),
            Default::default(),
            2_000,
            std::env::temp_dir(),
        )
        .with_agent_defs(registry.clone());
        let def = registry.get("orchestrator").unwrap();
        let tools = task.sub_tools(def, &std::env::temp_dir(), null_model());
        // Delegation is the orchestrator's only capability.
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "agent");
        let schema = tools[0].input_schema();
        let kinds: Vec<&str> = schema["properties"]["agent"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        for kind in ["explore", "plan", "general", "quant-dev"] {
            assert!(kinds.contains(&kind), "{kinds:?} misses {kind}");
        }
        assert!(!kinds.contains(&"orchestrator"), "{kinds:?}");
    }

    #[test]
    fn nesting_is_granted_by_the_agents_field_and_bounded_by_depth() {
        let registry = std::sync::Arc::new(registry_with(&[
            ("investor", "agents: quant-dev"),
            ("quant-dev", ""),
        ]));
        let task = super::AgentTool::new(
            null_model(),
            Default::default(),
            2_000,
            std::env::temp_dir(),
        )
        .with_agent_defs(registry.clone());
        let investor = registry.get("investor").unwrap();
        let leaf = registry.get("quant-dev").unwrap();
        let tmp = std::env::temp_dir();

        // A spawner gets an agent tool whose schema is exactly its spawn list.
        let tools = task.sub_tools(investor, &tmp, null_model());
        let child = tools
            .iter()
            .find(|tool| tool.name() == "agent")
            .expect("spawner receives an agent tool");
        assert_eq!(
            child.input_schema()["properties"]["agent"]["enum"],
            json!(["quant-dev"])
        );

        // A definition without `agents` is a leaf.
        assert!(!task
            .sub_tools(leaf, &tmp, null_model())
            .iter()
            .any(|tool| tool.name() == "agent"));

        // Depth bound: instances at MAX_TASK_DEPTH stop handing the tool out.
        let spawn = registry.spawn_list(investor);
        let d2 = task
            .child(spawn.clone(), null_model())
            .child(spawn.clone(), null_model());
        let d3 = d2.child(spawn, null_model());
        assert!(d2
            .sub_tools(investor, &tmp, null_model())
            .iter()
            .any(|tool| tool.name() == "agent"));
        assert!(!d3
            .sub_tools(investor, &tmp, null_model())
            .iter()
            .any(|tool| tool.name() == "agent"));
    }
}
