//! Wire-level tests: real HTTP + SSE against a scripted local server.
//! Verifies SSE parsing, connect retry, and the idle-timeout watchdog
//! without touching a real API.

use std::time::Duration;

use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use tcode_core::config::WatchdogConfig;
use tcode_core::{ContentBlock, Message, Provider, ProviderError, Request, Role, StreamEvent};
use tcode_providers::{AnthropicProvider, OpenAiProvider};

/// One scripted HTTP response; `chunks` are written with delays between
/// them. If `stall_after` is set the connection is held open silently.
struct Script {
    status: &'static str,
    chunks: Vec<&'static str>,
    delay: Duration,
    stall_after: bool,
}

impl Script {
    fn ok(chunks: Vec<&'static str>) -> Self {
        Self {
            status: "200 OK",
            chunks,
            delay: Duration::ZERO,
            stall_after: false,
        }
    }
}

/// Serve each scripted response to consecutive connections; returns base URL.
async fn serve(scripts: Vec<Script>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for script in scripts {
            let (mut sock, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => return,
            };
            // Drain the request: headers, then content-length body.
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let body_start;
            loop {
                let n = sock.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = find_headers_end(&buf) {
                    body_start = pos;
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&buf[..body_start]).to_lowercase();
            let content_length: usize = headers
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            let mut have = buf.len() - body_start;
            while have < content_length {
                let n = sock.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                have += n;
            }

            let head = format!(
                "HTTP/1.1 {}\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                script.status
            );
            let _ = sock.write_all(head.as_bytes()).await;
            for chunk in &script.chunks {
                if !script.delay.is_zero() {
                    tokio::time::sleep(script.delay).await;
                }
                let _ = sock.write_all(chunk.as_bytes()).await;
                let _ = sock.flush().await;
            }
            if script.stall_after {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }
    });
    format!("http://{addr}")
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn request() -> Request {
    Request {
        model: "test-model".into(),
        system: "test".into(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }],
        tools: vec![],
        max_tokens: 128,
    }
}

fn watchdog(idle_secs: u64) -> WatchdogConfig {
    WatchdogConfig {
        idle_timeout_secs: idle_secs,
        max_retries: 3,
        initial_backoff_ms: 10,
    }
}

const ANTHROPIC_HAPPY: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":3,"cache_creation_input_tokens":5}}}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
    "\n\n",
    "event: message_stop\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n",
);

#[tokio::test]
async fn anthropic_happy_path() {
    let base = serve(vec![Script::ok(vec![ANTHROPIC_HAPPY])]).await;
    let p = AnthropicProvider::new("key".into(), "test-model".into(), Some(base), watchdog(5));
    let stream = p.stream(request(), CancellationToken::new()).await.unwrap();
    let events: Vec<_> = stream.collect().await;

    let mut acc = tcode_core::accumulate::ResponseAccumulator::new();
    for ev in &events {
        acc.feed(ev.as_ref().expect("no stream errors"));
    }
    let (blocks, usage, stop) = acc.finish();
    assert!(matches!(&blocks[0], ContentBlock::Text { text } if text == "Hello"));
    assert_eq!(usage.input_tokens, 10);
    assert_eq!(usage.output_tokens, 7);
    assert_eq!(usage.cache_read_tokens, 3);
    assert_eq!(usage.cache_write_tokens, 5);
    assert_eq!(stop, Some(tcode_core::StopReason::EndTurn));
}

#[tokio::test]
async fn anthropic_retries_5xx_then_succeeds() {
    let base = serve(vec![
        Script {
            status: "500 Internal Server Error",
            chunks: vec!["boom"],
            delay: Duration::ZERO,
            stall_after: false,
        },
        Script::ok(vec![ANTHROPIC_HAPPY]),
    ])
    .await;
    let p = AnthropicProvider::new("key".into(), "test-model".into(), Some(base), watchdog(5));
    let stream = p.stream(request(), CancellationToken::new()).await.unwrap();
    let events: Vec<_> = stream.collect().await;
    assert!(events.iter().all(|e| e.is_ok()));
    assert!(events
        .iter()
        .any(|e| matches!(e, Ok(StreamEvent::Done(_)))));
}

#[tokio::test]
async fn watchdog_fires_on_mid_stream_stall() {
    let base = serve(vec![Script {
        status: "200 OK",
        chunks: vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n",
        ],
        delay: Duration::ZERO,
        stall_after: true,
    }])
    .await;
    let p = AnthropicProvider::new("key".into(), "test-model".into(), Some(base), watchdog(1));
    let stream = p.stream(request(), CancellationToken::new()).await.unwrap();
    let events: Vec<_> = stream.collect().await;
    assert!(
        matches!(events.last(), Some(Err(ProviderError::IdleTimeout(_)))),
        "expected idle timeout, got {:?}",
        events.last()
    );
}

#[tokio::test]
async fn cancellation_ends_stream() {
    let base = serve(vec![Script {
        status: "200 OK",
        chunks: vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n",
        ],
        delay: Duration::ZERO,
        stall_after: true,
    }])
    .await;
    let p = AnthropicProvider::new("key".into(), "test-model".into(), Some(base), watchdog(30));
    let cancel = CancellationToken::new();
    let mut stream = p.stream(request(), cancel.clone()).await.unwrap();
    // Consume the first events, then cancel; stream must end promptly.
    let first = stream.next().await;
    assert!(first.is_some());
    cancel.cancel();
    while let Some(item) = stream.next().await {
        assert!(item.is_ok());
    }
}

const OPENAI_HAPPY: &str = concat!(
    r#"data: {"choices":[{"delta":{"role":"assistant","content":"Hi"},"index":0}]}"#,
    "\n\n",
    r#"data: {"choices":[{"delta":{"content":" there"},"index":0}]}"#,
    "\n\n",
    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":""}}]},"index":0}]}"#,
    "\n\n",
    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"a.rs\"}"}}]},"index":0}]}"#,
    "\n\n",
    r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls","index":0}]}"#,
    "\n\n",
    r#"data: {"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":9,"prompt_tokens_details":{"cached_tokens":12}}}"#,
    "\n\n",
    "data: [DONE]\n\n",
);

#[tokio::test]
async fn openai_happy_path_with_tool_call() {
    let base = serve(vec![Script::ok(vec![OPENAI_HAPPY])]).await;
    let p = OpenAiProvider::new("key".into(), "test-model".into(), Some(base), watchdog(5));
    let stream = p.stream(request(), CancellationToken::new()).await.unwrap();
    let events: Vec<_> = stream.collect().await;

    let mut acc = tcode_core::accumulate::ResponseAccumulator::new();
    for ev in &events {
        acc.feed(ev.as_ref().expect("no stream errors"));
    }
    let (blocks, usage, stop) = acc.finish();
    assert!(matches!(&blocks[0], ContentBlock::Text { text } if text == "Hi there"));
    match &blocks[1] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "call_1");
            assert_eq!(name, "read");
            assert_eq!(input["path"], "a.rs");
        }
        other => panic!("expected tool use, got {other:?}"),
    }
    // Normalized: input excludes cached tokens.
    assert_eq!(usage.input_tokens, 8);
    assert_eq!(usage.cache_read_tokens, 12);
    assert_eq!(usage.output_tokens, 9);
    assert_eq!(stop, Some(tcode_core::StopReason::ToolUse));
}
