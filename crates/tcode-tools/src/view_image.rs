use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::{
    ActiveModel, AgentModels, AutoSafety, ContentBlock, Message, ModelCell, PermissionRequest,
    Request, Role, StreamEvent, Tool, ToolCtx, ToolOutput,
};

const SYSTEM: &str = include_str!("../../../prompts/view-image-system.md");
static RUN: AtomicU64 = AtomicU64::new(0);

/// Delegate image understanding to the configured vision model without putting
/// the image in the parent conversation's permanent ledger.
pub struct ViewImageTool {
    model: ModelCell,
    pinned: AgentModels,
}

impl ViewImageTool {
    pub fn new(model: ModelCell, pinned: AgentModels) -> Self {
        Self { model, pinned }
    }

    fn model_for_vision(&self) -> ActiveModel {
        self.pinned
            .get("vision")
            .unwrap_or_else(|| self.model.snapshot())
    }
}

#[async_trait]
impl Tool for ViewImageTool {
    fn name(&self) -> &str {
        "view_image"
    }

    fn display_name(&self) -> String {
        "View image".into()
    }

    fn description(&self) -> &str {
        "Ask a vision-capable model a specific question about up to 8 image files. Use this when read says the current model cannot view an image, or to inspect images without putting them into the main conversation context. Give a self-contained, concrete prompt, e.g. 'What exact error text does the dialog show?', not merely 'describe this image'."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "maxItems": 8,
                    "description": "Image paths, absolute or relative to cwd"
                },
                "prompt": { "type": "string", "description": "Specific self-contained question about the images" }
            },
            "required": ["paths", "prompt"]
        })
    }

    fn permission(&self, _input: &Value) -> PermissionRequest {
        PermissionRequest::None
    }

    fn auto_safety(&self, _input: &Value) -> AutoSafety {
        AutoSafety::Allow
    }

    async fn run(&self, input: Value, ctx: &ToolCtx, cancel: &CancellationToken) -> ToolOutput {
        let Some(paths) = input["paths"].as_array() else {
            return ToolOutput::err("missing required parameter: paths");
        };
        if paths.is_empty() || paths.len() > 8 || paths.iter().any(|path| path.as_str().is_none()) {
            return ToolOutput::err("paths must contain between 1 and 8 string paths");
        }
        let Some(prompt) = input["prompt"]
            .as_str()
            .filter(|prompt| !prompt.trim().is_empty())
        else {
            return ToolOutput::err("missing required parameter: prompt");
        };

        let model = self.model_for_vision();
        if !model.provider.supports_vision() {
            return ToolOutput::err(
                "the configured vision model cannot view images. Choose a vision-capable model with /agents → vision or configure [agents.vision].",
            );
        }

        let mut content = Vec::with_capacity(paths.len() + 1);
        for raw_path in paths {
            if cancel.is_cancelled() {
                return ToolOutput::err("view_image cancelled by user");
            }
            let path_str = raw_path.as_str().expect("validated paths");
            let path = ctx.resolve(path_str);
            let bytes = match tokio::fs::read(&path).await {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return ToolOutput::err(format!("image file not found: {}", path.display()));
                }
                Err(error) => {
                    return ToolOutput::err(format!("cannot read {}: {error}", path.display()))
                }
            };
            let normalized = match tokio::task::spawn_blocking(move || {
                tcode_core::images::normalize_image(&bytes)
            })
            .await
            {
                Ok(Ok(image)) => image,
                Ok(Err(error)) => {
                    return ToolOutput::err(format!(
                        "{} is not a usable image: {error}",
                        path.display()
                    ))
                }
                Err(error) => {
                    return ToolOutput::err(format!("image normalization failed: {error}"))
                }
            };
            // Do not use freshness here: these blocks live only in this isolated
            // request, never in the parent ledger, so 'already in context' is false.
            content.push(normalized.into_block());
        }
        content.push(ContentBlock::Text {
            text: prompt.to_string(),
        });

        let run = RUN.fetch_add(1, Ordering::Relaxed);
        let request = Request {
            model: model.provider.model().to_string(),
            system: SYSTEM.to_string(),
            system_suffix: None,
            cache_scope: Some(format!("vision-{run}")),
            messages: vec![Message {
                role: Role::User,
                content,
            }],
            tools: Vec::new(),
            max_tokens: model.max_tokens.min(2048),
            effort: model.effort.clone(),
        };
        let mut stream = match model.provider.stream(request, cancel.clone()).await {
            Ok(stream) => stream,
            Err(error) => return ToolOutput::err(format!("vision request failed: {error}")),
        };
        let mut answer = String::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(StreamEvent::TextDelta(text)) => answer.push_str(&text),
                Ok(StreamEvent::Usage(usage)) => {
                    if let Some(reporter) = ctx.usage_reporter() {
                        let _ = reporter.send(usage);
                    }
                }
                Err(error) => return ToolOutput::err(format!("vision request failed: {error}")),
                _ => {}
            }
        }
        if cancel.is_cancelled() {
            ToolOutput::err("view_image cancelled by user")
        } else if answer.trim().is_empty() {
            ToolOutput::err("vision model returned no text")
        } else {
            ToolOutput::ok(answer)
        }
    }
}
