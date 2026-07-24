//! The supervisor: one `Arc<Agent>`, many isolated sessions.
//!
//! Phase 1 opens exactly one session, but the shape is the multi-session one
//! from the start — a map keyed by session id, each entry owning its own
//! `Session`, cancel token and pending approvals. Sessions share the agent
//! (it is stateless) and nothing else.
//!
//! The `Session` lives in an `Option` that a running turn *takes*: that is how
//! "one turn at a time per session" is enforced by ownership rather than by a
//! flag someone has to remember to check.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use tcode_core::{Agent, AgentError, ContentBlock, Session};

use crate::bridge::{pump_events, Emit, Pending, TurnFinished, WebviewApprover, TURN_FINISHED};

/// One conversation, with everything that is private to it.
pub struct SessionHandle {
    pub id: String,
    pub cwd: PathBuf,
    /// `None` while a turn is running — see the module comment.
    session: Mutex<Option<Session>>,
    cancel: Mutex<CancellationToken>,
    pending: Pending,
}

/// A turn could not start.
#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error("session '{0}' is not open")]
    UnknownSession(String),
    #[error("session '{0}' is already running a turn")]
    Busy(String),
}

impl SessionHandle {
    pub fn new(id: String, cwd: PathBuf, session: Session) -> Self {
        Self {
            id,
            cwd,
            session: Mutex::new(Some(session)),
            cancel: Mutex::new(CancellationToken::new()),
            pending: Pending::default(),
        }
    }

    pub fn pending(&self) -> Pending {
        self.pending.clone()
    }

    /// Stop the running turn, if any. Also fails any open approval closed:
    /// an interrupted turn must not be authorized by an answer that arrives
    /// afterwards.
    pub fn interrupt(&self) {
        self.cancel.lock().unwrap().cancel();
        self.pending.clear();
    }

    fn take(&self) -> Option<Session> {
        self.session.lock().unwrap().take()
    }

    fn put_back(&self, session: Session) {
        *self.session.lock().unwrap() = Some(session);
    }
}

/// Holds the agent and every open session.
pub struct Supervisor {
    agent: Arc<Agent>,
    sessions: Mutex<HashMap<String, Arc<SessionHandle>>>,
}

impl Supervisor {
    pub fn new(agent: Arc<Agent>) -> Self {
        Self {
            agent,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn agent(&self) -> Arc<Agent> {
        self.agent.clone()
    }

    pub fn open(&self, handle: Arc<SessionHandle>) {
        self.sessions
            .lock()
            .unwrap()
            .insert(handle.id.clone(), handle);
    }

    pub fn get(&self, id: &str) -> Option<Arc<SessionHandle>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    pub fn ids(&self) -> Vec<String> {
        self.sessions.lock().unwrap().keys().cloned().collect()
    }
}

/// Run one turn to completion, streaming its events to `emit`.
///
/// Owns the `Session` for the duration and hands it back however the turn ends
/// — including on error, since the ledger is consistent either way and a
/// session that vanished on one failed request would be worse than the failure.
pub async fn run_turn(
    agent: Arc<Agent>,
    handle: Arc<SessionHandle>,
    emit: Arc<dyn Emit>,
    input: Vec<ContentBlock>,
) -> Result<(), TurnError> {
    let Some(mut session) = handle.take() else {
        return Err(TurnError::Busy(handle.id.clone()));
    };

    let cancel = CancellationToken::new();
    *handle.cancel.lock().unwrap() = cancel.clone();

    // Depth 1: the pump forwards as fast as the webview accepts, and a deeper
    // queue would only let the transcript drift further behind the ledger.
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let pump = tokio::spawn(pump_events(handle.id.clone(), emit.clone(), rx));
    let approver = WebviewApprover::new(handle.id.clone(), emit.clone(), handle.pending());

    // The turn's lifecycle goes to stderr as well as to the webview: when the
    // frontend shows nothing, these two lines are what distinguish "never
    // started" from "started and produced no events".
    eprintln!("tcode-app: turn started on session {}", handle.id);
    let result = agent
        .user_turn(&mut session, input, &tx, &approver, cancel)
        .await;
    match &result {
        Ok(()) => eprintln!("tcode-app: turn finished on session {}", handle.id),
        Err(error) => eprintln!("tcode-app: turn failed on session {}: {error}", handle.id),
    }

    drop(tx);
    let _ = pump.await;
    // Whatever the turn's fate, no dialog it opened may still be answerable.
    handle.pending.clear();
    handle.put_back(session);

    emit.emit(
        TURN_FINISHED,
        serde_json::to_value(TurnFinished {
            session: &handle.id,
            error: result.as_ref().err().map(describe_error),
        })
        .unwrap_or(serde_json::Value::Null),
    );
    Ok(())
}

fn describe_error(error: &AgentError) -> String {
    error.to_string()
}
