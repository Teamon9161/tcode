//! ACP v1 agent server for editor integrations such as Zed.
//!
//! This crate owns protocol transport and adaptation only. The agent loop,
//! session ledger, permission policy, and tools remain in the protocol-agnostic
//! core/frontend crates.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Mutex, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use tcode_core::config::{Config, McpServerConfig};
use tcode_core::{
    Agent, AgentEvent, Approval, ApprovalDecision, Approver, ContentBlock, ModelCell, Session,
};

const ACP_VERSION: u64 = 1;
const JSONRPC_INVALID_REQUEST: i64 = -32600;
const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
const JSONRPC_INVALID_PARAMS: i64 = -32602;
const JSONRPC_INTERNAL_ERROR: i64 = -32603;

/// Inputs selected by the root binary before it enters protocol-only stdio
/// mode. Configuration is loaded per `session/new`, because ACP sessions may
/// select different working directories in the same process.
#[derive(Clone)]
pub struct ServeSpec {
    pub config_file: PathBuf,
    pub profile: Option<String>,
    pub model: Option<String>,
    pub mode: Option<String>,
    pub agent: Option<String>,
}

/// Serve ACP v1 over the process's stdin/stdout. Stdout is reserved exclusively
/// for JSON-RPC frames; diagnostics belong on stderr.
pub async fn serve_stdio(spec: ServeSpec) -> anyhow::Result<()> {
    let (outbound, mut outbound_rx) = mpsc::channel::<Value>(128);
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(message) = outbound_rx.recv().await {
            let mut line = message.to_string();
            line.push('\n');
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if stdout.flush().await.is_err() {
                break;
            }
        }
    });

    let state = Arc::new(ServerState {
        spec,
        rpc: Rpc::new(outbound),
        sessions: Mutex::new(HashMap::new()),
        initialized: AtomicBool::new(false),
    });

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    while let Some(line) = lines.next_line().await.context("cannot read ACP stdin")? {
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(error) => {
                state
                    .rpc
                    .notification_error(
                        None,
                        JSONRPC_INVALID_REQUEST,
                        format!("invalid JSON-RPC: {error}"),
                    )
                    .await;
                continue;
            }
        };
        dispatch(state.clone(), message).await;
    }

    drop(state);
    writer.abort();
    let _ = writer.await;
    Ok(())
}

struct ServerState {
    spec: ServeSpec,
    rpc: Rpc,
    sessions: Mutex<HashMap<String, Arc<AcpSession>>>,
    initialized: AtomicBool,
}

struct AcpSession {
    agent: Arc<Agent>,
    session: Mutex<Session>,
    active_cancel: Mutex<Option<CancellationToken>>,
    turn: Arc<Semaphore>,
}

#[derive(Clone)]
struct Rpc {
    outbound: mpsc::Sender<Value>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    next_id: Arc<AtomicU64>,
}

impl Rpc {
    fn new(outbound: mpsc::Sender<Value>) -> Self {
        Self {
            outbound,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    async fn send(&self, message: Value) -> Result<(), ()> {
        self.outbound.send(message).await.map_err(|_| ())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), ()> {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    async fn response(&self, id: Value, result: Value) {
        let _ = self
            .send(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
            .await;
    }

    async fn error(&self, id: Value, code: i64, message: impl Into<String>) {
        let _ = self
            .send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": code, "message": message.into() }
            }))
            .await;
    }

    async fn notification_error(&self, id: Option<Value>, code: i64, message: impl Into<String>) {
        if let Some(id) = id {
            self.error(id, code, message).await;
        } else {
            eprintln!("ACP protocol error: {}", message.into());
        }
    }

    async fn receive_response(&self, id: &Value, message: Value) -> bool {
        let key = id_key(id);
        let tx = self.pending.lock().await.remove(&key);
        if let Some(tx) = tx {
            let _ = tx.send(message);
            true
        } else {
            false
        }
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let key = id.to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(key.clone(), tx);
        if self
            .send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await
            .is_err()
        {
            self.pending.lock().await.remove(&key);
            return Err("ACP client disconnected before it could receive the request".into());
        }
        tokio::select! {
            response = rx => match response {
                Ok(message) => {
                    if let Some(error) = message.get("error") {
                        Err(error.get("message").and_then(Value::as_str).unwrap_or("ACP client rejected the request").to_string())
                    } else {
                        Ok(message.get("result").cloned().unwrap_or(Value::Null))
                    }
                }
                Err(_) => Err("ACP client disconnected before answering".into()),
            },
            _ = cancel.cancelled() => {
                self.pending.lock().await.remove(&key);
                Err("prompt cancelled".into())
            }
        }
    }
}

async fn dispatch(state: Arc<ServerState>, message: Value) {
    if let Some(id) = message.get("id") {
        if message.get("method").is_none() && state.rpc.receive_response(id, message.clone()).await
        {
            return;
        }
    }

    let Some(method) = message.get("method").and_then(Value::as_str) else {
        state
            .rpc
            .notification_error(
                message.get("id").cloned(),
                JSONRPC_INVALID_REQUEST,
                "JSON-RPC message needs a method or a matching response id",
            )
            .await;
        return;
    };
    let id = message.get("id").cloned();
    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));

    match method {
        "initialize" => {
            let Some(id) = id else {
                eprintln!("ACP protocol error: initialize must be a request");
                return;
            };
            initialize(&state, id, params).await;
        }
        "session/new" => {
            let Some(id) = id else {
                eprintln!("ACP protocol error: session/new must be a request");
                return;
            };
            new_session(state, id, params).await;
        }
        "session/prompt" => {
            let Some(id) = id else {
                eprintln!("ACP protocol error: session/prompt must be a request");
                return;
            };
            prompt(state, id, params).await;
        }
        "session/cancel" => cancel(&state, params).await,
        _ if id.is_some() => {
            state
                .rpc
                .error(
                    id.expect("checked"),
                    JSONRPC_METHOD_NOT_FOUND,
                    format!("unsupported ACP method '{method}'"),
                )
                .await;
        }
        _ => eprintln!("ACP protocol warning: ignored notification '{method}'"),
    }
}

async fn initialize(state: &ServerState, id: Value, params: Value) {
    let Some(client_version) = params.get("protocolVersion").and_then(Value::as_u64) else {
        state
            .rpc
            .error(
                id,
                JSONRPC_INVALID_PARAMS,
                "initialize.protocolVersion must be an integer",
            )
            .await;
        return;
    };
    state.initialized.store(true, Ordering::Release);
    // ACP requires returning the newest version we support if the peer asks for
    // something else. This server currently implements v1 only.
    let version = client_version.min(ACP_VERSION);
    state
        .rpc
        .response(
            id,
            json!({
                "protocolVersion": version,
                "agentCapabilities": {
                    "loadSession": false,
                    "promptCapabilities": { "image": false, "audio": false, "embeddedContext": false },
                    "mcpCapabilities": { "http": false, "sse": false },
                    "sessionCapabilities": {},
                    "auth": {}
                },
                "authMethods": [],
                "agentInfo": {
                    "name": "tcode",
                    "title": "tcode",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        )
        .await;
}

async fn new_session(state: Arc<ServerState>, id: Value, params: Value) {
    if !state.initialized.load(Ordering::Acquire) {
        state
            .rpc
            .error(
                id,
                JSONRPC_INVALID_REQUEST,
                "initialize must complete before session/new",
            )
            .await;
        return;
    }
    match build_session(&state, &params).await {
        Ok(session) => {
            let session_id = format!("tcode-{}", uuid::Uuid::new_v4());
            state
                .sessions
                .lock()
                .await
                .insert(session_id.clone(), Arc::new(session));
            state
                .rpc
                .response(id, json!({ "sessionId": session_id }))
                .await;
        }
        Err(error) => {
            state
                .rpc
                .error(
                    id,
                    JSONRPC_INVALID_PARAMS,
                    format!("cannot create ACP session: {error:#}"),
                )
                .await
        }
    }
}

async fn build_session(state: &ServerState, params: &Value) -> anyhow::Result<AcpSession> {
    let cwd = required_string(params, "cwd")?;
    let cwd = PathBuf::from(cwd);
    if !cwd.is_absolute() {
        anyhow::bail!("session/new.cwd must be absolute");
    }
    let cwd = cwd
        .canonicalize()
        .context("cannot canonicalize session cwd")?;
    let client_mcp = parse_stdio_mcp(params.get("mcpServers"))?;

    let mut config = Config::load_at(&state.spec.config_file, &cwd).with_context(|| {
        format!(
            "cannot load tcode configuration at {}",
            state.spec.config_file.display()
        )
    })?;
    tcode_providers::hydrate_codex_models(&mut config);
    let model_state = config.apply_active_preset();
    let selection = config.select(
        state.spec.profile.as_deref(),
        state.spec.model.as_deref(),
        &model_state,
    )?;
    let profile = config
        .profiles
        .get(&selection.profile)
        .context("selected ACP profile disappeared")?;
    let active = tcode_providers::build_active(profile, &selection, &config.watchdog)?;
    let model_cell = ModelCell::new(active);

    for (name, server) in client_mcp {
        if config.mcp_servers.contains_key(&name) {
            anyhow::bail!(
                "client MCP server '{name}' conflicts with [mcp_servers.{name}] in tcode config"
            );
        }
        config.mcp_servers.insert(name, server);
    }

    let tcode_frontend::Booted {
        agent,
        shell_filters,
        warnings,
        ..
    } = tcode_frontend::boot(tcode_frontend::BootSpec {
        cwd: cwd.clone(),
        config: &mut config,
        selection,
        model_cell: model_cell.clone(),
        agent: state.spec.agent.clone(),
        // No pane of our own: an ACP client draws the conversation, so `show`
        // would open nothing.
        display_tools: Vec::new(),
    })
    .await?;
    for warning in warnings {
        eprintln!("ACP session warning: {warning}");
    }

    let mode = tcode_frontend::startup_mode(state.spec.mode.as_deref(), &model_state, &config)?;
    let rules = tcode_frontend::startup_rules(&config);
    let session = tcode_frontend::open_session(tcode_frontend::SessionSpec {
        cwd,
        config: &config,
        state: &model_state,
        model_cell,
        mode,
        rules,
        resume: tcode_frontend::ResumeSpec::New,
        shell_filters,
        opening_context: Arc::new(tcode_tools::startup_context_with_scratch),
        environment: Arc::new(tcode_tools::environment_snapshot),
    })?;

    Ok(AcpSession {
        agent,
        session: Mutex::new(session),
        active_cancel: Mutex::new(None),
        turn: Arc::new(Semaphore::new(1)),
    })
}

fn parse_stdio_mcp(raw: Option<&Value>) -> anyhow::Result<BTreeMap<String, McpServerConfig>> {
    let Some(raw) = raw else {
        anyhow::bail!(
            "session/new.mcpServers is required (use an empty array when none are configured)"
        );
    };
    let servers = raw
        .as_array()
        .context("session/new.mcpServers must be an array")?;
    let mut parsed = BTreeMap::new();
    for server in servers {
        if server.get("type").is_some() {
            anyhow::bail!("only stdio MCP servers are supported in ACP phase 1");
        }
        let name = required_string(server, "name")?.to_string();
        let command = required_string(server, "command")?.to_string();
        let args = server
            .get("args")
            .and_then(Value::as_array)
            .context("stdio MCP args must be an array")?
            .iter()
            .map(|arg| {
                arg.as_str()
                    .map(str::to_string)
                    .context("stdio MCP args must contain strings")
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let env = server
            .get("env")
            .and_then(Value::as_array)
            .context("stdio MCP env must be an array")?
            .iter()
            .map(|entry| {
                Ok((
                    required_string(entry, "name")?.to_string(),
                    required_string(entry, "value")?.to_string(),
                ))
            })
            .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
        if parsed
            .insert(name.clone(), McpServerConfig { command, args, env })
            .is_some()
        {
            anyhow::bail!("duplicate client MCP server '{name}'");
        }
    }
    Ok(parsed)
}

async fn prompt(state: Arc<ServerState>, id: Value, params: Value) {
    let session_id = match required_string(&params, "sessionId") {
        Ok(id) => id.to_string(),
        Err(error) => {
            state
                .rpc
                .error(id, JSONRPC_INVALID_PARAMS, error.to_string())
                .await;
            return;
        }
    };
    let input = match prompt_blocks(params.get("prompt")) {
        Ok(blocks) => blocks,
        Err(error) => {
            state
                .rpc
                .error(id, JSONRPC_INVALID_PARAMS, error.to_string())
                .await;
            return;
        }
    };
    let Some(session) = state.sessions.lock().await.get(&session_id).cloned() else {
        state
            .rpc
            .error(id, JSONRPC_INVALID_PARAMS, "unknown ACP sessionId")
            .await;
        return;
    };
    let permit = match session.turn.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            state
                .rpc
                .error(
                    id,
                    JSONRPC_INVALID_REQUEST,
                    "an ACP prompt is already running for this session",
                )
                .await;
            return;
        }
    };
    let rpc = state.rpc.clone();
    tokio::spawn(async move {
        run_prompt(rpc, session_id, session, input, id, permit).await;
    });
}

async fn run_prompt(
    rpc: Rpc,
    session_id: String,
    acp_session: Arc<AcpSession>,
    input: Vec<ContentBlock>,
    request_id: Value,
    _permit: OwnedSemaphorePermit,
) {
    let cancel = CancellationToken::new();
    *acp_session.active_cancel.lock().await = Some(cancel.clone());
    let updates = Arc::new(Updates::default());
    let approver = AcpApprover {
        rpc: rpc.clone(),
        session_id: session_id.clone(),
        cancel: cancel.clone(),
        updates: updates.clone(),
    };
    let (events_tx, events_rx) = mpsc::channel(32);
    let was_interrupted = Arc::new(AtomicBool::new(false));
    let pump = tokio::spawn(pump_events(
        rpc.clone(),
        session_id.clone(),
        events_rx,
        updates,
        was_interrupted.clone(),
    ));
    let result = {
        let mut session = acp_session.session.lock().await;
        acp_session
            .agent
            .user_turn(&mut session, input, &events_tx, &approver, cancel.clone())
            .await
    };
    drop(events_tx);
    let _ = pump.await;
    *acp_session.active_cancel.lock().await = None;

    if cancel.is_cancelled() || was_interrupted.load(Ordering::Acquire) {
        rpc.response(request_id, json!({ "stopReason": "cancelled" }))
            .await;
    } else if let Err(error) = result {
        rpc.error(
            request_id,
            JSONRPC_INTERNAL_ERROR,
            format!("tcode turn failed: {error}"),
        )
        .await;
    } else {
        rpc.response(request_id, json!({ "stopReason": "end_turn" }))
            .await;
    }
}

async fn cancel(state: &ServerState, params: Value) {
    let Ok(session_id) = required_string(&params, "sessionId") else {
        eprintln!("ACP protocol warning: ignored session/cancel without sessionId");
        return;
    };
    let session = state.sessions.lock().await.get(session_id).cloned();
    if let Some(session) = session {
        if let Some(cancel) = session.active_cancel.lock().await.as_ref() {
            cancel.cancel();
        }
    }
}

fn prompt_blocks(raw: Option<&Value>) -> anyhow::Result<Vec<ContentBlock>> {
    let blocks = raw
        .context("session/prompt.prompt is required")?
        .as_array()
        .context("session/prompt.prompt must be an array")?;
    blocks
        .iter()
        .map(|block| match block.get("type").and_then(Value::as_str) {
            Some("text") => Ok(ContentBlock::Text {
                text: required_string(block, "text")?.to_string(),
            }),
            // ACP requires resource links as a baseline. tcode has no distinct
            // resource-link input block, so preserve their identity as trusted
            // client-supplied context rather than silently discarding them.
            Some("resource_link") => Ok(ContentBlock::Text {
                text: format!(
                    "[ACP resource link: {} ({})]",
                    required_string(block, "name")?,
                    required_string(block, "uri")?
                ),
            }),
            Some(kind) => {
                anyhow::bail!("prompt content type '{kind}' is not supported by tcode ACP phase 1")
            }
            None => anyhow::bail!("every prompt block needs a type"),
        })
        .collect()
}

#[derive(Default)]
struct Updates {
    announced_tools: Mutex<HashSet<String>>,
    next_message: AtomicU64,
    agent_message_id: Mutex<Option<String>>,
    thought_message_id: Mutex<Option<String>>,
}

impl Updates {
    async fn announce_tool(&self, rpc: &Rpc, session_id: &str, tool_call: Value) -> Result<(), ()> {
        let call_id = tool_call
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        self.announced_tools.lock().await.insert(call_id);
        rpc.notify(
            "session/update",
            json!({ "sessionId": session_id, "update": tool_call }),
        )
        .await
    }

    async fn was_announced(&self, call_id: &str) -> bool {
        self.announced_tools.lock().await.contains(call_id)
    }

    async fn message_id(&self, prefix: &str) -> String {
        let slot = match prefix {
            "agent" => &self.agent_message_id,
            "thought" => &self.thought_message_id,
            _ => unreachable!("only ACP message stream kinds use message ids"),
        };
        let mut message_id = slot.lock().await;
        message_id
            .get_or_insert_with(|| {
                format!(
                    "{prefix}-{}",
                    self.next_message.fetch_add(1, Ordering::Relaxed)
                )
            })
            .clone()
    }
}

struct AcpApprover {
    rpc: Rpc,
    session_id: String,
    cancel: CancellationToken,
    updates: Arc<Updates>,
}

#[async_trait]
impl Approver for AcpApprover {
    async fn ask(
        &self,
        tool: &str,
        summary: &str,
        descriptor: &str,
        is_edit: bool,
        allows_project: bool,
        input: &Value,
    ) -> Approval {
        // Delegated approvals have no provider call id at this boundary. They
        // still fail closed; the primary tcode tool path uses ask_with_call.
        let call_id = format!("delegated-{}", uuid::Uuid::new_v4());
        self.ask_with_call(
            &call_id,
            tool,
            summary,
            descriptor,
            is_edit,
            allows_project,
            input,
        )
        .await
    }

    async fn ask_with_call(
        &self,
        call_id: &str,
        tool: &str,
        summary: &str,
        _descriptor: &str,
        is_edit: bool,
        _allows_project: bool,
        input: &Value,
    ) -> Approval {
        let kind = tool_kind(tool);
        let pending = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": call_id,
            "title": summary,
            "kind": kind,
            "status": "pending",
            "rawInput": input
        });
        if self
            .updates
            .announce_tool(&self.rpc, &self.session_id, pending.clone())
            .await
            .is_err()
        {
            return denied("ACP client disconnected before approval");
        }
        let options = if is_edit {
            vec![
                json!({ "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" }),
                json!({ "optionId": "allow_session", "name": "Allow all edits this session", "kind": "allow_always" }),
                json!({ "optionId": "reject", "name": "Reject", "kind": "reject_once" }),
            ]
        } else {
            vec![
                json!({ "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" }),
                json!({ "optionId": "allow_session", "name": "Allow this action this session", "kind": "allow_always" }),
                json!({ "optionId": "reject", "name": "Reject", "kind": "reject_once" }),
            ]
        };
        let result = self
            .rpc
            .request(
                "session/request_permission",
                json!({
                    "sessionId": self.session_id,
                    "toolCall": pending,
                    "options": options
                }),
                &self.cancel,
            )
            .await;
        match result {
            Ok(result) => match result.pointer("/outcome/outcome").and_then(Value::as_str) {
                Some("selected") => match result
                    .pointer("/outcome/optionId")
                    .and_then(Value::as_str)
                {
                    Some("allow_once") => Approval::simple(ApprovalDecision::Yes, None),
                    Some("allow_session") => Approval::simple(ApprovalDecision::YesSession, None),
                    _ => denied("ACP client rejected the requested action"),
                },
                _ => denied("ACP permission was cancelled or rejected"),
            },
            Err(error) => denied(&error),
        }
    }
}

fn denied(reason: &str) -> Approval {
    Approval::simple(ApprovalDecision::No, Some(reason.to_string()))
}

async fn pump_events(
    rpc: Rpc,
    session_id: String,
    mut events: mpsc::Receiver<AgentEvent>,
    updates: Arc<Updates>,
    interrupted: Arc<AtomicBool>,
) {
    while let Some(event) = events.recv().await {
        match event {
            AgentEvent::TextDelta(text) => {
                let message_id = updates.message_id("agent").await;
                let _ = send_update(
                    &rpc,
                    &session_id,
                    json!({
                        "sessionUpdate": "agent_message_chunk",
                        "messageId": message_id,
                        "content": { "type": "text", "text": text }
                    }),
                )
                .await;
            }
            AgentEvent::ThinkingDelta(text) => {
                let message_id = updates.message_id("thought").await;
                let _ = send_update(
                    &rpc,
                    &session_id,
                    json!({
                        "sessionUpdate": "agent_thought_chunk",
                        "messageId": message_id,
                        "content": { "type": "text", "text": text }
                    }),
                )
                .await;
            }
            AgentEvent::ToolStart {
                call_id,
                name,
                summary,
                input,
            } => {
                if updates.was_announced(&call_id).await {
                    let _ = send_update(
                        &rpc,
                        &session_id,
                        json!({
                            "sessionUpdate": "tool_call_update",
                            "toolCallId": call_id,
                            "status": "in_progress"
                        }),
                    )
                    .await;
                } else {
                    let _ = updates
                        .announce_tool(
                            &rpc,
                            &session_id,
                            json!({
                                "sessionUpdate": "tool_call",
                                "toolCallId": call_id,
                                "title": summary,
                                "kind": tool_kind(&name),
                                "status": "in_progress",
                                "rawInput": input
                            }),
                        )
                        .await;
                }
            }
            AgentEvent::ToolEnd {
                call_id,
                content,
                is_error,
                ..
            } => {
                let _ = send_update(
                    &rpc,
                    &session_id,
                    json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": call_id,
                        "status": if is_error { "failed" } else { "completed" },
                        "content": [{
                            "type": "content",
                            "content": { "type": "text", "text": content }
                        }]
                    }),
                )
                .await;
            }
            AgentEvent::Interrupted => interrupted.store(true, Ordering::Release),
            _ => {}
        }
    }
}

async fn send_update(rpc: &Rpc, session_id: &str, update: Value) -> Result<(), ()> {
    rpc.notify(
        "session/update",
        json!({ "sessionId": session_id, "update": update }),
    )
    .await
}

fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" => "read",
        "write" | "edit" | "append" => "edit",
        "grep" | "glob" => "search",
        "shell" | "bash" | "monitor" => "execute",
        "web_fetch" | "web_search" => "fetch",
        _ => "other",
    }
}

fn required_string<'a>(value: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("'{key}' must be a string"))
}

fn id_key(id: &Value) -> String {
    id.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_accepts_the_two_baseline_acp_content_types() {
        let blocks = prompt_blocks(Some(&json!([
            { "type": "text", "text": "hello" },
            { "type": "resource_link", "name": "main.rs", "uri": "file:///tmp/main.rs" }
        ])))
        .unwrap();
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn prompt_rejects_capability_gated_content() {
        let error = prompt_blocks(Some(&json!([{ "type": "image", "data": "abc" }]))).unwrap_err();
        assert!(error.to_string().contains("not supported"));
    }

    #[test]
    fn rejects_http_mcp_until_the_capability_is_implemented() {
        let error = parse_stdio_mcp(Some(&json!([{
            "type": "http",
            "name": "remote",
            "url": "https://example.test/mcp"
        }])))
        .unwrap_err();
        assert!(error.to_string().contains("stdio"));
    }

    #[test]
    fn maps_builtin_tool_kinds() {
        assert_eq!(tool_kind("edit"), "edit");
        assert_eq!(tool_kind("bash"), "execute");
        assert_eq!(tool_kind("grep"), "search");
    }

    #[tokio::test]
    async fn message_chunks_reuse_an_id_per_stream() {
        let updates = Updates::default();
        assert_eq!(
            updates.message_id("agent").await,
            updates.message_id("agent").await
        );
        assert_ne!(
            updates.message_id("agent").await,
            updates.message_id("thought").await
        );
    }

    #[tokio::test]
    async fn permission_request_uses_the_pending_tool_call_id() {
        let (outbound, mut frames) = mpsc::channel(4);
        let rpc = Rpc::new(outbound);
        let responder = rpc.clone();
        let approver = AcpApprover {
            rpc,
            session_id: "session-1".into(),
            cancel: CancellationToken::new(),
            updates: Arc::new(Updates::default()),
        };
        let approval = tokio::spawn(async move {
            approver
                .ask_with_call(
                    "tool-42",
                    "edit",
                    "update main.rs",
                    "edit(main.rs)",
                    true,
                    false,
                    &json!({ "path": "main.rs" }),
                )
                .await
        });

        let pending = frames.recv().await.expect("pending tool update");
        assert_eq!(
            pending.pointer("/params/update/toolCallId"),
            Some(&json!("tool-42"))
        );
        let request = frames.recv().await.expect("permission request");
        assert_eq!(request["method"], "session/request_permission");
        assert_eq!(
            request.pointer("/params/toolCall/toolCallId"),
            Some(&json!("tool-42"))
        );
        responder
            .receive_response(
                request.get("id").expect("request id"),
                json!({
                    "result": { "outcome": { "outcome": "selected", "optionId": "allow_once" } }
                }),
            )
            .await;
        assert_eq!(
            approval.await.expect("approval task").decision,
            ApprovalDecision::Yes
        );
    }
}
