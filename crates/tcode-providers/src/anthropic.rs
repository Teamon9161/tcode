use async_stream::stream;
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::config::WatchdogConfig;
use tcode_core::stream_util::with_idle_timeout;
use tcode_core::{
    CacheStrategy, ContentBlock, EventStream, Message, Provider, ProviderError, Request,
    StopReason, StreamEvent,
};

use crate::retry::connect_with_retry;

const API_VERSION: &str = "2023-06-01";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

pub struct AnthropicProvider {
    http: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
    watchdog: WatchdogConfig,
}

impl AnthropicProvider {
    pub fn new(
        api_key: String,
        model: String,
        base_url: Option<String>,
        watchdog: WatchdogConfig,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("reqwest client"),
            api_key,
            model,
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            watchdog,
        }
    }

    fn build_body(&self, req: &Request) -> Value {
        let mut messages: Vec<Value> = req.messages.iter().map(message_to_json).collect();
        // Moving cache breakpoint: last content block of the last message.
        // Together with the breakpoint on system this covers the whole
        // prefix; append-only history means the next turn extends it.
        if let Some(last) = messages.last_mut() {
            if let Some(blocks) = last["content"].as_array_mut() {
                if let Some(block) = blocks.last_mut() {
                    block["cache_control"] = json!({ "type": "ephemeral" });
                }
            }
        }
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();
        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "stream": true,
            // Breakpoint after system covers tools + system prefix.
            "system": [{
                "type": "text",
                "text": req.system,
                "cache_control": { "type": "ephemeral" },
            }],
            "messages": messages,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        body
    }
}

fn message_to_json(msg: &Message) -> Value {
    let role = match msg.role {
        tcode_core::Role::User => "user",
        tcode_core::Role::Assistant => "assistant",
    };
    let blocks: Vec<Value> = msg.content.iter().filter_map(block_to_json).collect();
    json!({ "role": role, "content": blocks })
}

fn block_to_json(block: &ContentBlock) -> Option<Value> {
    Some(match block {
        ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
        ContentBlock::Thinking {
            thinking,
            signature,
        } => {
            // Anthropic rejects unsigned thinking blocks on replay.
            let signature = signature.as_ref()?;
            json!({ "type": "thinking", "thinking": thinking, "signature": signature })
        }
        ContentBlock::Image { media_type, data } => json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media_type, "data": data },
        }),
        ContentBlock::ToolUse { id, name, input } => {
            json!({ "type": "tool_use", "id": id, "name": name, "input": input })
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let mut v = json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": [{ "type": "text", "text": content }],
            });
            if *is_error {
                v["is_error"] = json!(true);
            }
            v
        }
    })
}

fn usage_from(v: &Value) -> tcode_core::Usage {
    tcode_core::Usage {
        input_tokens: v["input_tokens"].as_u64().unwrap_or(0),
        output_tokens: v["output_tokens"].as_u64().unwrap_or(0),
        cache_read_tokens: v["cache_read_input_tokens"].as_u64().unwrap_or(0),
        cache_write_tokens: v["cache_creation_input_tokens"].as_u64().unwrap_or(0),
    }
}

fn stop_reason_from(s: &str) -> StopReason {
    match s {
        "end_turn" | "stop_sequence" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        other => StopReason::Other(other.to_string()),
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn cache_strategy(&self) -> CacheStrategy {
        CacheStrategy::ExplicitBreakpoints
    }

    async fn stream(
        &self,
        req: Request,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        let body = self.build_body(&req);
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let resp = connect_with_retry(&self.watchdog, || {
            self.http
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", API_VERSION)
                .json(&body)
                .send()
        })
        .await?;

        let mut sse = resp.bytes_stream().eventsource();
        let raw: EventStream = Box::pin(stream! {
            let mut stop_reason = StopReason::EndTurn;
            while let Some(item) = sse.next().await {
                let event = match item {
                    Ok(e) => e,
                    Err(e) => {
                        yield Err(ProviderError::Network(e.to_string()));
                        return;
                    }
                };
                let data: Value = match event.event.as_str() {
                    "ping" => continue,
                    _ => match serde_json::from_str(&event.data) {
                        Ok(v) => v,
                        Err(e) => {
                            yield Err(ProviderError::BadResponse(format!(
                                "bad SSE payload for '{}': {e}", event.event
                            )));
                            return;
                        }
                    },
                };
                match event.event.as_str() {
                    "message_start" => {
                        yield Ok(StreamEvent::Started);
                        yield Ok(StreamEvent::Usage(usage_from(&data["message"]["usage"])));
                    }
                    "content_block_start" => {
                        let index = data["index"].as_u64().unwrap_or(0) as usize;
                        let block = &data["content_block"];
                        if block["type"] == "tool_use" {
                            yield Ok(StreamEvent::ToolUseStart {
                                index,
                                id: block["id"].as_str().unwrap_or_default().to_string(),
                                name: block["name"].as_str().unwrap_or_default().to_string(),
                            });
                        }
                    }
                    "content_block_delta" => {
                        let index = data["index"].as_u64().unwrap_or(0) as usize;
                        let delta = &data["delta"];
                        match delta["type"].as_str().unwrap_or_default() {
                            "text_delta" => yield Ok(StreamEvent::TextDelta(
                                delta["text"].as_str().unwrap_or_default().to_string(),
                            )),
                            "thinking_delta" => yield Ok(StreamEvent::ThinkingDelta(
                                delta["thinking"].as_str().unwrap_or_default().to_string(),
                            )),
                            "input_json_delta" => yield Ok(StreamEvent::ToolUseInputDelta {
                                index,
                                fragment: delta["partial_json"]
                                    .as_str().unwrap_or_default().to_string(),
                            }),
                            _ => {}
                        }
                    }
                    "message_delta" => {
                        if let Some(s) = data["delta"]["stop_reason"].as_str() {
                            stop_reason = stop_reason_from(s);
                        }
                        yield Ok(StreamEvent::Usage(usage_from(&data["usage"])));
                    }
                    "message_stop" => {
                        yield Ok(StreamEvent::Done(stop_reason.clone()));
                        return;
                    }
                    "error" => {
                        yield Err(ProviderError::Api {
                            status: 0,
                            message: data["error"]["message"]
                                .as_str().unwrap_or("unknown stream error").to_string(),
                        });
                        return;
                    }
                    _ => {}
                }
            }
        });

        let guarded = with_idle_timeout(raw, self.watchdog.idle_timeout());
        Ok(Box::pin(guarded.take_until(cancel.cancelled_owned())))
    }
}
