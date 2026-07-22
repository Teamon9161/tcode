//! MCP stdio client integration test against a scripted python server.
//! Skips silently when python is not installed (CI without python stays
//! green; the protocol logic is still covered wherever python exists).

use serde_json::json;
use tokio_util::sync::CancellationToken;

use tcode_core::config::McpServerConfig;
use tcode_core::{PermissionRequest, ToolCtx};

const FAKE_SERVER: &str = r#"
import sys, json

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    msg = json.loads(line)
    m = msg.get("method")
    if m == "initialize":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fake", "version": "0"}}})
    elif m == "tools/list":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"tools": [{
            "name": "echo",
            "description": "Echo the text back",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]}})
    elif m == "tools/call":
        args = msg["params"]["arguments"]
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {
            "content": [{"type": "text", "text": "echo: " + args.get("text", "")}]}})
"#;

fn python_available() -> bool {
    std::process::Command::new("python")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[tokio::test]
async fn mcp_client_lists_and_calls_tools() {
    if !python_available() {
        eprintln!("skipping: python not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("fake_mcp.py");
    std::fs::write(&script, FAKE_SERVER).unwrap();

    let mut servers = std::collections::BTreeMap::new();
    servers.insert(
        "fake".to_string(),
        McpServerConfig {
            command: "python".into(),
            args: vec![script.to_string_lossy().into_owned()],
            env: Default::default(),
        },
    );
    let (tools, warnings) = tcode_tools::connect_mcp_servers(&servers, dir.path()).await;
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(tools.len(), 1);

    let tool = &tools[0];
    assert_eq!(tool.name(), "mcp__fake__echo");
    assert_eq!(tool.description(), "Echo the text back");
    // Permission descriptor matches the documented rule shape.
    match tool.permission(&json!({})) {
        PermissionRequest::Ask { descriptor, .. } => assert_eq!(descriptor, "mcp__fake__echo"),
        other => panic!("expected Ask, got {other:?}"),
    }

    let ctx = ToolCtx::for_test(dir.path().to_path_buf(), 2000);
    let out = tool
        .run(json!({"text": "hi"}), &ctx, &CancellationToken::new())
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content, "echo: hi");
}

#[tokio::test]
async fn broken_mcp_server_warns_instead_of_failing() {
    let mut servers = std::collections::BTreeMap::new();
    servers.insert(
        "ghost".to_string(),
        McpServerConfig {
            command: "definitely-not-a-real-command-xyz".into(),
            args: vec![],
            env: Default::default(),
        },
    );
    let dir = tempfile::tempdir().unwrap();
    let (tools, warnings) = tcode_tools::connect_mcp_servers(&servers, dir.path()).await;
    assert!(tools.is_empty());
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("ghost"), "{}", warnings[0]);
}
