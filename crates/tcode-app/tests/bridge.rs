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

use tcode_app::bridge::{ApprovalAnswer, Emit, AGENT_EVENT, APPROVAL_REQUEST, TURN_FINISHED};
use tcode_app::state::{run_turn, SessionHandle, Supervisor};

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
            max_tokens: 1024,
            context_window: 200_000,
            effort: None,
        }),
        models: AgentModels::default(),
        tools: tcode_tools::builtin_tools(cwd),
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
fn factory() -> tcode_app::boot::SessionFactory {
    tcode_app::boot::SessionFactory::new(
        PathBuf::from("/nonexistent/config.toml"),
        ModelCell::new(ActiveModel {
            provider: MockProvider::new(Vec::new()),
            max_tokens: 1024,
            context_window: 200_000,
            effort: None,
        }),
        Arc::new(tcode_tools::ShellFilters::disabled()),
    )
}

fn handle(id: &str, cwd: PathBuf) -> Arc<SessionHandle> {
    Arc::new(SessionHandle::new(
        id.to_string(),
        cwd.clone(),
        session(cwd),
    ))
}

fn say(text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text { text: text.into() }]
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

    run_turn(agent, session, emit, say("hi")).await.unwrap();

    let types = collector.event_types("s1");
    assert!(types.contains(&"Started".to_string()), "got {types:?}");
    assert!(types.contains(&"TextDelta".to_string()), "got {types:?}");
    assert_eq!(types.last().unwrap(), "TurnEnd");

    let finished = collector.payloads(TURN_FINISHED);
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["session"], "s1");
    assert_eq!(finished[0]["error"], Value::Null);
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
        async move { run_turn(agent, session, emit, say("write a note")).await }
    });

    let request = collector
        .wait_for(APPROVAL_REQUEST, |p| p["session"] == "s1")
        .await;
    assert_eq!(request["tool"], "write");
    assert_eq!(request["is_edit"], true);
    assert_eq!(request["input"]["content"], "written\n");

    let answered = session.pending().answer(ApprovalAnswer {
        id: request["id"].as_str().unwrap().to_string(),
        decision: "yes".into(),
        comment: None,
    });
    assert!(answered, "the pending approval accepted its answer");

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
        async move { run_turn(agent, session, emit, say("write a note")).await }
    });

    let request = collector
        .wait_for(APPROVAL_REQUEST, |p| p["session"] == "s1")
        .await;
    session.pending().answer(ApprovalAnswer {
        id: request["id"].as_str().unwrap().to_string(),
        decision: "sure-why-not".into(),
        comment: None,
    });

    turn.await.unwrap().unwrap();
    assert!(!target.exists(), "a decision we cannot read never runs");
}

/// Answering an approval twice is not an error the second time — it is a
/// no-op, because the answer that mattered was already delivered.
#[tokio::test]
async fn a_stale_answer_is_rejected_rather_than_replayed() {
    let pending = tcode_app::bridge::Pending::default();
    assert!(!pending.answer(ApprovalAnswer {
        id: "never-asked".into(),
        decision: "yes".into(),
        comment: None,
    }));
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
    let supervisor = Supervisor::new(agent_one.clone(), factory());
    let handle_one = handle("s1", one.path().to_path_buf());
    let handle_two = handle("s2", two.path().to_path_buf());
    supervisor.open(handle_one.clone());
    supervisor.open(handle_two.clone());

    let (a, b) = tokio::join!(
        run_turn(agent_one, handle_one, emit.clone(), say("one")),
        run_turn(agent_two, handle_two, emit, say("two")),
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
        async move { run_turn(agent, session, emit, say("first")).await }
    });
    // Park the turn on an approval, so it is provably still running.
    let request = collector
        .wait_for(APPROVAL_REQUEST, |p| p["session"] == "s1")
        .await;

    let second = run_turn(agent, session.clone(), emit, say("second")).await;
    assert!(
        matches!(second, Err(tcode_app::state::TurnError::Busy(id)) if id == "s1"),
        "a busy session refuses a second turn"
    );

    session.pending().answer(ApprovalAnswer {
        id: request["id"].as_str().unwrap().to_string(),
        decision: "no".into(),
        comment: None,
    });
    turn.await.unwrap().unwrap();
}
