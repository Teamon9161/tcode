use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use tcode_core::{
    ActiveModel, AgentModels, CacheStrategy, EventStream, ModelCell, Provider, ProviderError,
    Request, StreamEvent, Tool, ToolCtx,
};

use super::ViewImageTool;

struct MockVision {
    vision: bool,
    requests: Mutex<Vec<Request>>,
}

#[async_trait]
impl Provider for MockVision {
    fn name(&self) -> &str {
        "mock"
    }

    fn model(&self) -> &str {
        "mock-vision"
    }

    fn cache_strategy(&self) -> CacheStrategy {
        CacheStrategy::ImplicitPrefix
    }

    fn supports_vision(&self) -> bool {
        self.vision
    }

    async fn stream(
        &self,
        request: Request,
        _cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        self.requests.lock().unwrap().push(request);
        Ok(Box::pin(stream::iter(vec![Ok(StreamEvent::TextDelta(
            "visible text".into(),
        ))])))
    }
}

fn model(provider: Arc<MockVision>) -> ModelCell {
    ModelCell::new(ActiveModel {
        provider,
        max_tokens: 4096,
        context_window: 128_000,
        effort: None,
    })
}

fn png() -> Vec<u8> {
    tcode_core::images::normalize_rgba(1, 1, vec![0; 4])
        .unwrap()
        .bytes
}

#[tokio::test]
async fn sends_images_in_one_isolated_vision_request() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("shot.png"), png()).unwrap();
    let provider = Arc::new(MockVision {
        vision: true,
        requests: Mutex::new(Vec::new()),
    });
    let tool = ViewImageTool::new(model(provider.clone()), AgentModels::default());
    let ctx = ToolCtx::new(directory.path().to_path_buf(), 8_000);

    let result = tool
        .run(
            json!({ "paths": ["shot.png"], "prompt": "What text is visible?" }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;

    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "visible text");
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert!(request
        .cache_scope
        .as_deref()
        .is_some_and(|scope| scope.starts_with("vision-")));
    assert!(request.tools.is_empty());
    assert_eq!(request.messages.len(), 1);
    // Each image is preceded by a file-name label so the answer can refer to
    // images unambiguously; the prompt comes last.
    assert!(matches!(request.messages[0].content.as_slice(), [
        tcode_core::ContentBlock::Text { text: label },
        tcode_core::ContentBlock::Image { .. },
        tcode_core::ContentBlock::Text { text },
    ] if label == "shot.png:" && text == "What text is visible?"));
}

#[tokio::test]
async fn rejects_a_text_only_vision_model_with_a_fix() {
    let provider = Arc::new(MockVision {
        vision: false,
        requests: Mutex::new(Vec::new()),
    });
    let tool = ViewImageTool::new(model(provider), AgentModels::default());
    let directory = tempfile::tempdir().unwrap();
    let ctx = ToolCtx::new(directory.path().to_path_buf(), 8_000);

    let result = tool
        .run(
            json!({ "paths": ["does-not-matter.png"], "prompt": "read it" }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;

    assert!(result.is_error);
    assert!(result.content.contains("/agents"));
}
