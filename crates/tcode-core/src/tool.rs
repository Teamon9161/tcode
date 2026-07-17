use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::background::BackgroundTasks;
use crate::blobs::BlobStore;
use crate::freshness::FreshnessTracker;
use crate::provider::{AgentModels, ModelCell};
use crate::task_trace::{TaskRunStatus, TaskTraces};
use crate::types::Usage;

use crate::auto_mode::AutoSafety;
pub use crate::permission::{Approval, ApprovalDecision, Approver};

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    /// Image blocks (each a `ContentBlock::Image`) the tool wants inlined into
    /// its result, e.g. `read` on a screenshot. Providers that can't carry an
    /// image in a tool result degrade to the text alone.
    pub images: Vec<crate::types::ContentBlock>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            images: Vec::new(),
        }
    }
    /// Tool errors are written FOR the model: always include what it
    /// needs to fix the call without spending another turn on diagnosis.
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            images: Vec::new(),
        }
    }
    /// Attach image blocks to a successful output.
    pub fn with_images(mut self, images: Vec<crate::types::ContentBlock>) -> Self {
        self.images = images;
        self
    }
}

/// How the harness may batch several concurrent calls the model emits to the
/// same tool in one turn. This is the tool's own scheduling contract; the
/// agent loop reads it instead of matching on tool names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchPolicy {
    /// Approve and run one call at a time (default). No batching.
    Isolated,
    /// Read-only, no side effects — run every call concurrently, no approval.
    ParallelReadOnly,
    /// Mutates one file (see `touches`). Calls to the same normalized path run
    /// in model order; calls targeting other paths may run concurrently. All
    /// approvals, hooks and snapshots complete before the first write.
    ParallelPerFile,
    /// Side effects must be visible to later calls — approve the whole batch
    /// once, then run the calls sequentially.
    SequentialBatch,
}

/// What a specific invocation needs in terms of user consent.
#[derive(Debug, Clone)]
pub enum PermissionRequest {
    /// Read-only; never prompts.
    None,
    Ask {
        /// Canonical descriptor used for new session/project allow rules.
        descriptor: String,
        /// Legacy or interpreter-specific descriptors which keep old rules
        /// working and remain valid for explicit ask/deny constraints.
        aliases: Vec<String>,
        /// One line shown in the approval prompt.
        summary: String,
        /// File mutation — auto-allowed in accept-edits mode.
        is_edit: bool,
    },
    /// A model-facing question that must always reach the human, including in
    /// unsafe mode. It is not an authorization request and can never become a
    /// persistent allow rule.
    UserInput { descriptor: String, summary: String },
    /// A completed plan submitted from plan mode for review. The human either
    /// approves it — carrying a permission-mode transition back in the
    /// `Approval` — or returns feedback to keep planning. Only reachable in
    /// plan mode (see `PermissionRules::decide`); it never becomes an allow
    /// rule. The plan body travels in the tool input, not here.
    PlanReview { title: String },
}

impl PermissionRequest {
    /// One-line summary shown in an approval prompt, if this request prompts.
    pub fn summary(&self) -> &str {
        match self {
            PermissionRequest::Ask { summary, .. }
            | PermissionRequest::UserInput { summary, .. } => summary,
            PermissionRequest::PlanReview { title } => title,
            PermissionRequest::None => "",
        }
    }

    /// Rule-matching descriptor. Non-authorization requests (questions, plan
    /// review) use a stable name and can never become a persistent allow rule.
    pub fn descriptor(&self) -> &str {
        match self {
            PermissionRequest::Ask { descriptor, .. }
            | PermissionRequest::UserInput { descriptor, .. } => descriptor,
            PermissionRequest::PlanReview { .. } => "exit_plan",
            PermissionRequest::None => "",
        }
    }

    /// All descriptors that must be considered for deny/ask rules. Allow rules
    /// may match either too, preserving existing interpreter-specific rules.
    pub fn rule_descriptors(&self) -> Vec<&str> {
        match self {
            PermissionRequest::Ask {
                descriptor,
                aliases,
                ..
            } => std::iter::once(descriptor.as_str())
                .chain(aliases.iter().map(String::as_str))
                .collect(),
            _ => vec![self.descriptor()],
        }
    }

    /// Human-facing name for the permission being granted: the same string
    /// that would be persisted as an allow rule, so the option label always
    /// matches what approving it actually saves.
    pub fn approval_label(&self) -> String {
        self.descriptor().to_string()
    }

    /// Whether approving this may persist an allow rule. Questions and plan
    /// review never can.
    pub fn allows_rule(&self) -> bool {
        matches!(self, PermissionRequest::Ask { .. })
    }
}

#[cfg(test)]
mod permission_request_tests {
    use super::PermissionRequest;

    #[test]
    fn approval_label_shows_the_actual_command_not_a_fixed_interpreter_name() {
        let request = PermissionRequest::Ask {
            descriptor: "run(cargo build --release)".into(),
            aliases: vec!["shell(cargo build --release)".into()],
            summary: "run: cargo build --release".into(),
            is_edit: false,
        };
        // Regression: this used to collapse to a constant "Run (PowerShell)"
        // for every shell call regardless of the command being approved.
        assert_eq!(request.approval_label(), "run(cargo build --release)");
    }
}

/// What delegated work (a `task` sub-agent, a `view_image` request) reports
/// back to the agent loop while it runs. The loop translates each variant
/// into the matching `AgentEvent`, so every frontend sees delegated progress
/// through its ordinary event stream.
#[derive(Debug, Clone)]
pub enum DelegateEvent {
    /// Billable usage spent by a delegated helper with no run identity of its
    /// own (e.g. `view_image`). Surfaces as `AgentEvent::DelegatedUsage`.
    Usage(Usage),
    /// A `task` sub-agent run began. `parent_call` is the tool_use id of the
    /// spawning `task` call, tying the run to its ledger entry.
    TaskStarted {
        run: String,
        parent_call: String,
        kind: String,
        model: String,
        prompt: String,
        /// A one-line parent-authored description for task lists.
        summary: String,
    },
    /// One event from inside a running sub-agent, tagged with its run id.
    TaskEvent {
        run: String,
        event: Box<crate::agent::AgentEvent>,
    },
    TaskFinished {
        run: String,
        status: TaskRunStatus,
        tool_calls: usize,
        usage: Usage,
    },
}

/// Shared context handed to every tool invocation.
pub struct ToolCtx {
    pub cwd: PathBuf,
    /// Session-private temporary workspace. This is the only scratch path
    /// exposed to the model, and the boundary Auto Mode may fast-allow.
    pub scratch_dir: PathBuf,
    pub freshness: Mutex<FreshnessTracker>,
    pub blobs: Mutex<BlobStore>,
    pub background: Mutex<BackgroundTasks>,
    pub memory: Mutex<crate::memory::MemoryManager>,
    /// The model active for this invocation. Tools may use its capabilities
    /// without owning model selection or provider construction.
    pub model: Option<ModelCell>,
    /// Pinned auxiliary-role models (`[agents.<kind>]` / `/agents`). Tools
    /// that delegate work to a role (e.g. `web_fetch` summarizing with
    /// `fetch`) look their model up here; an unpinned role is simply absent.
    pub agent_models: AgentModels,
    /// Per-session task-run id allocator and trace persistence. Runs persist
    /// only after `bind_task_trace_root`; ids are issued regardless.
    pub task_traces: Mutex<TaskTraces>,
    /// Budget reused when a resume/import binds this context to another
    /// session's scratch root.
    output_budget_tokens: usize,
    delegate: Mutex<Option<mpsc::UnboundedSender<DelegateEvent>>>,
}

static EPHEMERAL_SESSION: AtomicU64 = AtomicU64::new(0);

fn ephemeral_session_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let sequence = EPHEMERAL_SESSION.fetch_add(1, Ordering::Relaxed);
    format!("ephemeral-{}-{millis:x}-{sequence:x}", std::process::id())
}

impl ToolCtx {
    pub fn new(cwd: PathBuf, output_budget_tokens: usize) -> Self {
        let scratch_dir = crate::store::session_scratchpad_dir(&cwd, &ephemeral_session_id());
        Self::with_scratch_dir(cwd, output_budget_tokens, scratch_dir)
    }

    /// Bind this context to a persistent session's private temporary workspace.
    /// Call this before any tool work begins (or immediately after `/resume`).
    pub fn with_scratch_dir(
        cwd: PathBuf,
        output_budget_tokens: usize,
        scratch_dir: PathBuf,
    ) -> Self {
        let memory = crate::memory::MemoryManager::new(&cwd);
        // Sweep every session's stale files from the shared parent; this run's
        // blobs and background logs then stay inside its own child directory.
        crate::store::sweep_scratchpad(&crate::store::scratchpad_dir(&cwd));
        let tool_output = scratch_dir.join("tool-output");
        Self {
            freshness: Mutex::new(FreshnessTracker::default()),
            blobs: Mutex::new(BlobStore::new(tool_output.clone(), output_budget_tokens)),
            background: Mutex::new(BackgroundTasks::new(tool_output)),
            memory: Mutex::new(memory),
            model: None,
            agent_models: AgentModels::default(),
            task_traces: Mutex::new(TaskTraces::default()),
            output_budget_tokens,
            delegate: Mutex::new(None),
            cwd,
            scratch_dir,
        }
    }

    /// Move a live context onto a resumed/imported session's scratch root.
    /// No task may be running when this is called: background logs and output
    /// blobs deliberately belong to the conversation that created them.
    pub fn rebind_scratch_dir(&mut self, scratch_dir: PathBuf) {
        self.scratch_dir = scratch_dir.clone();
        let tool_output = scratch_dir.join("tool-output");
        self.blobs = Mutex::new(BlobStore::new(
            tool_output.clone(),
            self.output_budget_tokens,
        ));
        self.background = Mutex::new(BackgroundTasks::new(tool_output));
    }

    pub fn with_model(mut self, model: ModelCell) -> Self {
        self.model = Some(model);
        self
    }

    /// Share the live auxiliary-role pin registry with tools.
    pub fn with_agent_models(mut self, models: AgentModels) -> Self {
        self.agent_models = models;
        self
    }

    /// Resolve a model-supplied path against the working directory.
    pub fn resolve(&self, path: &str) -> PathBuf {
        let p = PathBuf::from(path);
        if p.is_absolute() {
            p
        } else {
            self.cwd.join(p)
        }
    }

    pub fn set_delegate_reporter(&self, reporter: mpsc::UnboundedSender<DelegateEvent>) {
        *self.delegate.lock().expect("delegate reporter lock") = Some(reporter);
    }

    pub fn clear_delegate_reporter(&self) {
        *self.delegate.lock().expect("delegate reporter lock") = None;
    }

    /// Returns a clone so a nested task can forward events from its own drain
    /// without holding this mutex across awaits.
    pub fn delegate_reporter(&self) -> Option<mpsc::UnboundedSender<DelegateEvent>> {
        self.delegate
            .lock()
            .expect("delegate reporter lock")
            .clone()
    }

    /// Bind (or unbind) where task traces persist. Called whenever the
    /// context is bound to a persistent session (startup, resume, import).
    pub fn bind_task_trace_root(&self, root: Option<PathBuf>) {
        self.task_traces
            .lock()
            .expect("task traces lock")
            .bind_root(root);
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;
    fn permission(&self, input: &Value) -> PermissionRequest;
    /// How this invocation enters model-gated Auto Mode. The conservative
    /// default sends side effects to the classifier; direct-safe tools opt in.
    fn auto_safety(&self, _input: &Value) -> AutoSafety {
        AutoSafety::Classify
    }
    /// Path this call will mutate, if any. The harness checkpoints it
    /// before running so rewind can restore the file.
    fn touches(&self, _input: &Value) -> Option<String> {
        None
    }
    /// Path used exclusively for Auto Mode's safety boundary. Defaults to a
    /// file mutation target, but side-effecting tools such as shell can expose
    /// their working directory without pretending it is checkpointable.
    fn safety_target(&self, input: &Value) -> Option<String> {
        self.touches(input)
    }
    /// Files or directories whose scoped instructions apply to this call.
    fn context_paths(&self, _input: &Value) -> Vec<String> {
        Vec::new()
    }
    /// Whether this call can create externally visible side effects.
    fn is_mutating(&self) -> bool {
        false
    }
    /// How concurrent calls to this tool may be batched in one turn. The
    /// default keeps calls isolated (approved and run one at a time).
    fn batch_policy(&self) -> BatchPolicy {
        BatchPolicy::Isolated
    }
    /// Per-invocation scheduling contract. Most tools are static; tools whose
    /// input selects different capabilities (such as `task`) override this
    /// instead of forcing the agent loop to match on names.
    fn batch_policy_for(&self, _input: &Value) -> BatchPolicy {
        self.batch_policy()
    }
    /// Human-facing name for this tool in the UI, e.g. `shell` → "Run".
    /// Defaults to the title-cased tool name; override for a clearer verb.
    fn display_name(&self) -> String {
        title_case(self.name())
    }
    /// Header fragment for a parallel batch of this tool's calls, e.g.
    /// "Read 3 files". Shown alone for a homogeneous batch, or joined with
    /// " · " when a batch mixes tools. The default names the tool and count;
    /// override for a nicer noun or input-dependent wording.
    fn batch_label(&self, inputs: &[&Value]) -> String {
        let count = inputs.len();
        format!(
            "{} {count} {}",
            title_case(self.name()),
            if count == 1 { "call" } else { "calls" }
        )
    }
    /// Whether this tool's output should pass through the token-budget blob
    /// gate. Locating/content tools (`read`, `grep`, `glob`) return precise,
    /// self-paginating output; gating them only forces a re-`read` of a result
    /// the model needed whole. Command/fetch tools (`shell`, `web_fetch`) keep
    /// the gate so a 50KB log never floods the context.
    fn gates_output(&self) -> bool {
        true
    }
    /// Optionally reduce a successful result before the central output gate.
    /// Tools whose output has a stable, domain-specific success format can
    /// preserve its useful evidence without spending context on repetition.
    /// Failures always bypass this hook unchanged for diagnosis.
    fn compact_success_output(&self, output: String) -> String {
        output
    }
    async fn run(&self, input: Value, ctx: &ToolCtx, cancel: &CancellationToken) -> ToolOutput;
    /// Entry point the agent loop uses, carrying the provider-issued tool_use
    /// id. Most tools ignore it; `task` records it so a sub-agent run can be
    /// tied back to the exact call that spawned it.
    async fn run_with_call(
        &self,
        _call_id: &str,
        input: Value,
        ctx: &ToolCtx,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        self.run(input, ctx, cancel).await
    }
}

/// Capitalize the first character; used for default tool labels.
pub(crate) fn title_case(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

impl dyn Tool {
    pub fn def(&self) -> crate::ToolDef {
        crate::ToolDef {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: self.input_schema(),
        }
    }
}
