//! Tauri commands: the webview's half of the contract.
//!
//! These are thin on purpose. Each one validates its arguments, then hands off
//! to [`crate::state`] — so the logic worth testing is reachable without a
//! window, and everything here is the part that only exists because the
//! frontend is out of process.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, State};

use tcode_core::ContentBlock;

use crate::bridge::{ApprovalAnswer, Emit, TURN_FINISHED};
use crate::projects::{self, ProjectInfo, StoredSession};
use crate::state::{run_turn, Supervisor};

/// What the frontend needs to render a session before any turn has run.
#[derive(Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub cwd: String,
    /// Last path component of `cwd` — the rail's label for this session.
    pub name: String,
    /// Home directory, so the frontend can render `~/…` without a second
    /// round trip. Carried per session rather than fetched separately so a
    /// `SessionInfo` is enough to draw a session on its own.
    pub home: String,
}

impl SessionInfo {
    fn of(handle: &crate::state::SessionHandle) -> Self {
        Self {
            id: handle.id.clone(),
            cwd: handle.cwd.display().to_string(),
            name: folder_name(&handle.cwd),
            home: tcode_core::home_dir()
                .map(|home| home.display().to_string())
                .unwrap_or_default(),
        }
    }
}

fn folder_name(cwd: &Path) -> String {
    cwd.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.display().to_string())
}

#[tauri::command]
pub fn sessions(supervisor: State<'_, Arc<Supervisor>>) -> Vec<SessionInfo> {
    supervisor
        .ids()
        .into_iter()
        .filter_map(|id| supervisor.get(&id))
        .map(|handle| SessionInfo::of(&handle))
        .collect()
}

/// Every folder tcode has held a conversation in, for the launchpad.
#[derive(Serialize)]
pub struct Launchpad {
    pub projects: Vec<ProjectInfo>,
    /// The backend's clock, so relative times ("2 hours ago") are computed
    /// against the same clock that produced the timestamps.
    pub now: u64,
    /// Lets the frontend abbreviate paths to `~/…` without guessing at it.
    pub home: String,
}

#[tauri::command]
pub fn launchpad() -> Result<Launchpad, String> {
    let home = tcode_core::home_dir().ok_or("cannot locate the home directory")?;
    Ok(Launchpad {
        projects: projects::list(&home),
        now: projects::now_unix(),
        home: home.display().to_string(),
    })
}

/// The resumable conversations inside one project. Separate from [`launchpad`]
/// because it replays every log to build previews — affordable for the one
/// project being opened, not for all of them on every launch.
#[tauri::command]
pub fn project_sessions(path: String) -> Vec<StoredSession> {
    projects::sessions(Path::new(&path))
}

/// Open a folder as a session, optionally resuming one of its logs.
#[tauri::command]
pub fn open_folder(
    supervisor: State<'_, Arc<Supervisor>>,
    path: String,
    resume: Option<String>,
) -> Result<SessionInfo, String> {
    // Canonicalize before anything else: the session id, the project data
    // directory and the launchpad's grouping all key on the path, and two
    // spellings of one folder would otherwise become two projects.
    let cwd = PathBuf::from(&path)
        .canonicalize()
        .map_err(|error| format!("cannot open {path}: {error}"))?;
    let handle = supervisor
        .open_folder(&cwd, resume)
        .map_err(|error| error.to_string())?;
    eprintln!(
        "tcode-app: session {} open on {}",
        handle.id,
        handle.cwd.display()
    );
    Ok(SessionInfo::of(&handle))
}

/// Close a session, cancelling its turn if one is running.
#[tauri::command]
pub fn close_session(supervisor: State<'_, Arc<Supervisor>>, session: String) {
    supervisor.close(&session);
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
