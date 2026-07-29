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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use tcode_core::{Agent, AgentError, ContentBlock, Session};

use crate::boot::SessionFactory;
use crate::bridge::{pump_events, Emit, Pending, TurnFinished, WebviewApprover, TURN_FINISHED};

/// One conversation, with everything that is private to it.
pub struct SessionHandle {
    pub id: String,
    pub cwd: PathBuf,
    /// `None` while a turn is running — see the module comment.
    session: Mutex<Option<Session>>,
    cancel: Mutex<CancellationToken>,
    pending: Pending,
    /// A permission mode chosen while a turn held the session, applied when it
    /// comes back. The mode belongs to the `Session`, and the running turn owns
    /// that — so the choice is staged rather than dropped or forced. The TUI
    /// stages the same way and shows it as `→ auto`; this is that, without the
    /// mid-batch commit, because nothing here can reach into a running turn.
    staged_mode: Mutex<Option<tcode_core::PermissionMode>>,
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
            staged_mode: Mutex::new(None),
        }
    }

    /// The mode this conversation is under, and whether it is still waiting for
    /// the turn to end. Both, because "auto" and "auto as soon as this finishes"
    /// are different facts and a chip that shows only the first is a lie during
    /// the exact window where it matters.
    pub fn mode(&self) -> (tcode_core::PermissionMode, bool) {
        if let Some(staged) = *self.staged_mode.lock().unwrap() {
            return (staged, true);
        }
        let live = self
            .session
            .lock()
            .unwrap()
            .as_ref()
            .map(|session| session.mode)
            .unwrap_or_default();
        (live, false)
    }

    /// Choose the permission mode. Applies now when the session is idle, and is
    /// staged when a turn holds it.
    pub fn set_mode(&self, mode: tcode_core::PermissionMode) {
        let mut session = self.session.lock().unwrap();
        match session.as_mut() {
            Some(session) => {
                session.mode = mode;
                *self.staged_mode.lock().unwrap() = None;
            }
            None => *self.staged_mode.lock().unwrap() = Some(mode),
        }
    }

    pub fn pending(&self) -> Pending {
        self.pending.clone()
    }

    /// A display-only copy of the durable conversation. Project instructions
    /// remain model context and never cross into the webview transcript.
    pub fn history(&self) -> Vec<tcode_core::Entry> {
        self.session
            .lock()
            .unwrap()
            .as_ref()
            .map(|session| {
                session
                    .ledger
                    .history()
                    .filter(|entry| !matches!(entry, tcode_core::Entry::Instruction(_)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Where this session may drop files that are not the user's to keep —
    /// pasted images the current model cannot see, for one. `None` while a turn
    /// holds the session, which is also when nothing can be sent to it.
    pub fn scratch_dir(&self) -> Option<PathBuf> {
        self.session
            .lock()
            .unwrap()
            .as_ref()
            .map(|session| session.tool_ctx.scratch_dir.clone())
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

    fn put_back(&self, mut session: Session) {
        // A mode chosen mid-turn lands here, at the first moment this side owns
        // the session again.
        if let Some(staged) = self.staged_mode.lock().unwrap().take() {
            session.mode = staged;
        }
        *self.session.lock().unwrap() = Some(session);
    }
}

/// Holds the agent, every open session, and the means to open more.
pub struct Supervisor {
    agent: Arc<Agent>,
    factory: SessionFactory,
    /// The model/preset menus the composer's chips read and write. Process-wide
    /// rather than per-session: they act on the shared `ModelCell` and on the
    /// selected config file, both of which every conversation in this window
    /// already shares.
    menus: crate::picker::Menus,
    sessions: Mutex<HashMap<String, Arc<SessionHandle>>>,
    /// Insertion order, so the session rail does not reshuffle on every read.
    /// A `HashMap` alone would hand the UI a different order each time.
    order: Mutex<Vec<String>>,
}

impl Supervisor {
    pub fn new(
        agent: Arc<Agent>,
        factory: SessionFactory,
        menus: crate::picker::Menus,
    ) -> Self {
        Self {
            agent,
            factory,
            menus,
            sessions: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
        }
    }

    pub fn agent(&self) -> Arc<Agent> {
        self.agent.clone()
    }

    pub fn menus(&self) -> crate::picker::Menus {
        self.menus.clone()
    }

    pub fn open(&self, handle: Arc<SessionHandle>) {
        self.order.lock().unwrap().push(handle.id.clone());
        self.sessions
            .lock()
            .unwrap()
            .insert(handle.id.clone(), handle);
    }

    /// Open `cwd` as a new session and register it. `resume` replays an
    /// existing log for that folder.
    pub fn open_folder(
        &self,
        cwd: &Path,
        resume: Option<String>,
    ) -> anyhow::Result<Arc<SessionHandle>> {
        let session = self.factory.open(cwd, resume)?;
        let handle = Arc::new(SessionHandle::new(
            uuid::Uuid::new_v4().to_string(),
            cwd.to_path_buf(),
            session,
        ));
        self.open(handle.clone());
        Ok(handle)
    }

    /// Drop a session. Its turn, if any, is cancelled first — closing a panel
    /// must not leave a turn running against a session nothing can display.
    ///
    /// Returns false when the id was already gone, which is not an error: the
    /// webview can send this twice for one close.
    pub fn close(&self, id: &str) -> bool {
        let removed = self.sessions.lock().unwrap().remove(id);
        self.order.lock().unwrap().retain(|open| open != id);
        match removed {
            Some(handle) => {
                handle.interrupt();
                true
            }
            None => false,
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<SessionHandle>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    pub fn ids(&self) -> Vec<String> {
        self.order.lock().unwrap().clone()
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
