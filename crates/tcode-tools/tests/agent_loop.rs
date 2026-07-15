//! Agent-loop integration tests: a scripted MockProvider drives the real
//! loop with the real built-in tools against a temp directory.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use tcode_core::config::WatchdogConfig;
use tcode_core::{
    ActiveModel, Agent, AgentEvent, AgentModels, Approval, ApprovalDecision, Approver,
    CacheStrategy, ContentBlock, Entry, EventStream, ModelCell, PermissionMode, PermissionRules,
    Provider, ProviderError, ProviderSafetyClassifier, Request, SafetyClassifier, Session,
    StopReason, StreamEvent, ToolCtx, Usage,
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
    model: String,
    scripts: Mutex<VecDeque<Vec<StreamEvent>>>,
    requests: Mutex<Vec<Request>>,
}

impl MockProvider {
    fn new(scripts: Vec<Vec<StreamEvent>>) -> Arc<Self> {
        Self::named("mock-1", scripts)
    }

    /// A distinguishable model id, for tests that must prove *which* model a
    /// request went to (e.g. a sub-agent pinned to its own).
    fn named(model: &str, scripts: Vec<Vec<StreamEvent>>) -> Arc<Self> {
        Arc::new(Self {
            model: model.to_string(),
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }
    fn model(&self) -> &str {
        &self.model
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
            response: Approval::simple(decision, comment.map(String::from)),
            asked: Mutex::new(Vec::new()),
        }
    }

    fn with_response(response: Approval) -> Self {
        Self {
            response,
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

fn platform_shell_tool() -> &'static str {
    if cfg!(windows) {
        "shell"
    } else {
        "bash"
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

fn tool_uses(calls: &[(&str, &str, &str)]) -> Vec<StreamEvent> {
    let mut events = vec![StreamEvent::Started];
    for (index, (id, name, json)) in calls.iter().enumerate() {
        events.push(StreamEvent::ToolUseStart {
            index,
            id: (*id).into(),
            name: (*name).into(),
        });
        events.push(StreamEvent::ToolUseInputDelta {
            index,
            fragment: (*json).into(),
        });
    }
    events.push(StreamEvent::Usage(Usage::default()));
    events.push(StreamEvent::Done(StopReason::ToolUse));
    events
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
        models: AgentModels::default(),
        tools: tcode_tools::builtin_tools(&std::env::temp_dir()),
        system: "test".into(),
        watchdog: WatchdogConfig::default(),
        hooks: Default::default(),
        safety_classifier: None,
        auto_policy: String::new(),
        max_steps: tcode_core::DEFAULT_MAX_STEPS,
    }
}

/// The guess runs a conversation of its own — prose pairs, on its own pinnable
/// model, under its own cache scope — and that conversation is append-only: a
/// second turn adds one pair and leaves every earlier message byte-identical.
/// That is what makes it cost a cached prefix plus one pair instead of a turn.
#[tokio::test]
async fn the_next_prompt_guess_grows_a_prose_conversation_of_its_own() {
    let root = tempfile::tempdir().unwrap();
    let main = MockProvider::new(vec![
        // A turn with tool traffic: none of it may reach the guess.
        tool_use("t1", "read", r#"{"file_path":"lib.rs"}"#),
        text_done("## Fixed\nThe off-by-one is gone. Tests not run yet."),
        text_done("All 42 tests pass."),
    ]);
    let small = MockProvider::named(
        "small-1",
        vec![text_done("run the tests"), text_done("commit it")],
    );
    let roles = AgentModels::default();
    roles.pin(
        "suggest",
        ActiveModel {
            provider: small.clone(),
            max_tokens: 1024,
            context_window: 200_000,
            effort: None,
        },
    );
    let agent = Agent {
        models: roles,
        ..agent(main.clone())
    };
    let mut session = session(root.path(), PermissionMode::Default);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "fix the bug").await;
    let request = agent.suggest_request(&session).expect("a finished turn");
    let first = agent.suggest(request, CancellationToken::new()).await;
    assert_eq!(first.as_deref(), Some("run the tests"));

    run(&agent, &mut session, &approver, "now run them").await;
    let request = agent.suggest_request(&session).expect("a second turn");
    let second = agent.suggest(request, CancellationToken::new()).await;
    assert_eq!(second.as_deref(), Some("commit it"));

    let requests = small.requests.lock().unwrap();
    let (one, two) = (&requests[0], &requests[1]);

    // Its own model, its own scope, no tools.
    assert_eq!(one.model, "small-1");
    assert_eq!(one.cache_scope.as_deref(), Some("suggest"));
    assert!(one.tools.is_empty());
    assert_eq!(one.system, two.system);

    // One (asked, answered) pair per turn, plus the constant closing ask.
    assert_eq!(one.messages.len(), 3);
    assert_eq!(two.messages.len(), 5);

    let texts = |messages: &[tcode_core::Message]| {
        messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    // Append-only: turn two left turn one's pair byte-identical, which is the
    // whole reason its prefix stays in the provider's cache.
    assert_eq!(texts(&two.messages[..2]), texts(&one.messages[..2]));

    let text = texts(&two.messages).join("\n");
    assert!(text.contains("fix the bug") && text.contains("The off-by-one is gone"));
    assert!(text.contains("now run them") && text.contains("All 42 tests pass"));
    // The tool call and its result are the expensive part, and they never enter.
    assert!(!text.contains("lib.rs") && !text.contains("read"));
}

/// A prompt typed while the agent works is delivered at the first point where a
/// user entry is legal — after the tool batch commits — never between a tool
/// call and its result. The ledger merges it into that same user message, so
/// the model reads it on its very next step and the prefix stays append-only.
#[tokio::test]
async fn a_prompt_queued_mid_turn_lands_after_the_tool_results() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("lib.rs"), "fn main() {}\n").unwrap();
    let provider = MockProvider::new(vec![
        tool_use("t1", "read", r#"{"file_path":"lib.rs"}"#),
        text_done("Stopping there, as you asked."),
    ]);
    let agent = agent(provider.clone());
    let mut session = session(root.path(), PermissionMode::Default);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    // Typed while the turn was running (the frontend holds this same handle).
    // A queued prompt is a whole message: the screenshot pasted into it travels
    // with it, exactly as it would have if the user had waited to press enter.
    session.pending.push(tcode_core::PendingMessage {
        text: "actually, stop after the read".into(),
        attachments: vec!["screenshot 1".into()],
        blocks: vec![
            ContentBlock::Image {
                media_type: "image/png".into(),
                data: "iVBORw0KGgo=".into(),
            },
            ContentBlock::Text {
                text: "actually, stop after the read".into(),
            },
        ],
    });
    let events = run(&agent, &mut session, &approver, "read lib.rs").await;

    assert!(session.pending.is_empty(), "the queue was drained");
    let kinds: Vec<&str> = session
        .ledger
        .entries()
        .iter()
        .map(|entry| match entry {
            Entry::User(_) => "user",
            Entry::Assistant(_) => "assistant",
            Entry::ToolResults(_) => "results",
            _ => "other",
        })
        .collect();
    // Not between the call and its result: the tool_use must be answered first.
    assert_eq!(
        kinds,
        ["user", "assistant", "results", "user", "assistant"],
        "queued input lands only after the batch commits"
    );

    // On the wire it rides in the same user message as the tool results, which
    // is what makes it legal to append there at all.
    let messages = session.ledger.as_messages();
    let carrier = &messages[2];
    let has_result = carrier
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolResult { .. }));
    let has_prompt = carrier.content.iter().any(
        |block| matches!(block, ContentBlock::Text { text } if text.contains("actually, stop")),
    );
    let has_image = carrier
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::Image { .. }));
    assert!(has_result && has_prompt && has_image);

    // And the model actually saw it on its next request, not after the turn.
    let second = &provider.requests.lock().unwrap()[1];
    let seen = second
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .any(
            |block| matches!(block, ContentBlock::Text { text } if text.contains("actually, stop")),
        );
    assert!(seen, "the next model step carries the queued prompt");

    // The frontend renders it as a normal prompt — attachments and all —
    // tagged with its ledger index so rewind can jump to it.
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::QueuedInput { text, attachments, entry_index }
            if text.contains("actually, stop")
                && attachments == &["screenshot 1".to_string()]
                && *entry_index == 3
    )));
}

fn auto_agent(main: Arc<MockProvider>, classifier: Arc<MockProvider>) -> Agent {
    let model = cell(main);
    let roles = AgentModels::default();
    roles.pin(
        "auto",
        ActiveModel {
            provider: classifier,
            max_tokens: 1024,
            context_window: 200_000,
            effort: None,
        },
    );
    let safety_classifier: Arc<dyn SafetyClassifier> =
        Arc::new(ProviderSafetyClassifier::new(model.clone(), roles.clone()));
    Agent {
        model,
        models: roles,
        tools: tcode_tools::builtin_tools(&std::env::temp_dir()),
        system: "test".into(),
        watchdog: WatchdogConfig::default(),
        hooks: Default::default(),
        safety_classifier: Some(safety_classifier),
        auto_policy: "Classify dangerous actions conservatively.".into(),
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

#[tokio::test]
async fn first_request_uses_refreshed_opening_context_after_fresh_cd() {
    let root = tempfile::tempdir().unwrap();
    let child = root.path().join("child");
    std::fs::create_dir(&child).unwrap();
    let provider = MockProvider::new(vec![text_done("ready")]);
    let agent = agent(provider.clone());
    let mut session = session(root.path(), PermissionMode::Default);
    session.set_opening_context("old project map".into());

    let change = session.change_cwd("child").unwrap();
    assert!(change.refresh_opening_context);
    assert!(session.ledger.is_empty());
    session.set_opening_context("new project map".into());

    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);
    run(&agent, &mut session, &approver, "hello").await;

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].system, "test\n\nnew project map");
    assert!(!requests[0].system.contains("old project map"));
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
    assert!(results[0]
        .0
        .contains("newly discovered directory-scoped instructions"));
    assert!(!results[1].1, "retry should execute: {}", results[1].0);
    assert_eq!(std::fs::read_to_string(target).unwrap(), "written once");
}

#[tokio::test]
async fn new_directory_instructions_block_only_their_mutations() {
    let base = tempfile::tempdir().unwrap();
    let current = base.path().join("current");
    let external = base.path().join("external");
    std::fs::create_dir_all(current.join(".git")).unwrap();
    std::fs::create_dir_all(external.join(".git")).unwrap();
    std::fs::write(external.join("AGENTS.md"), "external write rule").unwrap();
    std::fs::write(external.join("input.txt"), "inspect me").unwrap();
    let external_write = external.join("blocked.txt");
    let safe_write = current.join("allowed.txt");
    let provider = MockProvider::new(vec![
        tool_uses(&[
            (
                "t1",
                "read",
                &serde_json::json!({ "path": external.join("input.txt") }).to_string(),
            ),
            (
                "t2",
                "write",
                &serde_json::json!({ "path": external_write, "content": "blocked" }).to_string(),
            ),
            (
                "t3",
                "write",
                &serde_json::json!({ "path": safe_write, "content": "allowed" }).to_string(),
            ),
        ]),
        text_done("done"),
    ]);
    let agent = agent(provider);
    let mut session = session(&current, PermissionMode::Unsafe);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "inspect then write").await;

    let results = tool_results(&session);
    assert!(!results[0].1, "read must still run: {}", results[0].0);
    assert!(results[1].1);
    assert!(results[1]
        .0
        .contains("newly discovered directory-scoped instructions"));
    assert!(!results[2].1, "unrelated write must run: {}", results[2].0);
    assert!(!external.join("blocked.txt").exists());
    assert_eq!(
        std::fs::read_to_string(current.join("allowed.txt")).unwrap(),
        "allowed"
    );
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

    let events = run(&agent, &mut session, &approver, "create a.txt").await;

    assert_eq!(approver.asked.lock().unwrap().len(), 1);
    assert!(dir.path().join("a.txt").exists());
    let note = session.ledger.entries().iter().find_map(|e| match e {
        Entry::UserNote {
            about,
            answer,
            text,
        } => Some((about, answer, text)),
        _ => None,
    });
    assert!(matches!(
        note,
        Some((about, false, text)) if about == "write" && text == "keep it ASCII only"
    ));
    let tool_end = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ToolEnd { name, .. } if name == "write"))
        .expect("write must finish");
    let note_event = events
        .iter()
        .position(|event| matches!(event, AgentEvent::UserNote { text, answer: false } if text == "keep it ASCII only"))
        .expect("approval comment must reach the UI");
    assert!(tool_end < note_event, "note must follow the tool result");
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
    let overflow = std::fs::read_dir(session.tool_ctx.scratch_dir.join("tool-output"))
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
async fn explore_tasks_share_the_parallel_read_only_batch_path() {
    let dir = tempfile::tempdir().unwrap();
    let parent = MockProvider::new(vec![
        tool_uses(&[
            (
                "t1",
                "task",
                r#"{"agent":"explore","prompt":"inspect first"}"#,
            ),
            (
                "t2",
                "task",
                r#"{"agent":"plan","prompt":"inspect second"}"#,
            ),
        ]),
        text_done("first report"),
        text_done("second report"),
        text_done("done"),
    ]);
    let mut agent = agent(parent);
    agent.tools.push(Arc::new(tcode_tools::TaskTool::new(
        agent.model.clone(),
        WatchdogConfig::default(),
        2_000,
        dir.path().to_path_buf(),
    )));
    let mut session = session(dir.path(), PermissionMode::Default);
    let approver = ScriptedApprover::new(ApprovalDecision::No, None);

    let events = run(&agent, &mut session, &approver, "delegate two inspections").await;

    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolBatchStart { label, calls }
            if label == "Task 2 calls" && calls.len() == 2
    )));
    assert!(approver.asked.lock().unwrap().is_empty());
    assert_eq!(
        agent.batch_display_label(&session, &assistant_calls(&session)),
        Some("Task 2 calls".into())
    );
}

#[tokio::test]
async fn plan_sub_agent_returns_a_draft_under_the_planning_prompt() {
    use tcode_core::Tool as _;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("lib.rs"), "pub fn f() {}\n").unwrap();
    let provider = MockProvider::new(vec![
        tool_use("t1", "read", r#"{"path": "lib.rs"}"#),
        text_done("## Implementation plan\n\n1. Update lib.rs and add a test."),
    ]);
    let task = tcode_tools::TaskTool::new(
        cell(provider.clone()),
        WatchdogConfig::default(),
        2000,
        dir.path().to_path_buf(),
    );
    let ctx = ToolCtx::new(dir.path().to_path_buf(), 2000);

    let out = task
        .run(
            serde_json::json!({"agent": "plan", "prompt": "plan the lib.rs change"}),
            &ctx,
            &CancellationToken::new(),
        )
        .await;

    assert!(!out.is_error, "sub-agent failed: {}", out.content);
    assert!(out.content.contains("[plan sub-agent"));
    assert!(out.content.contains("## Implementation plan"));
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0]
        .system
        .contains("implementation-planning specialist"));
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
    let log = session
        .tool_ctx
        .scratch_dir
        .join("tool-output")
        .join("b1.log");
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
async fn edit_lanes_continue_after_a_same_file_no_op_failure() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha").unwrap();
    std::fs::write(dir.path().join("b.txt"), "beta").unwrap();
    let provider = MockProvider::new(vec![
        tool_uses(&[
            (
                "t1",
                "edit",
                r#"{"path":"a.txt","old_string":"alpha","new_string":"ALPHA"}"#,
            ),
            (
                "t2",
                "edit",
                r#"{"path":"b.txt","old_string":"beta","new_string":"BETA"}"#,
            ),
            (
                "t3",
                "edit",
                r#"{"path":"a.txt","old_string":"ALPHA","new_string":"ALPHA"}"#,
            ),
            (
                "t4",
                "edit",
                r#"{"path":"a.txt","old_string":"ALPHA","new_string":"FINAL"}"#,
            ),
        ]),
        text_done("reported"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Unsafe);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    let events = run(&agent, &mut session, &approver, "batch edits").await;

    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "FINAL"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
        "BETA"
    );
    let results = tool_results(&session);
    assert_eq!(results.len(), 4);
    assert!(
        !results[0].1,
        "first same-file edit must succeed: {}",
        results[0].0
    );
    assert!(
        !results[1].1,
        "independent file lane must complete: {}",
        results[1].0
    );
    assert!(
        results[2].1
            && results[2]
                .0
                .contains("old_string and new_string are identical"),
        "no-op edit must be reported as an error: {}",
        results[2].0
    );
    assert!(
        !results[3].1,
        "a later same-file edit must still run: {}",
        results[3].0
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolBatchStart { label, .. } if label == "Edit 4 changes across 2 files"
    )));
    assert!(session.ledger.entries().iter().any(|entry| matches!(
        entry,
        Entry::Note(note)
            if note.contains("step 1 (t1, edit): succeeded")
                && note.contains("step 2 (t2, edit): succeeded")
                && note.contains("step 3 (t3, edit): failed")
                && note.contains("step 4 (t4, edit): succeeded")
    )));
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
        models: AgentModels::default(),
        model: ModelCell::new(ActiveModel {
            provider,
            max_tokens: 1024,
            context_window: 200_000,
            effort: None,
        }),
        tools: tcode_tools::builtin_tools(&std::env::temp_dir()),
        system: "test".into(),
        watchdog: WatchdogConfig {
            idle_timeout_secs: 5,
            connect_timeout_secs: 20,
            max_retries: 5,
            initial_backoff_ms: 1,
            max_backoff_ms: 5,
        },
        hooks: Default::default(),
        safety_classifier: None,
        auto_policy: String::new(),
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
        models: AgentModels::default(),
        model: ModelCell::new(ActiveModel {
            provider: provider.clone(),
            max_tokens: 1024,
            context_window: 200_000,
            effort: None,
        }),
        tools: tcode_tools::builtin_tools(&std::env::temp_dir()),
        system: "test".into(),
        watchdog: WatchdogConfig {
            idle_timeout_secs: 5,
            connect_timeout_secs: 20,
            max_retries: 1,
            initial_backoff_ms: 1,
            max_backoff_ms: 5,
        },
        hooks: Default::default(),
        safety_classifier: None,
        auto_policy: String::new(),
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

/// Two tool calls in one assistant message.
fn two_tool_uses(a: (&str, &str, &str), b: (&str, &str, &str)) -> Vec<StreamEvent> {
    let call = |index: u32, (id, name, json): (&str, &str, &str)| {
        vec![
            StreamEvent::ToolUseStart {
                index: index as usize,
                id: id.into(),
                name: name.into(),
            },
            StreamEvent::ToolUseInputDelta {
                index: index as usize,
                fragment: json.into(),
            },
        ]
    };
    let mut events = vec![StreamEvent::Started];
    events.extend(call(0, a));
    events.extend(call(1, b));
    events.push(StreamEvent::Usage(Usage::default()));
    events.push(StreamEvent::Done(StopReason::ToolUse));
    events
}

/// The calls of the last assistant message, as replay recovers them.
fn assistant_calls(session: &Session) -> Vec<(String, String, serde_json::Value)> {
    session
        .ledger
        .entries()
        .iter()
        .rev()
        .find_map(|entry| match entry {
            Entry::Assistant(blocks) => {
                let calls: Vec<_> = blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolUse { id, name, input } => {
                            Some((id.clone(), name.clone(), input.clone()))
                        }
                        _ => None,
                    })
                    .collect();
                (!calls.is_empty()).then_some(calls)
            }
            _ => None,
        })
        .unwrap_or_default()
}

/// Transcript replay reconstructs a batch by asking the agent which calls ran
/// as one — so a resumed conversation must show the very label the live turn
/// emitted, never a re-derived guess.
#[tokio::test]
async fn batch_display_label_matches_the_live_batch_header() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
    let provider = MockProvider::new(vec![
        two_tool_uses(
            ("t1", "read", r#"{"path":"a.txt"}"#),
            ("t2", "read", r#"{"path":"b.txt"}"#),
        ),
        text_done("done"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Default);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    let events = run(&agent, &mut session, &approver, "read both").await;

    let live_label = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolBatchStart { label, .. } => Some(label.clone()),
            _ => None,
        })
        .expect("parallel reads emit a batch header");
    let calls = assistant_calls(&session);
    assert_eq!(
        agent.batch_display_label(&session, &calls),
        Some(live_label)
    );
}

#[tokio::test]
async fn lone_and_unbatchable_calls_have_no_batch_label() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    let provider = MockProvider::new(vec![
        tool_use("t1", "read", r#"{"path":"a.txt"}"#),
        text_done("done"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Default);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    let events = run(&agent, &mut session, &approver, "read one").await;
    assert!(!events
        .iter()
        .any(|event| matches!(event, AgentEvent::ToolBatchStart { .. })));
    assert_eq!(
        agent.batch_display_label(&session, &assistant_calls(&session)),
        None
    );

    // A read alongside a shell command is neither parallel-read-only nor a
    // shell batch: the loop runs them one by one, and so must replay.
    let mixed = vec![
        (
            "t1".to_string(),
            "read".to_string(),
            serde_json::json!({"path":"a.txt"}),
        ),
        (
            "t2".to_string(),
            "shell".to_string(),
            serde_json::json!({"command":"echo hi"}),
        ),
    ];
    assert_eq!(agent.batch_display_label(&session, &mixed), None);
}

/// `/dogfood` is a hidden command, so nothing but the system prompt tells the
/// model to critique the tools. Pin that it lands there — and only when on.
#[tokio::test]
async fn dogfood_toggle_adds_the_tool_feedback_directive_to_the_system_prompt() {
    let root = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![text_done("ok"), text_done("ok")]);
    let agent = agent(provider.clone());
    let mut session = session(root.path(), PermissionMode::Default);

    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "first").await;

    session.set_dogfood(true);
    run(&agent, &mut session, &approver, "second").await;

    let requests = provider.requests.lock().unwrap();
    assert!(!requests[0].system.contains("Tool feedback"));
    assert!(requests[1].system.contains("Tool feedback"));
    // The directive is appended, so everything cached before it is unchanged.
    assert!(requests[1].system.starts_with(&requests[0].system));
}

/// `[agents.explore]` pins a sub-agent kind to its own model. The pin must
/// hold for that kind and for nothing else: the parent keeps its model, and
/// the sub-agent's request never reaches the parent's provider.
#[tokio::test]
async fn a_pinned_sub_agent_runs_on_its_own_model() {
    let root = tempfile::tempdir().unwrap();
    let parent = MockProvider::new(vec![
        tool_use(
            "t1",
            "task",
            r#"{"agent":"explore","prompt":"survey the repo"}"#,
        ),
        text_done("relayed"),
    ]);
    let explore = MockProvider::named("cheap-scout-1", vec![text_done("the report")]);

    let task = tcode_tools::TaskTool::new(
        cell(parent.clone()),
        WatchdogConfig::default(),
        2000,
        root.path().to_path_buf(),
    )
    .with_agent_models({
        let pins = AgentModels::default();
        pins.pin("explore", cell(explore.clone()).snapshot());
        pins
    });
    let agent = Agent {
        model: cell(parent.clone()),
        models: AgentModels::default(),
        tools: vec![Arc::new(task)],
        system: "test".into(),
        watchdog: WatchdogConfig::default(),
        hooks: Default::default(),
        safety_classifier: None,
        auto_policy: String::new(),
        max_steps: tcode_core::DEFAULT_MAX_STEPS,
    };
    let mut session = session(root.path(), PermissionMode::Default);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    let events = run(&agent, &mut session, &approver, "explore this").await;

    // The sub-agent ran on the pinned provider, exactly once...
    assert_eq!(explore.requests.lock().unwrap().len(), 1);
    // ...and the parent only served its own two steps.
    assert_eq!(parent.requests.lock().unwrap().len(), 2);

    let report = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolEnd { name, content, .. } if name == "task" => Some(content.clone()),
            _ => None,
        })
        .expect("task returned a report");
    // The model that actually did the work is named in the report.
    assert!(
        report.contains("explore sub-agent on cheap-scout-1"),
        "{report}"
    );
    assert!(report.contains("the report"), "{report}");
}

#[tokio::test]
async fn auto_mode_bypasses_classifier_for_normal_project_edits() {
    let root = tempfile::tempdir().unwrap();
    let main = MockProvider::new(vec![
        tool_use(
            "t1",
            "write",
            r#"{"path":"src/new.rs","content":"pub fn new() {}"}"#,
        ),
        text_done("done"),
    ]);
    let classifier = MockProvider::new(vec![]);
    let agent = auto_agent(main.clone(), classifier.clone());
    let mut session = session(root.path(), PermissionMode::Auto);
    let approver = ScriptedApprover::new(ApprovalDecision::No, None);

    run(&agent, &mut session, &approver, "add a source file").await;

    assert!(root.path().join("src/new.rs").is_file());
    assert!(approver.asked.lock().unwrap().is_empty());
    assert!(classifier.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn auto_mode_bypasses_classifier_for_session_scratch_work() {
    let root = tempfile::tempdir().unwrap();
    let mut session = session(root.path(), PermissionMode::Auto);
    let scratch = session.tool_ctx.scratch_dir.clone();
    let command = if cfg!(windows) {
        "Set-Content probe.txt scratch; Remove-Item probe.txt".to_string()
    } else {
        "printf scratch > probe.txt && rm probe.txt".to_string()
    };
    let main = MockProvider::new(vec![
        tool_use(
            "t1",
            platform_shell_tool(),
            &serde_json::json!({ "command": command, "cwd": scratch }).to_string(),
        ),
        text_done("done"),
    ]);
    let classifier = MockProvider::new(vec![]);
    let agent = auto_agent(main, classifier.clone());
    let approver = ScriptedApprover::new(ApprovalDecision::No, None);

    run(
        &agent,
        &mut session,
        &approver,
        "clean up the temporary probe",
    )
    .await;

    assert!(approver.asked.lock().unwrap().is_empty());
    assert!(classifier.requests.lock().unwrap().is_empty());
    assert!(!session.tool_ctx.scratch_dir.join("probe.txt").exists());
}

#[tokio::test]
async fn auto_mode_fast_allow_runs_shell_with_one_classifier_request() {
    let root = tempfile::tempdir().unwrap();
    let main = MockProvider::new(vec![
        tool_use("t1", platform_shell_tool(), r#"{"command":"echo auto-ok"}"#),
        text_done("done"),
    ]);
    let classifier = MockProvider::new(vec![text_done("ALLOW")]);
    let agent = auto_agent(main.clone(), classifier.clone());
    let mut session = session(root.path(), PermissionMode::Auto);
    let approver = ScriptedApprover::new(ApprovalDecision::No, None);

    run(&agent, &mut session, &approver, "run the test command").await;

    assert!(approver.asked.lock().unwrap().is_empty());
    assert_eq!(main.requests.lock().unwrap().len(), 2);
    let requests = classifier.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].effort.as_deref(), Some("off"));
    // A cap that only fits the verdict truncates models that think first.
    assert_eq!(requests[0].max_tokens, 1_024);
    // The classifier runs on the agent's provider but not its prefix.
    assert_eq!(requests[0].cache_scope.as_deref(), Some("auto-classifier"));
}

#[tokio::test]
async fn auto_mode_classifier_outages_pause_and_notify_the_frontend() {
    let root = tempfile::tempdir().unwrap();
    let main = MockProvider::new(vec![
        tool_use("t1", platform_shell_tool(), r#"{"command":"echo first"}"#),
        text_done("first done"),
        tool_use("t2", platform_shell_tool(), r#"{"command":"echo second"}"#),
        text_done("second done"),
        tool_use("t3", platform_shell_tool(), r#"{"command":"echo third"}"#),
        text_done("third done"),
    ]);
    let classifier = MockProvider::new(vec![
        text_done("not a verdict"),
        text_done("not a verdict"),
        text_done("not a verdict"),
    ]);
    let agent = auto_agent(main, classifier);
    let mut session = session(root.path(), PermissionMode::Auto);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "run first").await;
    run(&agent, &mut session, &approver, "run second").await;
    let events = run(&agent, &mut session, &approver, "run third").await;

    assert_eq!(session.mode, PermissionMode::Default);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::AutoModePaused(notice)
            if notice.contains("classifier failures") && notice.contains("/agents")
    )));
}

#[tokio::test]
async fn auto_mode_stage_two_block_prevents_shell_execution() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("blocked.txt");
    let command = if cfg!(windows) {
        format!("Set-Content -Path '{}' -Value blocked", target.display())
    } else {
        format!("printf blocked > '{}'", target.display())
    };
    let main = MockProvider::new(vec![
        tool_use(
            "t1",
            platform_shell_tool(),
            &serde_json::json!({"command": command}).to_string(),
        ),
        text_done("found a safer route"),
    ]);
    let classifier = MockProvider::new(vec![
        text_done("BLOCK"),
        text_done("BLOCK\nThe command writes a file without direct authorization."),
    ]);
    let agent = auto_agent(main.clone(), classifier.clone());
    let mut session = session(root.path(), PermissionMode::Auto);
    let approver = ScriptedApprover::new(ApprovalDecision::No, None);

    run(&agent, &mut session, &approver, "inspect the project").await;

    assert!(!target.exists());
    assert!(approver.asked.lock().unwrap().is_empty());
    let requests = classifier.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    // Both stages share one cacheable policy prefix and differ only in the
    // suffix, so stage two can reuse stage one's cached prefix.
    assert_eq!(requests[0].system, requests[1].system);
    assert_ne!(requests[0].system_suffix, requests[1].system_suffix);
    assert_eq!(requests[0].cache_scope, requests[1].cache_scope);
    assert!(session.ledger.as_messages().iter().any(|message| message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolResult { content, is_error: true, .. } if content.contains("Auto Mode safety classifier")))));
}

/// Stages a permission-mode switch the moment it is asked to approve a call,
/// via the shared `PendingMode` handle — the deterministic stand-in for a user
/// pressing shift+tab mid-turn. The staged switch must land at the batch
/// boundary, not inside the current batch.
struct StagingApprover {
    pending_mode: tcode_core::PendingMode,
    stage: PermissionMode,
}

#[async_trait]
impl Approver for StagingApprover {
    async fn ask(
        &self,
        _tool: &str,
        _summary: &str,
        _descriptor: &str,
        _input: &serde_json::Value,
    ) -> Approval {
        self.pending_mode.set(self.stage);
        Approval::simple(ApprovalDecision::Yes, None)
    }
}

fn plan_enter_notes(session: &Session) -> usize {
    session
        .ledger
        .entries()
        .iter()
        .filter(|e| matches!(e, Entry::Note(text) if text.contains("read-only planning phase")))
        .count()
}

#[tokio::test]
async fn exit_plan_approval_switches_mode_and_unblocks_edits() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "old").unwrap();
    let provider = MockProvider::new(vec![
        tool_use(
            "t1",
            "exit_plan",
            r#"{"plan":"Do the thing, carefully.","title":"Do it"}"#,
        ),
        tool_use(
            "t2",
            "edit",
            r#"{"path":"a.txt","old_string":"old","new_string":"new"}"#,
        ),
        text_done("done"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Plan);
    let revised_plan = "Do the thing, carefully, with the user revision.";
    let approver = ScriptedApprover::with_response(Approval {
        decision: ApprovalDecision::Yes,
        comment: Some(format!(
            "The user edited the plan before approving. Use this revised plan as the source of truth for execution, not the earlier draft:\n\n{revised_plan}"
        )),
        set_mode: Some(PermissionMode::AcceptEdits),
        approved_input: Some(serde_json::json!({
            "plan": revised_plan,
            "title": "Do it",
        })),
    });

    let events = run(&agent, &mut session, &approver, "make a plan").await;

    assert_eq!(session.mode, PermissionMode::AcceptEdits);
    // The follow-up edit ran without a prompt because accept-edits auto-allows.
    assert_eq!(approver.asked.lock().unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "new"
    );
    let plan_result = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolEnd { name, content, .. } if name == "exit_plan" => Some(content),
            _ => None,
        })
        .expect("exit_plan result");
    assert!(plan_result.contains("Permission mode is now accept-edits"));
    let plans_dir = tcode_core::store::plans_dir(dir.path());
    let saved = std::fs::read_dir(&plans_dir)
        .expect("exit_plan creates the mirror directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "md"))
        .expect("approved plan is mirrored");
    assert_eq!(std::fs::read_to_string(&saved).unwrap(), revised_plan);
    assert!(
        session.ledger.entries().iter().any(|entry| matches!(
            entry,
            Entry::UserNote { about, text, .. }
                if about == "exit_plan" && text.contains(revised_plan)
        )),
        "an approved plan comment must survive as a user note"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::UserNote { text, .. } if text.contains(revised_plan)
        )),
        "the TUI must receive the approved plan comment"
    );
    // The test owns this runtime mirror; avoid leaving project-state files in
    // the developer's home directory after the temporary workspace is gone.
    let _ = std::fs::remove_file(saved);
    let _ = std::fs::remove_dir(&plans_dir);
    if let Some(project_data) = tcode_core::store::project_data_dir(dir.path()) {
        let _ = std::fs::remove_dir(project_data);
    }
}

#[tokio::test]
async fn exit_plan_rejection_keeps_plan_mode_and_returns_feedback() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        tool_use("t1", "exit_plan", r#"{"plan":"Draft plan body."}"#),
        text_done("revising"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Plan);
    let approver = ScriptedApprover::new(ApprovalDecision::No, Some("add a rollback step"));

    run(&agent, &mut session, &approver, "make a plan").await;

    assert_eq!(session.mode, PermissionMode::Plan);
    let results = tool_results(&session);
    assert!(results[0].1, "rejection is an error result");
    assert!(results[0].0.contains("add a rollback step"));
}

#[tokio::test]
async fn exit_plan_outside_plan_mode_is_a_self_healing_error() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        tool_use("t1", "exit_plan", r#"{"plan":"Draft plan body."}"#),
        text_done("ok"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Default);
    let approver = ScriptedApprover::new(ApprovalDecision::Yes, None);

    run(&agent, &mut session, &approver, "exit plan").await;

    assert!(
        approver.asked.lock().unwrap().is_empty(),
        "no prompt outside plan mode"
    );
    let results = tool_results(&session);
    assert!(results[0].1);
    assert!(results[0].0.contains("not in plan mode"));
}

#[tokio::test]
async fn a_staged_switch_takes_effect_at_the_next_batch_boundary() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "one").unwrap();
    // Two edits in separate steps: the first runs under the old (default) mode
    // after the approval stages plan; the second, past the boundary, is blocked.
    let provider = MockProvider::new(vec![
        tool_use(
            "t1",
            "edit",
            r#"{"path":"a.txt","old_string":"one","new_string":"two"}"#,
        ),
        tool_use(
            "t2",
            "edit",
            r#"{"path":"a.txt","old_string":"two","new_string":"three"}"#,
        ),
        text_done("done"),
    ]);
    let agent = agent(provider);
    let mut session = session(dir.path(), PermissionMode::Default);
    let approver = StagingApprover {
        pending_mode: session.pending_mode.clone(),
        stage: PermissionMode::Plan,
    };

    let events = run(&agent, &mut session, &approver, "edit twice").await;

    // First edit executed under the pre-switch mode; the second was blocked
    // once the staged switch to plan committed at the boundary.
    assert_eq!(session.mode, PermissionMode::Plan);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "two"
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::ModeChanged(PermissionMode::Plan))));
    // Entering plan injects exactly one guidance note.
    assert_eq!(plan_enter_notes(&session), 1);
    let results = tool_results(&session);
    assert!(
        results.last().unwrap().0.contains("plan mode"),
        "second edit must be blocked by plan mode"
    );
}
