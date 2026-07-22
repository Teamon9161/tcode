//! MCP client (stdio transport). Each configured server is spawned once at
//! startup; its tools register through the normal `Tool` trait under
//! `mcp__server__tool`, which is also the permission-rule descriptor.
//!
//! Wire format: newline-delimited JSON-RPC 2.0 per the MCP stdio spec.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use tcode_core::config::McpServerConfig;
use tcode_core::{ContentBlock, PermissionRequest, Tool, ToolCtx, ToolOutput};

const PROTOCOL_VERSION: &str = "2025-06-18";
const INIT_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Cap on how many images one result may put into context. A browser server
/// can return a screenshot per step; the text still accounts for every one of
/// them, but the pixels stop here.
const MAX_RESULT_IMAGES: usize = 8;
/// Upper bound on a single encoded payload before it is decoded. Decoding
/// itself is separately bounded inside `normalize_image`.
const MAX_SOURCE_IMAGE_BYTES: usize = 16 * 1024 * 1024;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

pub struct McpClient {
    server: String,
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
}

impl McpClient {
    /// Spawn the server process, run the initialize handshake and list its
    /// tools. Any failure returns a message suitable for a startup warning.
    pub async fn connect(
        server: &str,
        config: &McpServerConfig,
        cwd: &std::path::Path,
    ) -> Result<(Arc<Self>, Vec<Arc<dyn Tool>>), String> {
        // Windows: resolve .cmd/.bat shims (npx, mise, …) through cmd.exe,
        // which CreateProcess alone does not do.
        let mut cmd = if cfg!(windows) {
            let mut c = tokio::process::Command::new("cmd");
            c.arg("/c").arg(&config.command).args(&config.args);
            c
        } else {
            let mut c = tokio::process::Command::new(&config.command);
            c.args(&config.args);
            c
        };
        cmd.envs(&config.env)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            format!(
                "mcp server '{server}': failed to start {}: {e}",
                config.command
            )
        })?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let pending: Pending = Arc::default();

        // Reader task: routes responses to their waiting request by id.
        // It owns the child so the process lives as long as the pipe does
        // and dies with tcode (kill_on_drop).
        let reader_pending = pending.clone();
        let reader_server = server.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let Some(id) = msg.get("id").and_then(Value::as_u64) else {
                    continue; // notification from the server
                };
                if msg.get("method").is_some() {
                    continue; // server-initiated request; none supported
                }
                let result = if let Some(err) = msg.get("error") {
                    Err(format!(
                        "{} (code {})",
                        err.get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("error"),
                        err.get("code").and_then(Value::as_i64).unwrap_or(0),
                    ))
                } else {
                    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                };
                if let Some(tx) = reader_pending.lock().expect("mcp pending lock").remove(&id) {
                    let _ = tx.send(result);
                }
            }
            // Pipe closed: fail everything still waiting.
            for (_, tx) in reader_pending.lock().expect("mcp pending lock").drain() {
                let _ = tx.send(Err(format!("mcp server '{reader_server}' exited")));
            }
            drop(child);
        });

        let client = Arc::new(Self {
            server: server.to_string(),
            stdin: tokio::sync::Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(1),
        });

        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "tcode", "version": env!("CARGO_PKG_VERSION") }
                }),
                INIT_TIMEOUT,
            )
            .await?;
        client
            .notify("notifications/initialized", json!({}))
            .await?;

        let tools = list_tools(&client).await?;
        Ok((client, tools))
    }

    async fn send(&self, msg: Value) -> Result<(), String> {
        let mut line = msg.to_string();
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("mcp server '{}': write failed: {e}", self.server))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("mcp pending lock")
            .insert(id, tx);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!("mcp server '{}' exited", self.server)),
            Err(_) => {
                self.pending.lock().expect("mcp pending lock").remove(&id);
                Err(format!(
                    "mcp server '{}': {method} timed out after {}s",
                    self.server,
                    timeout.as_secs()
                ))
            }
        }
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<(String, bool, Vec<ContentBlock>), String> {
        let result = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
                CALL_TIMEOUT,
            )
            .await?;
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // Rendering decodes and rescales images; keep that off the reactor.
        let (content, images) = tokio::task::spawn_blocking(move || render_content(&result))
            .await
            .map_err(|e| format!("mcp result decoding failed: {e}"))?;
        Ok((content, is_error, images))
    }
}

async fn list_tools(client: &Arc<McpClient>) -> Result<Vec<Arc<dyn Tool>>, String> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let params = match &cursor {
            Some(c) => json!({ "cursor": c }),
            None => json!({}),
        };
        let result = client.request("tools/list", params, INIT_TIMEOUT).await?;
        for def in result
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(name) = def.get("name").and_then(Value::as_str) else {
                continue;
            };
            tools.push(Arc::new(McpTool {
                client: client.clone(),
                tool_name: name.to_string(),
                full_name: format!("mcp__{}__{name}", client.server),
                description: def
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                schema: def
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object" })),
            }));
        }
        cursor = result
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(String::from);
        if cursor.is_none() {
            return Ok(tools);
        }
    }
}

/// Flatten an MCP content array to model-readable text plus the image blocks
/// that ride alongside it. Images go through the same normalization budget as
/// file reads and clipboard pastes, so a server cannot spend more context on
/// one picture than `read` can.
fn render_content(result: &Value) -> (String, Vec<ContentBlock>) {
    let Some(items) = result.get("content").and_then(Value::as_array) else {
        return (result.to_string(), Vec::new());
    };
    let mut images = Vec::new();
    let parts: Vec<String> = items
        .iter()
        .filter_map(|item| match item.get("type").and_then(Value::as_str) {
            Some("text") => item.get("text").and_then(Value::as_str).map(str::to_owned),
            Some("image") => Some(take_image(
                item.get("data").and_then(Value::as_str),
                &mut images,
            )),
            Some("resource") => item
                .pointer("/resource/text")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    item.pointer("/resource/blob")
                        .and_then(Value::as_str)
                        .map(|blob| take_image(Some(blob), &mut images))
                })
                .or_else(|| {
                    item.pointer("/resource/uri")
                        .and_then(Value::as_str)
                        .map(|uri| format!("(resource: {uri})"))
                }),
            _ => None,
        })
        .collect();
    let text = if parts.is_empty() {
        "(empty result)".to_string()
    } else {
        parts.join("\n")
    };
    (text, images)
}

/// Decode one base64 payload into a context-ready image block, returning the
/// text that stands in for it. A payload that cannot become an image always
/// leaves a trace in the text: silently dropping it would let the model reason
/// about a screenshot it never received. The declared mime type is ignored —
/// `normalize_image` sniffs magic bytes, and the server's claim is data.
fn take_image(data: Option<&str>, images: &mut Vec<ContentBlock>) -> String {
    use base64::Engine as _;

    let Some(data) = data else {
        return "(image dropped: no data)".to_string();
    };
    if images.len() >= MAX_RESULT_IMAGES {
        return format!("(image dropped: over {MAX_RESULT_IMAGES} images in one result)");
    }
    if data.len() / 4 * 3 > MAX_SOURCE_IMAGE_BYTES {
        return format!(
            "(image dropped: encoded payload exceeds {} MB)",
            MAX_SOURCE_IMAGE_BYTES / (1024 * 1024)
        );
    }
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
        return "(image dropped: payload is not valid base64)".to_string();
    };
    match tcode_core::images::normalize_image(&bytes) {
        Ok(image) => {
            let text = format!(
                "(image {}x{}, {:.0} KB)",
                image.width,
                image.height,
                image.bytes.len() as f64 / 1024.0
            );
            images.push(image.into_block());
            text
        }
        Err(error) => format!("(image dropped: {error})"),
    }
}

struct McpTool {
    client: Arc<McpClient>,
    tool_name: String,
    full_name: String,
    description: String,
    schema: Value,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.full_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.schema.clone()
    }

    fn permission(&self, _input: &Value) -> PermissionRequest {
        PermissionRequest::Ask {
            descriptor: self.full_name.clone(),
            aliases: Vec::new(),
            summary: format!("{} (mcp server '{}')", self.tool_name, self.client.server),
            is_edit: false,
        }
    }

    async fn run(&self, input: Value, _ctx: &ToolCtx, cancel: &CancellationToken) -> ToolOutput {
        let call = self.client.call_tool(&self.tool_name, input);
        tokio::select! {
            result = call => match result {
                Ok((content, is_error, images)) => ToolOutput {
                    content,
                    is_error,
                    images,
                },
                Err(e) => ToolOutput::err(e),
            },
            _ = cancel.cancelled() => {
                // The late response is dropped by the id map; nothing leaks.
                ToolOutput::err("mcp call cancelled by user".to_string())
            }
        }
    }
}

/// Connect every configured server. Failures never block startup — they
/// come back as warnings so a broken server cannot brick the CLI.
pub async fn connect_mcp_servers(
    servers: &std::collections::BTreeMap<String, McpServerConfig>,
    cwd: &std::path::Path,
) -> (Vec<Arc<dyn Tool>>, Vec<String>) {
    let mut tools = Vec::new();
    let mut warnings = Vec::new();
    for (name, config) in servers {
        match McpClient::connect(name, config, cwd).await {
            Ok((_client, server_tools)) => {
                if server_tools.is_empty() {
                    warnings.push(format!("mcp server '{name}' reported no tools"));
                }
                tools.extend(server_tools);
            }
            Err(e) => warnings.push(e),
        }
    }
    (tools, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_b64() -> String {
        use base64::Engine as _;
        let png = tcode_core::images::normalize_rgba(2, 2, vec![0; 16]).unwrap();
        base64::engine::general_purpose::STANDARD.encode(png.bytes)
    }

    #[test]
    fn content_rendering() {
        let result = json!({ "content": [
            { "type": "text", "text": "hello" },
            { "type": "image", "data": png_b64(), "mimeType": "image/png" },
            { "type": "resource", "resource": { "uri": "file:///x", "text": "body" } }
        ]});
        let (text, images) = render_content(&result);
        assert_eq!(text, "hello\n(image 2x2, 0 KB)\nbody");
        assert!(matches!(images.as_slice(), [ContentBlock::Image { .. }]));
        assert_eq!(
            render_content(&json!({ "content": [] })).0,
            "(empty result)"
        );
    }

    #[test]
    fn image_carried_by_an_embedded_resource_blob() {
        let result = json!({ "content": [
            { "type": "resource", "resource": { "uri": "ui://shot", "blob": png_b64() } }
        ]});
        let (text, images) = render_content(&result);
        assert_eq!(text, "(image 2x2, 0 KB)");
        assert_eq!(images.len(), 1);
    }

    /// A payload that cannot become an image must still be visible in the
    /// text, and must never take the whole result down with it.
    #[test]
    fn undecodable_payloads_degrade_to_text() {
        let result = json!({ "content": [
            { "type": "text", "text": "before" },
            { "type": "image", "data": "not base64 !!", "mimeType": "image/png" },
            { "type": "image", "mimeType": "image/png" },
            { "type": "image", "data": "aGVsbG8=", "mimeType": "image/png" },
            { "type": "text", "text": "after" },
        ]});
        let (text, images) = render_content(&result);
        assert!(images.is_empty());
        assert_eq!(
            text,
            "before\n(image dropped: payload is not valid base64)\n\
             (image dropped: no data)\n\
             (image dropped: unsupported image format)\nafter"
        );
    }

    #[test]
    fn images_beyond_the_cap_are_dropped_not_rendered() {
        let items: Vec<Value> = (0..MAX_RESULT_IMAGES + 2)
            .map(|_| json!({ "type": "image", "data": png_b64() }))
            .collect();
        let (text, images) = render_content(&json!({ "content": items }));
        assert_eq!(images.len(), MAX_RESULT_IMAGES);
        assert_eq!(text.matches("(image dropped: over").count(), 2);
    }
}
