use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use crate::types::{Message, RateLimits, StopReason, ToolDef, Usage};

#[derive(Debug, Clone)]
pub struct Request {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    /// Reasoning effort; None = provider default. Each provider maps it
    /// to its own dial (thinking budget / reasoning.effort).
    pub effort: Option<String>,
}

/// Unified stream events; provider wire-format differences end here.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Connection established, model accepted the request.
    Started,
    TextDelta(String),
    ThinkingDelta(String),
    /// Opaque payload that must be replayed with the current thinking
    /// block: Anthropic's signature, or the ChatGPT backend's whole
    /// encrypted reasoning item (as JSON).
    ThinkingSignature(String),
    ToolUseStart {
        index: usize,
        id: String,
        name: String,
    },
    ToolUseInputDelta {
        index: usize,
        fragment: String,
    },
    Usage(Usage),
    RateLimits(RateLimits),
    Done(StopReason),
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("network error: {0}")]
    Network(String),
    #[error("API error {status}: {message}")]
    Api { status: u16, message: String },
    #[error("stream stalled: no data for {0:?}")]
    IdleTimeout(Duration),
    /// The request was accepted but the server sent no reply at all — not one
    /// header — within the budget. Distinct from `IdleTimeout`, which is a
    /// stream that started and then stalled.
    #[error("no reply from the API within {0:?} (request sent, nothing came back)")]
    ConnectTimeout(Duration),
    #[error("malformed response: {0}")]
    BadResponse(String),
    #[error("configuration error: {0}")]
    Config(String),
}

impl ProviderError {
    /// Whether re-sending the whole request may succeed.
    pub fn retryable(&self) -> bool {
        match self {
            ProviderError::Network(_)
            | ProviderError::IdleTimeout(_)
            | ProviderError::ConnectTimeout(_) => true,
            ProviderError::Api { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }
}

pub type EventStream = BoxStream<'static, Result<StreamEvent, ProviderError>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStrategy {
    /// Anthropic-style explicit cache_control breakpoints.
    ExplicitBreakpoints,
    /// OpenAI-style implicit prefix caching.
    ImplicitPrefix,
}

/// Everything request-building needs to know about the active model.
#[derive(Clone)]
pub struct ActiveModel {
    pub provider: std::sync::Arc<dyn Provider>,
    pub max_tokens: u32,
    pub context_window: u64,
    pub effort: Option<String>,
}

impl ActiveModel {
    /// e.g. `deepseek-v4-flash[1m] (high)` — for status lines.
    pub fn describe(&self) -> String {
        match &self.effort {
            Some(e) => format!("{} ({e})", self.provider.model()),
            None => self.provider.model().to_string(),
        }
    }
}

/// Shared, swappable model handle: the agent loop and sub-agents read
/// through it, `/model` swaps it mid-session. Snapshots keep a whole
/// turn on one consistent model.
#[derive(Clone)]
pub struct ModelCell(std::sync::Arc<std::sync::RwLock<ActiveModel>>);

impl ModelCell {
    pub fn new(model: ActiveModel) -> Self {
        Self(std::sync::Arc::new(std::sync::RwLock::new(model)))
    }

    pub fn snapshot(&self) -> ActiveModel {
        self.0.read().expect("model cell lock").clone()
    }

    pub fn swap(&self, model: ActiveModel) {
        *self.0.write().expect("model cell lock") = model;
    }

    pub fn set_effort(&self, effort: Option<String>) {
        self.0.write().expect("model cell lock").effort = effort;
    }
}

/// Which model each sub-agent kind runs on, when it is not simply following
/// the main one. Shared between the `task` tool and the frontend that edits
/// the pins (`/agents`), and swappable for the same reason `ModelCell` is:
/// a pick must apply to the next sub-agent, not the next process.
///
/// Absent kind = inherit: that sub-agent uses the parent's `ModelCell` and so
/// follows `/model`. A pinned kind deliberately does not.
#[derive(Clone, Default)]
pub struct AgentModels(std::sync::Arc<std::sync::RwLock<BTreeMap<String, ActiveModel>>>);

impl AgentModels {
    pub fn get(&self, kind: &str) -> Option<ActiveModel> {
        self.0.read().expect("agent models lock").get(kind).cloned()
    }

    pub fn pin(&self, kind: &str, model: ActiveModel) {
        self.0
            .write()
            .expect("agent models lock")
            .insert(kind.to_string(), model);
    }

    pub fn unpin(&self, kind: &str) {
        self.0.write().expect("agent models lock").remove(kind);
    }

    /// `(kind,描述)` for every pin, for status/summary lines.
    pub fn describe(&self) -> Vec<(String, String)> {
        self.0
            .read()
            .expect("agent models lock")
            .iter()
            .map(|(kind, model)| (kind.clone(), model.describe()))
            .collect()
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    fn cache_strategy(&self) -> CacheStrategy;
    /// Open a streaming request. Establishing the connection retries
    /// internally; mid-stream failures surface as an `Err` item and the
    /// caller decides whether to re-send the turn.
    async fn stream(
        &self,
        req: Request,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError>;
}
