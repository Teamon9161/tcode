use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use crate::types::{Message, StopReason, ToolDef, Usage};

#[derive(Debug, Clone)]
pub struct Request {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
}

/// Unified stream events; provider wire-format differences end here.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Connection established, model accepted the request.
    Started,
    TextDelta(String),
    ThinkingDelta(String),
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
    #[error("malformed response: {0}")]
    BadResponse(String),
    #[error("configuration error: {0}")]
    Config(String),
}

impl ProviderError {
    /// Whether re-sending the whole request may succeed.
    pub fn retryable(&self) -> bool {
        match self {
            ProviderError::Network(_) | ProviderError::IdleTimeout(_) => true,
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
