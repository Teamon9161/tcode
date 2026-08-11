//! Backend integration tests: a scripted provider drives the real agent loop
//! through the real event and approval bridges, with a collector standing in
//! for the webview.
//!
//! No window, no API. What is under test is the boundary the desktop app adds:
//! that every event reaches the frontend tagged with its session, that an
//! approval round-trips through a command, and that two sessions running at
//! once never see each other's stream.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use tcode_core::config::WatchdogConfig;
use tcode_core::{
    ActiveModel, Agent, AgentModels, CacheStrategy, ContentBlock, EventStream, ModelCell,
    PermissionMode, PermissionRules, Provider, ProviderError, Request, Session, StopReason,
    StreamEvent, ToolCtx, Usage,
};

use tcode_app::bridge::{
    ApprovalAnswer, Emit, AGENT_EVENT, APPROVAL_REQUEST, TURN_FINISHED, TURN_STARTED,
};
use tcode_app::state::{run_compact, run_turn, watch_monitors, SessionHandle, Supervisor};

// ---------------------------------------------------------------- the webview

/// Stands in for the window: records everything that would have crossed the
/// IPC boundary, as the JSON the frontend would actually receive.
#[derive(Default)]
struct Collector {
    events: Mutex<Vec<(String, Value)>>,
    /// Woken on every emit, so a test can wait for a specific one instead of
    /// sleeping and hoping.
    notify: tokio::sync::Notify,
}

impl Collector {
    fn payloads(&self, name: &str) -> Vec<Value> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(event, _)| event == name)
            .map(|(_, payload)| payload.clone())
            .collect()
    }

    /// Agent events for one session, as `type` strings in arrival order.
    fn event_types(&self, session: &str) -> Vec<String> {
        self.payloads(AGENT_EVENT)
            .into_iter()
            .filter(|p| p["session"] == session)
            .map(|p| p["event"]["type"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// Block until `find` matches something, so approval tests never race the
    /// loop. Panics rather than hanging forever if the turn ends first.
    async fn wait_for(&self, name: &str, find: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..200 {
            if let Some(found) = self.payloads(name).into_iter().find(&find) {
                return found;
            }
            let _ =
                tokio::time::timeout(std::time::Duration::from_millis(50), self.notify.notified())
                    .await;
        }
        panic!("no '{name}' event arrived");
    }
}

/// The `Emit` side of a `Collector`. A newtype because `Emit` and `Arc` are
/// both foreign to this test crate.
struct Sink(Arc<Collector>);

impl Emit for Sink {
    fn emit(&self, event: &str, payload: Value) {
        self.0
            .events
            .lock()
            .unwrap()
            .push((event.to_string(), payload));
        self.0.notify.notify_waiters();
    }
}

fn sink(collector: &Arc<Collector>) -> Arc<dyn Emit> {
    Arc::new(Sink(collector.clone()))
}

// ---------------------------------------------------------------- the provider

struct MockProvider {
    scripts: Mutex<VecDeque<Vec<StreamEvent>>>,
    requests: Mutex<Vec<Request>>,
    vision: bool,
}

impl MockProvider {
    fn new(scripts: Vec<Vec<StreamEvent>>) -> Arc<Self> {
        Self::with_vision(scripts, false)
    }

    fn with_vision(scripts: Vec<Vec<StreamEvent>>, vision: bool) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::new(Vec::new()),
            vision,
        })
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
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
    fn supports_vision(&self) -> bool {
        self.vision
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

fn text_done(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::Started,
        StreamEvent::TextDelta(text.into()),
        StreamEvent::Usage(Usage::default()),
        StreamEvent::Done(StopReason::EndTurn),
    ]
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

// ---------------------------------------------------------------- the harness

fn agent(provider: Arc<MockProvider>, cwd: &std::path::Path) -> Arc<Agent> {
    Arc::new(Agent {
        model: ModelCell::new(ActiveModel {
            provider,
            max_tokens: Some(1024),
            context_window: 200_000,
            effort: None,
        }),
        models: AgentModels::default(),
        tools: main_agent_tools(cwd),
        system: "test".into(),
        watchdog: WatchdogConfig::default(),
        hooks: Default::default(),
        safety_classifier: None,
        auto_policy: String::new(),
        max_steps: tcode_core::DEFAULT_MAX_STEPS,
        auto_compact: true,
        auto_compact_percent: 85,
    })
}

/// The toolset a main conversation gets, which is not `builtin_tools` alone:
/// `progress` and `ask_user` are main-agent additions (a sub-agent must not be
/// able to submit a plan or ask the user a question), and `tcode-frontend`
/// assembles them for the real app. Tests that drive a plan through the loop
/// need the same set, or the call they script is for a tool nobody registered.
fn main_agent_tools(cwd: &std::path::Path) -> Vec<Arc<dyn tcode_core::Tool>> {
    let mut tools = tcode_tools::builtin_tools(cwd);
    tools.push(Arc::new(tcode_tools::ProgressTool));
    tools.push(Arc::new(tcode_tools::AskUserTool));
    tools
}

/// A session with no persistence: these tests are about the bridge, and a
/// JSONL sink would only add a temp directory to clean up.
fn session(cwd: PathBuf) -> Session {
    Session::new(
        ToolCtx::new(cwd, 25_000),
        PermissionMode::Default,
        PermissionRules::default(),
    )
}

/// A factory these tests never call `open` on: they hand the supervisor
/// sessions directly. It exists so `Supervisor` can hold one unconditionally
/// rather than an `Option` that production code would have to branch on.
/// This test is about event isolation, not model switching, so the menus are
/// the "nothing configured" ones — a real state, not a stub.
fn menus() -> tcode_app::picker::Menus {
    Arc::new(std::sync::Mutex::new(
        tcode_app::picker::Pickers::unavailable(
            PathBuf::from("/nonexistent/config.toml"),
            "no provider is configured",
        ),
    ))
}

fn factory() -> tcode_app::boot::SessionFactory {
    factory_at(PathBuf::from("/nonexistent/config.toml"))
}

/// A factory that can really open a session, for the one test that needs a
/// second one to exist. It selects a built-in profile with a harmless inline
/// test key; no turn runs on the fresh session, so no network request is made.
fn working_factory(dir: &std::path::Path) -> tcode_app::boot::SessionFactory {
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        "default_profile = \"anthropic\"\n[profiles.anthropic]\napi_key = \"test-key\"\n",
    )
    .unwrap();
    factory_at(config)
}

fn factory_at(config: PathBuf) -> tcode_app::boot::SessionFactory {
    tcode_app::boot::SessionFactory::new(
        config,
        Arc::new(tcode_tools::ShellFilters::disabled()),
        tcode_core::AgentModels::default(),
        Arc::new(tcode_tools::AgentRegistry::builtin()),
    )
}

fn handle(id: &str, cwd: PathBuf) -> Arc<SessionHandle> {
    let session = session(cwd.clone());
    handle_of(id, cwd, session)
}

/// The same, for the tests that care what is already in the ledger.
fn handle_of(id: &str, cwd: PathBuf, session: Session) -> Arc<SessionHandle> {
    Arc::new(SessionHandle::new(id.to_string(), cwd, session))
}

fn say(text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text { text: text.into() }]
}

/// An ordinary turn: no harness-authored instruction in front of it.
fn plain() -> Vec<String> {
    Vec::new()
}

/// A prompt typed while a turn was running.
fn queued(text: &str) -> tcode_core::PendingMessage {
    tcode_core::PendingMessage {
        text: text.into(),
        attachments: Vec::new(),
        blocks: say(text),
        instructions: Vec::new(),
        expects_plan: false,
    }
}

/// An ordinary approval answer. The plan-review fields are what the desktop
/// review adds on top; the tests that exercise them fill those in themselves.
fn answer(id: &Value, decision: &str) -> ApprovalAnswer {
    ApprovalAnswer {
        id: id.as_str().unwrap_or_default().to_string(),
        decision: decision.into(),
        comment: None,
        set_mode: None,
        phases: None,
        notes: Vec::new(),
        fresh_session: false,
    }
}

// ---------------------------------------------------------------- the tests

/// The baseline contract: a turn's events reach the frontend in order, each
/// tagged with the session that produced it, and the turn reports a clean
/// finish separately from `TurnEnd` (a failed turn never emits `TurnEnd`).
#[tokio::test]
async fn a_turn_streams_its_events_to_the_frontend() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let agent = agent(MockProvider::new(vec![text_done("hello")]), cwd.path());
    let collector = Arc::new(Collector::default());
    let emit = sink(&collector);
    let session = handle("s1", cwd.path().to_path_buf());

    run_turn(agent, session, emit, say("hi"), plain(), false)
        .await
        .unwrap();

    let types = collector.event_types("s1");
    assert!(types.contains(&"Started".to_string()), "got {types:?}");
    assert!(types.contains(&"TextDelta".to_string()), "got {types:?}");
    assert_eq!(types.last().unwrap(), "TurnEnd");

    let finished = collector.payloads(TURN_FINISHED);
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["session"], "s1");
    assert_eq!(finished[0]["error"], Value::Null);
}

/// A mode chosen while the first call waits must reach Core's next batch
/// boundary, not wait until the turn returns. The visible event is a transcript
/// record only; a bare setting change must not become a model-visible ledger
/// note.
#[tokio::test]
async fn a_mid_turn_mode_switch_gates_the_next_call_and_reports_its_boundary() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let first = cwd.path().join("first.txt");
    let second = cwd.path().join("second.txt");
    let agent = agent(
        MockProvider::new(vec![
            tool_use(
                "call-1",
                "write",
                &serde_json::json!({ "path": first.to_string_lossy(), "content": "first\n" })
                    .to_string(),
            ),
            tool_use(
                "call-2",
                "write",
                &serde_json::json!({ "path": second.to_string_lossy(), "content": "second\n" })
                    .to_string(),
            ),
            text_done("done"),
        ]),
        cwd.path(),
    );
    let collector = Arc::new(Collector::default());
    let emit = sink(&collector);
    let session = handle("s1", cwd.path().to_path_buf());

    let turn = tokio::spawn({
        let (agent, session, emit) = (agent, session.clone(), emit);
        async move { run_turn(agent, session, emit, say("write two notes"), plain(), false).await }
    });

    let request = collector
        .wait_for(APPROVAL_REQUEST, |payload| payload["session"] == "s1")
        .await;
    session.set_mode(PermissionMode::AcceptEdits);
    assert_eq!(session.mode(), (PermissionMode::AcceptEdits, true));
    session
        .pending()
        .answer(answer(&request["id"], "yes"))
        .expect("the pending approval accepted its answer");

    turn.await.unwrap().unwrap();

    assert_eq!(std::fs::read_to_string(&first).unwrap(), "first\n");
    assert_eq!(std::fs::read_to_string(&second).unwrap(), "second\n");
    assert_eq!(collector.payloads(APPROVAL_REQUEST).len(), 1);
    assert_eq!(session.mode(), (PermissionMode::AcceptEdits, false));
    assert!(
        !session.history().iter().any(|entry| {
            matches!(entry, tcode_core::Entry::Note(text) if text == "permission mode → accept-edits")
        }),
        "the mode marker belongs to the frontend transcript, not the ledger"
    );

    let events = collector.payloads(AGENT_EVENT);
    let changed = events
        .iter()
        .position(|payload| {
            payload["event"]["type"] == "ModeChanged" && payload["event"]["data"] == "accept-edits"
        })
        .expect("the mode boundary reached the webview");
    let second_start = events
        .iter()
        .position(|payload| {
            payload["event"]["type"] == "ToolStart"
                && payload["event"]["data"]["call_id"] == "call-2"
        })
        .expect("the second call started");
    assert!(
        changed < second_start,
        "mode event must precede the re-gated call"
    );
}

/// Images queued during a running turn use that turn's frozen capability even
/// if the session is switched to a vision model before the queue is composed.
#[tokio::test]
async fn queued_images_follow_the_running_model_snapshot() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let target = cwd.path().join("note.txt");
    let text_only = MockProvider::new(vec![
        tool_use(
            "call-1",
            "write",
            &serde_json::json!({ "path": target.to_string_lossy(), "content": "written\n" })
                .to_string(),
        ),
        text_done("done"),
    ]);
    let agent = agent(text_only.clone(), cwd.path());
    let cell = ModelCell::new(ActiveModel {
        provider: text_only,
        max_tokens: Some(1024),
        context_window: 200_000,
        effort: None,
    });
    let live = Session::new(
        ToolCtx::new(cwd.path().to_path_buf(), 25_000).with_model(cell.clone()),
        PermissionMode::Default,
        PermissionRules::default(),
    );
    let handle = handle_of("s1", cwd.path().to_path_buf(), live);
    let supervisor = Arc::new(Supervisor::new(
        agent.clone(),
        factory(),
        menus(),
        Vec::new(),
    ));
    supervisor.open(handle.clone());
    let collector = Arc::new(Collector::default());
    let turn = tokio::spawn({
        let (agent, handle, emit) = (agent.clone(), handle.clone(), sink(&collector));
        async move { run_turn(agent, handle, emit, say("write a note"), plain(), false).await }
    });
    let request = collector
        .wait_for(APPROVAL_REQUEST, |payload| payload["session"] == "s1")
        .await;

    cell.swap(ActiveModel {
        provider: MockProvider::with_vision(vec![text_done("queued done")], true),
        max_tokens: Some(1024),
        context_window: 200_000,
        effort: None,
    });
    assert!(
        !handle.input_supports_vision(&agent.model),
        "the running turn keeps its text-only capability after the live cell switches"
    );
    tcode_app::commands::send_message(
        &sink(&collector),
        &supervisor,
        "s1".into(),
        "inspect this".into(),
        Some(vec![tcode_app::commands::ImageInput {
            media_type: "image/png".into(),
            data: "AQID".into(),
        }]),
        None,
    )
    .unwrap();

    let queued = handle.queued();
    let saved = match queued[0].blocks.first() {
        Some(ContentBlock::Text { text }) => text
            .strip_prefix("[pasted image saved to ")
            .and_then(|text| text.strip_suffix(']'))
            .expect("the queued image was saved rather than discarded"),
        other => panic!("expected a saved-image text block, got {other:?}"),
    };
    assert_eq!(std::fs::read(saved).unwrap(), [1, 2, 3]);
    handle
        .pending()
        .answer(answer(&request["id"], "no"))
        .unwrap();
    turn.await.unwrap().unwrap();
}

/// The approval round trip, which is the one thing the desktop app cannot
/// borrow from the terminal frontends: the request goes out as an event, the
/// loop parks, and a command carries the answer back in.
#[tokio::test]
async fn an_approval_crosses_the_boundary_and_comes_back() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let target = cwd.path().join("note.txt");
    let agent = agent(
        MockProvider::new(vec![
            tool_use(
                "call-1",
                "write",
                &serde_json::json!({ "path": target.to_string_lossy(), "content": "written\n" })
                    .to_string(),
            ),
            text_done("done"),
        ]),
        cwd.path(),
    );
    let collector = Arc::new(Collector::default());
    let emit = sink(&collector);
    let session = handle("s1", cwd.path().to_path_buf());

    let turn = tokio::spawn({
        let (agent, session, emit) = (agent, session.clone(), emit);
        async move { run_turn(agent, session, emit, say("write a note"), plain(), false).await }
    });

    let request = collector
        .wait_for(APPROVAL_REQUEST, |p| p["session"] == "s1")
        .await;
    assert_eq!(request["tool"], "write");
    assert_eq!(request["is_edit"], true);
    assert_eq!(request["input"]["content"], "written\n");

    session
        .pending()
        .answer(answer(&request["id"], "yes"))
        .expect("the pending approval accepted its answer");

    turn.await.unwrap().unwrap();
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "written\n",
        "the approved write actually ran"
    );
}

/// An unreadable decision is not consent. The wire is untrusted input, so
/// anything the backend cannot parse must fail closed rather than default to
/// the permissive branch.
#[tokio::test]
async fn an_unrecognized_decision_denies() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let target = cwd.path().join("note.txt");
    let agent = agent(
        MockProvider::new(vec![
            tool_use(
                "call-1",
                "write",
                &serde_json::json!({ "path": target.to_string_lossy(), "content": "nope\n" })
                    .to_string(),
            ),
            text_done("understood"),
        ]),
        cwd.path(),
    );
    let collector = Arc::new(Collector::default());
    let emit = sink(&collector);
    let session = handle("s1", cwd.path().to_path_buf());

    let turn = tokio::spawn({
        let (agent, session, emit) = (agent, session.clone(), emit);
        async move { run_turn(agent, session, emit, say("write a note"), plain(), false).await }
    });

    let request = collector
        .wait_for(APPROVAL_REQUEST, |p| p["session"] == "s1")
        .await;
    let _ = session
        .pending()
        .answer(answer(&request["id"], "sure-why-not"));

    turn.await.unwrap().unwrap();
    assert!(!target.exists(), "a decision we cannot read never runs");
}

/// Answering an approval twice is not an error the second time — it is a
/// no-op, because the answer that mattered was already delivered.
#[tokio::test]
async fn a_stale_answer_is_rejected_rather_than_replayed() {
    let pending = tcode_app::bridge::Pending::default();
    assert!(pending
        .answer(answer(&Value::from("never-asked"), "yes"))
        .is_err());
}

/// The reason the supervisor exists. Two sessions run at the same time over
/// one shared `Arc<Agent>`; neither may see the other's events.
#[tokio::test]
async fn concurrent_sessions_never_cross_streams() {
    tcode_core::home::testing::temp_home();
    let one = tempfile::tempdir().unwrap();
    let two = tempfile::tempdir().unwrap();
    let collector = Arc::new(Collector::default());
    let emit = sink(&collector);

    let agent_one = agent(MockProvider::new(vec![text_done("from one")]), one.path());
    let agent_two = agent(MockProvider::new(vec![text_done("from two")]), two.path());
    let supervisor = Supervisor::new(agent_one.clone(), factory(), menus(), Vec::new());
    let handle_one = handle("s1", one.path().to_path_buf());
    let handle_two = handle("s2", two.path().to_path_buf());
    supervisor.open(handle_one.clone());
    supervisor.open(handle_two.clone());

    let (a, b) = tokio::join!(
        run_turn(
            agent_one,
            handle_one,
            emit.clone(),
            say("one"),
            plain(),
            false
        ),
        run_turn(agent_two, handle_two, emit, say("two"), plain(), false),
    );
    a.unwrap();
    b.unwrap();

    let text_of = |session: &str| {
        collector
            .payloads(AGENT_EVENT)
            .into_iter()
            .filter(|p| p["session"] == session && p["event"]["type"] == "TextDelta")
            .map(|p| p["event"]["data"].as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>()
            .join("")
    };
    assert_eq!(text_of("s1"), "from one");
    assert_eq!(text_of("s2"), "from two");
    assert_eq!(collector.payloads(TURN_FINISHED).len(), 2);
    assert_eq!(supervisor.ids().len(), 2);
}

/// One session runs one turn at a time. The `Session` is *taken* for the
/// duration, so a second send while one is running is refused by ownership
/// rather than by a flag that could drift.
#[tokio::test]
async fn a_second_turn_on_a_busy_session_is_refused() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let target = cwd.path().join("note.txt");
    let agent = agent(
        MockProvider::new(vec![
            tool_use(
                "call-1",
                "write",
                &serde_json::json!({ "path": target.to_string_lossy(), "content": "x\n" })
                    .to_string(),
            ),
            text_done("done"),
        ]),
        cwd.path(),
    );
    let collector = Arc::new(Collector::default());
    let emit = sink(&collector);
    let session = handle("s1", cwd.path().to_path_buf());

    let turn = tokio::spawn({
        let (agent, session, emit) = (agent.clone(), session.clone(), emit.clone());
        async move { run_turn(agent, session, emit, say("first"), plain(), false).await }
    });
    // Park the turn on an approval, so it is provably still running.
    let request = collector
        .wait_for(APPROVAL_REQUEST, |p| p["session"] == "s1")
        .await;

    let second = run_turn(agent, session.clone(), emit, say("second"), plain(), false).await;
    assert!(
        matches!(second, Err(tcode_app::state::TurnError::Busy(id)) if id == "s1"),
        "a busy session refuses a second turn"
    );

    let _ = session.pending().answer(answer(&request["id"], "no"));
    turn.await.unwrap().unwrap();
}

// ------------------------------------------------------------------- queue

/// Typing while a turn runs queues the prompt, and the queue is emptied by a
/// turn — always. The delivery core does at a safe boundary covers the common
/// case; this covers the other one, where the turn ends before reaching a
/// boundary, and without the flush in `run_turn` the message would sit in a
/// queue nothing ever drains: accepted, shown, and silently never sent.
#[tokio::test]
async fn a_prompt_typed_during_a_turn_is_queued_and_then_sent() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let target = cwd.path().join("note.txt");
    let agent = agent(
        MockProvider::new(vec![
            tool_use(
                "call-1",
                "write",
                &serde_json::json!({ "path": target.to_string_lossy(), "content": "x\n" })
                    .to_string(),
            ),
            text_done("first done"),
            text_done("second done"),
        ]),
        cwd.path(),
    );
    let collector = Arc::new(Collector::default());
    let emit = sink(&collector);
    let session = handle("s1", cwd.path().to_path_buf());

    let turn = tokio::spawn({
        let (agent, session, emit) = (agent.clone(), session.clone(), emit.clone());
        async move { run_turn(agent, session, emit, say("first"), plain(), false).await }
    });
    // Park the turn on an approval, so it is provably still holding the session.
    let request = collector
        .wait_for(APPROVAL_REQUEST, |p| p["session"] == "s1")
        .await;

    // Free hands the message back for the caller to send; busy takes it.
    assert!(
        session.send_or_queue(queued("second thoughts")).is_none(),
        "a busy session takes the message rather than refusing it"
    );
    assert!(session.send_or_queue(queued("while you work")).is_none());
    assert_eq!(
        session
            .queued()
            .iter()
            .map(|m| m.text.clone())
            .collect::<Vec<_>>(),
        ["second thoughts", "while you work"]
    );

    // Taking one back is by position *and* text: the queue drains whole, so a
    // stale index normally finds nothing — but queue, watch it delivered, queue
    // again, and a late withdraw would take a message meant to be kept.
    assert!(session
        .withdraw_queued(0, "something else entirely")
        .is_none());
    assert_eq!(session.queued().len(), 2, "a mismatch removes nothing");
    assert_eq!(
        session
            .withdraw_queued(0, "second thoughts")
            .map(|m| m.text),
        Some("second thoughts".to_string())
    );

    let _ = session.pending().answer(answer(&request["id"], "no"));
    turn.await.unwrap().unwrap();

    assert!(
        session.queued().is_empty(),
        "the queue is drained by a turn"
    );
    let finished = collector.payloads(TURN_FINISHED);
    assert_eq!(
        finished.len(),
        1,
        "the fallback queue handoff stays within one visible turn lifecycle"
    );
}

/// Stop-and-send is a handoff, not just a cancellation: the queued prompt must
/// become the next model request even when the previous turn is parked on an
/// approval and never reaches a normal safe boundary.
#[tokio::test]
async fn interrupt_and_send_delivers_the_queued_message_in_a_successor_turn() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let target = cwd.path().join("note.txt");
    let provider = MockProvider::new(vec![
        tool_use(
            "call-1",
            "write",
            &serde_json::json!({ "path": target.to_string_lossy(), "content": "first\n" })
                .to_string(),
        ),
        tool_use(
            "call-2",
            "write",
            &serde_json::json!({ "path": target.to_string_lossy(), "content": "second\n" })
                .to_string(),
        ),
        text_done("sent now"),
    ]);
    let agent = agent(provider.clone(), cwd.path());
    let collector = Arc::new(Collector::default());
    let emit = sink(&collector);
    let session = handle("s1", cwd.path().to_path_buf());

    let running = tokio::spawn({
        let (agent, session, emit) = (agent, session.clone(), emit);
        async move { run_turn(agent, session, emit, say("first"), plain(), false).await }
    });
    let first_request = collector
        .wait_for(APPROVAL_REQUEST, |payload| payload["session"] == "s1")
        .await;

    let mut immediate = queued("say this now");
    immediate.instructions = vec!["Plan before making changes.".into()];
    assert!(session.send_or_queue(immediate).is_none());
    let (turn, waiting) = session.queued_with_turn();
    assert_eq!(waiting.len(), 1);
    let old_turn = turn.expect("running turn owns the queue");
    assert!(session.interrupt_and_flush(old_turn));

    // The replacement reaches its own approval before this old queue-strip
    // action arrives. It must not cancel the successor's cancellation token.
    let successor_request = collector
        .wait_for(APPROVAL_REQUEST, |payload| {
            payload["session"] == "s1" && payload["id"] != first_request["id"]
        })
        .await;
    assert!(
        !session.interrupt_and_flush(old_turn),
        "a stale stop action cannot cancel the successor turn"
    );
    assert!(
        session.queued().is_empty(),
        "the successor owns the queued prompt"
    );
    session
        .pending()
        .answer(answer(&successor_request["id"], "yes"))
        .expect("the successor approval remains live");

    running.await.unwrap().unwrap();

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        3,
        "the successor made its own model request and completed its tool call"
    );
    let successor_messages = serde_json::to_string(&requests[1].messages).unwrap();
    assert!(
        successor_messages.contains("say this now"),
        "the queued prompt reached the successor request"
    );
    assert!(
        successor_messages.contains("Plan before making changes."),
        "the queued instructions reach the successor request too"
    );
    assert!(
        collector
            .event_types("s1")
            .iter()
            .any(|kind| kind == "QueuedInput"),
        "the webview receives the delivered queued prompt"
    );
    assert_eq!(
        collector.payloads(TURN_FINISHED).len(),
        1,
        "the handoff stays one visible running lifecycle"
    );
}

// ------------------------------------------------------------------ rewind

/// Rewinding truncates at a real user entry, hands the prompt back, and returns
/// the conversation as replay will rebuild it.
#[tokio::test]
async fn rewinding_drops_the_tail_and_returns_the_prompt() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let agent = agent(
        MockProvider::new(vec![text_done("first answer"), text_done("second answer")]),
        cwd.path(),
    );
    let collector = Arc::new(Collector::default());
    let emit = sink(&collector);
    let session = handle("s1", cwd.path().to_path_buf());

    run_turn(
        agent.clone(),
        session.clone(),
        emit.clone(),
        say("one"),
        plain(),
        false,
    )
    .await
    .unwrap();
    run_turn(agent, session.clone(), emit, say("two"), plain(), false)
        .await
        .unwrap();

    let targets = session.rewind_targets();
    assert_eq!(
        targets.iter().map(|t| t.text.clone()).collect::<Vec<_>>(),
        ["one", "two"]
    );

    let before = session.history().len();
    let (text, restored) = session.rewind(targets[1].index, false).unwrap();

    assert_eq!(text, "two", "the prompt comes back to be edited and resent");
    assert!(
        restored.is_empty(),
        "nothing asked for files to be rolled back"
    );
    assert!(
        session.history().len() < before,
        "the tail stopped existing"
    );
    assert_eq!(
        session
            .rewind_targets()
            .iter()
            .map(|t| t.text.clone())
            .collect::<Vec<_>>(),
        ["one"]
    );
}

/// The index arrives from the webview, so it is data: an entry that is not a
/// rewind point must not truncate the ledger anywhere at all (AGENTS.md rule 3).
#[tokio::test]
async fn a_rewind_to_something_that_is_not_a_prompt_is_refused() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let agent = agent(MockProvider::new(vec![text_done("answer")]), cwd.path());
    let collector = Arc::new(Collector::default());
    let session = handle("s1", cwd.path().to_path_buf());

    run_turn(
        agent,
        session.clone(),
        sink(&collector),
        say("one"),
        plain(),
        false,
    )
    .await
    .unwrap();

    let before = session.history().len();
    assert!(session.rewind(9_999, false).is_err());
    // The assistant's own reply is a real entry and still not a rewind point.
    assert!(session.rewind(1, false).is_err());
    assert_eq!(
        session.history().len(),
        before,
        "a refused rewind changes nothing"
    );
}

// ------------------------------------------------------------- tool routing

/// The webview asks the backend where each tool's calls belong, rather than
/// keeping its own copy of the table. What matters is that the answer is
/// derived from the live tool set, not that any particular tool is in it.
#[test]
fn tool_views_report_routing_derived_from_the_live_tools() {
    use tcode_core::{BatchPolicy, PermissionRequest, Tool, ToolOutput};

    struct Fake(&'static str, BatchPolicy);

    #[async_trait]
    impl Tool for Fake {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "test double"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({ "type": "object" })
        }
        fn permission(&self, _: &Value) -> PermissionRequest {
            PermissionRequest::None
        }
        fn batch_policy(&self) -> BatchPolicy {
            self.1
        }
        async fn run(&self, _: Value, _: &ToolCtx, _: &CancellationToken) -> ToolOutput {
            ToolOutput::ok("ok")
        }
    }

    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(Fake("read", BatchPolicy::ParallelReadOnly)),
        Arc::new(Fake("edit", BatchPolicy::ParallelPerFile)),
        // Named like the real thing, and deliberately not it: routing comes from
        // the tool, so a look-alike must route like the ordinary tool it is.
        // The name list this replaced could not tell the two apart.
        Arc::new(Fake("progress", BatchPolicy::Isolated)),
        Arc::new(tcode_tools::ProgressTool),
        Arc::new(tcode_tools::AskUserTool),
    ];

    let metas = tcode_app::commands::tool_view_metas(&tools);
    let find = |name: &str| {
        metas
            .iter()
            .find(|meta| meta.name == name)
            .unwrap_or_else(|| panic!("{name} missing from tool_views"))
    };

    // Quiet output is the one field core can actually derive, and it must track
    // `batch_policy` rather than a name list.
    assert!(find("read").quiet_output);
    assert!(!find("edit").quiet_output);

    // An edit's diff already told the story at the call site.
    assert!(find("edit").hide_success_result);
    assert!(!find("read").hide_success_result);

    assert_eq!(find("read").route, "transcript");
    assert_eq!(find("ask_user").route, "silent");
    // Two entries answer to "progress" here; the metas are per tool, and the
    // real one is the one that owns the plan surface.
    assert_eq!(
        metas
            .iter()
            .filter(|meta| meta.name == "progress" && meta.route == "progress")
            .count(),
        1,
        "the real progress tool routes to the plan surface"
    );
    assert_eq!(
        metas
            .iter()
            .filter(|meta| meta.name == "progress" && meta.route == "transcript")
            .count(),
        1,
        "a tool that merely shares its name does not"
    );

    // The webview labels a call with core's own display name. Without it the
    // far side has to title-case tool names itself, which is a second naming
    // rule for the same tools sitting in the same column as core's own.
    assert_eq!(find("read").display_name, "Read");
    assert_eq!(find("progress").display_name, "Progress");
    // A retired name wears the live tool's, exactly as the TUI's registry does
    // it: an old log must not label the same call differently.
    assert_eq!(find("update_progress").display_name, "Progress");

    // A resumed session holds whatever the tool was called when it was recorded.
    // Without an entry the webview falls back to the plain transcript treatment,
    // so an old `update_progress` call came back as a tool card in the
    // conversation instead of feeding the plan surface.
    for retired in ["update_progress", "update_plan", "exit_plan"] {
        assert_eq!(
            metas
                .iter()
                .filter(|meta| meta.name == retired && meta.route == "progress")
                .count(),
            1,
            "{retired} routes where the live progress tool does"
        );
    }
}

// -------------------------------------------------------------- window occupancy

/// What the webview is told the conversation occupies must follow the *model's*
/// ledger, not the display history it is handed alongside it.
///
/// This is the whole reason the figure crossed the boundary instead of being
/// worked out on the far side. `history()` keeps the compacted era so the
/// transcript can still show it, so anything measuring what it receives charges
/// a compacted conversation for precisely the history compaction removed: a
/// conversation that was not full read as full the moment it was resumed.
#[test]
fn a_compacted_conversation_is_not_charged_for_the_history_it_shed() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let agent = agent(MockProvider::new(vec![]), cwd.path());

    // A conversation worth compacting: enough text that the archived era is a
    // large share of the prompt rather than noise beside the tool schemas.
    let long = |turn: usize| {
        tcode_core::Entry::User(vec![ContentBlock::Text {
            text: format!("{turn}: {}", "an earlier exchange, at length. ".repeat(120)),
        }])
    };

    let whole = {
        let mut session = session(cwd.path().to_path_buf());
        for turn in 0..40 {
            session.ledger.append(long(turn));
        }
        handle_of("whole", cwd.path().to_path_buf(), session)
    };
    let compacted = {
        let mut session = session(cwd.path().to_path_buf());
        for turn in 0..40 {
            session.ledger.append(long(turn));
        }
        session
            .ledger
            .compact("a short summary of all of it".into(), 40);
        handle_of("compacted", cwd.path().to_path_buf(), session)
    };

    let (before, estimated) = whole.context(&agent);
    let (after, _) = compacted.context(&agent);
    assert!(
        estimated,
        "no provider has answered, so this is an estimate"
    );

    // The display history did not shrink — that is the trap, stated as an
    // assertion so nobody "simplifies" this back to measuring what the webview
    // is handed.
    assert_eq!(
        compacted.history().len(),
        whole.history().len() + 1,
        "the human's view keeps the archived era, plus the summary"
    );
    assert!(
        after * 4 < before,
        "compaction must show up as a large drop: {before} -> {after}"
    );

    // And the floor is the prompt that is there whatever the conversation says:
    // system prompt and tool schemas, neither of which the webview ever sees.
    assert!(after > 0, "an empty reading would mean the prompt was free");
}

// ---------------------------------------------------------------- plan review

/// A plan-flagged turn that ends without a plan cannot do so silently: the
/// harness notice reaches the webview as a `Note` event. This is the desktop
/// side of the turn-end guard, reached through the composer's plan switch
/// rather than a slash command.
#[tokio::test]
async fn a_plan_flagged_turn_that_ends_without_a_plan_says_so() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let agent = agent(
        MockProvider::new(vec![
            text_done("here are my thoughts, no plan"),
            text_done("still no plan"),
        ]),
        cwd.path(),
    );
    let collector = Arc::new(Collector::default());
    let emit = sink(&collector);
    let session = handle("s1", cwd.path().to_path_buf());

    run_turn(
        agent,
        session,
        emit.clone(),
        Vec::new(),
        vec![tcode_core::commands::plan::planning_instruction("")],
        true,
    )
    .await
    .unwrap();

    let notes = collector
        .payloads(AGENT_EVENT)
        .into_iter()
        .filter_map(|payload| {
            let event = payload["event"].clone();
            (event["type"] == "Note")
                .then(|| event["data"].as_str().unwrap_or_default().to_string())
        })
        .collect::<Vec<_>>();
    assert!(
        notes
            .iter()
            .any(|text| text.contains("No plan was produced")),
        "{notes:?}"
    );
}

/// A `progress` submission, which is what puts a plan in front of the user.
fn submit_plan(title: &str, phases: &[(&str, &str)]) -> Vec<StreamEvent> {
    let phases: Vec<Value> = phases
        .iter()
        .map(|(phase, detail)| {
            serde_json::json!({ "phase": phase, "status": "pending", "detail": detail })
        })
        .collect();
    tool_use(
        "call-plan",
        "progress",
        // A submission carries the whole plan, not just its checklist: core
        // refuses one that is missing the description or the prose no phase
        // holds, before the user is asked anything.
        &serde_json::json!({
            "title": title,
            "description": "what this plan is for",
            "background": "## Decision\nWorth doing.",
            "state": "active",
            "phases": phases,
        })
        .to_string(),
    )
}

/// A plan review with the user's edits, comments and choice of where to run.
fn plan_answer(
    id: &Value,
    decision: &str,
    phases: Option<Value>,
    notes: Vec<(&str, &str)>,
    fresh_session: bool,
) -> ApprovalAnswer {
    ApprovalAnswer {
        id: id.as_str().unwrap_or_default().to_string(),
        decision: decision.into(),
        comment: None,
        set_mode: None,
        phases,
        notes: notes
            .into_iter()
            .map(|(quote, text)| tcode_app::bridge::AnswerNote {
                quote: Some(quote.to_string()),
                text: text.to_string(),
            })
            .collect(),
        fresh_session,
    }
}

/// Every note this session's ledger holds, which is where the comment on an
/// *approved* call lands — and therefore where to check what the model was told.
fn notes_in(handle: &SessionHandle) -> String {
    handle
        .history()
        .iter()
        .filter_map(|entry| match entry {
            tcode_core::Entry::UserNote { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n---\n")
}

/// Tool results, which is where a *declined* call's reason goes: the call never
/// ran, so there is no approved action for a note to annotate.
fn tool_results_in(handle: &SessionHandle) -> String {
    handle
        .history()
        .iter()
        .filter_map(|entry| match entry {
            tcode_core::Entry::ToolResults(blocks) => Some(
                blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n---\n")
}

/// The desktop review's whole point: the user rewrites the plan in the panel,
/// and the plan that executes — and the file on disk — is theirs, not the
/// model's. The edit arrives as a *breakdown*, and the tool input it becomes is
/// rebuilt here from the request this side sent.
#[tokio::test]
async fn a_plan_edited_in_the_review_is_the_one_that_lands() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let agent = agent(
        MockProvider::new(vec![
            submit_plan(
                "Rewrite the resume path",
                &[("survey the call sites", "read only")],
            ),
            text_done("starting"),
        ]),
        cwd.path(),
    );
    let collector = Arc::new(Collector::default());
    let emit = sink(&collector);
    let session = handle("s1", cwd.path().to_path_buf());

    let turn = tokio::spawn({
        let (agent, session, emit) = (agent, session.clone(), emit);
        async move { run_turn(agent, session, emit, say("plan it"), plain(), false).await }
    });

    let request = collector
        .wait_for(APPROVAL_REQUEST, |p| p["session"] == "s1")
        .await;
    assert_eq!(request["tool"], "progress");
    assert!(
        request["input"]["plan"]
            .as_str()
            .is_some_and(|plan| plan.contains("survey the call sites")),
        "the review carries the plan body, not just the call: {request}"
    );

    session
        .pending()
        .answer(plan_answer(
            &request["id"],
            "yes",
            Some(serde_json::json!([
                { "phase": "write the regression test", "status": "pending", "detail": "cross a Summary boundary" },
                { "phase": "survey the call sites", "status": "pending" },
            ])),
            vec![("read only", "do this second, after the test")],
            false,
        ))
        .expect("the review was answered");
    turn.await.unwrap().unwrap();

    let plan = session.plan().expect("the approved plan is this session's");
    assert_eq!(plan.state(), tcode_core::progress::ProgressState::Active);
    assert_eq!(
        plan.phases()
            .iter()
            .map(|phase| phase.phase.as_str())
            .collect::<Vec<_>>(),
        ["write the regression test", "survey the call sites"],
        "the user's order and their new phase, on disk"
    );
    // Detail the user did not touch survives their edit: the phases they sent
    // carried no `detail` for it, which core reads as "keep what you wrote".
    assert_eq!(plan.phases()[1].detail, "read only");

    let notes = notes_in(&session);
    assert!(
        notes.contains("The user edited the plan before approving"),
        "the model must be told which plan won: {notes}"
    );
    assert!(
        notes.contains("> read only") && notes.contains("do this second, after the test"),
        "an anchored comment reaches the model with its passage: {notes}"
    );
}

/// Keep planning: the comments and a diff go back, and the draft on disk stays
/// the model's own. Declining is not a quiet way to rewrite someone's plan.
#[tokio::test]
async fn keeping_planning_sends_the_diff_and_leaves_the_draft_alone() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let agent = agent(
        MockProvider::new(vec![
            submit_plan(
                "Rewrite the resume path",
                &[("survey the call sites", "read only")],
            ),
            text_done("understood"),
        ]),
        cwd.path(),
    );
    let collector = Arc::new(Collector::default());
    let emit = sink(&collector);
    let session = handle("s1", cwd.path().to_path_buf());

    let turn = tokio::spawn({
        let (agent, session, emit) = (agent, session.clone(), emit);
        async move { run_turn(agent, session, emit, say("plan it"), plain(), false).await }
    });
    let request = collector
        .wait_for(APPROVAL_REQUEST, |p| p["session"] == "s1")
        .await;
    session
        .pending()
        .answer(plan_answer(
            &request["id"],
            "no",
            Some(serde_json::json!([
                { "phase": "start with the test", "status": "pending" },
            ])),
            vec![("survey the call sites", "not a phase, do it while planning")],
            false,
        ))
        .expect("keep planning is an answer");
    turn.await.unwrap().unwrap();

    let plan = session.plan().expect("the draft is still open");
    assert_eq!(plan.state(), tcode_core::progress::ProgressState::Draft);
    assert_eq!(
        plan.phases()[0].phase,
        "survey the call sites",
        "a declined review must not rewrite the file"
    );
    let told = tool_results_in(&session);
    assert!(told.contains("User declined this action"), "{told}");
    assert!(told.contains("The user edited the plan:"), "{told}");
    assert!(told.contains("+## [ ] 1. start with the test"), "{told}");
    assert!(told.contains("> survey the call sites"), "{told}");
}

/// A plan edit that cannot be applied leaves the question standing. The turn is
/// parked on this approval; consuming it because a phase title was blank would
/// strand the conversation with no way back to it.
#[tokio::test]
async fn an_unreadable_plan_edit_leaves_the_review_answerable() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let agent = agent(
        MockProvider::new(vec![
            submit_plan(
                "Rewrite the resume path",
                &[("survey the call sites", "read only")],
            ),
            text_done("starting"),
        ]),
        cwd.path(),
    );
    let collector = Arc::new(Collector::default());
    let emit = sink(&collector);
    let session = handle("s1", cwd.path().to_path_buf());

    let turn = tokio::spawn({
        let (agent, session, emit) = (agent, session.clone(), emit);
        async move { run_turn(agent, session, emit, say("plan it"), plain(), false).await }
    });
    let request = collector
        .wait_for(APPROVAL_REQUEST, |p| p["session"] == "s1")
        .await;

    let refused = session
        .pending()
        .answer(plan_answer(
            &request["id"],
            "yes",
            Some(serde_json::json!([{ "phase": "  ", "status": "pending" }])),
            Vec::new(),
            false,
        ))
        .expect_err("a phase with no title is not a plan");
    assert!(refused.contains("non-empty"), "{refused}");

    session
        .pending()
        .answer(plan_answer(&request["id"], "yes", None, Vec::new(), false))
        .expect("the review is still there to answer");
    turn.await.unwrap().unwrap();
    assert_eq!(
        session.plan().unwrap().state(),
        tcode_core::progress::ProgressState::Active
    );
}

/// The plan surface reads the file, so an edit made behind the app's back — in
/// an editor, by the user — is what it shows. Reading is not repairing: the
/// model still finds out through its own next call.
#[tokio::test]
async fn the_plan_view_follows_the_file_and_a_hand_edit_writes_it() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let agent = agent(
        MockProvider::new(vec![
            tool_use(
                "call-plan",
                "progress",
                &serde_json::json!({
                    "title": "Track my own work",
                    "phases": [
                        { "phase": "one", "status": "in_progress", "detail": "why one" },
                        { "phase": "two", "status": "pending" },
                    ]
                })
                .to_string(),
            ),
            text_done("tracking"),
        ]),
        cwd.path(),
    );
    let collector = Arc::new(Collector::default());
    let emit = sink(&collector);
    let session = handle("s1", cwd.path().to_path_buf());
    run_turn(agent, session.clone(), emit, say("do it"), plain(), false)
        .await
        .unwrap();

    let plan = session.plan().expect("the model opened a plan");
    assert_eq!(
        (plan.title.as_str(), plan.counts()),
        ("Track my own work", (0, 2))
    );
    assert_eq!(
        plan.phases()[0].detail,
        "why one",
        "detail reaches the panel"
    );

    // The user edits the file directly; the panel shows theirs.
    let text = std::fs::read_to_string(plan.path())
        .unwrap()
        .replace("two", "two, renamed by hand");
    std::fs::write(plan.path(), text).unwrap();
    assert_eq!(
        session.plan().unwrap().phases()[1].phase,
        "two, renamed by hand"
    );

    // And an edit made in the panel lands on disk, phases validated on the way.
    let written = session
        .write_plan(&serde_json::json!([
            { "phase": "one", "status": "completed" },
            { "phase": "two, renamed by hand", "status": "in_progress" },
        ]))
        .expect("the plan is the user's to edit");
    assert_eq!(written.counts(), (1, 2));
    assert_eq!(
        tcode_core::progress::Progress::load(written.path())
            .unwrap()
            .counts(),
        (1, 2),
        "the file, not just the copy in memory"
    );
    assert!(
        session
            .write_plan(&serde_json::json!([{ "phase": "x", "status": "pending",
                "phases": [{ "phase": "y", "status": "pending",
                    "phases": [{ "phase": "z", "status": "pending" }] }] }]))
            .is_err(),
        "webview input gets the same validation the model's does"
    );
}

/// The handoff: an approved plan opens a second conversation on the same folder
/// that has adopted the file, and its first turn is told to execute it. A draft
/// is refused — the whole point is that this plan was approved.
#[tokio::test]
async fn an_approved_plan_can_be_handed_to_a_fresh_session() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let agent = agent(
        MockProvider::new(vec![
            submit_plan(
                "Rewrite the resume path",
                &[("survey the call sites", "read only")],
            ),
            text_done("starting"),
        ]),
        cwd.path(),
    );
    let collector = Arc::new(Collector::default());
    let emit = sink(&collector);
    let session = handle("s1", cwd.path().to_path_buf());
    let supervisor = Supervisor::new(
        agent.clone(),
        working_factory(cwd.path()),
        menus(),
        Vec::new(),
    );
    supervisor.open(session.clone());

    let turn = tokio::spawn({
        let (agent, session, emit) = (agent, session.clone(), emit);
        async move { run_turn(agent, session, emit, say("plan it"), plain(), false).await }
    });
    let request = collector
        .wait_for(APPROVAL_REQUEST, |p| p["session"] == "s1")
        .await;

    // While the plan is still a draft, there is nothing to hand on.
    let refused = match tcode_app::state::hand_off_plan(&supervisor, "s1") {
        Err(reason) => reason,
        Ok(_) => panic!("a draft must not be handed on"),
    };
    assert!(refused.contains("still a draft"), "{refused}");

    session
        .pending()
        .answer(plan_answer(&request["id"], "yes", None, Vec::new(), true))
        .expect("the review was answered");
    turn.await.unwrap().unwrap();

    let (fresh, instructions) =
        tcode_app::state::hand_off_plan(&supervisor, "s1").expect("an approved plan travels");
    assert_ne!(fresh.id, session.id);
    assert_eq!(fresh.cwd, session.cwd, "the same folder, a clean context");
    assert_eq!(instructions.len(), 1);
    assert!(instructions[0].contains("Execute the approved plan"));
    assert!(
        instructions[0].contains("survey the call sites"),
        "the plan travels in the text: that session has none of the conversation"
    );
    let adopted = fresh.plan().expect("the fresh session adopted the file");
    assert_eq!(adopted.path(), session.plan().unwrap().path());
}

/// A model pick is private to the addressed session, and the shared Agent
/// resolves each turn through that session's ToolCtx rather than its fallback.
#[tokio::test]
async fn a_model_pick_moves_only_the_selected_session() {
    let cwd = tempfile::tempdir().unwrap();
    let fallback = MockProvider::new(Vec::new());
    let selected = MockProvider::new(vec![text_done("selected answer")]);
    let untouched = MockProvider::new(vec![text_done("untouched answer")]);
    let agent = agent(fallback.clone(), cwd.path());

    let cell_a = ModelCell::new(ActiveModel {
        provider: fallback.clone(),
        max_tokens: Some(1024),
        context_window: 200_000,
        effort: None,
    });
    let cell_b = ModelCell::new(ActiveModel {
        provider: untouched.clone(),
        max_tokens: Some(1024),
        context_window: 300_000,
        effort: None,
    });
    let session_a = Session::new(
        ToolCtx::new(cwd.path().to_path_buf(), 25_000).with_model(cell_a.clone()),
        PermissionMode::Default,
        PermissionRules::default(),
    );
    let session_b = Session::new(
        ToolCtx::new(cwd.path().to_path_buf(), 25_000).with_model(cell_b.clone()),
        PermissionMode::Default,
        PermissionRules::default(),
    );

    let mut def = tcode_core::config::ModelDef::bare("mock-1");
    def.efforts = vec!["low".to_string(), "high".to_string()];
    let picked = selected.clone();
    let switch = Box::new(
        move |_: &tcode_frontend::ModelOption, effort: Option<&str>| {
            Ok(ActiveModel {
                provider: picked.clone(),
                max_tokens: Some(2048),
                context_window: 400_000,
                effort: effort.map(String::from),
            })
        },
    );
    let mut main_a = tcode_app::picker::MainPickers::unavailable("no presets in this test");
    main_a.models = tcode_frontend::ModelMenu {
        options: vec![tcode_frontend::ModelOption {
            profile: "mock".to_string(),
            def: def.clone(),
        }],
        current: 0,
        switch,
    };
    let handle_a = Arc::new(SessionHandle::with_main_pickers(
        "a".into(),
        cwd.path().to_path_buf(),
        session_a,
        main_a,
    ));
    let handle_b = handle_of("b", cwd.path().to_path_buf(), session_b);

    let mut pickers = tcode_app::picker::Pickers::unavailable(
        PathBuf::from("/nonexistent/config.toml"),
        "no provider is configured",
    );
    pickers.models.options = vec![tcode_frontend::ModelOption {
        profile: "mock".to_string(),
        def,
    }];
    let menus = Arc::new(std::sync::Mutex::new(pickers));
    let supervisor = Arc::new(Supervisor::new(agent.clone(), factory(), menus, Vec::new()));
    supervisor.open(handle_a.clone());
    supervisor.open(handle_b.clone());

    tcode_app::commands::choose_model(&supervisor, "a".into(), 0, Some("high".into()))
        .expect("the pick applies");

    assert_eq!(cell_a.snapshot().effort.as_deref(), Some("high"));
    assert_eq!(cell_a.snapshot().context_window, 400_000);
    assert_eq!(cell_b.snapshot().effort, None);
    assert_eq!(cell_b.snapshot().context_window, 300_000);
    let state_a = tcode_app::commands::picker_state(&supervisor, "a".into()).unwrap();
    let state_b = tcode_app::commands::picker_state(&supervisor, "b".into()).unwrap();
    assert_eq!(state_a.effort.as_deref(), Some("high"));
    assert_eq!(state_b.effort, None);

    let collector = Arc::new(Collector::default());
    run_turn(
        agent.clone(),
        handle_a,
        sink(&collector),
        say("use selected"),
        plain(),
        false,
    )
    .await
    .unwrap();
    run_turn(
        agent,
        handle_b,
        sink(&collector),
        say("stay untouched"),
        plain(),
        false,
    )
    .await
    .unwrap();

    assert_eq!(selected.requests().len(), 1);
    assert_eq!(untouched.requests().len(), 1);
    assert!(
        fallback.requests().is_empty(),
        "the Agent fallback is unused"
    );
}

/// Saving a desktop preset names the session-owned main selection and the
/// process-global role pin through their independent model catalogs.
#[test]
fn preset_save_keeps_main_and_role_index_domains_separate() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(Vec::new());
    let model = ModelCell::new(ActiveModel {
        provider,
        max_tokens: Some(1024),
        context_window: 200_000,
        effort: Some("medium".into()),
    });

    let mut main = tcode_app::picker::MainPickers::unavailable("unused");
    main.models.options = vec![tcode_frontend::ModelOption {
        profile: "project".into(),
        def: tcode_core::config::ModelDef::bare("project-main"),
    }];
    main.models.current = 0;
    main.presets.save = Box::new(|name, draft, main_models, role_models| {
        assert_eq!(name, "team");
        let main_at = draft.main.expect("main selection");
        assert_eq!(main_models.options[main_at].def.name, "project-main");
        let (_, role) = draft
            .roles
            .iter()
            .find(|(kind, _)| kind == "explore")
            .expect("explore role");
        let tcode_frontend::AgentModelChoice::Model { option, effort } = role else {
            panic!("explore is explicitly pinned")
        };
        assert_eq!(role_models[*option].def.name, "global-helper");
        assert_eq!(effort.as_deref(), Some("high"));
        Ok((
            vec![tcode_frontend::PresetOption {
                key: "team".into(),
                label: "team".into(),
            }],
            0,
            tcode_frontend::PresetSaveOutcome {
                replaced: false,
                changes: Vec::new(),
            },
        ))
    });

    let mut menus =
        tcode_app::picker::Pickers::unavailable(cwd.path().join("config.toml"), "unused");
    menus.models.options = vec![tcode_frontend::ModelOption {
        profile: "global".into(),
        def: tcode_core::config::ModelDef::bare("global-helper"),
    }];
    menus.agents = tcode_frontend::AgentMenu {
        roles: vec![tcode_frontend::AgentRole {
            key: "explore".into(),
            label: "explore".into(),
            allows_off: false,
            section: tcode_frontend::RoleSection::Task,
        }],
        pins: vec![tcode_frontend::AgentModelChoice::Model {
            option: 0,
            effort: Some("high".into()),
        }],
        pin: Box::new(|_, _| Ok("unused".into())),
    };

    tcode_app::picker::save_preset(&mut main, &menus, &model, "team").unwrap();
    assert_eq!(main.presets.current, Some(0));
}

#[test]
fn a_new_session_uses_the_latest_persisted_picker_identity() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let config = cwd.path().join("config.toml");
    std::fs::write(
        &config,
        r#"
default_profile = "local"

[profiles.local]
provider = "anthropic"
api_key = "test-key"
models = ["first", "second"]

[presets.latest]
profile = "local"
model = "second"

[tcode_state]
profile = "local"
model = "first"
"#,
    )
    .unwrap();

    let mut pickers = tcode_app::picker::Pickers::unavailable(config.clone(), "unused");
    pickers.models.options = vec![
        tcode_frontend::ModelOption {
            profile: "local".into(),
            def: tcode_core::config::ModelDef::bare("first"),
        },
        tcode_frontend::ModelOption {
            profile: "local".into(),
            def: tcode_core::config::ModelDef::bare("second"),
        },
    ];
    pickers.models.current = 0;
    pickers.presets.options = vec![tcode_frontend::PresetOption {
        key: "latest".into(),
        label: "latest".into(),
    }];
    pickers.presets.current = None;
    let agent = agent(MockProvider::new(Vec::new()), cwd.path());
    let supervisor = Arc::new(Supervisor::new(
        agent,
        factory_at(config.clone()),
        Arc::new(std::sync::Mutex::new(pickers)),
        Vec::new(),
    ));

    std::fs::write(
        &config,
        r#"
default_profile = "local"

[profiles.local]
provider = "anthropic"
api_key = "test-key"
models = ["first", "second", "team-model"]

[models."team-model"]
label = "Team Model (fresh)"
context_window = 345678
efforts = ["low", "high"]

[presets.latest]
profile = "local"
model = "team-model"

[tcode_state]
preset = "latest"
"#,
    )
    .unwrap();
    let handle = supervisor.open_folder(cwd.path(), None).unwrap();
    let state = tcode_app::commands::picker_state(&supervisor, handle.id.clone()).unwrap();

    let fresh = state
        .models
        .iter()
        .position(|model| model.label == "Team Model (fresh)")
        .expect("fresh model is present");
    assert_eq!(state.model, fresh);
    assert_eq!(state.preset, Some(0));
    assert_eq!(state.models[fresh].efforts, ["low", "high"]);
    assert_eq!(state.context_window, 345_678);
    assert_eq!(
        handle.model().unwrap().snapshot().provider.model(),
        "team-model"
    );
}

/// Desktop slash commands keep their ledger semantics in Core: the bridge only
/// selects the current session and interprets frontend-only effects.
#[tokio::test]
async fn desktop_clear_keeps_its_core_ledger_semantics() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let agent = agent(MockProvider::new(vec![text_done("answer")]), cwd.path());
    let handle = handle("s1", cwd.path().to_path_buf());
    let supervisor = Arc::new(Supervisor::new(
        agent.clone(),
        factory(),
        menus(),
        Vec::new(),
    ));
    supervisor.open(handle.clone());

    run_turn(
        agent,
        handle.clone(),
        Arc::new(Sink(Arc::new(Collector::default()))),
        say("hello"),
        plain(),
        false,
    )
    .await
    .unwrap();
    assert!(!handle.history().is_empty());

    let cleared = supervisor.dispatch_slash("s1", "/clear").unwrap();
    assert!(matches!(
        cleared.effects.as_slice(),
        [tcode_core::commands::CommandEffect::ConversationCleared]
    ));
    assert!(
        handle.history().is_empty(),
        "/clear must also remove archived history"
    );

    let offered = tcode_app::commands::slash_commands(&supervisor);
    assert!(offered.iter().any(|command| command.name == "/clear"));
    assert!(offered.iter().any(|command| command.name == "/compact"));
    assert!(
        offered.iter().all(|command| command.name != "/resume"),
        "stored conversations live in the rail, not the composer menu"
    );
    let Err(error) = tcode_app::commands::slash_command(
        &sink(&Arc::new(Collector::default())),
        &supervisor,
        "s1".into(),
        "/resume".into(),
    ) else {
        panic!("desktop /resume must be unavailable")
    };
    assert!(error.contains("unsupported desktop command"), "{error}");
}

/// Manual compaction is a real turn boundary for the desktop bridge: it streams
/// its dedicated events and settles the session before reporting completion.
#[tokio::test]
async fn desktop_compaction_streams_its_result_and_finishes_the_turn() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let agent = agent(
        MockProvider::new(vec![text_done("answer"), text_done("compact summary")]),
        cwd.path(),
    );
    let collector = Arc::new(Collector::default());
    let emit = sink(&collector);
    let handle = handle("s1", cwd.path().to_path_buf());

    run_turn(
        agent.clone(),
        handle.clone(),
        emit.clone(),
        say("hello"),
        plain(),
        false,
    )
    .await
    .unwrap();
    run_compact(
        agent,
        handle.clone(),
        emit,
        Some("keep the decision".into()),
    )
    .await
    .unwrap();

    let types = collector.event_types("s1");
    assert!(types.contains(&"Compacting".to_string()), "got {types:?}");
    assert!(types.contains(&"Compacted".to_string()), "got {types:?}");
    assert!(handle.history().iter().any(
        |entry| matches!(entry, tcode_core::Entry::Summary(summary) if summary == "compact summary")
    ));
    let finished = collector.payloads(TURN_FINISHED);
    assert_eq!(finished.len(), 2, "normal turn and compaction each finish");
    assert_eq!(finished.last().unwrap()["error"], Value::Null);
}

/// `/name` loads a skill here exactly as it does in the terminal, and the
/// transcript shows the line the person typed rather than the file it loaded.
///
/// Both halves matter. Skills were reachable by the `skill` tool but had no
/// slash route at all in this window, so a name you knew was still a way to
/// spend a whole extra round trip. And the rendered body is what goes to the
/// model — the ledger keeps it, `/export` keeps it — but a transcript that
/// printed it would answer "what did I ask" with a repository file.
#[tokio::test]
async fn a_skill_loads_as_a_prompt_and_shows_as_the_line_that_asked_for_it() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let dir = cwd.path().join(".tcode/skills/survey");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: survey\ndescription: look around\n---\n\nWalk the tree first.\n",
    )
    .unwrap();
    let skills = tcode_tools::discover_skills(cwd.path());
    assert!(skills.iter().any(|skill| skill.name == "survey"));

    let agent = agent(MockProvider::new(vec![text_done("done")]), cwd.path());
    let handle = handle("s1", cwd.path().to_path_buf());
    let supervisor = Supervisor::new(agent.clone(), factory(), menus(), skills);
    supervisor.open(handle.clone());

    let prompt =
        tcode_app::commands::skill_prompt(&supervisor, &handle, "survey", "the ledger").unwrap();
    assert!(
        prompt.contains("Walk the tree first."),
        "the model gets the rendered file: {prompt}"
    );
    let echo = tcode_tools::parse_skill_echo(&prompt).expect("wrapped as a skill echo");
    assert_eq!(
        (echo.name.as_str(), echo.args.as_str()),
        ("survey", "the ledger")
    );

    // The ledger keeps the body; the display copy keeps the ask.
    run_turn(
        agent,
        handle.clone(),
        Arc::new(Sink(Arc::new(Collector::default()))),
        say(&prompt),
        plain(),
        false,
    )
    .await
    .unwrap();
    let shown = handle.history();
    assert!(
        matches!(
            shown.first(),
            Some(tcode_core::Entry::User(blocks))
                if matches!(&blocks[0], ContentBlock::Text { text } if text == "/survey the ledger")
        ),
        "{shown:?}"
    );
    // Rewind pairs targets to prompts by text, so both sides fold or neither.
    assert_eq!(
        handle
            .rewind_targets()
            .into_iter()
            .map(|target| target.text)
            .collect::<Vec<_>>(),
        ["/survey the ledger"]
    );
}

/// An unknown name is still refused. The skill fallback widens what `/` can
/// mean; it must not turn a typo into something that runs.
#[test]
fn a_name_that_is_neither_a_command_nor_a_skill_stays_unknown() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let agent = agent(MockProvider::new(Vec::new()), cwd.path());
    let handle = handle("s1", cwd.path().to_path_buf());
    let supervisor = Supervisor::new(agent, factory(), menus(), Vec::new());
    supervisor.open(handle.clone());

    assert!(tcode_app::commands::skill_prompt(&supervisor, &handle, "nope", "").is_err());
}

/// A monitor firing while nobody is typing must reach the model on its own.
///
/// This is the whole point of the tool: `monitor` says "tell me when this
/// happens", and core answers by making an idle session's wake time readable
/// (`monitor_wake_deadline`). The terminal has always honoured it; the desktop
/// app did not, so every event sat in the registry until the user happened to
/// send a message — the watch fired, and the window showed nothing at all.
#[tokio::test]
async fn a_monitor_event_wakes_an_idle_conversation_by_itself() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let agent = agent(
        MockProvider::new(vec![text_done("the service is refusing connections")]),
        cwd.path(),
    );
    let collector = Arc::new(Collector::default());
    let emit = sink(&collector);

    let session = session(cwd.path().to_path_buf());
    let (id, watch) = session
        .tool_ctx
        .background
        .lock()
        .unwrap()
        .register_monitor(
            "tail -F app.log",
            "errors in app.log",
            std::time::Duration::from_millis(20),
        )
        .unwrap();
    assert_eq!(id, "m1");
    let handle = handle_of("s1", cwd.path().to_path_buf(), session);
    let watcher = tokio::spawn(watch_monitors(agent, handle.clone(), emit));

    // Nobody types anything. The line alone has to start a turn.
    watch.push_line("ERROR connection refused");

    let note = collector
        .wait_for(AGENT_EVENT, |payload| payload["event"]["type"] == "Note")
        .await;
    let text = note["event"]["data"].as_str().unwrap();
    assert!(text.contains("ERROR connection refused"), "{text}");
    assert!(text.contains("errors in app.log"), "{text}");

    let finished = collector
        .wait_for(TURN_FINISHED, |payload| payload["session"] == "s1")
        .await;
    assert_eq!(finished["error"], Value::Null);
    // The pane is told a turn began, and that it was not one anybody typed.
    let started = collector.payloads(TURN_STARTED);
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["kind"], "monitor");
    // The model was actually asked, and answered.
    let types = collector.event_types("s1");
    assert!(types.contains(&"TextDelta".to_string()), "got {types:?}");

    watcher.abort();
}

/// Closing a pane ends its watch. A session nothing can display must not keep
/// starting turns against the model — and the watcher parks on a `Notify`, so
/// cancelling without waking it would leave it asleep forever.
#[tokio::test]
async fn closing_a_conversation_stops_its_monitor_watch() {
    tcode_core::home::testing::temp_home();
    let cwd = tempfile::tempdir().unwrap();
    let agent = agent(MockProvider::new(Vec::new()), cwd.path());
    let collector = Arc::new(Collector::default());
    let handle = handle("s1", cwd.path().to_path_buf());
    let supervisor = Supervisor::new(agent.clone(), factory(), menus(), Vec::new());
    supervisor.open(handle.clone());

    let watcher = tokio::spawn(watch_monitors(agent, handle, sink(&collector)));
    assert!(supervisor.close("s1"));

    tokio::time::timeout(std::time::Duration::from_secs(5), watcher)
        .await
        .expect("the watcher returns once its session is closed")
        .unwrap();
}
