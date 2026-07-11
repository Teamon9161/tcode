use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::background::BackgroundTasks;
use crate::blobs::BlobStore;
use crate::freshness::FreshnessTracker;
use crate::types::Usage;

pub use crate::permission::{Approval, ApprovalDecision, Approver};

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }
    /// Tool errors are written FOR the model: always include what it
    /// needs to fix the call without spending another turn on diagnosis.
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
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
}

/// Shared context handed to every tool invocation.
pub struct ToolCtx {
    pub cwd: PathBuf,
    pub freshness: Mutex<FreshnessTracker>,
    pub blobs: Mutex<BlobStore>,
    pub background: Mutex<BackgroundTasks>,
    pub memory: Mutex<crate::memory::MemoryManager>,
    /// A parent agent installs this only while a tool is running. Nested
    /// agents use it to report their own billable usage without pretending it
    /// occupies the parent's context window.
    usage_reporter: Mutex<Option<mpsc::UnboundedSender<Usage>>>,
}

impl ToolCtx {
    pub fn new(cwd: PathBuf, output_budget_tokens: usize) -> Self {
        let memory = crate::memory::MemoryManager::new(&cwd);
        Self {
            cwd,
            freshness: Mutex::new(FreshnessTracker::default()),
            blobs: Mutex::new(BlobStore::new(output_budget_tokens)),
            background: Mutex::new(BackgroundTasks::default()),
            memory: Mutex::new(memory),
            usage_reporter: Mutex::new(None),
        }
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
    /// Whether this tool's output should pass through the token-budget blob
    /// gate. Locating/content tools (`read`, `grep`, `glob`) return precise,
    /// self-paginating output; gating them only forces `read_output` paging
    /// of a result the model needed whole. Command/fetch tools (`shell`,
    /// `web_fetch`) keep the gate so a 50KB log never floods the context.
    fn gates_output(&self) -> bool {
        true
    }
    async fn run(&self, input: Value, ctx: &ToolCtx, cancel: &CancellationToken) -> ToolOutput;
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
