//! Tauri commands: the webview's half of the contract.
//!
//! These are thin on purpose. Each one validates its arguments, then hands off
//! to [`crate::state`] — so the logic worth testing is reachable without a
//! window, and everything here is the part that only exists because the
//! frontend is out of process.

use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, State};

use tcode_core::ContentBlock;

use crate::bridge::{ApprovalAnswer, Emit, TURN_FINISHED};
use crate::state::{run_turn, Supervisor};

/// What the frontend needs to render a session before any turn has run.
#[derive(Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub cwd: String,
}

#[tauri::command]
pub fn sessions(supervisor: State<'_, Arc<Supervisor>>) -> Vec<SessionInfo> {
    supervisor
        .ids()
        .into_iter()
        .filter_map(|id| supervisor.get(&id))
        .map(|handle| SessionInfo {
            id: handle.id.clone(),
            cwd: handle.cwd.display().to_string(),
        })
        .collect()
}

/// Start a turn. Returns as soon as it is running, not when it finishes:
/// progress arrives as events, and the webview must stay responsive to answer
/// the approvals this very turn may raise.
///
/// The task goes on `tauri::async_runtime`, not `tokio::spawn`. A sync command
/// runs on the main thread, where no Tokio runtime is guaranteed to be entered
/// — `tokio::spawn` there panics, and a panicking command is an `invoke` that
/// never settles, which the frontend can only render as a turn that started
/// and produced nothing.
#[tauri::command]
pub fn send_message(
    app: AppHandle,
    supervisor: State<'_, Arc<Supervisor>>,
    session: String,
    text: String,
) -> Result<(), String> {
    let handle = supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    let agent = supervisor.agent();
    let emit: Arc<dyn Emit> = Arc::new(app);
    tauri::async_runtime::spawn(async move {
        let input = vec![ContentBlock::Text { text }];
        if let Err(error) = run_turn(agent, handle.clone(), emit.clone(), input).await {
            // `Busy` is the only way here, and it is a frontend bug (two sends
            // for one session). The command already returned, so the only way
            // to tell the user is the same channel the turn would have used.
            emit.emit(
                TURN_FINISHED,
                serde_json::json!({ "session": handle.id, "error": error.to_string() }),
            );
        }
    });
    Ok(())
}

/// Answer an approval the agent is parked on.
#[tauri::command]
pub fn respond_approval(
    supervisor: State<'_, Arc<Supervisor>>,
    session: String,
    answer: ApprovalAnswer,
) -> Result<(), String> {
    let handle = supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    if !handle.pending().answer(answer) {
        // Answering twice, or answering a turn that was interrupted while the
        // dialog was open. Nothing ran on the strength of it either way.
        return Err("that approval is no longer waiting for an answer".into());
    }
    Ok(())
}

/// Stop the running turn. Safe to call when nothing is running.
#[tauri::command]
pub fn interrupt(supervisor: State<'_, Arc<Supervisor>>, session: String) -> Result<(), String> {
    let handle = supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    handle.interrupt();
    Ok(())
}
