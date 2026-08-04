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

use tcode_core::progress::{Progress, ProgressState};
use tcode_core::{
    commands::{CommandCtx, CommandOutcome, CommandRegistry, EnvironmentFn, OpeningContextFn},
    Agent, AgentError, AgentEvent, ContentBlock, Session,
};

use crate::boot::SessionFactory;
use crate::bridge::{pump_events, Emit, Pending, TurnFinished, WebviewApprover, TURN_FINISHED};

struct TurnControl {
    id: Option<u64>,
    cancel: CancellationToken,
}

/// One conversation, with everything that is private to it.
pub struct SessionHandle {
    pub id: String,
    pub cwd: PathBuf,
    /// `None` while a turn is running — see the module comment.
    session: Mutex<Option<Session>>,
    /// The session's plan cell, held separately *because* of that `Option`: a
    /// phase flips and a draft reaches review while the turn owns the session,
    /// which is exactly when the plan surface has something new to show. Reading
    /// the plan is not a reason to wait for a turn to end.
    progress: Arc<Mutex<Option<Progress>>>,
    turn: Mutex<TurnControl>,
    next_turn: std::sync::atomic::AtomicU64,
    pending: Pending,
    /// Prompts typed while a turn was running. A clone of the `Session`'s own
    /// handle, held here *because* of the `Option` above: the running turn owns
    /// the session, and "let me say one more thing" is the single most natural
    /// thing to do while it does. Core made this a shared handle for exactly
    /// this, and the desktop app is the second frontend to need it.
    queue: tcode_core::PendingInput,
    /// A permission mode chosen while a turn holds the session. This is the
    /// session-owned shared cell the agent commits at its next permission
    /// boundary, so a mid-turn switch can gate the next call rather than merely
    /// changing the mode once the turn returns.
    pending_mode: tcode_core::PendingMode,
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
            progress: session.tool_ctx.progress_cell(),
            queue: session.pending.clone(),
            pending_mode: session.pending_mode.clone(),
            session: Mutex::new(Some(session)),
            turn: Mutex::new(TurnControl {
                id: None,
                cancel: CancellationToken::new(),
            }),
            next_turn: std::sync::atomic::AtomicU64::new(0),
            pending: Pending::default(),
        }
    }

    /// The mode this conversation is under, and whether a running turn still
    /// has a target waiting for Core's next permission boundary.
    pub fn mode(&self) -> (tcode_core::PermissionMode, bool) {
        if let Some(staged) = self.pending_mode.get() {
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

    /// Choose the permission mode. Applies now when the session is idle, and
    /// stages through Core's shared cell when a turn owns it.
    pub fn set_mode(&self, mode: tcode_core::PermissionMode) {
        let mut session = self.session.lock().unwrap();
        match session.as_mut() {
            Some(session) => {
                session.mode = mode;
                self.pending_mode.clear();
            }
            None => self.pending_mode.set(mode),
        }
    }

    pub fn pending(&self) -> Pending {
        self.pending.clone()
    }

    /// Send this prompt, or queue it if a turn already holds the session.
    ///
    /// The decision is made **under the session lock**, which is the same lock
    /// `take` uses to start a turn — so "is it busy" and "then queue it" cannot
    /// be separated by a turn starting in between. Asking `is_busy()` first and
    /// acting on the answer afterwards is the version of this that drops a
    /// message roughly never, which is the worst rate for a bug like that.
    ///
    /// Hands the message back when the session is free and the caller must
    /// start a turn with it; `None` once it has been queued.
    pub fn send_or_queue(
        &self,
        message: tcode_core::PendingMessage,
    ) -> Option<tcode_core::PendingMessage> {
        let held = self.session.lock().unwrap();
        if held.is_some() {
            return Some(message);
        }
        self.queue.push(message);
        None
    }

    /// What this conversation still owes the model, oldest first.
    pub fn queued(&self) -> Vec<tcode_core::PendingMessage> {
        self.queue.queued()
    }

    /// A queue snapshot plus the running turn that owns its eventual delivery.
    /// The turn id is part of the webview action boundary: a late "send now"
    /// must not stop a successor.
    pub fn queued_with_turn(&self) -> (Option<u64>, Vec<tcode_core::PendingMessage>) {
        let turn = self.turn.lock().unwrap().id;
        (turn, self.queue.queued())
    }

    /// Take one queued prompt back. See `PendingInput::withdraw` for why the
    /// text is checked as well as the position.
    pub fn withdraw_queued(&self, index: usize, text: &str) -> Option<tcode_core::PendingMessage> {
        self.queue.withdraw(index, text)
    }

    /// "Stop what you are doing and say this now." Returns false when the
    /// visible queue belonged to a turn that has already ended, so a delayed
    /// webview event cannot cancel the successor it was meant to start.
    pub fn interrupt_and_flush(&self, turn: u64) -> bool {
        let control = self.turn.lock().unwrap();
        if control.id != Some(turn) {
            return false;
        }
        self.queue.defer_to_next_turn();
        control.cancel.cancel();
        self.pending.clear();
        true
    }

    /// Where this conversation can be rewound to. Empty while a turn holds the
    /// session: rewinding under a running turn would truncate a ledger it is
    /// still appending to.
    pub fn rewind_targets(&self) -> Vec<tcode_core::RewindTarget> {
        self.session
            .lock()
            .unwrap()
            .as_ref()
            .map(|session| session.rewind_targets())
            .unwrap_or_default()
    }

    /// Drop everything from `index` onward, optionally rolling back the files
    /// that era touched. Returns the prompt to hand back for editing, and what
    /// happened to each file.
    ///
    /// Refuses while a turn is running rather than waiting for one: the turn is
    /// appending to the ledger this would truncate, and "your rewind will happen
    /// in a minute" is not a thing a person can act on. The index is checked
    /// against the session's own targets, because it arrived from the webview
    /// and an arbitrary integer here would truncate the ledger anywhere at all
    /// (AGENTS.md rule 3).
    pub fn rewind(
        &self,
        index: usize,
        restore_files: bool,
    ) -> Result<(String, Vec<(PathBuf, tcode_core::checkpoint::Restore)>), String> {
        let mut held = self.session.lock().unwrap();
        let session = held
            .as_mut()
            .ok_or("this conversation is busy; stop the turn before rewinding it")?;
        let target = session
            .rewind_targets()
            .into_iter()
            .find(|target| target.index == index)
            .ok_or("that point is no longer in this conversation")?;
        let restored = session.rewind_to(index, restore_files);
        Ok((target.text, restored))
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

    /// Run a core slash command while this conversation is idle.
    ///
    /// The webview supplies only the line. Command lookup, argument parsing and
    /// ledger mutation remain in Core, and a running turn refuses the request
    /// rather than racing a command against its append-only ledger.
    pub fn dispatch_slash(
        &self,
        registry: &CommandRegistry,
        opening_context: &OpeningContextFn,
        environment: &EnvironmentFn,
        line: &str,
    ) -> Result<CommandOutcome, String> {
        let mut held = self.session.lock().unwrap();
        let session = held
            .as_mut()
            .ok_or("this conversation is busy; wait for the current turn to finish")?;
        let mut ctx = CommandCtx {
            session,
            opening_context,
            environment,
            turn_usage: tcode_core::Usage::default(),
        };
        registry
            .dispatch(&mut ctx, line)
            .ok_or_else(|| format!("unknown command {line} — use /resume, /clear, or /compact"))
    }

    /// The same two lines the TUI runs at every turn end and every conversation
    /// reset (`app/turn.rs`, `app/views.rs`), and deliberately *not* something
    /// the webview can work out for itself. Two reasons, both load-bearing:
    ///
    ///  - The prompt is more than the conversation. `estimate_context_tokens`
    ///    counts the system prompt and every tool schema; neither ever crosses
    ///    into the webview, so any figure computed over there starts short by
    ///    tens of thousands of tokens.
    ///  - **Compaction.** It counts the *model-visible* ledger, while the
    ///    webview only ever receives `history()` — the human's view, which keeps
    ///    the archived era so the transcript can still show it. Measuring that
    ///    charges a compacted conversation for exactly the history compaction
    ///    removed, which is how a conversation that was not full read as full
    ///    the moment it was resumed.
    ///
    /// `(0, false)` while a turn holds the session. Both callers read this at a
    /// point where it is back (`open_folder`, and the turn-end emit after
    /// `put_back`), and a running turn's own `Usage` events are the authority
    /// anyway.
    pub fn context(&self, agent: &Agent) -> (u64, bool) {
        let mut held = self.session.lock().unwrap();
        let Some(session) = held.as_mut() else {
            return (0, false);
        };
        let estimated = session.last_prompt_tokens == 0 && !session.ledger.is_empty();
        if estimated {
            session.last_prompt_tokens = agent.estimate_context_tokens(session);
        }
        (session.last_prompt_tokens, estimated)
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

    /// This conversation's plan as it is right now, or `None` when it has none.
    ///
    /// Read from disk, not from the cell, whenever the file is still there: the
    /// file is the authority on a plan (the user may have edited it by hand
    /// since the last tool call, and core's own contract says their version
    /// wins), while the cell is what this session last wrote or read. Falling
    /// back to the cell covers the moment between a draft being created and
    /// saved, and a file the user deleted underneath us.
    ///
    /// Reading never repairs the mismatch: the session's copy keeps the hash it
    /// wrote, so the next `progress` call still reports the conflict and hands
    /// the user's text to the model. Showing their edit is not the same as
    /// telling the model about it, and this side must not do the second.
    pub fn plan(&self) -> Option<Progress> {
        let held = self.progress.lock().unwrap().clone()?;
        Some(Progress::load(held.path()).unwrap_or(held))
    }

    /// Apply a breakdown the user edited by hand. Returns the plan as saved.
    ///
    /// The `phases` come from the webview, so they go through the same
    /// validation the model's own input does — nesting cap included. The write
    /// deliberately goes through a freshly loaded copy rather than the session's:
    /// leaving the session's hash alone is what makes the next `progress` call
    /// notice the file changed and hand the user's version to the model, which
    /// is the whole self-healing contract for a hand-edited plan.
    pub fn write_plan(&self, phases: &serde_json::Value) -> Result<Progress, String> {
        let phases = tcode_core::progress::phases_from_json(phases)?;
        let held = self
            .progress
            .lock()
            .unwrap()
            .clone()
            .ok_or("no plan is open in this conversation")?;
        let mut plan = Progress::load(held.path()).unwrap_or(held);
        if plan.state() == ProgressState::Done {
            return Err("this plan is finished; nothing left to edit".into());
        }
        // Same bargain as the review panel: a phase that came back without
        // `detail` keeps the prose already in the file rather than losing it.
        let body = tcode_core::progress::revise_plan_body(&plan.body(), &phases);
        plan.set_body(&body);
        plan.save()?;
        Ok(plan)
    }

    /// Take over a plan file, for a session opened to execute one. Re-reads the
    /// file rather than being handed a parsed plan: the approved bytes on disk
    /// are the artifact, and `adopt_progress` also refuses a finished one.
    pub fn adopt_plan(&self, path: &Path) -> Result<String, String> {
        let mut session = self.session.lock().unwrap();
        let session = session
            .as_mut()
            .ok_or("this conversation is busy; its plan cannot be adopted right now")?;
        let title = session.adopt_progress(path)?;
        // A fresh execution starts from the ordinary manual-approval default:
        // the planning session's mode was a choice about planning, and it does
        // not silently carry across a session boundary. The TUI's handoff makes
        // the same choice.
        session.apply_approved_mode(tcode_core::PermissionMode::Default);
        Ok(title)
    }

    fn take(&self) -> Option<Session> {
        self.session.lock().unwrap().take()
    }

    /// Return the session to the idle pool only when there is no queued prompt
    /// to continue with. Holding it through this decision means a foreground
    /// submission cannot claim it between destructive queue drain and successor
    /// startup.
    fn put_back_or_take_queued(
        &self,
        session: Session,
    ) -> (Option<Session>, Vec<tcode_core::PendingMessage>) {
        let mut held = self.session.lock().unwrap();
        let queued = self.queue.take_for_next_turn();
        if queued.is_empty() {
            *held = Some(session);
            (None, queued)
        } else {
            (Some(session), queued)
        }
    }

    fn begin_turn(&self) -> (u64, CancellationToken) {
        use std::sync::atomic::Ordering;

        let id = self.next_turn.fetch_add(1, Ordering::Relaxed);
        let cancel = CancellationToken::new();
        let mut control = self.turn.lock().unwrap();
        control.id = Some(id);
        control.cancel = cancel.clone();
        (id, cancel)
    }

    fn finish_turn(&self, id: u64) {
        let mut control = self.turn.lock().unwrap();
        if control.id == Some(id) {
            control.id = None;
        }
    }

    /// Stop the running turn, if any. Also fails any open approval closed:
    /// an interrupted turn must not be authorized by an answer that arrives
    /// afterwards.
    pub fn interrupt(&self) {
        self.turn.lock().unwrap().cancel.cancel();
        self.pending.clear();
    }
}

/// Open a second conversation on the same folder to execute an approved plan, and return it with the instruction its first turn carries.
///
/// Separate from the command that calls it because it is the whole decision:
/// only an approved plan may be handed on, the new session adopts the file
/// rather than a copy of its text, and the instruction comes from core. The
/// command is left with a spawn and an `AppHandle` (AGENTS.md rule 2).
pub fn hand_off_plan(
    supervisor: &Supervisor,
    session: &str,
) -> Result<(Arc<SessionHandle>, Vec<String>), String> {
    let origin = supervisor
        .get(session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    let plan = origin.plan().ok_or("this conversation has no plan")?;
    // The frontend calls this after the planning turn *ended*, by which time the
    // `progress` tool has marked the approved plan active. A draft here means
    // something else happened — a declined review, a failed write — and adopting
    // it would hand the new session a plan nobody approved.
    if plan.state() != ProgressState::Active {
        return Err(format!(
            "this plan is still a {}; only an approved plan can be handed to another session",
            plan.state().label()
        ));
    }
    let handle = supervisor
        .open_folder(&origin.cwd, None)
        .map_err(|error| format!("cannot open a session for the approved plan: {error}"))?;
    handle.adopt_plan(plan.path())?;
    let instruction = tcode_core::commands::plan::execution_instruction(&plan.body());
    Ok((handle, vec![instruction]))
}

/// Holds the agent, every open session, and the means to open more.
pub struct Supervisor {
    agent: Arc<Agent>,
    factory: SessionFactory,
    commands: CommandRegistry,
    opening_context: OpeningContextFn,
    environment: EnvironmentFn,
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
    pub fn new(agent: Arc<Agent>, factory: SessionFactory, menus: crate::picker::Menus) -> Self {
        let (opening_context, environment) = factory.command_context();
        Self {
            agent,
            factory,
            commands: CommandRegistry::builtin(),
            opening_context,
            environment,
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

    /// Dispatch a core-owned command for one idle conversation. UI-only commands
    /// deliberately have no route through this boundary.
    pub fn dispatch_slash(&self, id: &str, line: &str) -> Result<CommandOutcome, String> {
        let handle = self
            .get(id)
            .ok_or_else(|| format!("session '{id}' is not open"))?;
        handle.dispatch_slash(
            &self.commands,
            &self.opening_context,
            &self.environment,
            line,
        )
    }

    pub fn ids(&self) -> Vec<String> {
        self.order.lock().unwrap().clone()
    }
}

/// Run a manually requested history compaction through the same event, cancel
/// and queue lifecycle as an ordinary turn.
pub async fn run_compact(
    agent: Arc<Agent>,
    handle: Arc<SessionHandle>,
    emit: Arc<dyn Emit>,
    focus: Option<String>,
) -> Result<(), TurnError> {
    let Some(mut session) = handle.take() else {
        return Err(TurnError::Busy(handle.id.clone()));
    };
    let (turn, cancel) = handle.begin_turn();
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let pump = tokio::spawn(pump_events(handle.id.clone(), emit.clone(), rx));
    eprintln!("tcode-app: compaction started on session {}", handle.id);
    let _ = tx.send(AgentEvent::Compacting).await;
    let result = agent
        .compact_with_focus(&mut session, focus.as_deref(), &tx, &cancel)
        .await;
    match &result {
        // `false` means the summary never arrived (an empty answer, or the user
        // stopped it): the ledger is untouched, which the transcript already
        // shows by having no compaction boundary in it.
        Ok(landed) => eprintln!(
            "tcode-app: compaction finished on session {} ({})",
            handle.id,
            if *landed { "history replaced" } else { "nothing compacted" }
        ),
        Err(error) => eprintln!(
            "tcode-app: compaction failed on session {}: {error}",
            handle.id
        ),
    }
    if let Some(mode) = session.commit_pending_mode() {
        let _ = tx.send(AgentEvent::ModeChanged(mode)).await;
    }
    drop(tx);
    let _ = pump.await;
    handle.pending.clear();

    let (next_session, waiting) = handle.put_back_or_take_queued(session);
    handle.finish_turn(turn);
    if let Some(session) = next_session {
        let message = merge_queued(waiting).expect("non-empty queue reserves the session");
        return run_owned_turn(
            agent,
            handle,
            emit,
            session,
            message.blocks,
            message.instructions,
            Some((message.text, message.attachments)),
        )
        .await;
    }

    let (context_tokens, context_estimated) = handle.context(&agent);
    emit.emit(
        TURN_FINISHED,
        serde_json::to_value(TurnFinished {
            session: &handle.id,
            error: result.as_ref().err().map(describe_error),
            context_tokens,
            context_estimated,
        })
        .unwrap_or(serde_json::Value::Null),
    );
    Ok(())
}

/// Run one turn to completion, streaming its events to `emit`.
///
/// Owns the `Session` for the duration and hands it back however the turn ends
/// — including on error, since the ledger is consistent either way and a
/// session that vanished on one failed request would be worse than the failure.
/// `instructions` is harness-authored model context, not anything the user
/// wrote: it stays out of the transcript, replay and export. Every string in it
/// comes from core (planning, plan execution) — the webview asks for a *kind* of
/// turn and never supplies the text, or it could impersonate the harness to the
/// model.
pub async fn run_turn(
    agent: Arc<Agent>,
    handle: Arc<SessionHandle>,
    emit: Arc<dyn Emit>,
    input: Vec<ContentBlock>,
    instructions: Vec<String>,
) -> Result<(), TurnError> {
    let Some(session) = handle.take() else {
        return Err(TurnError::Busy(handle.id.clone()));
    };
    run_owned_turn(agent, handle, emit, session, input, instructions, None).await
}

/// Continue a session while it remains exclusively owned by the current turn
/// driver. This reserves queued input before the session is externally idle, so
/// a foreground submission cannot race a destructive queue drain.
async fn run_owned_turn(
    agent: Arc<Agent>,
    handle: Arc<SessionHandle>,
    emit: Arc<dyn Emit>,
    mut session: Session,
    mut input: Vec<ContentBlock>,
    mut instructions: Vec<String>,
    mut queued_echo: Option<(String, Vec<String>)>,
) -> Result<(), TurnError> {
    loop {
        let (turn, cancel) = handle.begin_turn();

        // Depth 1: the pump forwards as fast as the webview accepts, and a deeper
        // queue would only let the transcript drift further behind the ledger.
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let pump = tokio::spawn(pump_events(handle.id.clone(), emit.clone(), rx));
        let approver = WebviewApprover::new(handle.id.clone(), emit.clone(), handle.pending());

        if let Some((text, attachments)) = queued_echo.take() {
            // The fallback starts a fresh physical turn, so core cannot emit its
            // safe-boundary notification. Its user entry follows any instructions.
            let entry_index = session.ledger.len() + instructions.len();
            let _ = tx
                .send(tcode_core::AgentEvent::QueuedInput {
                    text,
                    attachments,
                    entry_index,
                })
                .await;
        }

        // The turn's lifecycle goes to stderr as well as to the webview: when the
        // frontend shows nothing, these two lines are what distinguish "never
        // started" from "started and produced no events".
        eprintln!("tcode-app: turn started on session {}", handle.id);
        let result = match instructions.is_empty() {
            true => {
                agent
                    .user_turn(&mut session, input, &tx, &approver, cancel)
                    .await
            }
            false => {
                agent
                    .instruction_turn(&mut session, instructions, input, &tx, &approver, cancel)
                    .await
            }
        };
        match &result {
            Ok(()) => eprintln!("tcode-app: turn finished on session {}", handle.id),
            Err(error) => eprintln!("tcode-app: turn failed on session {}: {error}", handle.id),
        }

        if let Some(mode) = session.commit_pending_mode() {
            // Core commits at every permission boundary it reaches. A switch made
            // during the closing answer has no further boundary, so mirror the
            // TUI's turn-end fallback while the event pump is still alive.
            let _ = tx.send(tcode_core::AgentEvent::ModeChanged(mode)).await;
        }

        drop(tx);
        let _ = pump.await;
        // Whatever the turn's fate, no dialog it opened may still be answerable.
        handle.pending.clear();

        let (next_session, waiting) = handle.put_back_or_take_queued(session);
        handle.finish_turn(turn);
        let Some(next_session) = next_session else {
            // The session is back only after no queued successor was reserved.
            let (context_tokens, context_estimated) = handle.context(&agent);
            emit.emit(
                TURN_FINISHED,
                serde_json::to_value(TurnFinished {
                    session: &handle.id,
                    error: result.as_ref().err().map(describe_error),
                    context_tokens,
                    context_estimated,
                })
                .unwrap_or(serde_json::Value::Null),
            );
            return Ok(());
        };

        // Several prompts queued behind a turn become one successor, exactly as in
        // the TUI: the model sees every prompt before answering either one.
        let message = merge_queued(waiting).expect("non-empty queue reserves the session");
        session = next_session;
        input = message.blocks;
        instructions = message.instructions;
        queued_echo = Some((message.text, message.attachments));
    }
}

fn merge_queued(mut queued: Vec<tcode_core::PendingMessage>) -> Option<tcode_core::PendingMessage> {
    let mut merged = queued.drain(..).next()?;
    for next in queued {
        merged.text.push('\n');
        merged.text.push_str(&next.text);
        merged.attachments.extend(next.attachments);
        merged.blocks.extend(next.blocks);
        merged.instructions.extend(next.instructions);
    }
    Some(merged)
}

fn describe_error(error: &AgentError) -> String {
    error.to_string()
}
