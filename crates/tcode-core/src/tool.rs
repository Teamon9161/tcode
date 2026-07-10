use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::blobs::BlobStore;
use crate::freshness::FreshnessTracker;

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
}

/// Shared context handed to every tool invocation.
pub struct ToolCtx {
    pub cwd: PathBuf,
    pub freshness: Mutex<FreshnessTracker>,
    pub blobs: Mutex<BlobStore>,
}

impl ToolCtx {
    pub fn new(cwd: PathBuf, output_budget_tokens: usize) -> Self {
        Self {
            cwd,
            freshness: Mutex::new(FreshnessTracker::default()),
            blobs: Mutex::new(BlobStore::new(output_budget_tokens)),
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
