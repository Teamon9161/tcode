//! Codex backend: the Responses API endpoint the Codex CLI uses
//! (`chatgpt.com/backend-api/codex/responses`), authenticated with the
//! OAuth tokens from `~/.codex/auth.json`. No API key involved — usage
//! bills against the ChatGPT subscription. Named for the backend, not the
//! protocol, because that OAuth/Codex path is what sets it apart; a plain
//! Chat Completions endpoint is `OpenAiProvider`.
//!
//! Wire differences from Chat Completions worth knowing:
//! - History is a flat list of typed *items* (message / function_call /
//!   function_call_output / reasoning), not role messages.
//! - With `store: false` the model's chain-of-thought comes back as an
//!   encrypted reasoning item that MUST be replayed before its
//!   function_call on the next request; we stash the whole item JSON in
//!   `ContentBlock::Thinking.signature`.

use async_stream::stream;
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::codex;
use tcode_core::config::WatchdogConfig;
use tcode_core::stream_util::with_idle_timeout;
use tcode_core::{
    CacheStrategy, ContentBlock, EventStream, Message, Provider, ProviderError, RateLimit,
    RateLimits, Request, Role, StopReason, StreamEvent,
};

use crate::retry::{short, with_connect_timeout};

const BACKEND_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Codex CLI's OAuth client id; the refresh grant is tied to it.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

pub struct CodexProvider {
    http: reqwest::Client,
    model: String,
    watchdog: WatchdogConfig,
    /// Stable per-session id; doubles as the prompt cache key.
    session_id: String,
}

impl CodexProvider {
    pub fn new(model: String, watchdog: WatchdogConfig) -> Self {
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("reqwest client"),
            model,
            watchdog,
            session_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    fn build_body(&self, req: &Request) -> Value {
        let mut input: Vec<Value> = Vec::new();
        for msg in &req.messages {
            push_items(msg, &mut input);
        }
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "strict": false,
                    "parameters": t.input_schema,
                })
            })
            .collect();
        let mut body = json!({
            "model": req.model,
            "instructions": req.system,
            "input": input,
            "tools": tools,
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "store": false,
            "stream": true,
            // Without this the reasoning items come back unreplayable.
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": self.session_id,
        });
        if let Some(effort) = req.effort.as_deref() {
            body["reasoning"] = json!({ "effort": effort, "summary": "auto" });
        } else {
            body["reasoning"] = json!({ "summary": "auto" });
        }
        body
    }

    async fn send(
        &self,
        auth: &codex::CodexAuth,
        body: &Value,
    ) -> Result<reqwest::Response, reqwest::Error> {
        self.http
            .post(BACKEND_URL)
            .bearer_auth(&auth.access_token)
            .header("chatgpt-account-id", &auth.account_id)
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", "codex_cli_rs")
            .header("accept", "text/event-stream")
            .header("session_id", &self.session_id)
            .json(body)
            .send()
            .await
    }

    /// Exchange the refresh token for fresh credentials and persist
    /// them back to auth.json (same as Codex itself does).
    async fn refresh(&self, auth: &codex::CodexAuth) -> Result<codex::CodexAuth, ProviderError> {
        if auth.refresh_token.is_empty() {
            return Err(ProviderError::Config(
                "ChatGPT token expired and no refresh token available; run `codex login`".into(),
            ));
        }
        let resp = self
            .http
            .post(TOKEN_URL)
            .json(&json!({
                "client_id": CLIENT_ID,
                "grant_type": "refresh_token",
                "refresh_token": auth.refresh_token,
                "scope": "openid profile email",
            }))
            .send()
            .await
            .map_err(|e| ProviderError::Network(format!("token refresh: {e}")))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ProviderError::Config(format!(
                "ChatGPT token refresh failed ({status}): {}; run `codex login`",
                short(&body)
            )));
        }
        let v: Value = serde_json::from_str(&body)
            .map_err(|e| ProviderError::BadResponse(format!("token refresh: {e}")))?;
        let access = v["access_token"].as_str().unwrap_or_default().to_string();
        if access.is_empty() {
            return Err(ProviderError::BadResponse(
                "token refresh returned no access_token".into(),
            ));
        }
        let refresh = v["refresh_token"].as_str().unwrap_or_default().to_string();
        codex::save_tokens(&access, &refresh, v["id_token"].as_str());
        Ok(codex::CodexAuth {
            access_token: access,
            refresh_token: if refresh.is_empty() {
                auth.refresh_token.clone()
            } else {
                refresh
            },
            account_id: auth.account_id.clone(),
        })
    }

    /// One connection attempt; a 401 triggers a single token refresh and one
    /// resend. Backoff retries are the agent loop's job, so failures surface as
    /// a classified error rather than being retried silently here.
    async fn connect(&self, body: &Value) -> Result<reqwest::Response, ProviderError> {
        let mut auth = codex::load_auth().ok_or_else(|| {
            ProviderError::Config(
                "no ChatGPT credentials found (~/.codex/auth.json); run `codex login`".into(),
            )
        })?;
        let mut refreshed = false;
        loop {
            match self.send(&auth, body).await {
                Ok(resp) if resp.status().is_success() => return Ok(resp),
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if status == 401 && !refreshed {
                        refreshed = true;
                        auth = self.refresh(&auth).await?;
                        continue;
                    }
                    let text = resp.text().await.unwrap_or_default();
                    return Err(ProviderError::Api {
                        status,
                        message: short(&text),
                    });
                }
                Err(e) => return Err(ProviderError::Network(e.to_string())),
            }
        }
    }
}

/// Our neutral message → Responses API items.
fn push_items(msg: &Message, out: &mut Vec<Value>) {
    match msg.role {
        Role::Assistant => {
            for block in &msg.content {
                match block {
                    // The signature holds the raw reasoning item.
                    ContentBlock::Thinking {
                        signature: Some(sig),
                        ..
                    } => {
                        if let Ok(item) = serde_json::from_str::<Value>(sig) {
                            out.push(item);
                        }
                    }
                    ContentBlock::Text { text } => out.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    })),
                    ContentBlock::ToolUse { id, name, input } => out.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": serde_json::to_string(input).unwrap_or_default(),
                    })),
                    _ => {}
                }
            }
        }
        Role::User => {
            for block in &msg.content {
                if let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    images,
                    ..
                } = block
                {
                    // function_call_output takes a text string; images can't
                    // ride along, so note their omission honestly.
                    let output = if images.is_empty() {
                        content.clone()
                    } else {
                        format!(
                            "{content}\n[{} image(s) omitted: images returned from a tool \
                             cannot be viewed by this model]",
                            images.len()
                        )
                    };
                    out.push(json!({
                        "type": "function_call_output",
                        "call_id": tool_use_id,
                        "output": output,
                    }));
                }
            }
            let parts: Vec<Value> = msg
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => {
                        Some(json!({ "type": "input_text", "text": text }))
                    }
                    ContentBlock::Image { media_type, data } => Some(json!({
                        "type": "input_image",
                        "image_url": format!("data:{media_type};base64,{data}"),
                    })),
                    _ => None,
                })
                .collect();
            if !parts.is_empty() {
                out.push(json!({ "type": "message", "role": "user", "content": parts }));
            }
        }
    }
}

fn usage_from(v: &Value) -> tcode_core::Usage {
    let input = v["input_tokens"].as_u64().unwrap_or(0);
    let cached = v["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);
    tcode_core::Usage {
        input_tokens: input.saturating_sub(cached),
        output_tokens: v["output_tokens"].as_u64().unwrap_or(0),
        cache_read_tokens: cached,
        cache_write_tokens: 0,
    }
}

#[async_trait]
impl Provider for CodexProvider {
    fn name(&self) -> &str {
        "codex"
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
        let resp =
            with_connect_timeout(self.watchdog.connect_timeout(), self.connect(&body)).await?;

        let mut sse = resp.bytes_stream().eventsource();
        let raw: EventStream = Box::pin(stream! {
            let mut saw_tool_use = false;
            while let Some(item) = sse.next().await {
                let event = match item {
                    Ok(e) => e,
                    Err(e) => {
                        yield Err(ProviderError::Network(e.to_string()));
                        return;
                    }
                };
                let data: Value = match serde_json::from_str(&event.data) {
                    Ok(v) => v,
                    Err(_) => continue, // e.g. "[DONE]"
                };
                match data["type"].as_str().unwrap_or_default() {
                    "response.created" => yield Ok(StreamEvent::Started),
                    "response.output_text.delta" => {
                        if let Some(t) = data["delta"].as_str() {
                            yield Ok(StreamEvent::TextDelta(t.to_string()));
                        }
                    }
                    "response.reasoning_summary_text.delta" => {
                        if let Some(t) = data["delta"].as_str() {
                            yield Ok(StreamEvent::ThinkingDelta(t.to_string()));
                        }
                    }
                    "response.reasoning_summary_part.done" => {
                        yield Ok(StreamEvent::ThinkingDelta("\n\n".into()));
                    }
                    "response.output_item.added" => {
                        let item = &data["item"];
                        if item["type"] == "function_call" {
                            saw_tool_use = true;
                            yield Ok(StreamEvent::ToolUseStart {
                                index: data["output_index"].as_u64().unwrap_or(0) as usize,
                                id: item["call_id"].as_str().unwrap_or_default().to_string(),
                                name: item["name"].as_str().unwrap_or_default().to_string(),
                            });
                        }
                    }
                    "response.function_call_arguments.delta" => {
                        if let Some(frag) = data["delta"].as_str() {
                            yield Ok(StreamEvent::ToolUseInputDelta {
                                index: data["output_index"].as_u64().unwrap_or(0) as usize,
                                fragment: frag.to_string(),
                            });
                        }
                    }
                    "response.output_item.done" => {
                        let item = &data["item"];
                        // Keep the full encrypted reasoning item for
                        // replay (minus transient status field).
                        if item["type"] == "reasoning" {
                            let mut keep = item.clone();
                            keep.as_object_mut().map(|o| o.remove("status"));
                            yield Ok(StreamEvent::ThinkingSignature(keep.to_string()));
                        }
                    }
                    "response.completed" => {
                        let resp = &data["response"];
                        if let Some(limits) = rate_limits_from(resp.get("rate_limits").unwrap_or(&data["rate_limits"])) {
                            yield Ok(StreamEvent::RateLimits(limits));
                        }
                        yield Ok(StreamEvent::Usage(usage_from(&resp["usage"])));
                        let stop = if saw_tool_use {
                            StopReason::ToolUse
                        } else {
                            StopReason::EndTurn
                        };
                        yield Ok(StreamEvent::Done(stop));
                        return;
                    }
                    "response.failed" | "error" => {
                        let msg = data["response"]["error"]["message"]
                            .as_str()
                            .or(data["message"].as_str())
                            .unwrap_or("response failed")
                            .to_string();
                        yield Err(ProviderError::Api { status: 0, message: msg });
                        return;
                    }
                    _ => {}
                }
            }
            yield Err(ProviderError::BadResponse(
                "stream ended without response.completed".into(),
            ));
        });

        let guarded = with_idle_timeout(raw, self.watchdog.idle_timeout());
        Ok(Box::pin(guarded.take_until(cancel.cancelled_owned())))
    }
}

fn rate_limits_from(value: &Value) -> Option<RateLimits> {
    let parse = |value: &Value| {
        Some(RateLimit {
            used_percent: value["used_percent"].as_f64()?,
            window_minutes: value["window_minutes"].as_u64()?,
            resets_at: value["resets_at"].as_u64()?,
        })
    };
    Some(RateLimits {
        primary: parse(&value["primary"])?,
        secondary: parse(&value["secondary"]),
    })
}
