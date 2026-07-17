use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use crate::types::{Message, RateLimits, StopReason, ToolDef, Usage};

#[derive(Debug, Clone)]
pub struct Request {
    pub model: String,
    /// Stable, cacheable system prefix.
    pub system: String,
    /// Optional per-request system tail. Classifier stage two changes only this
    /// field, preserving `system` as a cacheable prefix.
    pub system_suffix: Option<String>,
    /// Which conversation this request belongs to. `None` is the main session.
    /// Anything with its own prefix — the Auto Mode classifier, a sub-agent —
    /// must name its own scope: providers keyed by an explicit cache id (Codex)
    /// give distinct scopes distinct ids, because interleaving unrelated
    /// prefixes on one id costs all of them cache affinity. Providers whose
    /// cache is addressed by the prefix itself (Anthropic, OpenAI) ignore it.
    pub cache_scope: Option<String>,
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

/// One `/agents` assignment for a role.
#[derive(Clone)]
pub enum AgentPin {
    /// Explicitly follow the main model, resolved live at each use. For most
    /// roles this is indistinguishable from being absent; roles that are off
    /// by default (`web-fetch`) use it to switch the capability on without
    /// pinning a model.
    Inherit,
    Model(ActiveModel),
}

/// Which auxiliary model role runs on a pinned model rather than following
/// the main one. Roles cover sub-agents and the Auto Mode classifier. Shared
/// between the consumers and the frontend that edits pins (`/agents`), and
/// swappable for the same reason `ModelCell` is:
/// a pick must apply to the next sub-agent, not the next process.
///
/// Absent kind = inherit for sub-agent roles (the parent's `ModelCell`, so it
/// follows `/model`); off for off-by-default roles. A pinned kind
/// deliberately does not follow `/model`.
#[derive(Clone, Default)]
pub struct AgentModels(std::sync::Arc<std::sync::RwLock<BTreeMap<String, AgentPin>>>);

impl AgentModels {
    /// The model `kind` is pinned to. `Inherit` and absent both yield None so
    /// existing callers fall back to the parent's live handle.
    pub fn get(&self, kind: &str) -> Option<ActiveModel> {
        match self.0.read().expect("agent models lock").get(kind) {
            Some(AgentPin::Model(model)) => Some(model.clone()),
            _ => None,
        }
    }

    /// The raw assignment, for callers that must distinguish "inherit" from
    /// "absent" (off-by-default roles).
    pub fn pin_state(&self, kind: &str) -> Option<AgentPin> {
        self.0.read().expect("agent models lock").get(kind).cloned()
    }

    pub fn inherits(&self, kind: &str) -> bool {
        matches!(
            self.0.read().expect("agent models lock").get(kind),
            Some(AgentPin::Inherit)
        )
    }

    pub fn pin(&self, kind: &str, model: ActiveModel) {
        self.0
            .write()
            .expect("agent models lock")
            .insert(kind.to_string(), AgentPin::Model(model));
    }

    pub fn pin_inherit(&self, kind: &str) {
        self.0
            .write()
            .expect("agent models lock")
            .insert(kind.to_string(), AgentPin::Inherit);
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
            .map(|(kind, pin)| {
                let label = match pin {
                    AgentPin::Inherit => "inherit".to_string(),
                    AgentPin::Model(model) => model.describe(),
                };
                (kind.clone(), label)
            })
            .collect()
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    fn cache_strategy(&self) -> CacheStrategy;
    /// Whether this model can accept image blocks in user messages. The default
    /// preserves existing multimodal behavior; text-only models opt out in config.
    fn supports_vision(&self) -> bool {
        true
    }
    /// Open a streaming request. Establishing the connection retries
    /// internally; mid-stream failures surface as an `Err` item and the
    /// caller decides whether to re-send the turn.
    async fn stream(
        &self,
        req: Request,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError>;
}
