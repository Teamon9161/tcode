use async_stream::stream;
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::config::WatchdogConfig;
use tcode_core::{
    CacheStrategy, ContentBlock, EventStream, Message, Provider, ProviderError, Request, Role,
    StopReason, StreamEvent,
};

use crate::idle::{classify, idle_guard};
use crate::retry::connect_once;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Chat Completions-compatible backend: OpenAI, DeepSeek, OpenRouter,
/// local servers... Prefix caching is implicit; our append-only history
/// is exactly what it needs.
pub struct OpenAiProvider {
    http: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
    watchdog: WatchdogConfig,
}

impl OpenAiProvider {
    pub fn new(
        api_key: String,
        model: String,
        base_url: Option<String>,
        watchdog: WatchdogConfig,
    ) -> Self {
        Self {
            http: crate::http::client(),
            api_key,
            model,
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            watchdog,
        }
    }

    fn build_body(&self, req: &Request) -> Value {
        let mut messages = vec![json!({ "role": "system", "content": req.system })];
        if let Some(suffix) = &req.system_suffix {
            messages.push(json!({ "role": "system", "content": suffix }));
        }
        for msg in &req.messages {
            flatten_message(msg, &mut messages);
        }
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    },
                })
            })
            .collect();
        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "stream": true,
            "stream_options": { "include_usage": true },
            "messages": messages,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
            body["parallel_tool_calls"] = json!(true);
        }
        // Reasoning models accept an effort dial; "off" means "send
        // nothing" for endpoints without one.
        if let Some(effort) = req.effort.as_deref() {
            if effort != "off" {
                body["reasoning_effort"] = json!(effort);
            }
        }
        body
    }
}

/// Our neutral message maps to 1..n OpenAI messages: tool results become
/// separate `role:"tool"` messages, everything else stays in place.
fn flatten_message(msg: &Message, out: &mut Vec<Value>) {
    match msg.role {
        Role::Assistant => {
            let mut text = String::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text: t } => text.push_str(t),
                    ContentBlock::ToolUse { id, name, input } => tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": serde_json::to_string(input).unwrap_or_default(),
                        },
                    })),
                    // Reasoning is not replayable on this API.
                    ContentBlock::Thinking { .. } => {}
                    _ => {}
                }
            }
            let mut m = json!({ "role": "assistant" });
            m["content"] = if text.is_empty() {
                Value::Null
            } else {
                Value::String(text)
            };
            if !tool_calls.is_empty() {
                m["tool_calls"] = Value::Array(tool_calls);
            }
            out.push(m);
        }
        Role::User => {
            // Tool results must come first, directly after the assistant
            // message that issued the calls.
            for block in &msg.content {
                if let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    images,
                    ..
                } = block
                {
                    // The chat-completions `tool` role carries text only, so
                    // images can't be inlined here. Be honest about it rather
                    // than letting the model assume it can see them.
                    let content = if images.is_empty() {
                        content.clone()
                    } else {
                        format!(
                            "{content}\n[{} image(s) omitted: this model cannot view images \
                             returned from a tool]",
                            images.len()
                        )
                    };
                    out.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_use_id,
                        "content": content,
                    }));
                }
            }
            let mut parts: Vec<Value> = Vec::new();
            let mut has_image = false;
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        parts.push(json!({ "type": "text", "text": text }))
                    }
                    ContentBlock::Image { media_type, data } => {
                        has_image = true;
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": { "url": format!("data:{media_type};base64,{data}") },
                        }));
                    }
                    _ => {}
                }
            }
            if parts.is_empty() {
                return;
            }
            let content = if has_image {
                Value::Array(parts)
            } else {
                // Plain string keeps maximum compatibility with
                // OpenAI-compatible endpoints that reject part arrays.
                Value::String(
                    parts
                        .iter()
                        .filter_map(|p| p["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                )
            };
            out.push(json!({ "role": "user", "content": content }));
        }
    }
}

fn usage_from(v: &Value) -> tcode_core::Usage {
    let prompt = v["prompt_tokens"].as_u64().unwrap_or(0);
    let cached = v["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);
    tcode_core::Usage {
        input_tokens: prompt.saturating_sub(cached),
        output_tokens: v["completion_tokens"].as_u64().unwrap_or(0),
        cache_read_tokens: cached,
        cache_write_tokens: 0,
    }
}

fn stop_reason_from(s: &str) -> StopReason {
    match s {
        "stop" => StopReason::EndTurn,
        "tool_calls" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        other => StopReason::Other(other.to_string()),
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn cache_strategy(&self) -> CacheStrategy {
        CacheStrategy::ImplicitPrefix
    }

    async fn stream(
        &self,
        req: Request,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        let body = self.build_body(&req);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let resp = connect_once(self.watchdog.connect_timeout(), || {
            self.http
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
        })
        .await?;

        let mut sse = idle_guard(resp.bytes_stream(), self.watchdog.idle_timeout()).eventsource();
        let raw: EventStream = Box::pin(stream! {
            let mut started = false;
            let mut finish: Option<StopReason> = None;
            // OpenAI repeats tool-call metadata per fragment; emit Start once.
            let mut open_calls: std::collections::HashSet<usize> =
                std::collections::HashSet::new();
            while let Some(item) = sse.next().await {
                let event = match item {
                    Ok(e) => e,
                    Err(e) => {
                        yield Err(classify(e));
                        return;
                    }
                };
                if event.data.trim() == "[DONE]" {
                    yield Ok(StreamEvent::Done(finish.unwrap_or(StopReason::EndTurn)));
                    return;
                }
                let data: Value = match serde_json::from_str(&event.data) {
                    Ok(v) => v,
                    Err(e) => {
                        yield Err(ProviderError::BadResponse(format!("bad chunk: {e}")));
                        return;
                    }
                };
                if !started {
                    started = true;
                    yield Ok(StreamEvent::Started);
                }
                if data["usage"].is_object() {
                    yield Ok(StreamEvent::Usage(usage_from(&data["usage"])));
                }
                let Some(choice) = data["choices"].get(0) else { continue };
                if let Some(s) = choice["finish_reason"].as_str() {
                    finish = Some(stop_reason_from(s));
                }
                let delta = &choice["delta"];
                if let Some(t) = delta["content"].as_str() {
                    if !t.is_empty() {
                        yield Ok(StreamEvent::TextDelta(t.to_string()));
                    }
                }
                // DeepSeek-style reasoning stream.
                if let Some(t) = delta["reasoning_content"].as_str() {
                    if !t.is_empty() {
                        yield Ok(StreamEvent::ThinkingDelta(t.to_string()));
                    }
                }
                if let Some(calls) = delta["tool_calls"].as_array() {
                    for call in calls {
                        let index = call["index"].as_u64().unwrap_or(0) as usize;
                        if open_calls.insert(index) {
                            yield Ok(StreamEvent::ToolUseStart {
                                index,
                                id: call["id"].as_str().unwrap_or_default().to_string(),
                                name: call["function"]["name"]
                                    .as_str().unwrap_or_default().to_string(),
                            });
                        }
                        if let Some(frag) = call["function"]["arguments"].as_str() {
                            if !frag.is_empty() {
                                yield Ok(StreamEvent::ToolUseInputDelta {
                                    index,
                                    fragment: frag.to_string(),
                                });
                            }
                        }
                    }
                }
            }
            // Stream ended without [DONE]; treat as complete if we saw a
            // finish_reason, otherwise report the truncation.
            match finish {
                Some(reason) => yield Ok(StreamEvent::Done(reason)),
                None => yield Err(ProviderError::BadResponse(
                    "stream ended without finish_reason".into(),
                )),
            }
        });

        Ok(Box::pin(raw.take_until(cancel.cancelled_owned())))
    }
}
