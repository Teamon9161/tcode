use async_stream::stream;
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::config::WatchdogConfig;
use tcode_core::{
    CacheStrategy, ContentBlock, EventStream, Message, Provider, ProviderError, Request,
    StopReason, StreamEvent,
};

use crate::idle::{classify, idle_guard};
use crate::retry::connect_once;

const API_VERSION: &str = "2023-06-01";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

pub struct AnthropicProvider {
    http: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
    /// True when talking to the first-party Anthropic API (vs an
    /// Anthropic-compatible backend like DeepSeek). Decides the effort
    /// wire format: native uses adaptive thinking + `output_config.effort`;
    /// compatible backends still take the classic `thinking.budget_tokens`.
    native: bool,
    watchdog: WatchdogConfig,
}

impl AnthropicProvider {
    pub fn new(
        api_key: String,
        model: String,
        base_url: Option<String>,
        watchdog: WatchdogConfig,
    ) -> Self {
        // A `None` base_url means the default first-party endpoint; an
        // explicit URL is native only if it is on anthropic.com (matches
        // api.anthropic.com but not e.g. api.deepseek.com/anthropic).
        let native = base_url
            .as_deref()
            .is_none_or(|u| u.contains("anthropic.com"));
        Self {
            http: crate::http::client(),
            api_key,
            model,
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            native,
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
        let mut system = vec![json!({
            "type": "text",
            "text": req.system,
            "cache_control": { "type": "ephemeral" },
        })];
        if let Some(suffix) = &req.system_suffix {
            system.push(json!({ "type": "text", "text": suffix }));
        }
        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "stream": true,
            // Breakpoint after the stable system prefix; classifier stages put
            // their differing instruction into an uncached tail block.
            "system": system,
            "messages": messages,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        // Effort mapping. None (picker "auto") leaves the server default
        // untouched — always safe. "off" forces thinking off (some backends,
        // e.g. DeepSeek, think by default). Explicit low/medium/high depends
        // on the endpoint:
        //   - Native Anthropic (Opus 4.8, Sonnet 5, …) removed the legacy
        //     `thinking:{enabled,budget_tokens}` field (it now 400s) in
        //     favour of adaptive thinking guided by `output_config.effort`.
        //   - Compatible backends still take the classic budget form.
        // Fable 5 has no effort dial (its models carry no efforts), so it
        // only ever hits the None arm — thinking is omitted entirely, which
        // is what it requires.
        match req.effort.as_deref() {
            None => {}
            Some("off") => body["thinking"] = json!({ "type": "disabled" }),
            Some(effort) if self.native => {
                let level = match effort {
                    "low" | "medium" | "high" => effort,
                    _ => "high",
                };
                body["thinking"] = json!({ "type": "adaptive" });
                body["output_config"] = json!({ "effort": level });
            }
            Some(effort) => {
                let budget: u32 = match effort {
                    "low" => 4096,
                    "medium" => 12288,
                    _ => 24576,
                };
                body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
                // The API requires max_tokens > budget_tokens; keep the
                // configured amount available for the answer itself.
                body["max_tokens"] = json!(req.max_tokens.saturating_add(budget));
            }
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
            images,
        } => {
            // Anthropic tool_result content is an array, so image blocks ride
            // right next to the text — no separate user message needed.
            let mut parts = vec![json!({ "type": "text", "text": content })];
            for img in images {
                if let ContentBlock::Image { media_type, data } = img {
                    parts.push(json!({
                        "type": "image",
                        "source": { "type": "base64", "media_type": media_type, "data": data },
                    }));
                }
            }
            let mut v = json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": parts,
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
        let resp = connect_once(self.watchdog.connect_timeout(), || {
            self.http
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", API_VERSION)
                .json(&body)
                .send()
        })
        .await?;

        let mut sse = idle_guard(resp.bytes_stream(), self.watchdog.idle_timeout()).eventsource();
        let raw: EventStream = Box::pin(stream! {
            let mut stop_reason = StopReason::EndTurn;
            while let Some(item) = sse.next().await {
                let event = match item {
                    Ok(e) => e,
                    Err(e) => {
                        yield Err(classify(e));
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
                            // Required for replaying thinking blocks.
                            "signature_delta" => yield Ok(StreamEvent::ThinkingSignature(
                                delta["signature"].as_str().unwrap_or_default().to_string(),
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
            model: "m".into(),
            system: "sys".into(),
            system_suffix: None,
            cache_scope: None,
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
            }],
            tools: vec![],
            max_tokens: 1000,
            effort: effort.map(str::to_string),
        }
    }

    #[test]
    fn tool_result_inlines_image_blocks() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: "Read image shot.png (image/png, 4 KB).".into(),
            is_error: false,
            images: vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "AAAA".into(),
            }],
        };
        let v = block_to_json(&block).unwrap();
        let parts = v["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image");
        assert_eq!(parts[1]["source"]["media_type"], "image/png");
        assert_eq!(parts[1]["source"]["data"], "AAAA");
    }

    fn native() -> AnthropicProvider {
        AnthropicProvider::new("k".into(), "m".into(), None, watchdog())
    }

    fn compatible() -> AnthropicProvider {
        AnthropicProvider::new(
            "k".into(),
            "m".into(),
            Some("https://api.deepseek.com/anthropic".into()),
            watchdog(),
        )
    }

    #[test]
    fn base_url_decides_native() {
        assert!(native().native);
        assert!(!compatible().native);
        // An explicit first-party URL is still native.
        assert!(
            AnthropicProvider::new(
                "k".into(),
                "m".into(),
                Some("https://api.anthropic.com".into()),
                watchdog(),
            )
            .native
        );
    }

    #[test]
    fn classifier_suffix_follows_the_cached_system_prefix() {
        let mut request = req(Some("off"));
        request.system_suffix = Some("stage-specific verdict instruction".into());
        let system = native().build_body(&request)["system"].clone();
        let blocks = system.as_array().expect("system blocks");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["text"], "sys");
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(blocks[1]["text"], "stage-specific verdict instruction");
        assert!(blocks[1].get("cache_control").is_none());
    }

    #[test]
    fn none_effort_omits_thinking() {
        for p in [native(), compatible()] {
            let body = p.build_body(&req(None));
            assert!(body.get("thinking").is_none());
            assert!(body.get("output_config").is_none());
            assert_eq!(body["max_tokens"], json!(1000));
        }
    }

    #[test]
    fn off_disables_thinking_both_backends() {
        for p in [native(), compatible()] {
            let body = p.build_body(&req(Some("off")));
            assert_eq!(body["thinking"], json!({ "type": "disabled" }));
            assert!(body.get("output_config").is_none());
        }
    }

    #[test]
    fn native_uses_adaptive_and_output_config() {
        let body = native().build_body(&req(Some("medium")));
        assert_eq!(body["thinking"], json!({ "type": "adaptive" }));
        assert_eq!(body["output_config"], json!({ "effort": "medium" }));
        // No legacy budget bump on native.
        assert_eq!(body["max_tokens"], json!(1000));
    }

    #[test]
    fn compatible_uses_legacy_budget() {
        let body = compatible().build_body(&req(Some("medium")));
        assert_eq!(
            body["thinking"],
            json!({ "type": "enabled", "budget_tokens": 12288 })
        );
        assert!(body.get("output_config").is_none());
        assert_eq!(body["max_tokens"], json!(1000 + 12288));
    }
}
