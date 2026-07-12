//! Agent-loop integration tests: a scripted MockProvider drives the real
//! loop with the real built-in tools against a temp directory.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use tcode_core::config::WatchdogConfig;
use tcode_core::{
    ActiveModel, Agent, AgentEvent, Approval, ApprovalDecision, Approver, CacheStrategy,
    ContentBlock, Entry, EventStream, ModelCell, PermissionMode, PermissionRules, Provider,
    ProviderError, Request, Session, StopReason, StreamEvent, ToolCtx, Usage,
};

fn cell(provider: Arc<MockProvider>) -> ModelCell {
    ModelCell::new(ActiveModel {
        provider,
        max_tokens: 1024,
        context_window: 200_000,
        effort: None,
    })
}

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
    async fn ask(
        &self,
        _tool: &str,
        _summary: &str,
        descriptor: &str,
        _input: &serde_json::Value,
    ) -> Approval {
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
        model: cell(provider),
        tools: tcode_tools::builtin_tools(),
        system: "test".into(),
        watchdog: WatchdogConfig::default(),
        hooks: Default::default(),
        max_steps: tcode_core::DEFAULT_MAX_STEPS,
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
async fn external_read_loads_project_instructions_without_blocking() {
    let base = tempfile::tempdir().unwrap();
    let current = base.path().join("current");
    let external = base.path().join("external");
    std::fs::create_dir_all(current.join(".git")).unwrap();
    std::fs::create_dir_all(external.join(".git")).unwrap();
    std::fs::write(external.join("AGENTS.md"), "external read rule").unwrap();
    let target = external.join("data.txt");
    std::fs::write(&target, "external data").unwrap();
    let input = serde_json::json!({"path": target}).to_string();
    let provider = MockProvider::new(vec![
        tool_use("t1", "read", &input),
        text_done("rules applied"),
    ]);
    let agent = agent(provider);
    let mut session = session(&current, PermissionMode::Unsafe);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "read external data").await;

    let results = tool_results(&session);
    assert!(!results[0].1, "read should execute: {}", results[0].0);
    assert!(results[0].0.contains("external data"));
    assert!(session
        .ledger
        .entries()
        .iter()
        .any(|entry| matches!(entry, Entry::Note(note) if note.contains("external read rule"))));
}

#[tokio::test]
async fn first_external_write_is_blocked_then_retry_executes() {
    let base = tempfile::tempdir().unwrap();
    let current = base.path().join("current");
    let external = base.path().join("external");
    std::fs::create_dir_all(current.join(".git")).unwrap();
    std::fs::create_dir_all(external.join(".git")).unwrap();
    std::fs::write(external.join("AGENTS.md"), "external write rule").unwrap();
    let target = external.join("created.txt");
    let input = serde_json::json!({"path": target, "content": "written once"}).to_string();
    let provider = MockProvider::new(vec![
        tool_use("t1", "write", &input),
        tool_use("t2", "write", &input),
        text_done("done"),
    ]);
    let agent = agent(provider);
    let mut session = session(&current, PermissionMode::Unsafe);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "write external file").await;

    let results = tool_results(&session);
    assert!(results[0].1);
    assert!(results[0].0.contains("no side effects occurred"));
    assert!(!results[1].1, "retry should execute: {}", results[1].0);
    assert_eq!(std::fs::read_to_string(target).unwrap(), "written once");
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

    assert!(
        approver.asked.lock().unwrap().is_empty(),
        "plan mode must not prompt"
    );
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
    let mut session = session(dir.path(), PermissionMode::Unsafe);
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

    assert!(
        !dir.path().join("a.txt").exists(),
        "cancelled call must not run"
    );
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
    let provider = MockProvider::new(vec![tool_use("t1", tool_name, cmd), text_done("done")]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Unsafe);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "ping").await;

    let results = tool_results(&session);
    assert!(results[0].0.contains(expect), "{}", results[0].0);
    assert!(!results[0].1);
}

#[tokio::test]
async fn oversized_tool_output_spills_to_a_readable_file() {
    // A command tool's output is unbounded, so it stays gated (unlike the
    // self-paginating `read`/`grep`, which opt out). Produce ~5000 lines.
    let dir = tempfile::tempdir().unwrap();
    let (tool_name, cmd) = if cfg!(windows) {
        (
            "shell",
            r#"{"command":"1..5000 | ForEach-Object { \"log line $_\" }"}"#,
        )
    } else {
        ("bash", r#"{"command":"seq 5000 | sed 's/^/log line /'"}"#)
    };
    let provider = MockProvider::new(vec![tool_use("t1", tool_name, cmd), text_done("done")]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Unsafe);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "spew a big log").await;

    let results = tool_results(&session);
    // Overflow is parked in a file the model can read/grep — no bespoke tool.
    assert!(
        results[0].0.contains("full output saved to") && results[0].0.contains(".txt"),
        "big output must point at a saved file: …{}",
        &results[0].0[results[0].0.len().saturating_sub(200)..]
    );
    // The gated preview is bounded near the blob budget (8000 tokens), far
    // below the full ~20k-token output.
    assert!(tcode_core::blobs::approx_tokens(&results[0].0) < 9000);

    // The saved file holds the complete output.
    let overflow = std::fs::read_dir(tcode_core::store::tool_output_dir(dir.path()))
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|x| x == "txt"))
        .expect("overflow .txt file must exist");
    let saved = std::fs::read_to_string(&overflow).unwrap();
    assert!(
        saved.lines().count() >= 5000,
        "saved {} lines",
        saved.lines().count()
    );
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
        cell(provider),
        WatchdogConfig::default(),
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
    let mut session = session(dir.path(), PermissionMode::Unsafe);
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
    let mut session = session(dir.path(), PermissionMode::Unsafe);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "write it").await;

    assert!(
        !dir.path().join("a.txt").exists(),
        "hook must block the write"
    );
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
    let mut session = session(dir.path(), PermissionMode::Unsafe);
    session.checkpoints = tcode_core::CheckpointStore::new(dir.path().join(".ckpts"));
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

#[tokio::test]
async fn background_shell_task_reports_completion_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let tool_name = if cfg!(windows) { "shell" } else { "bash" };
    let provider = MockProvider::new(vec![
        tool_use(
            "t1",
            tool_name,
            r#"{"command":"echo bg-done","run_in_background":true}"#,
        ),
        text_done("started it"),
        text_done("noted"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Unsafe);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "run echo in background").await;

    // The tool returns immediately with a task id, not the echo output.
    let results = tool_results(&session);
    assert!(
        results[0].0.contains("Started background task b1"),
        "{}",
        results[0].0
    );
    assert!(!results[0].1);

    // Wait for the process to finish.
    for _ in 0..100 {
        if session
            .tool_ctx
            .background
            .lock()
            .unwrap()
            .running()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        session
            .tool_ctx
            .background
            .lock()
            .unwrap()
            .running()
            .is_empty(),
        "background echo did not finish in time"
    );

    // Its output streamed to a log file the model reads with `read`.
    let log = tcode_core::store::tool_output_dir(dir.path()).join("b1.log");
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(logged.contains("bg-done"), "{logged}");

    // The next turn starts by telling the model the task finished, pointing at
    // the log file rather than a bespoke paging tool.
    run(&agent, &mut session, &approver, "anything else").await;
    let note = session
        .ledger
        .entries()
        .iter()
        .find_map(|e| match e {
            Entry::Note(n) if n.contains("Background task b1") => Some(n.clone()),
            _ => None,
        })
        .expect("completion note must be appended at the next turn start");
    assert!(note.contains("exited with code 0"), "{note}");
    assert!(note.contains("b1.log"), "{note}");
}

#[tokio::test]
async fn kill_task_stops_a_background_process() {
    let dir = tempfile::tempdir().unwrap();
    let tool_name = if cfg!(windows) { "shell" } else { "bash" };
    let command = if cfg!(windows) {
        r#"{"command":"Start-Sleep -Seconds 60","run_in_background":true}"#
    } else {
        r#"{"command":"sleep 60","run_in_background":true}"#
    };
    let provider = MockProvider::new(vec![
        tool_use("t1", tool_name, command),
        tool_use("t2", "kill_task", r#"{"id":"b1"}"#),
        text_done("killed"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Unsafe);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "start then kill").await;

    let results = tool_results(&session);
    assert!(
        results[1].0.contains("kill signal sent"),
        "{}",
        results[1].0
    );
    for _ in 0..100 {
        if session
            .tool_ctx
            .background
            .lock()
            .unwrap()
            .running()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let notes = session
        .tool_ctx
        .background
        .lock()
        .unwrap()
        .take_completion_notes();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("killed"), "{}", notes[0]);
}

#[tokio::test]
async fn edit_succeeds_without_prior_read() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "alpha beta gamma").unwrap();
    let provider = MockProvider::new(vec![
        tool_use(
            "t1",
            "edit",
            r#"{"path":"a.txt","old_string":"beta","new_string":"BETA"}"#,
        ),
        text_done("edited"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Unsafe);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "edit it").await;

    let results = tool_results(&session);
    assert!(
        !results[0].1,
        "exact match needs no prior read: {}",
        results[0].0
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha BETA gamma");
}

#[tokio::test]
async fn edit_miss_without_read_suggests_reading() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha beta gamma").unwrap();
    let provider = MockProvider::new(vec![
        tool_use(
            "t1",
            "edit",
            r#"{"path":"a.txt","old_string":"delta","new_string":"DELTA"}"#,
        ),
        text_done("gave up"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Unsafe);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "edit it").await;

    let results = tool_results(&session);
    assert!(results[0].1);
    assert!(
        results[0].0.contains("not read the current version"),
        "{}",
        results[0].0
    );
}

#[tokio::test]
async fn step_limit_ends_turn_gracefully() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x").unwrap();
    // Three scripted tool steps, but the guard allows only two.
    let provider = MockProvider::new(vec![
        tool_use("t1", "read", r#"{"path":"a.txt"}"#),
        tool_use("t2", "read", r#"{"path":"a.txt","force":true}"#),
        tool_use("t3", "read", r#"{"path":"a.txt","force":true}"#),
        text_done("never reached"),
    ]);
    let mut agent = agent(provider);
    agent.max_steps = 2;
    let mut session = session(dir.path(), PermissionMode::Unsafe);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    let events = run(&agent, &mut session, &approver, "loop forever").await;

    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::StepLimitReached { max: 2 })));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::TurnEnd)));
    // The ledger stays consistent and tells the model why the turn ended.
    assert!(session
        .ledger
        .entries()
        .iter()
        .any(|entry| matches!(entry, Entry::Note(note) if note.contains("runaway guard"))));
}

#[tokio::test]
async fn tiny_read_limits_are_widened() {
    let dir = tempfile::tempdir().unwrap();
    let body: String = (1..=200).map(|i| format!("line{i}\n")).collect();
    std::fs::write(dir.path().join("big.txt"), body).unwrap();
    let provider = MockProvider::new(vec![
        tool_use("t1", "read", r#"{"path":"big.txt","offset":10,"limit":5}"#),
        text_done("saw it"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Default);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "peek at big.txt").await;

    // A 5-line request is widened to the 120-line floor: one round-trip
    // instead of a dozen slivers.
    let results = tool_results(&session);
    assert!(results[0].0.contains("line100"), "{}", results[0].0);
    assert!(results[0].0.contains("line129"), "{}", results[0].0);
    assert!(!results[0].0.contains("line130"), "{}", results[0].0);
}

/// A provider whose first `stream()` calls fail with a retryable error before
/// eventually connecting — exercises the agent-owned connect retry.
struct FlakyProvider {
    remaining_failures: Mutex<u32>,
    inner: Arc<MockProvider>,
}

#[async_trait]
impl Provider for FlakyProvider {
    fn name(&self) -> &str {
        "flaky"
    }
    fn model(&self) -> &str {
        "flaky-1"
    }
    fn cache_strategy(&self) -> CacheStrategy {
        CacheStrategy::ImplicitPrefix
    }
    async fn stream(
        &self,
        req: Request,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        let fail = {
            let mut n = self.remaining_failures.lock().unwrap();
            (*n > 0).then(|| *n -= 1).is_some()
        };
        if fail {
            return Err(ProviderError::Api {
                status: 503,
                message: "temporarily unavailable".into(),
            });
        }
        self.inner.stream(req, cancel).await
    }
}

#[tokio::test]
async fn connect_failure_is_retried_and_reported() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(FlakyProvider {
        remaining_failures: Mutex::new(2),
        inner: MockProvider::new(vec![text_done("recovered")]),
    });
    let agent = Agent {
        model: ModelCell::new(ActiveModel {
            provider,
            max_tokens: 1024,
            context_window: 200_000,
            effort: None,
        }),
        tools: tcode_tools::builtin_tools(),
        system: "test".into(),
        watchdog: WatchdogConfig {
            idle_timeout_secs: 5,
            connect_timeout_secs: 20,
            max_retries: 5,
            initial_backoff_ms: 1,
            max_backoff_ms: 5,
        },
        hooks: Default::default(),
        max_steps: tcode_core::DEFAULT_MAX_STEPS,
    };
    let mut session = session(dir.path(), PermissionMode::Default);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    let events = run(&agent, &mut session, &approver, "hi").await;

    // Two connect failures each announced a retry with a backoff delay...
    let retries: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Retrying { .. }))
        .collect();
    assert_eq!(retries.len(), 2, "expected two retries, got {retries:?}");
    assert!(matches!(
        retries[0],
        AgentEvent::Retrying { attempt: 1, delay_ms, .. } if *delay_ms > 0
    ));
    // ...and the turn still completed once the provider recovered.
    assert!(events.iter().any(|e| matches!(e, AgentEvent::TurnEnd)));
}

/// Emits a retryable stream error after text, then returns a normal response.
/// This verifies failed text is kept only in human-facing transcript history.
struct PartialFailureProvider {
    scripts: Mutex<VecDeque<Vec<Result<StreamEvent, ProviderError>>>>,
    requests: Mutex<Vec<Request>>,
}

#[async_trait]
impl Provider for PartialFailureProvider {
    fn name(&self) -> &str {
        "partial-failure"
    }
    fn model(&self) -> &str {
        "partial-failure-1"
    }
    fn cache_strategy(&self) -> CacheStrategy {
        CacheStrategy::ImplicitPrefix
    }
    async fn stream(
        &self,
        req: Request,
        _cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        self.requests.lock().unwrap().push(req);
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .expect("partial-failure provider ran out of responses");
        Ok(Box::pin(futures::stream::iter(script)))
    }
}

#[tokio::test]
async fn partial_stream_output_is_retained_but_not_replayed_to_provider() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(PartialFailureProvider {
        scripts: Mutex::new(
            vec![
                vec![
                    Ok(StreamEvent::Started),
                    Ok(StreamEvent::TextDelta("partial answer".into())),
                    Err(ProviderError::Network("connection dropped".into())),
                ],
                text_done("recovered answer").into_iter().map(Ok).collect(),
            ]
            .into(),
        ),
        requests: Mutex::new(Vec::new()),
    });
    let agent = Agent {
        model: ModelCell::new(ActiveModel {
            provider: provider.clone(),
            max_tokens: 1024,
            context_window: 200_000,
            effort: None,
        }),
        tools: tcode_tools::builtin_tools(),
        system: "test".into(),
        watchdog: WatchdogConfig {
            idle_timeout_secs: 5,
            connect_timeout_secs: 20,
            max_retries: 1,
            initial_backoff_ms: 1,
            max_backoff_ms: 5,
        },
        hooks: Default::default(),
        max_steps: tcode_core::DEFAULT_MAX_STEPS,
    };
    let mut session = session(dir.path(), PermissionMode::Default);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    let events = run(&agent, &mut session, &approver, "hi").await;

    assert!(matches!(
        &session.ledger.entries()[1],
        Entry::IncompleteAssistant { text, error }
            if text == "partial answer" && error.contains("connection dropped")
    ));
    assert!(matches!(
        &session.ledger.entries()[2],
        Entry::Assistant(blocks)
            if matches!(&blocks[..], [ContentBlock::Text { text }] if text == "recovered answer")
    ));
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1]
        .messages
        .iter()
        .all(|message| message.content.iter().all(|block| match block {
            ContentBlock::Text { text } => !text.contains("partial answer"),
            _ => true,
        })));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::Retrying {
            partial_output_retained: true,
            ..
        }
    )));
}
