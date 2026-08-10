//! The `browser` tool, driven with a fake shell in place of a window.
//!
//! No Electron, no `WebContentsView`, no CDP. What is under test is everything
//! this side of the pipe: which verb each action sends, what it does with the
//! answer, and the three refusals that are the tool's boundary rather than its
//! behaviour (`../AGENT-BROWSER.md`).
//!
//! That the *other* side answers those verbs is pinned separately and
//! differently — `dispatch.rs` reads `electron/browser.js` — because nothing
//! here can execute JavaScript, and a test that pretended otherwise would be
//! testing its own fake.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::{AutoSafety, PermissionRequest, Tool, ToolCtx};

use tcode_app::browser::BrowserTool;
use tcode_app::sidecar::Shell;

/// Stands in for the Electron main process: records the calls, answers with
/// whatever the test queued.
#[derive(Default)]
struct FakeShell {
    calls: Mutex<Vec<(String, Value)>>,
    replies: Mutex<Vec<Result<Value, String>>>,
}

impl FakeShell {
    fn answering(replies: Vec<Result<Value, String>>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into_iter().rev().collect()),
        })
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl Shell for FakeShell {
    async fn call(&self, method: &str, args: Value) -> Result<Value, String> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), args.clone()));
        self.replies
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| Ok(Value::Null))
    }
}

fn ctx() -> ToolCtx {
    ToolCtx::for_test(std::env::temp_dir(), 8_000)
}

async fn run(tool: &BrowserTool, input: Value) -> tcode_core::ToolOutput {
    tool.run(input, &ctx(), &CancellationToken::new()).await
}

/// A minimal accessibility tree: a heading and a link inside a wrapper.
fn page() -> Value {
    json!({
        "url": "https://example.com/docs",
        "title": "Docs",
        "nodes": [
            { "nodeId": "1", "role": { "value": "RootWebArea" }, "name": { "value": "Docs" },
              "backendDOMNodeId": 1, "ignored": false },
            { "nodeId": "2", "parentId": "1", "role": { "value": "generic" },
              "name": { "value": "" }, "backendDOMNodeId": 2, "ignored": false },
            { "nodeId": "3", "parentId": "2", "role": { "value": "heading" },
              "name": { "value": "Getting started" }, "backendDOMNodeId": 3, "ignored": false },
            { "nodeId": "4", "parentId": "2", "role": { "value": "link" },
              "name": { "value": "Install" }, "backendDOMNodeId": 44, "ignored": false }
        ]
    })
}

/// A tab is opened without `select`, and that omission is the whole of "an
/// agent never takes the screen". The shell reads the flag strictly, so sending
/// nothing is sending "background".
#[tokio::test]
async fn opening_a_tab_does_not_ask_for_the_screen() {
    let shell = FakeShell::answering(vec![Ok(json!("tab-7"))]);
    let tool = BrowserTool::new(shell.clone(), None);

    let out = run(&tool, json!({ "action": "open" })).await;

    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        out.ui_metadata,
        Some(tcode_core::ToolUiMetadata::BrowserTab { id: "tab-7".into() })
    );
    let (method, args) = shell.calls().remove(0);
    assert_eq!(method, "browser_open");
    assert_eq!(
        args.get("select"),
        None,
        "the tool asked for the screen: {args}"
    );
}

/// The address bar's guesswork, reused rather than reimplemented: a model gets
/// the same answer for `localhost:5173` as somebody typing it.
#[tokio::test]
async fn navigating_resolves_an_address_the_way_the_address_bar_does() {
    let shell = FakeShell::answering(vec![Ok(Value::Null)]);
    let tool = BrowserTool::new(shell.clone(), None);

    let out = run(
        &tool,
        json!({ "action": "navigate", "tab": "t", "url": "localhost:5173" }),
    )
    .await;

    assert!(!out.is_error, "{}", out.content);
    let (method, args) = shell.calls().remove(0);
    assert_eq!(method, "browser_navigate");
    assert_eq!(args["url"], "http://localhost:5173");
}

/// A refused target never reaches the shell. Refusing after the call would be
/// refusing after the page had already loaded.
#[tokio::test]
async fn a_refused_target_is_not_sent_to_the_shell() {
    let shell = FakeShell::answering(vec![Ok(Value::Null)]);
    let tool = BrowserTool::new(shell.clone(), None);

    let out = run(
        &tool,
        json!({ "action": "navigate", "tab": "t", "url": "file:///etc/passwd" }),
    )
    .await;

    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("read"), "{}", out.content);
    assert!(shell.calls().is_empty(), "the shell was asked anyway");
}

/// The snapshot is the page as a model can use it: wrappers gone, interactive
/// elements addressable, the whole thing fenced.
#[tokio::test]
async fn a_snapshot_is_filtered_and_fenced() {
    let shell = FakeShell::answering(vec![Ok(page())]);
    let tool = BrowserTool::new(shell.clone(), None);

    let out = run(&tool, json!({ "action": "snapshot", "tab": "t" })).await;

    assert!(!out.is_error, "{}", out.content);
    assert!(out
        .content
        .contains("<web-page-content url=\"https://example.com/docs\">"));
    assert!(out.content.contains("</web-page-content>"));
    assert!(
        out.content.contains("heading \"Getting started\""),
        "{}",
        out.content
    );
    // The browser's own node id, so nothing has to keep a table of refs.
    assert!(
        out.content.contains("ref_44 link \"Install\""),
        "{}",
        out.content
    );
    // The wrapper and the page root carry nothing their children do not.
    assert!(!out.content.contains("generic"), "{}", out.content);
    assert!(!out.content.contains("RootWebArea"), "{}", out.content);
}

/// Navigating is where a site first hears from this machine, so that is where
/// the question is. Reading a page that is already loaded is not the same act
/// and is not asked about.
#[tokio::test]
async fn navigating_asks_per_host() {
    let tool = BrowserTool::new(FakeShell::answering(vec![]), None);

    for quiet in ["open", "snapshot", "close"] {
        assert!(
            matches!(
                tool.permission(&json!({ "action": quiet, "tab": "t" })),
                PermissionRequest::None
            ),
            "{quiet} asked for permission"
        );
    }

    let asking = tool.permission(&json!({
        "action": "navigate", "tab": "t", "url": "https://github.com/x"
    }));
    match asking {
        PermissionRequest::Ask { descriptor, .. } => {
            // Per host, the same vocabulary `web_fetch` uses — and deliberately
            // not the same descriptor, so allowing a page to be *read* never
            // silently allows a later phase to press that site's buttons while
            // logged in as the user.
            assert_eq!(descriptor, "browser(navigate github.com)");
        }
        other => panic!("navigate did not ask: {other:?}"),
    }
}

/// An unknown action says what the known ones are. The model should not have to
/// spend a call finding out (the zero-guessing rule in the root CLAUDE.md).
#[tokio::test]
async fn an_unknown_action_names_the_ones_that_exist() {
    let tool = BrowserTool::new(FakeShell::answering(vec![]), None);

    let out = run(&tool, json!({ "action": "dance", "tab": "t" })).await;

    assert!(out.is_error);
    for known in ["open", "navigate", "snapshot", "click", "type", "close"] {
        assert!(out.content.contains(known), "{}", out.content);
    }
}

// ------------------------------------------------------------------ acting

/// The refusal that has to come *before* the shell hears anything.
///
/// A model holding a `ref` it did not get from a snapshot of this tab is about
/// to click something at random, and the tool has no host to put in front of
/// the user either. Both problems have the same fix, and the error says it.
#[tokio::test]
async fn clicking_a_tab_nobody_has_looked_at_is_refused_before_it_is_sent() {
    let shell = FakeShell::answering(vec![Ok(Value::Null)]);
    let tool = BrowserTool::new(shell.clone(), None);

    let out = run(
        &tool,
        json!({ "action": "click", "tab": "t", "ref": "ref_9" }),
    )
    .await;

    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("Snapshot"), "{}", out.content);
    assert!(shell.calls().is_empty(), "the shell was asked anyway");
}

/// Looking at a page is what tells the tool which host an approval would be
/// about — and the host it passes on is the one the *page* answered with, never
/// one the model supplied. The shell re-checks it against the live URL, so this
/// is the value that comparison is made against.
#[tokio::test]
async fn a_snapshot_is_what_makes_clicking_answerable() {
    let shell = FakeShell::answering(vec![Ok(page()), Ok(Value::Null)]);
    let tool = BrowserTool::new(shell.clone(), None);

    run(&tool, json!({ "action": "snapshot", "tab": "t" })).await;
    let out = run(
        &tool,
        json!({ "action": "click", "tab": "t", "ref": "ref_44" }),
    )
    .await;

    assert!(!out.is_error, "{}", out.content);
    let (method, args) = shell.calls().remove(1);
    assert_eq!(method, "browser_click");
    assert_eq!(args["ref"], 44, "the ref_ prefix should be stripped");
    assert_eq!(args["host"], "example.com");
}

/// Clicking and typing share one descriptor because they are one decision:
/// "may this agent act on that site as me". Which element and which words are
/// the summary's job — that is what a person reads — while the descriptor is
/// what a permanent rule gets written against.
#[tokio::test]
async fn acting_asks_once_per_site_rather_than_once_per_input_device() {
    let shell = FakeShell::answering(vec![Ok(page())]);
    let tool = BrowserTool::new(shell, None);
    run(&tool, json!({ "action": "snapshot", "tab": "t" })).await;

    let click = tool.permission(&json!({ "action": "click", "tab": "t", "ref": "ref_44" }));
    let typing = tool.permission(&json!({
        "action": "type", "tab": "t", "ref": "ref_44", "text": "hello"
    }));

    for (asking, expected) in [(click, "click ref_44"), (typing, "type \"hello\"")] {
        match asking {
            PermissionRequest::Ask {
                descriptor,
                summary,
                ..
            } => {
                assert_eq!(descriptor, "browser(interact example.com)");
                assert!(summary.contains(expected), "{summary}");
                assert!(summary.contains("example.com"), "{summary}");
            }
            other => panic!("acting did not ask: {other:?}"),
        }
    }
}

/// Nothing to act on means nothing to ask about. `run` refuses with the reason,
/// and a panel asking permission for a page nobody has been to would be a
/// question with no subject in it.
#[tokio::test]
async fn there_is_nothing_to_approve_before_a_page_has_been_seen() {
    let tool = BrowserTool::new(FakeShell::answering(vec![]), None);
    assert!(matches!(
        tool.permission(&json!({ "action": "click", "tab": "t", "ref": "ref_1" })),
        PermissionRequest::None
    ));
}

/// Reading a page and scrolling it leave no trace on anybody's server, so
/// neither is a question. The judgement is "does it show up somewhere other
/// than here", not "does it sound dangerous".
#[tokio::test]
async fn looking_and_scrolling_are_free() {
    let shell = FakeShell::answering(vec![Ok(page())]);
    let tool = BrowserTool::new(shell, None);
    run(&tool, json!({ "action": "snapshot", "tab": "t" })).await;

    for quiet in [
        "snapshot",
        "screenshot",
        "scroll",
        "wait",
        "back",
        "reload",
        "close",
    ] {
        assert!(
            matches!(
                tool.permission(&json!({ "action": quiet, "tab": "t" })),
                PermissionRequest::None
            ),
            "{quiet} asked for permission"
        );
    }
}

/// Stepping the history moves the page somewhere this tool did not watch, so
/// what it thought it knew about the tab stops being true. Forgetting is the
/// safe direction: the next click asks for a snapshot instead of acting on a
/// host that may be two pages old.
#[tokio::test]
async fn going_back_forgets_where_the_tab_was() {
    let shell = FakeShell::answering(vec![Ok(page()), Ok(Value::Null)]);
    let tool = BrowserTool::new(shell, None);
    run(&tool, json!({ "action": "snapshot", "tab": "t" })).await;
    run(&tool, json!({ "action": "back", "tab": "t" })).await;

    assert!(matches!(
        tool.permission(&json!({ "action": "click", "tab": "t", "ref": "ref_44" })),
        PermissionRequest::None
    ));
}

/// A model that cannot see is told so, and told where to go instead. The
/// alternative is silent: providers drop an image they cannot carry, so the
/// model would receive a sentence saying a picture was attached and then answer
/// about a page it never saw.
#[tokio::test]
async fn a_screenshot_for_a_blind_model_is_an_error_and_not_an_empty_promise() {
    let shell = FakeShell::answering(vec![Ok(json!({
        "url": "https://example.com", "data": "iVBORw0KGgo=", "width": 800, "height": 600
    }))]);
    let tool = BrowserTool::new(shell.clone(), None);

    // `ToolCtx::for_test` carries no model, which stands for "no reason to
    // think it cannot" — a test asking for a screenshot means to get one.
    let out = run(&tool, json!({ "action": "screenshot", "tab": "t" })).await;

    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.images.len(), 1, "the picture never reached the model");
    assert!(out.content.contains("800"), "{}", out.content);
    assert!(
        out.content.contains("snapshot"),
        "a screenshot should point at where the refs are: {}",
        out.content
    );
}

/// `ref_44`, `44` and 44 are the same element. A model should not spend a turn
/// discovering which of three obvious spellings this tool wanted.
#[tokio::test]
async fn a_ref_is_accepted_however_it_is_spelt() {
    for spelling in [json!("ref_44"), json!("44"), json!(44)] {
        let shell = FakeShell::answering(vec![Ok(page()), Ok(Value::Null)]);
        let tool = BrowserTool::new(shell.clone(), None);
        run(&tool, json!({ "action": "snapshot", "tab": "t" })).await;
        let out = run(
            &tool,
            json!({ "action": "click", "tab": "t", "ref": spelling }),
        )
        .await;

        assert!(!out.is_error, "{spelling}: {}", out.content);
        assert_eq!(shell.calls().remove(1).1["ref"], 44, "{spelling}");
    }
}

/// A failure from the shell reaches the model as a tool error, not as a
/// success with an empty page in it.
#[tokio::test]
async fn a_shell_failure_is_a_tool_error() {
    let shell = FakeShell::answering(vec![Err("that browser tab is not open".into())]);
    let tool = BrowserTool::new(shell, None);

    let out = run(&tool, json!({ "action": "snapshot", "tab": "gone" })).await;

    assert!(out.is_error);
    assert!(out.content.contains("not open"), "{}", out.content);
}

/// Auto Mode: `navigate` is the read half of the browser tool, so a trusted
/// public-read host skips the safety classifier exactly as `web_fetch` would —
/// same normalized list, same judgment (`tcode_tools::trusted_public_read`).
#[test]
fn navigate_skips_the_classifier_for_a_trusted_public_read_host() {
    let tool = BrowserTool::new(FakeShell::answering(vec![]), None)
        .with_trusted_read_hosts(tcode_tools::trusted_read_hosts(vec!["github.com".into()]));

    assert_eq!(
        tool.auto_safety(
            &json!({ "action": "navigate", "tab": "t", "url": "https://github.com/x" })
        ),
        AutoSafety::Allow
    );
}

/// Loopback and RFC 1918 private ranges are this window's local half — a dev
/// server, a router, a NAS — so a navigation there is direct-safe even with an
/// empty trusted list.
#[test]
fn navigate_skips_the_classifier_for_local_addresses() {
    let tool = BrowserTool::new(FakeShell::answering(vec![]), None);
    for target in [
        "localhost:5173",
        "http://127.0.0.1:8080",
        "http://[::1]:3000",
        "https://localhost",
        "http://10.0.0.5:3000",
        "http://192.168.1.1/admin",
        "http://172.16.4.2:8080",
        "http://[fd00::1]:80",
    ] {
        assert_eq!(
            tool.auto_safety(&json!({ "action": "navigate", "tab": "t", "url": target })),
            AutoSafety::Allow,
            "{target} should be a direct-safe navigation"
        );
    }
}

/// Anything the trusted list does not name — public destinations included —
/// and anything that could not run anyway stays on the classifier path.
#[test]
fn navigate_stays_on_the_classifier_path_for_other_targets() {
    let tool = BrowserTool::new(FakeShell::answering(vec![]), None);
    for target in [
        "https://example.com/x",
        "http://8.8.8.8",
        "https://172.32.0.1",  // public, just outside RFC 1918's 172.16/12
        "http://github.com/x", // trusted requires https on the default port
        "file:///etc/passwd",  // cannot run at all
        "data:text/html,<p>x", // cannot run at all
    ] {
        assert_eq!(
            tool.auto_safety(&json!({ "action": "navigate", "tab": "t", "url": target })),
            AutoSafety::Classify,
            "{target} must stay on the classifier path"
        );
    }
}

/// Acting is the other half of the tool and deliberately never shares the fast
/// path: pressing buttons on a site as the user is a different question from
/// reading it, trusted or not.
#[test]
fn acting_never_takes_the_trusted_read_fast_path() {
    let tool = BrowserTool::new(FakeShell::answering(vec![]), None)
        .with_trusted_read_hosts(tcode_tools::trusted_read_hosts(vec!["github.com".into()]));
    for input in [
        json!({ "action": "click", "tab": "t", "ref": 44 }),
        json!({ "action": "type", "tab": "t", "ref": 44, "text": "hi" }),
        json!({ "action": "snapshot", "tab": "t" }),
        json!({ "action": "scroll", "tab": "t", "direction": "down" }),
        json!({ "action": "open" }),
    ] {
        assert_eq!(
            tool.auto_safety(&input),
            AutoSafety::Classify,
            "{input} must stay on the classifier path"
        );
    }
}
