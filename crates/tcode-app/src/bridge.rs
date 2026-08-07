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
use tokio::sync::oneshot;

use tcode_core::progress::{self, PlanNote};
use tcode_core::{
    AgentEvent, Approval, ApprovalDecision, Approver, BatchApproval, BatchAsk, PermissionMode,
};

/// Event names the frontend listens on. Constants because the TypeScript side
/// hard-codes the same strings; a typo here is a silently dead UI.
pub const AGENT_EVENT: &str = "tcode://agent-event";
pub const APPROVAL_REQUEST: &str = "tcode://approval-request";
/// A turn began. The composer already flips its own pane to running when the
/// person presses enter, so this is redundant for prompts — and load-bearing
/// for every turn nobody typed: a monitor wake, a queued successor, a plan
/// handoff. Without it those run invisibly and the transcript grows on its own.
pub const TURN_STARTED: &str = "tcode://turn-started";
pub const TURN_FINISHED: &str = "tcode://turn-finished";
/// The window was maximized or restored. Emitted by **the shell**, not by
/// anything below it — `electron/main.js` — because a window is the one thing
/// the backend deliberately knows nothing about. It is named here anyway so
/// every event the frontend listens on has one list, and so the check below
/// covers it.
///
/// It exists because the state changes without a button here being pressed: a
/// snap gesture, a double-click on the title bar, the OS restoring the window.
/// The title bar is app-drawn (rule 9c), so its icon has to follow the window
/// rather than the last click.
pub const WINDOW_STATE: &str = "tcode://window-state";

/// Somewhere to send an event. The Electron shell's main process implements it
/// by pushing a JSON frame down the pipe; tests substitute a collector so no
/// window is needed to assert on the stream.
pub trait Emit: Send + Sync + 'static {
    fn emit(&self, event: &str, payload: Value);
}

/// Every event carries its session id. Phase 1 has one session, but the
/// frontend must never learn to assume that — the whole point of the desktop
/// app is several running at once.
#[derive(Serialize)]
pub struct SessionEvent<'a> {
    pub session: &'a str,
    pub event: &'a AgentEvent,
}

/// A turn began, and why. `kind` is what the pane shows before the first token
/// arrives — "monitor" is the difference between a transcript that appears to
/// grow by itself and one that says a watch fired.
#[derive(Serialize)]
pub struct TurnStarted<'a> {
    pub session: &'a str,
    /// `prompt` | `monitor` | `compact`.
    pub kind: &'a str,
}

/// A turn ended. `error` is `None` on a clean finish; the frontend needs this
/// as a separate signal because a failed turn never produces `TurnEnd`.
///
/// The context figure rides along because a turn is exactly where the webview's
/// running total can have gone wrong and cannot fix itself: an auto-compaction
/// mid-turn rewrites the window out from under the last `Usage` event the
/// webview saw, and nothing in the stream says how big the summary came out.
/// One authoritative reading per turn (`SessionHandle::context`) is the TUI's
/// answer too, and it costs nothing — the session is already back in hand here.
#[derive(Serialize)]
pub struct TurnFinished<'a> {
    pub session: &'a str,
    pub error: Option<String>,
    pub context_tokens: u64,
    pub context_estimated: bool,
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

/// One reviewer comment as the webview sends it. The quote is the passage the
/// user selected; core turns the pair into the note the model reads.
#[derive(serde::Deserialize)]
pub struct AnswerNote {
    pub quote: Option<String>,
    pub text: String,
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
    /// Ordinary approval only: a constrained mode transition requested by a
    /// button the desktop UI owns. Unknown values are rejected, never treated
    /// as extra consent.
    #[serde(default)]
    pub set_mode: Option<String>,
    /// Plan review only: the breakdown as the reviewer edited it. It is a
    /// breakdown and not a rendered body on purpose — the markdown grammar of a
    /// progress file is core's, so the webview never writes it, and this arrives
    /// as data to be validated like any model input.
    #[serde(default)]
    pub phases: Option<Value>,
    /// Plan review only: comments anchored to passages of the plan.
    #[serde(default)]
    pub notes: Vec<AnswerNote>,
    /// Plan review only: execute the approved plan in a fresh conversation
    /// instead of this one.
    #[serde(default)]
    pub fresh_session: bool,
}

impl ApprovalAnswer {
    fn decision(&self) -> ApprovalDecision {
        match self.decision.as_str() {
            "yes" => ApprovalDecision::Yes,
            "yes-session" => ApprovalDecision::YesSession,
            "yes-project" => ApprovalDecision::YesProject,
            _ => ApprovalDecision::No,
        }
    }

    fn free_comment(&self) -> &str {
        self.comment.as_deref().unwrap_or("")
    }

    fn plan_notes(&self) -> Vec<PlanNote> {
        self.notes
            .iter()
            .map(|note| PlanNote {
                quote: note.quote.clone(),
                text: note.text.clone(),
            })
            .collect()
    }

    /// Turn this into an `Approval`, given the request it answers.
    ///
    /// `asked` is the input **this side** sent to the webview, and the edited
    /// plan is rebuilt on top of it rather than from anything the webview
    /// returned. That is the whole reason the input is kept: an approval whose
    /// tool input came back from the frontend would let a compromised or simply
    /// buggy webview authorize one call and execute another.
    fn into_approval(self, asked: &Value, is_edit: bool) -> Result<Approval, String> {
        let decision = self.decision();
        let set_mode = match self.set_mode.as_deref() {
            None => None,
            Some("accept-edits") if is_edit && decision == ApprovalDecision::Yes => {
                Some(PermissionMode::AcceptEdits)
            }
            Some("accept-edits") => {
                return Err("only a one-time file-edit approval can enable accept-edits".into())
            }
            Some(_) => return Err("unrecognized permission mode".into()),
        };
        if !progress::is_plan_document(asked) {
            return Ok(Approval {
                decision,
                comment: self.comment.filter(|c| !c.trim().is_empty()),
                set_mode,
                approved_input: None,
                end_turn_after_execution: false,
            });
        }
        let original = asked[progress::REVIEW_BODY_FIELD].as_str().unwrap_or("");
        // Webview data, so the same validation the model's own breakdown gets.
        // Rejecting leaves the approval unanswered rather than guessing, and the
        // error names what is wrong with it.
        let revised = match self.phases.as_ref() {
            Some(phases) => {
                let phases = progress::phases_from_json(phases)?;
                // Detail the reviewer never opened is not detail they deleted:
                // core carries the stored text onto phases that came back
                // without any, exactly as it does for the model's own resends.
                let body = progress::revise_plan_body(original, &phases);
                (body.trim() != original.trim()).then_some(body)
            }
            None => None,
        };
        let notes = self.plan_notes();
        if decision == ApprovalDecision::No {
            return Ok(Approval {
                decision,
                comment: progress::declined_plan_note(
                    revised
                        .as_deref()
                        .and_then(|revised| progress::plan_revision_diff(original, revised))
                        .as_deref(),
                    &notes,
                    self.free_comment(),
                ),
                set_mode: None,
                approved_input: None,
                end_turn_after_execution: false,
            });
        }
        let approved_input = revised.as_deref().map(|revised| {
            let mut input = asked.clone();
            input[progress::REVIEW_BODY_FIELD] = Value::String(revised.to_string());
            input
        });
        Ok(Approval {
            decision,
            comment: progress::approved_plan_note(revised.as_deref(), &notes, self.free_comment()),
            set_mode: None,
            approved_input,
            // The plan still executes here; ending the turn afterwards is what
            // lets the handoff hand it to a session with a clean context.
            end_turn_after_execution: self.fresh_session,
        })
    }
}

/// One approval awaiting an answer: where to send it, and what was asked.
struct PendingAsk {
    reply: oneshot::Sender<Approval>,
    /// The exact input the webview was shown. Kept so an answer can only ever
    /// modify what was actually proposed — see `into_approval`.
    asked: Value,
    /// Whether this request was a file edit, as determined by core rather than
    /// by a mode string supplied from the webview.
    is_edit: bool,
}

/// Approvals awaiting an answer, keyed by request id.
#[derive(Clone, Default)]
pub struct Pending(Arc<Mutex<HashMap<String, PendingAsk>>>);

impl Pending {
    /// Deliver an answer.
    ///
    /// An unreadable plan edit is refused *without* consuming the request, so
    /// the dialog stays answerable: the turn is parked on this question, and
    /// dropping the only way back to it because a phase title was empty would
    /// strand the conversation. An unknown id is the other error — a double
    /// answer, or one for a turn interrupted while the dialog was open.
    pub fn answer(&self, answer: ApprovalAnswer) -> Result<(), String> {
        const GONE: &str = "this request is no longer waiting for an answer";
        let id = answer.id.clone();
        let approval = {
            let held = self.0.lock().unwrap();
            let ask = held.get(&id).ok_or(GONE)?;
            answer.into_approval(&ask.asked, ask.is_edit)?
        };
        let ask = self.0.lock().unwrap().remove(&id).ok_or(GONE)?;
        ask.reply
            .send(approval)
            .map_err(|_| "the conversation stopped waiting for this answer".to_string())
    }

    /// Fail every outstanding request closed. Called when a turn ends by any
    /// route other than the user answering, so a stale dialog cannot authorize
    /// something later.
    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    fn register(&self, id: String, asked: &Value, is_edit: bool) -> oneshot::Receiver<Approval> {
        let (tx, rx) = oneshot::channel();
        self.0.lock().unwrap().insert(
            id,
            PendingAsk {
                reply: tx,
                asked: asked.clone(),
                is_edit,
            },
        );
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
        let rx = self.pending.register(id.clone(), input, is_edit);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(set_mode: Option<&str>) -> ApprovalAnswer {
        ApprovalAnswer {
            id: "approval".into(),
            decision: "yes".into(),
            comment: None,
            set_mode: set_mode.map(str::to_owned),
            phases: None,
            notes: vec![],
            fresh_session: false,
        }
    }

    /// The event names are a contract with a file in another language (AGENTS.md
    /// rule 5), and nothing else checks it: the backend emits fine, the listener
    /// registers fine, and the pane simply never hears about the turn. The same
    /// mechanical check `terminal.rs` runs, for the events this file owns.
    #[test]
    fn the_event_names_match_the_frontend() {
        let types =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/src/types.ts"))
                .expect("ui/src/types.ts");

        for name in [
            AGENT_EVENT,
            APPROVAL_REQUEST,
            TURN_STARTED,
            TURN_FINISHED,
            WINDOW_STATE,
        ] {
            assert!(
                types.contains(&format!("\"{name}\"")),
                "ui/src/types.ts does not carry `{name}` — the frontend is listening on a \
                 different event and will never receive anything"
            );
        }
    }

    #[test]
    fn edit_approval_can_enable_accept_edits() {
        let approval = answer(Some("accept-edits"))
            .into_approval(&serde_json::json!({"path": "src/main.rs"}), true)
            .unwrap();

        assert_eq!(approval.decision, ApprovalDecision::Yes);
        assert_eq!(approval.set_mode, Some(PermissionMode::AcceptEdits));
    }

    #[test]
    fn approval_mode_is_rejected_outside_a_one_time_edit_approval() {
        assert!(answer(Some("unsafe"))
            .into_approval(&serde_json::json!({}), true)
            .is_err());
        assert!(answer(Some("accept-edits"))
            .into_approval(&serde_json::json!({}), false)
            .is_err());
    }
}
