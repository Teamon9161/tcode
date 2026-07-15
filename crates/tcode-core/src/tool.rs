use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::background::BackgroundTasks;
use crate::blobs::BlobStore;
use crate::freshness::FreshnessTracker;
use crate::provider::ModelCell;
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
        /// Matched against permission rules, e.g. "shell(git status)".
        descriptor: String,
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

    /// Whether approving this may persist an allow rule. Questions and plan
    /// review never can.
    pub fn allows_rule(&self) -> bool {
        matches!(self, PermissionRequest::Ask { .. })
    }
}

/// Shared context handed to every tool invocation.
pub struct ToolCtx {
    pub cwd: PathBuf,
    pub freshness: Mutex<FreshnessTracker>,
    pub blobs: Mutex<BlobStore>,
    pub background: Mutex<BackgroundTasks>,
    pub memory: Mutex<crate::memory::MemoryManager>,
    /// The model active for this invocation. Tools may use its capabilities
    /// without owning model selection or provider construction.
    pub model: Option<ModelCell>,
    /// A parent agent installs this only while a tool is running. Nested
    /// agents use it to report their own billable usage without pretending it
    /// occupies the parent's context window.
    usage_reporter: Mutex<Option<mpsc::UnboundedSender<Usage>>>,
}

impl ToolCtx {
    pub fn new(cwd: PathBuf, output_budget_tokens: usize) -> Self {
        let memory = crate::memory::MemoryManager::new(&cwd);
        // Overflowed output and background logs share one per-project scratch
        // dir that `read`/`grep` can reach. Sweeping starts one level up: the
        // model's own throwaway files live in the same scratchpad and go stale
        // by the same clock.
        let tool_output = crate::store::tool_output_dir(&cwd);
        crate::store::sweep_scratchpad(&crate::store::scratchpad_dir(&cwd));
        Self {
            freshness: Mutex::new(FreshnessTracker::default()),
            blobs: Mutex::new(BlobStore::new(tool_output.clone(), output_budget_tokens)),
            background: Mutex::new(BackgroundTasks::new(tool_output)),
            memory: Mutex::new(memory),
            model: None,
            usage_reporter: Mutex::new(None),
            cwd,
        }
    }

    pub fn with_model(mut self, model: ModelCell) -> Self {
        self.model = Some(model);
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

    pub fn set_usage_reporter(&self, reporter: mpsc::UnboundedSender<Usage>) {
        *self.usage_reporter.lock().expect("usage reporter lock") = Some(reporter);
    }

    pub fn clear_usage_reporter(&self) {
        *self.usage_reporter.lock().expect("usage reporter lock") = None;
    }

    /// Returns a clone so a nested task can forward usage from its own event
    /// drain without holding this mutex across awaits.
    pub fn usage_reporter(&self) -> Option<mpsc::UnboundedSender<Usage>> {
        self.usage_reporter
            .lock()
            .expect("usage reporter lock")
            .clone()
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
    async fn run(&self, input: Value, ctx: &ToolCtx, cancel: &CancellationToken) -> ToolOutput;
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
