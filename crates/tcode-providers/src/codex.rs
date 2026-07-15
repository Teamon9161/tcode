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
use tcode_core::{
    CacheStrategy, ContentBlock, EventStream, Message, Provider, ProviderError, RateLimit,
    RateLimits, Request, Role, StopReason, StreamEvent,
};

use crate::idle::{classify, idle_guard};
use crate::retry::{short, with_connect_timeout};

const BACKEND_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Codex CLI's OAuth client id; the refresh grant is tied to it.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

pub struct CodexProvider {
    http: reqwest::Client,
    model: String,
    watchdog: WatchdogConfig,
    /// Stable per-session id. The backend *overwrites* the body's
    /// `prompt_cache_key` with this header, so it — not the body — is what
    /// scopes the prompt cache.
    session_id: uuid::Uuid,
    vision: bool,
}

impl CodexProvider {
    pub fn new(model: String, watchdog: WatchdogConfig) -> Self {
        Self {
            http: crate::http::client(),
            model,
            watchdog,
            session_id: uuid::Uuid::new_v4(),
            vision: true,
        }
    }

    pub fn with_vision(mut self, vision: bool) -> Self {
        self.vision = vision;
        self
    }

    /// One cache id per conversation. Derived rather than random so a scope
    /// keeps its cache across calls, and distinct so the classifier and the
    /// sub-agents never share the main session's id.
    fn session_id(&self, req: &Request) -> String {
        match req.cache_scope.as_deref() {
            None => self.session_id.to_string(),
            Some(scope) => uuid::Uuid::new_v5(&self.session_id, scope.as_bytes()).to_string(),
        }
    }

    fn build_body(&self, req: &Request) -> Value {
        let mut input: Vec<Value> = Vec::new();
        for msg in &req.messages {
            push_items(msg, &mut input, self.vision);
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
        let instructions = match &req.system_suffix {
            Some(suffix) => format!("{}\n\n{suffix}", req.system),
            None => req.system.clone(),
        };
        let mut body = json!({
            "model": req.model,
            "instructions": instructions,
            "input": input,
            "tools": tools,
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "store": false,
            "stream": true,
            // The subscription endpoint 400s on `max_output_tokens` at any
            // value, so `req.max_tokens` cannot be honoured here. Callers that
            // need a short answer (the Auto Mode classifier) must get it from
            // the prompt, not from a cap.
            // Without this the reasoning items come back unreplayable.
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": self.session_id(req),
        });
        match req.effort.as_deref() {
            // `off` is our name for "do not reason"; the Responses API spells
            // it `none`. Sending `off` verbatim is a 400. Absent effort stays
            // absent: that means "server default", which is not the same thing.
            Some("off") => body["reasoning"] = json!({ "effort": "none", "summary": "auto" }),
            Some(effort) => body["reasoning"] = json!({ "effort": effort, "summary": "auto" }),
            None => body["reasoning"] = json!({ "summary": "auto" }),
        }
        body
    }

    async fn send(
        &self,
        auth: &codex::CodexAuth,
        body: &Value,
        session_id: &str,
    ) -> Result<reqwest::Response, reqwest::Error> {
        self.http
            .post(BACKEND_URL)
            .bearer_auth(&auth.access_token)
            .header("chatgpt-account-id", &auth.account_id)
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", "codex_cli_rs")
            .header("accept", "text/event-stream")
            .header("session_id", session_id)
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
    async fn connect(
        &self,
        body: &Value,
        session_id: &str,
    ) -> Result<(reqwest::Response, Option<RateLimits>), ProviderError> {
        let mut auth = codex::load_auth().ok_or_else(|| {
            ProviderError::Config(
                "no ChatGPT credentials found (~/.codex/auth.json); run `codex login`".into(),
            )
        })?;
        let mut refreshed = false;
        loop {
            match self.send(&auth, body, session_id).await {
                Ok(resp) if resp.status().is_success() => {
                    let limits = rate_limits_from_headers(resp.headers());
                    return Ok((resp, limits));
                }
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
fn push_items(msg: &Message, out: &mut Vec<Value>, vision: bool) {
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
                    ContentBlock::Image { media_type, data } if vision => Some(json!({
                        "type": "input_image",
                        "image_url": format!("data:{media_type};base64,{data}"),
                    })),
                    ContentBlock::Image { .. } => Some(json!({
                        "type": "input_text",
                        "text": "[image omitted: this model cannot view images; use the view_image tool to delegate]",
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

    fn supports_vision(&self) -> bool {
        self.vision
    }

    async fn stream(
        &self,
        req: Request,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        let body = self.build_body(&req);
        let session_id = self.session_id(&req);
        let (resp, header_limits) = with_connect_timeout(
            self.watchdog.connect_timeout(),
            self.connect(&body, &session_id),
        )
        .await?;

        let mut sse = idle_guard(resp.bytes_stream(), self.watchdog.idle_timeout()).eventsource();
        let raw: EventStream = Box::pin(stream! {
            let mut saw_tool_use = false;
            // Codex reports subscription usage only in the response headers of
            // each /responses call — never in the SSE body — so this is the one
            // place we learn it.
            if let Some(limits) = header_limits {
                yield Ok(StreamEvent::RateLimits(limits));
            }
            while let Some(item) = sse.next().await {
                let event = match item {
                    Ok(e) => e,
                    Err(e) => {
                        yield Err(classify(e));
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

        Ok(Box::pin(raw.take_until(cancel.cancelled_owned())))
    }
}

/// Subscription usage rides on the response headers of every /responses call,
/// mirroring the `x-codex-*` family the Codex CLI reads. `used_percent` is the
/// real signal (and all the status line renders); the window/reset fields are
/// best-effort and default to 0 when absent.
fn rate_limits_from_headers(headers: &reqwest::header::HeaderMap) -> Option<RateLimits> {
    let window = |kind: &str| -> Option<RateLimit> {
        let used = header_f64(headers, &format!("x-codex-{kind}-used-percent"))?;
        Some(RateLimit {
            used_percent: used,
            window_minutes: header_u64(headers, &format!("x-codex-{kind}-window-minutes"))
                .unwrap_or(0),
            resets_at: header_u64(headers, &format!("x-codex-{kind}-reset-at")).unwrap_or(0),
        })
    };
    Some(RateLimits {
        primary: window("primary")?,
        secondary: window("secondary"),
    })
}

fn header_f64(headers: &reqwest::header::HeaderMap, name: &str) -> Option<f64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

fn header_u64(headers: &reqwest::header::HeaderMap, name: &str) -> Option<u64> {
    // reset-at is a unix timestamp the server sends as a signed int; treat any
    // negative/garbage value as "unknown" rather than failing the whole parse.
    let raw: i64 = headers.get(name)?.to_str().ok()?.trim().parse().ok()?;
    u64::try_from(raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderName};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    fn request(effort: Option<&str>, cache_scope: Option<&str>) -> Request {
        Request {
            model: "gpt-5.6-luna".into(),
            system: "stable policy".into(),
            system_suffix: Some("fast stage".into()),
            cache_scope: cache_scope.map(String::from),
            messages: vec![],
            tools: vec![],
            max_tokens: 16,
            effort: effort.map(String::from),
        }
    }

    fn body_with_effort(effort: Option<&str>) -> Value {
        let provider = CodexProvider::new("gpt-5.6-luna".into(), WatchdogConfig::default());
        provider.build_body(&request(effort, None))
    }

    /// The backend 400s on both `max_output_tokens` and `"effort":"off"`, which
    /// took Auto Mode's fast stage offline on every classification.
    #[test]
    fn classifier_fast_stage_maps_off_to_none_and_sends_no_output_cap() {
        let body = body_with_effort(Some("off"));
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(
            body["reasoning"],
            json!({ "effort": "none", "summary": "auto" })
        );
        assert_eq!(body["instructions"], json!("stable policy\n\nfast stage"));
    }

    /// The backend keys the prompt cache off the `session_id` header, which it
    /// also writes back over the body's `prompt_cache_key`. A scope must
    /// therefore keep one id across calls, and never borrow another's.
    #[test]
    fn each_cache_scope_gets_its_own_stable_session_id() {
        let provider = CodexProvider::new("gpt-5.6-luna".into(), WatchdogConfig::default());
        let main = provider.session_id(&request(None, None));
        let classifier = provider.session_id(&request(None, Some("auto-classifier")));
        let sub_agent = provider.session_id(&request(None, Some("task-explore-0")));

        assert_eq!(main, provider.session_id(&request(Some("high"), None)));
        assert_eq!(
            classifier,
            provider.session_id(&request(Some("off"), Some("auto-classifier")))
        );
        assert_ne!(main, classifier);
        assert_ne!(main, sub_agent);
        assert_ne!(classifier, sub_agent);
        assert_eq!(
            provider.build_body(&request(None, Some("auto-classifier")))["prompt_cache_key"],
            json!(classifier)
        );
    }

    #[test]
    fn absent_effort_means_server_default_not_none() {
        assert_eq!(
            body_with_effort(None)["reasoning"],
            json!({ "summary": "auto" })
        );
        assert_eq!(
            body_with_effort(Some("high"))["reasoning"],
            json!({ "effort": "high", "summary": "auto" })
        );
    }

    #[test]
    fn rate_limits_come_from_x_codex_headers() {
        let limits = rate_limits_from_headers(&headers(&[
            ("x-codex-primary-used-percent", "30.5"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-at", "1704069000"),
            ("x-codex-secondary-used-percent", "66"),
            ("x-codex-secondary-window-minutes", "10080"),
        ]))
        .expect("primary present");
        assert_eq!(limits.primary.used_percent, 30.5);
        assert_eq!(limits.primary.window_minutes, 300);
        assert_eq!(limits.primary.resets_at, 1704069000);
        let weekly = limits.secondary.expect("secondary present");
        assert_eq!(weekly.used_percent, 66.0);
        assert_eq!(weekly.resets_at, 0); // absent header defaults to 0
    }

    #[test]
    fn no_primary_header_means_no_snapshot() {
        // Without the used-percent signal there is nothing to show.
        assert!(
            rate_limits_from_headers(&headers(&[("x-codex-primary-window-minutes", "300")]))
                .is_none()
        );
    }

    #[test]
    fn primary_only_leaves_secondary_none() {
        let limits =
            rate_limits_from_headers(&headers(&[("x-codex-primary-used-percent", "12")])).unwrap();
        assert!(limits.secondary.is_none());
    }
}
