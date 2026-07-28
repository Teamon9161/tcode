use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "tcode-{name}-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create temporary directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn acp_stdio_initializes_and_creates_a_session_without_provider_io() {
    let temp = TempDir::new("acp-stdio");
    let config = temp.path().join("config.toml");
    fs::write(
        &config,
        r#"
default_profile = "probe"

[profiles.probe]
provider = "anthropic"
api_key = "not-a-real-key"
model = "claude-sonnet-4-5"
"#,
    )
    .expect("write temporary config");

    let cwd = Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .expect("canonicalize project cwd");
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientCapabilities": {},
            "clientInfo": { "name": "test", "version": "0" }
        }
    });
    let session_new = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": { "cwd": cwd, "mcpServers": [] }
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_tcode"))
        .args([
            "--acp",
            "--config",
            config.to_str().expect("UTF-8 config path"),
        ])
        .env("TCODE_HOME", temp.path().join("home"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start tcode ACP server");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    writeln!(stdin, "{initialize}").expect("send initialize");
    writeln!(stdin, "{session_new}").expect("send session/new");
    drop(stdin);

    let output = child.wait_with_output().expect("wait for ACP server");
    assert!(
        output.status.success(),
        "ACP server exited unsuccessfully: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let frames = String::from_utf8(output.stdout)
        .expect("ACP stdout is UTF-8")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("stdout line is JSON-RPC"))
        .collect::<Vec<_>>();
    assert_eq!(
        frames.len(),
        2,
        "ACP emitted only the two request responses"
    );
    assert_eq!(frames[0].pointer("/id"), Some(&json!(1)));
    assert_eq!(
        frames[0].pointer("/result/protocolVersion"),
        Some(&json!(1))
    );
    assert_eq!(frames[1].pointer("/id"), Some(&json!(2)));
    assert!(
        frames[1]
            .pointer("/result/sessionId")
            .and_then(Value::as_str)
            .is_some_and(|id| id.starts_with("tcode-")),
        "session/new did not return an ACP session id: {}",
        frames[1]
    );
}
