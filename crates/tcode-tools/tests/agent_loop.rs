//! Agent-loop integration tests: a scripted MockProvider drives the real
//! loop with the real built-in tools against a temp directory.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use tcode_core::config::WatchdogConfig;
use tcode_core::{
    Agent, AgentEvent, Approval, ApprovalDecision, Approver, CacheStrategy, ContentBlock, Entry,
    EventStream, PermissionMode, PermissionRules, Provider, ProviderError, Request, Session,
    StopReason, StreamEvent, ToolCtx, Usage,
};

struct MockProvider {
    scripts: Mutex<VecDeque<Vec<StreamEvent>>>,
}

impl MockProvider {
    fn new(scripts: Vec<Vec<StreamEvent>>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
        })
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }
    fn model(&self) -> &str {
        "mock-1"
    }
    fn cache_strategy(&self) -> CacheStrategy {
        CacheStrategy::ImplicitPrefix
    }
    async fn stream(
        &self,
        _req: Request,
        _cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .expect("mock provider ran out of scripted responses");
        Ok(Box::pin(futures::stream::iter(
            script.into_iter().map(Ok).collect::<Vec<_>>(),
        )))
    }
}

struct ScriptedApprover {
    response: Approval,
    asked: Mutex<Vec<String>>,
}

impl ScriptedApprover {
    fn new(decision: ApprovalDecision, comment: Option<&str>) -> Self {
        Self {
            response: Approval {
                decision,
                comment: comment.map(String::from),
            },
            asked: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Approver for ScriptedApprover {
    async fn ask(&self, _tool: &str, _summary: &str, descriptor: &str) -> Approval {
        self.asked.lock().unwrap().push(descriptor.to_string());
        self.response.clone()
    }
}

fn tool_use(id: &str, name: &str, json: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::Started,
        StreamEvent::ToolUseStart {
            index: 0,
            id: id.into(),
            name: name.into(),
        },
        StreamEvent::ToolUseInputDelta {
            index: 0,
            fragment: json.into(),
        },
        StreamEvent::Usage(Usage::default()),
        StreamEvent::Done(StopReason::ToolUse),
    ]
}

fn text_done(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::Started,
        StreamEvent::TextDelta(text.into()),
        StreamEvent::Usage(Usage::default()),
        StreamEvent::Done(StopReason::EndTurn),
    ]
}

fn agent(provider: Arc<MockProvider>) -> Agent {
    Agent {
        provider,
        tools: tcode_tools::builtin_tools(),
        system: "test".into(),
        max_tokens: 1024,
        context_window: 200_000,
        watchdog: WatchdogConfig::default(),
        hooks: Default::default(),
    }
}

fn session(dir: &std::path::Path, mode: PermissionMode) -> Session {
    Session::new(
        ToolCtx::new(dir.to_path_buf(), 2000),
        mode,
        PermissionRules::default(),
    )
}

async fn run(
    agent: &Agent,
    session: &mut Session,
    approver: &dyn Approver,
    input: &str,
) -> Vec<AgentEvent> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let collector = tokio::spawn(async move {
        let mut v = Vec::new();
        while let Some(e) = rx.recv().await {
            v.push(e);
        }
        v
    });
    agent
        .user_turn(
            session,
            vec![ContentBlock::Text { text: input.into() }],
            &tx,
            approver,
            CancellationToken::new(),
        )
        .await
        .expect("turn failed");
    drop(tx);
    collector.await.unwrap()
}

fn tool_results(session: &Session) -> Vec<(String, bool)> {
    session
        .ledger
        .entries()
        .iter()
        .filter_map(|e| match e {
            Entry::ToolResults(blocks) => Some(blocks.iter().filter_map(|b| match b {
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => Some((content.clone(), *is_error)),
                _ => None,
            })),
            _ => None,
        })
        .flatten()
        .collect()
}

#[tokio::test]
async fn loop_runs_tool_and_feeds_result_back() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "hello tcode\n").unwrap();

    let provider = MockProvider::new(vec![
        tool_use("t1", "read", r#"{"path":"hello.txt"}"#),
        text_done("the file says hello"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Default);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    let events = run(&agent, &mut session, &approver, "read hello.txt").await;

    // read is permission-free: no prompt must have happened.
    assert!(approver.asked.lock().unwrap().is_empty());
    let results = tool_results(&session);
    assert_eq!(results.len(), 1);
    assert!(results[0].0.contains("hello tcode"), "{}", results[0].0);
    assert!(!results[0].1);
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolStart { name, .. } if name == "read")));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::TurnEnd)));
}

#[tokio::test]
async fn approval_comment_becomes_note_for_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        tool_use("t1", "write", r#"{"path":"a.txt","content":"hi"}"#),
        text_done("written"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Default);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, Some("keep it ASCII only"));

    run(&agent, &mut session, &approver, "create a.txt").await;

    assert_eq!(approver.asked.lock().unwrap().len(), 1);
    assert!(dir.path().join("a.txt").exists());
    let note = session.ledger.entries().iter().find_map(|e| match e {
        Entry::Note(n) => Some(n.clone()),
        _ => None,
    });
    assert!(
        note.as_deref().unwrap_or("").contains("keep it ASCII only"),
        "approval comment must reach the model: {note:?}"
    );
}

#[tokio::test]
async fn decline_reason_reaches_the_model_and_blocks_the_write() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        tool_use("t1", "write", r#"{"path":"a.txt","content":"hi"}"#),
        text_done("understood"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Default);
    let approver = ScriptedApprover::new(ApprovalDecision::No, Some("wrong directory"));

    run(&agent, &mut session, &approver, "create a.txt").await;

    assert!(!dir.path().join("a.txt").exists());
    let results = tool_results(&session);
    assert!(results[0].1, "declined call must be an error result");
    assert!(results[0].0.contains("wrong directory"));
}

#[tokio::test]
async fn plan_mode_blocks_edits_without_prompting() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        tool_use("t1", "write", r#"{"path":"a.txt","content":"hi"}"#),
        text_done("ok"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Plan);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "create a.txt").await;

    assert!(approver.asked.lock().unwrap().is_empty(), "plan mode must not prompt");
    assert!(!dir.path().join("a.txt").exists());
    let results = tool_results(&session);
    assert!(results[0].0.contains("plan mode"));
}

#[tokio::test]
async fn repeated_read_of_unchanged_file_is_deduped() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "content here\n").unwrap();
    let provider = MockProvider::new(vec![
        tool_use("t1", "read", r#"{"path":"f.txt"}"#),
        tool_use("t2", "read", r#"{"path":"f.txt"}"#),
        text_done("done"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Default);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "read f.txt twice").await;

    let results = tool_results(&session);
    assert_eq!(results.len(), 2);
    assert!(results[0].0.contains("content here"));
    assert!(
        results[1].0.contains("unchanged"),
        "second read must be deduped, got: {}",
        results[1].0
    );
}

#[tokio::test]
async fn interrupt_contract_pairs_results_and_explains_state() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![tool_use(
        "t1",
        "write",
        r#"{"path":"a.txt","content":"hi"}"#,
    )]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Auto);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    let cancel = CancellationToken::new();
    cancel.cancel(); // user hit Esc while the model was streaming
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let collector = tokio::spawn(async move {
        let mut v = Vec::new();
        while let Some(e) = rx.recv().await {
            v.push(e);
        }
        v
    });
    agent
        .user_turn(
            &mut session,
            vec![ContentBlock::Text { text: "go".into() }],
            &tx,
            &approver,
            cancel,
        )
        .await
        .unwrap();
    drop(tx);
    let events = collector.await.unwrap();

    assert!(!dir.path().join("a.txt").exists(), "cancelled call must not run");
    // API invariant: the committed tool_use still got a (cancelled) result.
    let results = tool_results(&session);
    assert_eq!(results.len(), 1);
    assert!(results[0].0.contains("Cancelled"));
    // Contract note tells the model exactly what did not happen.
    let note = session.ledger.entries().iter().find_map(|e| match e {
        Entry::Note(n) => Some(n.clone()),
        _ => None,
    });
    let note = note.expect("interrupt note must exist");
    assert!(note.contains("did NOT run"), "{note}");
    assert!(note.contains("do not re-verify"), "{note}");
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Interrupted)));
}

#[tokio::test]
async fn shell_tool_runs_and_reports_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let (cmd, expect) = if cfg!(windows) {
        (r#"{"command":"Write-Output tcode-ping"}"#, "tcode-ping")
    } else {
        (r#"{"command":"echo tcode-ping"}"#, "tcode-ping")
    };
    let tool_name = if cfg!(windows) { "shell" } else { "bash" };
    let provider = MockProvider::new(vec![
        tool_use("t1", tool_name, cmd),
        text_done("done"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Auto);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "ping").await;

    let results = tool_results(&session);
    assert!(results[0].0.contains(expect), "{}", results[0].0);
    assert!(!results[0].1);
}

#[tokio::test]
async fn oversized_tool_output_is_gated_with_paging_handle() {
    let dir = tempfile::tempdir().unwrap();
    let big: String = (1..=5000).map(|i| format!("log line {i}\n")).collect();
    std::fs::write(dir.path().join("big.log"), &big).unwrap();
    let provider = MockProvider::new(vec![
        tool_use("t1", "read", r#"{"path":"big.log","limit":5000}"#),
        text_done("done"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Auto);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "read the log").await;

    let results = tool_results(&session);
    assert!(
        results[0].0.contains("id=o1"),
        "big output must carry a paging handle: …{}",
        &results[0].0[results[0].0.len().saturating_sub(200)..]
    );
    assert!(tcode_core::blobs::approx_tokens(&results[0].0) < 3000);
}

#[tokio::test]
async fn yes_always_adds_session_rule() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        tool_use("t1", "write", r#"{"path":"a.txt","content":"1"}"#),
        tool_use("t2", "write", r#"{"path":"a.txt","content":"1"}"#),
        text_done("done"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Default);
    let approver = ScriptedApprover::new(ApprovalDecision::YesAlways, None);

    run(&agent, &mut session, &approver, "write twice").await;

    // Identical descriptor: second call must not prompt again.
    assert_eq!(approver.asked.lock().unwrap().len(), 1);
    assert!(session.rules.allow.iter().any(|r| r.contains("a.txt")));
}

#[tokio::test]
async fn explore_sub_agent_returns_only_the_report() {
    use tcode_core::Tool as _;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("lib.rs"), "pub fn f() {}\n").unwrap();
    // Sub-agent script: read a file, then report.
    let provider = MockProvider::new(vec![
        tool_use("t1", "read", r#"{"path": "lib.rs"}"#),
        text_done("report: lib.rs defines f()"),
    ]);
    let task = tcode_tools::TaskTool::new(
        provider,
        WatchdogConfig::default(),
        1024,
        200_000,
        2000,
        dir.path().to_path_buf(),
    );
    let ctx = ToolCtx::new(dir.path().to_path_buf(), 2000);
    let out = task
        .run(
            serde_json::json!({"agent": "explore", "prompt": "what is in lib.rs?"}),
            &ctx,
            &CancellationToken::new(),
        )
        .await;
    assert!(!out.is_error, "sub-agent failed: {}", out.content);
    // The parent sees the report + a stats line — not the tool traffic.
    assert!(out.content.contains("report: lib.rs defines f()"));
    assert!(out.content.contains("1 tool calls"));
    assert!(!out.content.contains("pub fn f()"));
}

#[tokio::test]
async fn post_tool_hook_failure_reaches_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        tool_use("t1", "write", r#"{"path": "a.txt", "content": "x"}"#),
        text_done("done"),
    ]);
    let mut agent = agent(provider);
    let cmd = if cfg!(windows) {
        "echo fmt failed 1>&2 & exit 1"
    } else {
        "echo 'fmt failed' >&2; exit 1"
    };
    agent.hooks = tcode_core::Hooks::new(vec![tcode_core::HookDef {
        event: tcode_core::HookEvent::PostToolUse,
        matcher: "edit|write".into(),
        command: cmd.into(),
        timeout_secs: 10,
    }]);
    let mut session = session(dir.path(), PermissionMode::Auto);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "write it").await;

    let results = tool_results(&session);
    assert!(
        results
            .iter()
            .any(|(c, _)| c.contains("[hook]") && c.contains("fmt failed")),
        "hook stderr must be appended to the tool result: {results:?}"
    );
}

#[tokio::test]
async fn pre_tool_hook_blocks_the_call() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        tool_use("t1", "write", r#"{"path": "a.txt", "content": "x"}"#),
        text_done("ok"),
    ]);
    let mut agent = agent(provider);
    let cmd = if cfg!(windows) {
        "echo protected file 1>&2 & exit 2"
    } else {
        "echo 'protected file' >&2; exit 2"
    };
    agent.hooks = tcode_core::Hooks::new(vec![tcode_core::HookDef {
        event: tcode_core::HookEvent::PreToolUse,
        matcher: "write".into(),
        command: cmd.into(),
        timeout_secs: 10,
    }]);
    let mut session = session(dir.path(), PermissionMode::Auto);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "write it").await;

    assert!(!dir.path().join("a.txt").exists(), "hook must block the write");
    let results = tool_results(&session);
    assert!(results[0].1, "blocked call must be an error result");
    assert!(results[0].0.contains("protected file"));
}

#[tokio::test]
async fn agent_checkpoints_files_before_mutating_tools() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "original").unwrap();
    let provider = MockProvider::new(vec![
        tool_use("t1", "write", r#"{"path": "a.txt", "content": "changed"}"#),
        text_done("done"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Auto);
    session.checkpoints =
        tcode_core::CheckpointStore::new(dir.path().join(".ckpts"));
    // The write tool demands a prior read before overwriting.
    session
        .tool_ctx
        .freshness
        .lock()
        .unwrap()
        .record_write(&file, tcode_core::freshness::content_hash(b"original"));
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "change it").await;
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "changed");

    // Rewind to the beginning restores the original content.
    let restored = session.checkpoints.restore_to(0);
    assert!(!restored.is_empty());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
}
