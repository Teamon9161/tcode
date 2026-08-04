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
    /// True when the backend is DeepSeek's OpenAI-compatible endpoint,
    /// detected from the base URL. Its V4 models think by default: tool-call
    /// turns must round-trip `reasoning_content` (the API 400s otherwise) and
    /// `effort = off` must send `thinking: {type: "disabled"}`.
    deepseek: bool,
    watchdog: WatchdogConfig,
    vision: bool,
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
            deepseek: base_url
                .as_deref()
                .is_some_and(|u| u.contains("deepseek.com")),
            api_key,
            model,
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            watchdog,
            vision: true,
        }
    }

    pub fn with_vision(mut self, vision: bool) -> Self {
        self.vision = vision;
        self
    }

    fn build_body(&self, req: &Request) -> Value {
        let mut messages = vec![json!({ "role": "system", "content": req.system })];
        if let Some(suffix) = &req.system_suffix {
            messages.push(json!({ "role": "system", "content": suffix }));
        }
        for msg in &req.messages {
            flatten_message(msg, &mut messages, self.vision, self.deepseek);
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
            "stream": true,
            "stream_options": { "include_usage": true },
            "messages": messages,
        });
        // Optional here, unlike Anthropic: omitting it leaves the endpoint's
        // own output limit in force, which is the right default for a chat
        // turn. Sending one anyway used to cut long answers — and on backends
        // that think by default (DeepSeek) the reasoning is spent against the
        // same budget, so the cap could be gone before the answer started.
        if let Some(max_tokens) = req.max_tokens {
            body["max_tokens"] = json!(max_tokens);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
            body["parallel_tool_calls"] = json!(true);
        }
        // Reasoning models accept an effort dial; "off" means "send
        // nothing" for endpoints without one. DeepSeek's V4 models think by
        // default, so "off" must disable thinking explicitly there.
        if let Some(effort) = req.effort.as_deref() {
            if effort == "off" {
                if self.deepseek {
                    body["thinking"] = json!({ "type": "disabled" });
                }
            } else {
                body["reasoning_effort"] = json!(effort);
            }
        }
        body
    }
}

/// Our neutral message maps to 1..n OpenAI messages: tool results become
/// separate `role:"tool"` messages, everything else stays in place.
fn flatten_message(msg: &Message, out: &mut Vec<Value>, vision: bool, keep_reasoning: bool) {
    match msg.role {
        Role::Assistant => {
            let mut text = String::new();
            let mut reasoning = String::new();
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
                    // Reasoning is not replayable on OpenAI's own API, but
                    // DeepSeek requires the reasoning_content of tool-call
                    // turns to be round-tripped on any later request that
                    // carries tools (it 400s without it).
                    ContentBlock::Thinking { thinking, .. } if keep_reasoning => {
                        reasoning.push_str(thinking)
                    }
                    ContentBlock::Thinking { .. } => {}
                    _ => {}
                }
            }
            // A turn that produced only reasoning has nothing replayable on
            // the chat-completions wire: `content: null` with no `tool_calls`
            // is a hard 400 ("content or tool_calls must be set"). DeepSeek
            // documents pure-thinking turns as droppable; drop them here too.
            if text.is_empty() && tool_calls.is_empty() {
                return;
            }
            let mut m = json!({ "role": "assistant" });
            m["content"] = if text.is_empty() {
                Value::Null
            } else {
                Value::String(text)
            };
            if !tool_calls.is_empty() {
                m["tool_calls"] = Value::Array(tool_calls);
                if keep_reasoning && !reasoning.is_empty() {
                    m["reasoning_content"] = Value::String(reasoning);
                }
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
                            "{content}\n[{} image(s) omitted: {}]",
                            images.len(),
                            if vision {
                                "this API cannot carry images returned from a tool"
                            } else {
                                "this model cannot view images; use the view_image tool to delegate"
                            }
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
                    ContentBlock::Image { media_type, data } if vision => {
                        has_image = true;
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": { "url": format!("data:{media_type};base64,{data}") },
                        }));
                    }
                    ContentBlock::Image { .. } => parts.push(json!({
                        "type": "text",
                        "text": "[image omitted: this model cannot view images; use the view_image tool to delegate]",
                    })),
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
    // OpenAI reports cached input under `prompt_tokens_details.cached_tokens`;
    // DeepSeek uses the top-level `prompt_cache_hit_tokens` field instead.
    let cached = v["prompt_tokens_details"]
        .get("cached_tokens")
        .and_then(|c| c.as_u64())
        .or_else(|| v.get("prompt_cache_hit_tokens").and_then(|c| c.as_u64()))
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

    fn supports_vision(&self) -> bool {
        self.vision
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

#[cfg(test)]
mod tests {
    use super::*;
    use tcode_core::{ContentBlock, Message, Role};

    fn watchdog() -> WatchdogConfig {
        WatchdogConfig {
            idle_timeout_secs: 5,
            connect_timeout_secs: 20,
            max_retries: 1,
            initial_backoff_ms: 1,
            max_backoff_ms: 10,
        }
    }

    fn req(effort: Option<&str>) -> Request {
        Request {
            model: "deepseek-v4-flash".into(),
            system: "sys".into(),
            system_suffix: None,
            cache_scope: None,
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
            }],
            tools: vec![],
            max_tokens: Some(1000),
            effort: effort.map(str::to_string),
        }
    }

    fn provider(base_url: &str) -> OpenAiProvider {
        OpenAiProvider::new(
            "k".into(),
            "deepseek-v4-flash".into(),
            Some(base_url.into()),
            watchdog(),
        )
    }

    /// The field is optional here, and omitting it is the point: it leaves the
    /// endpoint's own output limit in force instead of a number of ours that
    /// the model's reasoning would be spent against first.
    #[test]
    fn an_unset_cap_sends_no_max_tokens_at_all() {
        let mut request = req(None);
        request.max_tokens = None;
        let body = provider("https://api.deepseek.com").build_body(&request);
        assert!(body.get("max_tokens").is_none(), "{body}");

        let capped = provider("https://api.deepseek.com").build_body(&req(None));
        assert_eq!(capped["max_tokens"], json!(1000));
    }

    #[test]
    fn deepseek_off_effort_disables_thinking() {
        let body = provider("https://api.deepseek.com").build_body(&req(Some("off")));
        assert_eq!(body["thinking"], json!({ "type": "disabled" }));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn deepseek_effort_sends_reasoning_effort_only() {
        let body = provider("https://api.deepseek.com").build_body(&req(Some("high")));
        assert_eq!(body["reasoning_effort"], "high");
        assert!(
            body.get("thinking").is_none(),
            "thinking is on by default on DeepSeek"
        );
    }

    #[test]
    fn openai_off_effort_sends_nothing() {
        let body = provider("https://api.openai.com/v1").build_body(&req(Some("off")));
        assert!(body.get("thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    fn tool_turn() -> Message {
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "need the weather".into(),
                    signature: None,
                },
                ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "get_weather".into(),
                    input: json!({ "city": "beijing" }),
                },
            ],
        }
    }

    #[test]
    fn deepseek_tool_turn_round_trips_reasoning_content() {
        let mut out = Vec::new();
        flatten_message(&tool_turn(), &mut out, true, true);
        assert_eq!(out[0]["reasoning_content"], "need the weather");
        assert!(out[0]["tool_calls"].is_array());
    }

    #[test]
    fn openai_tool_turn_still_drops_reasoning() {
        let mut out = Vec::new();
        flatten_message(&tool_turn(), &mut out, true, false);
        assert!(out[0].get("reasoning_content").is_none());
        assert!(out[0]["tool_calls"].is_array());
    }

    #[test]
    fn thinking_only_turn_is_dropped_not_nulled() {
        let mut out = Vec::new();
        flatten_message(
            &Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Thinking {
                    thinking: "ponder ponder".into(),
                    signature: None,
                }],
            },
            &mut out,
            true,
            true,
        );
        assert!(
            out.is_empty(),
            "a pure-thinking assistant turn must not replay as content:null without tool_calls"
        );
    }

    #[test]
    fn usage_reads_deepseek_cache_hit_field() {
        let u = usage_from(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 5,
            "prompt_cache_hit_tokens": 90,
        }));
        assert_eq!(u.cache_read_tokens, 90);
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 5);
    }

    #[test]
    fn usage_reads_openai_cached_tokens_first() {
        let u = usage_from(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 5,
            "prompt_tokens_details": { "cached_tokens": 80 },
        }));
        assert_eq!(u.cache_read_tokens, 80);
        assert_eq!(u.input_tokens, 20);
    }
}
