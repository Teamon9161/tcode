//! The two directions across the webview boundary.
//!
//! Out: `AgentEvent`s, tagged with the session that produced them. In:
//! approval answers, matched back to the `ask` call that is still awaiting one.
//!
//! Everything here is written against [`Emit`] rather than `AppHandle`, so the
//! turn-driving logic can be tested with a collector instead of a window. The
//! webview is a renderer, not a participant in the loop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use tauri::Emitter;
use tokio::sync::oneshot;

use tcode_core::{AgentEvent, Approval, ApprovalDecision, Approver, BatchApproval, BatchAsk};

/// Event names the frontend listens on. Constants because the TypeScript side
/// hard-codes the same strings; a typo here is a silently dead UI.
pub const AGENT_EVENT: &str = "tcode://agent-event";
pub const APPROVAL_REQUEST: &str = "tcode://approval-request";
pub const TURN_FINISHED: &str = "tcode://turn-finished";

/// Somewhere to send an event. `AppHandle` is the real implementation; tests
/// substitute a collector so no webview is needed to assert on the stream.
pub trait Emit: Send + Sync + 'static {
    fn emit(&self, event: &str, payload: Value);
}

impl Emit for tauri::AppHandle {
    fn emit(&self, event: &str, payload: Value) {
        // A closed window is the ordinary way this fails, and it is not the
        // agent loop's problem: the turn keeps running and keeps recording to
        // the ledger, which is what a resume will read. But it is never
        // swallowed — a silently dropped event stream looks exactly like a
        // hung turn from the frontend, and that is not a debuggable failure.
        if let Err(error) = Emitter::emit(self, event, payload) {
            eprintln!("tcode-app: could not emit '{event}': {error}");
        }
    }
}

/// Every event carries its session id. Phase 1 has one session, but the
/// frontend must never learn to assume that — the whole point of the desktop
/// app is several running at once.
#[derive(Serialize)]
pub struct SessionEvent<'a> {
    pub session: &'a str,
    pub event: &'a AgentEvent,
}

/// A turn ended. `error` is `None` on a clean finish; the frontend needs this
/// as a separate signal because a failed turn never produces `TurnEnd`.
#[derive(Serialize)]
pub struct TurnFinished<'a> {
    pub session: &'a str,
    pub error: Option<String>,
}

/// One approval the frontend has been asked about and has not answered yet.
#[derive(Serialize)]
pub struct ApprovalRequest<'a> {
    pub session: &'a str,
    /// Correlates the answer back to the awaiting `ask`.
    pub id: &'a str,
    pub tool: &'a str,
    pub summary: &'a str,
    pub descriptor: &'a str,
    pub is_edit: bool,
    /// Whether "always allow in this project" is an offer here at all.
    pub allows_project: bool,
    pub input: &'a Value,
}

/// The answer coming back. A separate type from `Approval` because the wire
/// side names decisions as strings and knows nothing about mode transitions.
#[derive(serde::Deserialize)]
pub struct ApprovalAnswer {
    pub id: String,
    /// `yes` | `yes-session` | `yes-project` | `no`. Anything else is a denial:
    /// an answer this side cannot read must not be taken as consent.
    pub decision: String,
    pub comment: Option<String>,
}

impl ApprovalAnswer {
    fn into_approval(self) -> Approval {
        let decision = match self.decision.as_str() {
            "yes" => ApprovalDecision::Yes,
            "yes-session" => ApprovalDecision::YesSession,
            "yes-project" => ApprovalDecision::YesProject,
            _ => ApprovalDecision::No,
        };
        Approval::simple(decision, self.comment.filter(|c| !c.trim().is_empty()))
    }
}

/// Approvals awaiting an answer, keyed by request id.
#[derive(Clone, Default)]
pub struct Pending(Arc<Mutex<HashMap<String, oneshot::Sender<Approval>>>>);

impl Pending {
    /// Deliver an answer. Returns false if the id is unknown — a double answer,
    /// or one for a turn that was interrupted while the dialog was open.
    pub fn answer(&self, answer: ApprovalAnswer) -> bool {
        let sender = self.0.lock().unwrap().remove(&answer.id);
        match sender {
            Some(sender) => sender.send(answer.into_approval()).is_ok(),
            None => false,
        }
    }

    /// Fail every outstanding request closed. Called when a turn ends by any
    /// route other than the user answering, so a stale dialog cannot authorize
    /// something later.
    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    fn register(&self, id: String) -> oneshot::Receiver<Approval> {
        let (tx, rx) = oneshot::channel();
        self.0.lock().unwrap().insert(id, tx);
        rx
    }
}

/// Asks the webview, and waits.
///
/// Deliberately has no timeout: a human reading a diff is not a stalled
/// request, and the agent loop is already parked on this call. The failure
/// mode that matters is the *other* one — a dropped channel — and that denies.
pub struct WebviewApprover {
    session: String,
    emit: Arc<dyn Emit>,
    pending: Pending,
}

impl WebviewApprover {
    pub fn new(session: String, emit: Arc<dyn Emit>, pending: Pending) -> Self {
        Self {
            session,
            emit,
            pending,
        }
    }
}

#[async_trait]
impl Approver for WebviewApprover {
    async fn ask(
        &self,
        tool: &str,
        summary: &str,
        descriptor: &str,
        is_edit: bool,
        allows_project: bool,
        input: &Value,
    ) -> Approval {
        let id = uuid::Uuid::new_v4().to_string();
        let rx = self.pending.register(id.clone());
        self.emit.emit(
            APPROVAL_REQUEST,
            serde_json::to_value(ApprovalRequest {
                session: &self.session,
                id: &id,
                tool,
                summary,
                descriptor,
                is_edit,
                allows_project,
                input,
            })
            .unwrap_or(Value::Null),
        );
        match rx.await {
            Ok(approval) => approval,
            // The window closed, or the turn was cancelled out from under the
            // dialog. Nobody consented, so nothing runs.
            Err(_) => Approval::simple(
                ApprovalDecision::No,
                Some("the approval dialog closed before it was answered".into()),
            ),
        }
    }

    /// Phase 1 reviews calls one at a time. The default trait implementation
    /// already means that; it is spelled out here because a batch UI is a real
    /// planned feature and the next person should see where it hooks in.
    async fn ask_batch(&self, _label: &str, _calls: &[BatchAsk<'_>]) -> BatchApproval {
        BatchApproval::Individually
    }
}

/// Forward a session's event stream to the webview until the sender is dropped.
pub async fn pump_events(
    session: String,
    emit: Arc<dyn Emit>,
    mut rx: tokio::sync::mpsc::Receiver<AgentEvent>,
) {
    while let Some(event) = rx.recv().await {
        emit.emit(
            AGENT_EVENT,
            serde_json::to_value(SessionEvent {
                session: &session,
                event: &event,
            })
            .unwrap_or(Value::Null),
        );
    }
}
