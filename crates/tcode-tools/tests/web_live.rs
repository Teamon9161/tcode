//! Live-network smoke tests for web_fetch / web_search. Ignored by default
//! (CI and normal runs stay offline); run explicitly with:
//! `cargo test -p tcode-tools --test web_live -- --ignored --nocapture`

use serde_json::json;
use tokio_util::sync::CancellationToken;

use tcode_core::{PermissionMode, PermissionRules, Session, ToolCtx};

fn ctx() -> Session {
    Session::new(
        ToolCtx::new(std::env::temp_dir(), 4000),
        PermissionMode::Default,
        PermissionRules::default(),
    )
}

#[tokio::test]
#[ignore = "hits the real network"]
async fn web_fetch_reads_a_real_page() {
    let session = ctx();
    let tools = tcode_tools::builtin_tools(&std::env::temp_dir());
    let fetch = tools.iter().find(|t| t.name() == "web_fetch").unwrap();
    let out = fetch
        .run(
            json!({"url": "https://example.com/"}),
            &session.tool_ctx,
            &CancellationToken::new(),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("Example Domain"), "{}", out.content);
}

#[tokio::test]
#[ignore = "hits the real network"]
async fn web_fetch_extracts_main_content_of_an_article() {
    let session = ctx();
    let tools = tcode_tools::builtin_tools(&std::env::temp_dir());
    let fetch = tools.iter().find(|t| t.name() == "web_fetch").unwrap();
    let url = "https://en.wikipedia.org/wiki/Rust_(programming_language)";
    let out = fetch
        .run(
            json!({"url": url}),
            &session.tool_ctx,
            &CancellationToken::new(),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("(main content"), "{}", out.content);
    println!("{}", out.content);

    // raw=true returns the unstripped page under its own cache key.
    let raw = fetch
        .run(
            json!({"url": url, "raw": true}),
            &session.tool_ctx,
            &CancellationToken::new(),
        )
        .await;
    assert!(!raw.is_error, "{}", raw.content);
    assert!(!raw.content.contains("(main content"), "{}", raw.content);
    assert!(
        raw.content.len() > out.content.len(),
        "raw should be larger"
    );
}

#[tokio::test]
#[ignore = "hits the real network"]
async fn web_search_returns_results() {
    let session = ctx();
    let tools = tcode_tools::builtin_tools(&std::env::temp_dir());
    let search = tools.iter().find(|t| t.name() == "web_search").unwrap();
    let out = search
        .run(
            json!({"query": "rust tokio async runtime"}),
            &session.tool_ctx,
            &CancellationToken::new(),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("1. "), "{}", out.content);
    assert!(out.content.contains("http"), "{}", out.content);
    println!("{}", out.content);
}
