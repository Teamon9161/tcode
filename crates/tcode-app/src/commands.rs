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

/// How the webview should treat one tool's calls.
///
/// The webview owns *presentation*; this owns *routing*, so `Transcript.tsx`
/// never grows a chain of `if (name === …)`.
///
/// Only `quiet_output` is genuinely derived: it comes from the live
/// `Tool::batch_policy()`, exactly as `RenderRegistry::from_tools` does, so it
/// cannot drift from core's parallel-read-only set.
///
/// `route` and `hide_success_result` are **not** derivable today, because
/// `CallRoute` lives in `tcode-tui` rather than in core — and this crate must
/// not depend on the TUI. They are therefore a name list, kept here in one
/// place rather than duplicated in TypeScript. That is a real drift risk: a new
/// progress-style or silent tool will render as an ordinary call until this
/// list learns about it.
///
/// The fix is to promote `CallRoute` to a `Tool` trait method in core (a
/// default of `Transcript`, `Progress` on `UpdateProgressTool`, `Silent` on
/// `AskUserTool`) and have both the TUI registry and this function read it.
/// That is the shape CLAUDE.md asks for — a capability expressed by a trait
/// method instead of a match on names — and it is deliberately left out of this
/// change because it touches core, tools and the TUI together.
#[derive(Serialize)]
pub struct ToolViewMeta {
    pub name: String,
    pub route: &'static str,
    pub quiet_output: bool,
    pub hide_success_result: bool,
}

/// Tools whose story is told somewhere other than the transcript.
const PROGRESS_TOOLS: &[&str] = &["update_progress"];
const SILENT_TOOLS: &[&str] = &["ask_user"];
/// Tools whose call body already showed the change, so a success line under it
/// only repeats what the diff said.
const BODY_IS_THE_RESULT: &[&str] = &["edit", "write", "multi_edit", "notebook_edit"];

#[tauri::command]
pub fn tool_views(supervisor: State<'_, Arc<Supervisor>>) -> Vec<ToolViewMeta> {
    tool_view_metas(&supervisor.agent().tools)
}

/// The command's whole body, reachable without a window (AGENTS.md rule 2).
pub fn tool_view_metas(tools: &[Arc<dyn tcode_core::Tool>]) -> Vec<ToolViewMeta> {
    tools
        .iter()
        .map(|tool| {
            let name = tool.name().to_string();
            ToolViewMeta {
                route: if PROGRESS_TOOLS.contains(&name.as_str()) {
                    "progress"
                } else if SILENT_TOOLS.contains(&name.as_str()) {
                    "silent"
                } else {
                    "transcript"
                },
                quiet_output: matches!(
                    tool.batch_policy(),
                    tcode_core::BatchPolicy::ParallelReadOnly
                ),
                hide_success_result: BODY_IS_THE_RESULT.contains(&name.as_str()),
                name,
            }
        })
        .collect()
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
