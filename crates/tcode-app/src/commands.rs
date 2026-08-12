//! The commands: the frontend's half of the contract.
//!
//! These are thin on purpose. Each one validates its arguments, then hands off
//! to [`crate::state`] — so the logic worth testing is reachable without a
//! window, and everything here is the part that only exists because the
//! frontend is out of process.
//!
//! **Nothing here knows which shell is drawing the window.** They are ordinary
//! functions taking what they need and returning `Result<_, String>`; the
//! argument-by-name and serialization that `#[tauri::command]` used to generate
//! is [`crate::dispatch`], which is also what lets a JSON-RPC line on stdin
//! reach them. The `browser_*` verbs are not here at all for the same reason:
//! a browser tab is a native view, so they live with whoever owns the views
//! (`electron/browser.js`; see `AGENTS.md` rule 9h).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;

use tcode_core::{config::Config, ContentBlock};

use crate::bridge::{ApprovalAnswer, Emit, TURN_FINISHED};
use crate::projects::{self, ProjectInfo, StoredSessionsPage};
use crate::state::{run_compact, run_turn, Supervisor};
use crate::workspace::{EntryKind, TextFile, Workspace, WorkspaceEntry, WorkspaceStat};

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
    /// The session log this conversation is writing to, when it is persisted.
    /// The rail lists live conversations and resumable logs in one group, and
    /// this is what stops one conversation appearing as both — a resume offered
    /// on an already-open log would put two ledgers on one file.
    pub log_id: Option<String>,
}

impl SessionInfo {
    fn of(handle: &crate::state::SessionHandle) -> Self {
        Self {
            id: handle.id.clone(),
            cwd: handle.cwd().display().to_string(),
            name: folder_name(&handle.cwd()),
            log_id: handle.log_id.clone(),
            home: tcode_core::home_dir()
                .map(|home| home.display().to_string())
                .unwrap_or_default(),
        }
    }
}

/// The initial state of a just-opened session. A resumed ledger has no live
/// event stream to replay, so its display history travels with this response.
///
/// The context figure travels with it too, and must: `history` is the *human's*
/// view of the conversation and includes everything compaction moved out of the
/// model's window, so a webview that measured what it was given would charge a
/// resumed conversation for history that no longer costs anything. See
/// `SessionHandle::context`.
#[derive(Serialize)]
pub struct OpenedSession {
    pub session: SessionInfo,
    pub history: Vec<tcode_core::Entry>,
    pub context_tokens: u64,
    pub context_estimated: bool,
}

impl OpenedSession {
    fn of(handle: &crate::state::SessionHandle, agent: &tcode_core::Agent) -> Self {
        let (context_tokens, context_estimated) = handle.context(agent);
        Self {
            session: SessionInfo::of(handle),
            history: handle.history(),
            context_tokens,
            context_estimated,
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
/// `route` and `quiet_output` are both derived from the live tools —
/// `Tool::route` and `Tool::batch_policy` — exactly as the TUI's
/// `RenderRegistry` derives them, so neither can drift from core. `route` is the
/// tool's *default* answer (asked with a null input); the one tool whose route
/// depends on the call is `progress`, and the frontend refines that at the call
/// site (see `plan.ts::isPlanSubmission`).
///
/// `hide_success_result` stays a name list: it is a presentation judgement about
/// tools whose body is a diff, not a capability core has any opinion about.
/// `display_name` is core's own answer (`Tool::display_name`), the same one the
/// TUI's `RenderRegistry` snapshots. It travels because otherwise the webview
/// has to invent a second naming rule for the same tools, and two title-casing
/// rules is how `Read 15 files` ends up next to `read 3 files` in one column.
#[derive(Serialize)]
pub struct ToolViewMeta {
    pub name: String,
    pub display_name: String,
    pub route: &'static str,
    pub quiet_output: bool,
    pub hide_success_result: bool,
}

/// Tools whose call body already showed the change, so a success line under it
/// only repeats what the diff said.
const BODY_IS_THE_RESULT: &[&str] = &["edit", "write", "append", "multi_edit", "notebook_edit"];

/// Names a tool used to answer to, and the live tool that still owns their
/// routing. Sessions on disk hold whatever the tool was called when they were
/// recorded, and a resumed call with no meta falls back to the plain transcript
/// treatment — which is how `update_progress` calls came back as tool cards in
/// the conversation instead of feeding the plan surface. The TUI's
/// `RenderRegistry` keeps the same table for the same reason; both are aliases
/// onto the live tool, so neither can drift from what `Tool::route` says.
const RETIRED_NAMES: &[(&str, &str)] = &[
    ("update_progress", "progress"),
    ("update_plan", "progress"),
    ("exit_plan", "progress"),
    ("task", "agent"),
];

pub fn tool_views(supervisor: &Arc<Supervisor>) -> Vec<ToolViewMeta> {
    tool_view_metas(&supervisor.agent().tools)
}

/// The command's whole body, reachable without a window (AGENTS.md rule 2).
pub fn tool_view_metas(tools: &[Arc<dyn tcode_core::Tool>]) -> Vec<ToolViewMeta> {
    let meta = |name: String, tool: &Arc<dyn tcode_core::Tool>| ToolViewMeta {
        display_name: tool.display_name(),
        route: tool.route(&serde_json::Value::Null).label(),
        quiet_output: matches!(
            tool.batch_policy(),
            tcode_core::BatchPolicy::ParallelReadOnly
        ),
        hide_success_result: BODY_IS_THE_RESULT.contains(&name.as_str()),
        name,
    };
    // Emitted from the tool outward rather than by looking each retired name up:
    // an alias is then the same read as the live entry, so it cannot answer
    // differently from the tool it stands for.
    tools
        .iter()
        .flat_map(|tool| {
            let live = tool.name();
            std::iter::once(meta(live.to_string(), tool)).chain(
                RETIRED_NAMES
                    .iter()
                    .filter(move |(_, owner)| *owner == live)
                    .map(|(retired, _)| meta((*retired).to_string(), tool)),
            )
        })
        .collect()
}

/// One conversation's plan, as the plan surface draws it.
///
/// Phases carry their `detail` because the desktop review edits it in place;
/// that is the difference between this and what the TUI's pane gets from a tool
/// call's input, which has detail only when the model happened to resend it.
#[derive(Serialize)]
pub struct PlanView {
    /// Absolute path, for the pane's subtitle. Display only — nothing writes
    /// through it, and `progress_write` reaches the file through the session.
    pub path: String,
    pub file: String,
    pub title: String,
    /// The plan's one-liner, for the pane's subtitle.
    pub description: String,
    /// The plan's prose — the part that belongs to no phase. Sent because this
    /// pane is a review surface and the reviewer has to be able to read what
    /// they are approving; the structured editor below it only edits phases, so
    /// this rides back untouched through `revise_plan_body`.
    pub background: String,
    /// `draft` · `active` · `done`.
    pub state: &'static str,
    pub done: usize,
    pub total: usize,
    pub phases: Vec<tcode_core::progress::Phase>,
}

impl PlanView {
    fn of(plan: &tcode_core::progress::Progress) -> Self {
        let (done, total) = plan.counts();
        Self {
            path: plan.path().display().to_string(),
            file: plan
                .path()
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            title: plan.title.clone(),
            description: plan.description.clone(),
            background: plan.background().to_string(),
            state: plan.state().label(),
            done,
            total,
            phases: plan.phases().to_vec(),
        }
    }
}

/// The plan this conversation is working through, or `null` when it has none.
///
/// The webview asks for this after every `progress` call, when a turn ends, and
/// when an approval arrives — the three moments a plan can have changed. It is
/// deliberately not polled: nothing here changes on its own.
pub fn plan(supervisor: &Arc<Supervisor>, session: String) -> Option<PlanView> {
    let handle = supervisor.get(&session)?;
    handle.plan().as_ref().map(PlanView::of)
}

/// Apply a breakdown the user edited by hand, outside any review.
///
/// This is the one write to a progress file that does not come from the model,
/// and it is a legal one: the file is the user's. The model finds out the same
/// way it finds out about an edit made in `$EDITOR` — its next `progress` call
/// is handed their version.
pub fn write_plan(
    supervisor: &Arc<Supervisor>,
    session: String,
    phases: serde_json::Value,
) -> Result<PlanView, String> {
    let handle = supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    handle.write_plan(&phases).map(|plan| PlanView::of(&plan))
}

/// Execute this conversation's approved plan in a fresh one.
///
/// The review option that leads here is the desktop's best answer to a real
/// problem: a planning conversation is full of the exploration that produced the
/// plan, and executing in it spends that context on every step. A second pane on
/// the same folder, holding only the plan, is what this window is for.
///
/// Called by the frontend once the planning turn has *ended*, not when the
/// approval was answered: the `progress` tool still had to run to mark the plan
/// active, and adopting a draft would hand the new session a plan nobody
/// approved. The state check below is that ordering made explicit rather than
/// assumed.
pub fn execute_plan_elsewhere(
    emit: &Arc<dyn Emit>,
    supervisor: &Arc<Supervisor>,
    session: String,
) -> Result<OpenedSession, String> {
    let (handle, instructions) = crate::state::hand_off_plan(supervisor, &session)?;
    let agent = supervisor.agent();
    let opened = OpenedSession::of(&handle, &agent);
    let emit = emit.clone();
    tokio::spawn(async move {
        if let Err(error) = run_turn(
            agent,
            handle.clone(),
            emit.clone(),
            Vec::new(),
            instructions,
            false,
        )
        .await
        {
            emit.emit(
                TURN_FINISHED,
                serde_json::json!({ "session": handle.id, "error": error.to_string() }),
            );
        }
    });
    Ok(opened)
}

pub fn sessions(supervisor: &Arc<Supervisor>) -> Vec<SessionInfo> {
    supervisor
        .ids()
        .into_iter()
        .filter_map(|id| supervisor.get(&id))
        .map(|handle| SessionInfo::of(&handle))
        .collect()
}

/// Every folder tcode has held a conversation in: the rail's `Recent` band and
/// the folder menu's list are both this.
#[derive(Serialize)]
pub struct ProjectList {
    pub projects: Vec<ProjectInfo>,
    /// The backend's clock, so relative times ("2 hours ago") are computed
    /// against the same clock that produced the timestamps.
    pub now: u64,
    /// Lets the frontend abbreviate paths to `~/…` without guessing at it.
    pub home: String,
}

/// Both project readers scan the session store, so both are `async` with
/// their work on a blocking thread. A sync command runs on the main thread
/// (see [`send_message`] for the other half of that rule), and the main thread
/// is the one drawing: the disclosure that opened a project used to freeze the
/// whole window — buttons, drag region, the sessions running in the other
/// panes — for as long as the read took. `spawn_blocking` and not `spawn`,
/// because this is file IO and the runtime it would otherwise sit on is the
/// one carrying every running turn.
pub async fn project_list() -> Result<ProjectList, String> {
    let home = tcode_core::home_dir().ok_or("cannot locate the home directory")?;
    let read = move || ProjectList {
        projects: projects::list(&home),
        now: projects::now_unix(),
        home: home.display().to_string(),
    };
    tokio::task::spawn_blocking(read)
        .await
        .map_err(|error| format!("cannot read the project list: {error}"))
}

/// One bounded page of resumable conversations inside a project. `before` is
/// the previous page's cursor; keeping it separate from [`project_list`] avoids
/// parsing history for folders the reader never expands.
pub async fn project_sessions(
    path: String,
    before: Option<String>,
) -> Result<StoredSessionsPage, String> {
    tokio::task::spawn_blocking(move || projects::session_page(Path::new(&path), before.as_deref()))
        .await
        .map_err(|error| format!("cannot read this project's conversations: {error}"))
}

/// Open a folder as a session, optionally resuming one of its logs.
pub fn open_folder(
    supervisor: &Arc<Supervisor>,
    path: String,
    resume: Option<String>,
) -> Result<OpenedSession, String> {
    // Canonicalize before anything else: the session id, the project data
    // directory and the rail's grouping all key on the path, and two
    // spellings of one folder would otherwise become two projects.
    let cwd = crate::paths::canonical_dir(Path::new(&path))
        .map_err(|error| format!("cannot open {path}: {error}"))?;
    let handle = supervisor
        .open_folder(&cwd, resume)
        .map_err(|error| error.to_string())?;
    eprintln!(
        "tcode-app: session {} open on {}",
        handle.id,
        handle.cwd().display()
    );
    Ok(OpenedSession::of(&handle, &supervisor.agent()))
}

/// Switch an existing conversation to another folder without creating a new
/// session. The core session keeps its `/cd` history semantics and the desktop
/// handle updates its workspace root to match.
pub fn change_folder(
    supervisor: &Arc<Supervisor>,
    session: String,
    path: String,
) -> Result<OpenedSession, String> {
    let cwd = crate::paths::canonical_dir(Path::new(&path))
        .map_err(|error| format!("cannot change to {path}: {error}"))?;
    supervisor.change_folder(&session, &cwd)?;
    let handle = supervisor
        .get(&session)
        .expect("a changed open session remains registered");
    eprintln!(
        "tcode-app: session {} changed directory to {}",
        handle.id,
        handle.cwd().display()
    );
    Ok(OpenedSession::of(&handle, &supervisor.agent()))
}

/// Close a session, cancelling its turn if one is running.
pub fn close_session(supervisor: &Arc<Supervisor>, session: String) {
    supervisor.close(&session);
}

/// One workspace entry in the webview contract.
#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct WorkspaceEntryView {
    pub name: String,
    /// Slash-separated path relative to the session's workspace root.
    pub path: String,
    /// `file`, `directory`, or `link`.
    pub kind: &'static str,
}

impl From<WorkspaceEntry> for WorkspaceEntryView {
    fn from(entry: WorkspaceEntry) -> Self {
        Self {
            name: entry.name,
            path: entry.path,
            kind: match entry.kind {
                EntryKind::File => "file",
                EntryKind::Directory => "directory",
                EntryKind::Link => "link",
            },
        }
    }
}

/// A text file response in the webview contract.
#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct WorkspaceTextView {
    pub path: String,
    pub text: String,
    pub revision: String,
    pub fingerprint: String,
    pub bytes: u64,
    pub truncated: bool,
}

impl From<TextFile> for WorkspaceTextView {
    fn from(file: TextFile) -> Self {
        Self {
            path: file.path,
            text: file.text,
            revision: file.revision,
            fingerprint: file.fingerprint,
            bytes: file.bytes,
            truncated: file.truncated,
        }
    }
}

/// The metadata answer the editor polls to notice the disk moved: same
/// `fingerprint` vocabulary as [`WorkspaceTextView`], without the bytes.
#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct WorkspaceStatView {
    pub path: String,
    pub fingerprint: String,
    pub bytes: u64,
}

impl From<WorkspaceStat> for WorkspaceStatView {
    fn from(stat: WorkspaceStat) -> Self {
        Self {
            path: stat.path,
            fingerprint: stat.fingerprint,
            bytes: stat.bytes,
        }
    }
}

/// A file the viewer draws rather than reads, in the webview contract.
///
/// Which files these are is the frontend's one extension table
/// (`ui/src/show.ts`), exactly as it is for `shown_file`: this side is a byte
/// server that was told which of its two doors to open, not a second opinion
/// about what a `.png` is.
#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct WorkspaceBinaryView {
    pub path: String,
    /// `data:<media type>;base64,…`. The webview's CSP already allows `data:`
    /// in `img-src`, so drawing this needs no asset protocol and no new origin.
    pub url: String,
    pub bytes: u64,
}

fn session_workspace(supervisor: &Supervisor, session: &str) -> Result<Workspace, String> {
    let handle = supervisor
        .get(session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    Workspace::open(&handle.cwd()).map_err(|error| error.to_string())
}

/// How many completions a menu shows before the answer is "keep typing".
const COMPLETION_LIMIT: usize = 12;

/// Entries that could finish an `@path` the composer is part-way through.
///
/// Async and off the main thread, unlike its neighbours: this one runs while
/// somebody is typing, and the main thread is the one drawing the window (see
/// AGENTS.md rule 22). A directory read that blocks it would stall the caret in
/// the field that asked for it.
pub async fn workspace_complete(
    supervisor: &Arc<Supervisor>,
    session: String,
    prefix: String,
) -> Result<Vec<WorkspaceEntryView>, String> {
    let workspace = session_workspace(supervisor, &session)?;
    tokio::task::spawn_blocking(move || {
        workspace
            .complete(&prefix, COMPLETION_LIMIT)
            .into_iter()
            .map(WorkspaceEntryView::from)
            .collect()
    })
    .await
    .map_err(|error| error.to_string())
}

/// Which of these `@path` mentions name something that is really in this
/// folder, so the composer can draw the ones that resolve differently from the
/// ones that do not.
///
/// The whole draft is asked about in one call and answered off the main thread,
/// for the same reason as `workspace_complete`: this runs while somebody types.
pub async fn workspace_present(
    supervisor: &Arc<Supervisor>,
    session: String,
    paths: Vec<String>,
) -> Result<Vec<String>, String> {
    let workspace = session_workspace(supervisor, &session)?;
    tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .filter(|path| workspace.exists(path))
            .collect()
    })
    .await
    .map_err(|error| error.to_string())
}

/// List the direct children of the session workspace root or one of its directories.
pub fn workspace_list(
    supervisor: &Arc<Supervisor>,
    session: String,
    path: Option<String>,
) -> Result<Vec<WorkspaceEntryView>, String> {
    session_workspace(supervisor, &session)?
        .list(path.as_deref())
        .map(|entries| entries.into_iter().map(WorkspaceEntryView::from).collect())
        .map_err(|error| error.to_string())
}

/// Read a UTF-8 text file from the session workspace.
pub fn workspace_read_text(
    supervisor: &Arc<Supervisor>,
    session: String,
    path: String,
) -> Result<WorkspaceTextView, String> {
    session_workspace(supervisor, &session)?
        .read(&path)
        .map(WorkspaceTextView::from)
        .map_err(|error| error.to_string())
}

/// Read a file from the session workspace as a `data:` URL.
pub fn workspace_read_binary(
    supervisor: &Arc<Supervisor>,
    session: String,
    path: String,
) -> Result<WorkspaceBinaryView, String> {
    session_workspace(supervisor, &session)?
        .read_binary(&path)
        .map(|file| WorkspaceBinaryView {
            url: data_url(Path::new(&file.path), &file.data),
            path: file.path,
            bytes: file.bytes,
        })
        .map_err(|error| error.to_string())
}

/// The metadata answer the editor polls to notice the disk moved.
pub fn workspace_stat(
    supervisor: &Arc<Supervisor>,
    session: String,
    path: String,
) -> Result<WorkspaceStatView, String> {
    session_workspace(supervisor, &session)?
        .stat(&path)
        .map(WorkspaceStatView::from)
        .map_err(|error| error.to_string())
}

/// Write a UTF-8 text file when its revision still matches — or, when `force`
/// is set, regardless of what changed on disk. `force` is the "overwrite"
/// answer the editor offers after telling the reader the file moved.
pub fn workspace_write_text(
    supervisor: &Arc<Supervisor>,
    session: String,
    path: String,
    text: String,
    revision: String,
    force: bool,
) -> Result<WorkspaceTextView, String> {
    let workspace = session_workspace(supervisor, &session)?;
    if force {
        workspace
            .write_force(&path, &text)
            .map(WorkspaceTextView::from)
    } else {
        workspace
            .write(&path, &text, &revision)
            .map(WorkspaceTextView::from)
    }
    .map_err(|error| error.to_string())
}

/// Create an empty file or directory under the session workspace.
pub fn workspace_create(
    supervisor: &Arc<Supervisor>,
    session: String,
    parent: Option<String>,
    name: String,
    kind: String,
) -> Result<WorkspaceEntryView, String> {
    let workspace = session_workspace(supervisor, &session)?;
    create_workspace_entry(&workspace, parent.as_deref(), &name, &kind)
}

fn create_workspace_entry(
    workspace: &Workspace,
    parent: Option<&str>,
    name: &str,
    kind: &str,
) -> Result<WorkspaceEntryView, String> {
    match kind {
        "file" => workspace
            .create_file(parent, name, "")
            .map(|file| WorkspaceEntryView {
                name: name.to_owned(),
                path: file.path,
                kind: "file",
            }),
        "directory" => workspace
            .create_dir(parent, name)
            .map(WorkspaceEntryView::from),
        _ => return Err(format!("'{kind}' is not a workspace entry kind")),
    }
    .map_err(|error| error.to_string())
}

/// Rename a file or directory without moving it from its parent directory.
pub fn workspace_rename(
    supervisor: &Arc<Supervisor>,
    session: String,
    path: String,
    name: String,
) -> Result<WorkspaceEntryView, String> {
    session_workspace(supervisor, &session)?
        .rename(&path, &name)
        .map(WorkspaceEntryView::from)
        .map_err(|error| error.to_string())
}

/// One way to hand a workspace file to a program outside this app.
#[derive(Serialize)]
pub struct OpenerView {
    pub id: String,
    pub name: String,
}

/// The external openers installed on this machine, file manager first.
///
/// Read once per menu rather than at startup: an editor installed while the app
/// was running should turn up without a restart, and the probe is a handful of
/// `stat` calls.
pub fn workspace_openers() -> Vec<OpenerView> {
    crate::openers::available()
        .into_iter()
        .map(|opener| OpenerView {
            id: opener.id,
            name: opener.name,
        })
        .collect()
}

/// Open one workspace entry in an external program.
///
/// The webview sends an opener *id*, never a command: the table of programs is
/// in `openers.rs` and an id that is not in it is refused (rule 3). The path is
/// resolved through the same confinement as every read, so this cannot reach
/// outside the session's workspace whatever the webview says.
pub fn workspace_open_external(
    supervisor: &Arc<Supervisor>,
    session: String,
    path: String,
    opener: String,
) -> Result<(), String> {
    let resolved = session_workspace(supervisor, &session)?
        .host_path(&path)
        .map_err(|error| error.to_string())?;
    crate::openers::open(&opener, &resolved)
}

/// Delete a file, link, or empty directory from the session workspace.
pub fn workspace_delete(
    supervisor: &Arc<Supervisor>,
    session: String,
    path: String,
) -> Result<(), String> {
    session_workspace(supervisor, &session)?
        .delete(&path)
        .map_err(|error| error.to_string())
}

/// Move a file, directory or link from the session workspace to the platform
/// trash. Recoverable, so the frontend does not ask before calling it — the
/// same reason a file manager's "Move to trash" does not confirm.
pub fn workspace_trash(
    supervisor: &Arc<Supervisor>,
    session: String,
    path: String,
) -> Result<(), String> {
    session_workspace(supervisor, &session)?
        .trash(&path)
        .map_err(|error| error.to_string())
}

/// One prompt still waiting for the turn to reach a delivery point.
#[derive(Serialize)]
pub struct QueuedView {
    pub text: String,
    pub attachments: Vec<String>,
    /// The active turn that owns this queue snapshot. A stop request must name
    /// it so a late webview click cannot interrupt its successor.
    pub turn: Option<u64>,
}

fn queue_of(handle: &crate::state::SessionHandle) -> Vec<QueuedView> {
    let (turn, queued) = handle.queued_with_turn();
    queued
        .into_iter()
        .map(|message| QueuedView {
            text: message.text,
            attachments: message.attachments,
            turn,
        })
        .collect()
}

/// Result of a core-owned slash command.
///
/// A command either replaced the conversation (which the frontend must replay
/// from the authoritative ledger), started asynchronous compaction, needs the
/// existing resume picker, or has a one-line status response.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SlashResult {
    Conversation {
        opened: OpenedSession,
        notice: Option<String>,
    },
    CompactStarted,
    /// A `/name` skill was loaded. Loading one is not a command that answers —
    /// it is a prompt — so this reports what `send_message` reports: the queue
    /// as it now stands, empty when the turn is already running. `prompt` is
    /// the line to show in the transcript, composed here rather than in the
    /// webview because the message the model received is the rendered file and
    /// no frontend may author its own account of that.
    SkillLoaded {
        prompt: String,
        queued: Vec<QueuedView>,
    },
    Notice {
        text: String,
        error: bool,
    },
}

/// The core commands whose semantics fit this desktop surface.
///
/// One list, read by both the dispatcher below and the menu the composer
/// offers. Two lists would let the menu advertise a command the dispatcher
/// answers with "unsupported desktop command" — a completion for something
/// that cannot be completed.
const DESKTOP_COMMANDS: [&str; 2] = ["clear", "compact"];

/// What the composer offers after a `/`.
#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct SlashCommandView {
    /// With its leading slash, as core writes it.
    pub name: String,
    pub help: String,
}

/// The slash commands this window can run, with core's own one-line help.
///
/// The help text is core's rather than the webview's for the same reason tool
/// names are (`Tool::display_name`): `/help` in the terminal and this menu
/// describe the same command, and two descriptions of one thing is one of them
/// being wrong.
///
/// Skills come after them, exactly as `/help` orders them in the terminal.
/// `/name` is shorthand for loading that skill — the same fallback
/// `App::dispatch_skill` implements there — so leaving them out of this menu
/// did not make them unavailable, it made them undiscoverable: a person had to
/// already know the name to type it. The dispatcher below reads this same
/// list, so the menu still cannot advertise something it would refuse.
pub fn slash_commands(supervisor: &Arc<Supervisor>) -> Vec<SlashCommandView> {
    tcode_core::commands::CommandRegistry::builtin()
        .entries()
        .filter(|(name, _)| DESKTOP_COMMANDS.contains(&name.trim_start_matches('/')))
        .map(|(name, help)| SlashCommandView {
            name: name.to_string(),
            help: help.to_string(),
        })
        .chain(supervisor.skills().iter().map(|skill| SlashCommandView {
            name: format!("/{}", skill.name),
            help: skill.description.clone(),
        }))
        .collect()
}

/// A slash line's name and everything after it, as the skill table reads them.
fn slash_parts(line: &str) -> Option<(&str, &str)> {
    let rest = line.trim().strip_prefix('/')?;
    Some(match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim()),
        None => (rest, ""),
    })
}

/// Dispatch the core commands whose semantics fit this desktop surface.
///
/// The webview sends text, not an operation: parsing and all session mutations
/// remain in Core's command registry. Effects that only a frontend can perform
/// are interpreted here, where the existing turn lifecycle is available.
///
/// A line that is no command falls back to the skill table, exactly as
/// `App::dispatch_skill` does in the terminal: `/name` is shorthand for loading
/// that skill. The fallback is last, so a skill can never shadow a command.
pub fn slash_command(
    emit: &Arc<dyn Emit>,
    supervisor: &Arc<Supervisor>,
    session: String,
    line: String,
) -> Result<SlashResult, String> {
    match slash_parts(&line) {
        Some((name, _)) if DESKTOP_COMMANDS.contains(&name) => {}
        Some((name, args)) if supervisor.skills().iter().any(|skill| skill.name == name) => {
            return load_skill(emit, supervisor, &session, name, args)
        }
        Some(_) => {
            return Err(format!(
                "unsupported desktop command {line} — use {}",
                DESKTOP_COMMANDS
                    .iter()
                    .map(|name| format!("/{name}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
        None => return Err("a slash command must start with '/'".into()),
    }
    let tcode_core::commands::CommandOutcome { messages, effects } =
        supervisor.dispatch_slash(&session, &line)?;
    // At most one effect, and every one of them is a whole answer for this
    // surface — so this takes the first rather than iterating. It was written
    // as a `for` that returned on its first pass, which clippy's `never_loop`
    // was right to call out: the loop implied later effects might be handled,
    // and they never were.
    if let Some(effect) = effects.into_iter().next() {
        match effect {
            tcode_core::commands::CommandEffect::ConversationCleared
            | tcode_core::commands::CommandEffect::ConversationReplaced => {
                let handle = supervisor
                    .get(&session)
                    .ok_or_else(|| format!("session '{session}' is not open"))?;
                let notice = messages.first().map(|message| message.text.clone());
                return Ok(SlashResult::Conversation {
                    opened: OpenedSession::of(&handle, &supervisor.agent()),
                    notice,
                });
            }
            tcode_core::commands::CommandEffect::Compact { focus } => {
                let handle = supervisor
                    .get(&session)
                    .ok_or_else(|| format!("session '{session}' is not open"))?;
                let agent = supervisor.agent();
                let emit = emit.clone();
                tokio::spawn(async move {
                    if let Err(error) =
                        run_compact(agent, handle.clone(), emit.clone(), focus).await
                    {
                        emit.emit(
                            TURN_FINISHED,
                            serde_json::json!({ "session": handle.id, "error": error.to_string() }),
                        );
                    }
                });
                return Ok(SlashResult::CompactStarted);
            }
            tcode_core::commands::CommandEffect::PersistDogfood(on) => {
                if let Ok(cf) = selected_config_file(supervisor) {
                    Config::update_tcode_state(&cf, move |state| state.dogfood = on);
                }
            }
            tcode_core::commands::CommandEffect::PersistKong(on) => {
                if let Ok(cf) = selected_config_file(supervisor) {
                    Config::update_tcode_state(&cf, move |state| state.kong = on);
                }
            }
            tcode_core::commands::CommandEffect::PersistSuggestions(on) => {
                if let Ok(cf) = selected_config_file(supervisor) {
                    Config::update_tcode_state(&cf, move |state| state.suggestions = Some(on));
                }
            }
            _ => return Err("that command is not available in the desktop app".into()),
        }
    }
    let message = messages
        .into_iter()
        .next()
        .ok_or("command returned no result")?;
    Ok(SlashResult::Notice {
        error: matches!(message.kind, tcode_core::commands::MessageKind::Error),
        text: message.text,
    })
}

/// The prompt a `/name` line loads: the skill's rendered body, wrapped in the
/// sentinel that marks it as a repository file rather than the person speaking.
///
/// `/name` is not a command that answers; it is a cheaper way to spend a turn —
/// one round trip less than making the model call the `skill` tool. Every step
/// of it (find, read, render, wrap) happens here rather than in the webview for
/// the same reason `plan` is a flag on `send_message`: no frontend may author
/// context that claims to be the harness. Kept free of `AppHandle` so the whole
/// decision is testable without a window (AGENTS.md rule 2); the command below
/// is left with the sending.
pub fn skill_prompt(
    supervisor: &Supervisor,
    handle: &crate::state::SessionHandle,
    name: &str,
    args: &str,
) -> Result<String, String> {
    let skill = supervisor
        .skills()
        .iter()
        .find(|skill| skill.name == name)
        .ok_or_else(|| format!("no skill named '{name}'"))?;
    // One small file, read because the user just asked for it by name. Not the
    // class of read AGENTS.md rule 22 is about (a folder of session logs
    // replayed for previews): it is the same blocking read the terminal does at
    // the same moment, and it is over before the click is.
    let body = match &skill.source {
        tcode_tools::SkillSource::Dir(dir) => {
            let path = dir.join("SKILL.md");
            std::fs::read_to_string(&path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?
        }
        tcode_tools::SkillSource::Builtin(body) => body.to_string(),
    };
    // The session's own scratch directory when it is not mid-turn; otherwise the
    // project's, which is that directory's parent and equally writable. A skill
    // body that mentions scratch gets a real path either way.
    let scratch = handle
        .scratch_dir()
        .unwrap_or_else(|| tcode_core::store::scratchpad_dir(&handle.cwd()));
    let rendered = tcode_tools::render_skill(skill, &body, args, &handle.cwd(), &scratch);
    Ok(tcode_tools::wrap_skill_echo(name, args, &rendered))
}

/// Load a skill and send it as this conversation's next prompt. Runs whether or
/// not a turn is in flight — a prompt queues, where a registry command would
/// need the `&mut Session` a running turn holds.
fn load_skill(
    emit: &Arc<dyn Emit>,
    supervisor: &Supervisor,
    session: &str,
    name: &str,
    args: &str,
) -> Result<SlashResult, String> {
    let handle = supervisor
        .get(session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    let text = skill_prompt(supervisor, &handle, name, args)?;
    // What the transcript shows: the line the person typed. The body is in the
    // ledger, in the export and in the model's context — it is just not a prompt.
    let prompt = crate::state::folded_prompt(&text)
        .expect("a wrapped skill echo is what `folded_prompt` recognizes");
    let queued = deliver(
        emit,
        supervisor.agent(),
        handle,
        tcode_core::PendingMessage {
            text: text.clone(),
            attachments: Vec::new(),
            blocks: vec![ContentBlock::Text { text }],
            instructions: Vec::new(),
            expects_plan: false,
        },
    );
    Ok(SlashResult::SkillLoaded { prompt, queued })
}

/// Hand a composed message to a session: start its turn, or leave it queued.
///
/// The one place either kind of prompt — typed or loaded from a skill — meets
/// the turn lifecycle, and the only part of that which needs an `AppHandle`.
/// Returns the queue as it now stands, so an empty answer means "this is being
/// sent" and a non-empty one is exactly what to draw above the composer.
fn deliver(
    emit: &Arc<dyn Emit>,
    agent: Arc<tcode_core::Agent>,
    handle: Arc<crate::state::SessionHandle>,
    message: tcode_core::PendingMessage,
) -> Vec<QueuedView> {
    let Some(message) = handle.send_or_queue(message) else {
        return queue_of(&handle);
    };
    let (input, instructions, expects_plan) =
        (message.blocks, message.instructions, message.expects_plan);
    let emit = emit.clone();
    tokio::spawn(async move {
        if let Err(error) = run_turn(
            agent,
            handle.clone(),
            emit.clone(),
            input,
            instructions,
            expects_plan,
        )
        .await
        {
            // `Busy` still reaches here: `send_or_queue` closed the ordinary
            // race, but two sends can both find the session free before either
            // spawned task has taken it. The command already returned, so the
            // only way to tell the user is the channel the turn would have used.
            emit.emit(
                TURN_FINISHED,
                serde_json::json!({ "session": handle.id, "error": error.to_string() }),
            );
        }
    });
    Vec::new()
}

/// Start a turn — or queue the prompt when one is already running.
///
/// Returns the queue as it now stands, so an empty answer means "this is being
/// sent" and a non-empty one is exactly what to draw above the composer. The
/// webview never decides which of the two happened: "is this session busy" is
/// only answerable under the lock that starts turns, and a frontend that
/// answered it from its own state would drop the prompt typed in the same
/// moment a turn ended.
///
/// Returns as soon as it is running, not when it finishes: progress arrives as
/// events, and the webview must stay responsive to answer the approvals this
/// very turn may raise.
///
/// The task goes on `tokio::spawn`, and that only became safe when every
/// command started arriving through one async entry point (`dispatch::Registry`
/// awaited from a single `rpc` command, or from the sidecar's read loop). It
/// used to be `tauri::async_runtime::spawn`, for a reason worth keeping: a
/// *sync* `#[tauri::command]` runs on the main thread, where no Tokio runtime is
/// guaranteed to be entered, and `tokio::spawn` there panics — a panicking
/// command is an `invoke` that never settles, which the frontend can only render
/// as a turn that started and produced nothing. There is no longer a path that
/// reaches this function from outside a runtime; if one is ever added, this is
/// the failure it produces.
pub fn send_message(
    emit: &Arc<dyn Emit>,
    supervisor: &Arc<Supervisor>,
    session: String,
    text: String,
    images: Option<Vec<ImageInput>>,
    plan: Option<bool>,
) -> Result<Vec<QueuedView>, String> {
    let handle = supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    let agent = supervisor.agent();
    let vision = handle.input_supports_vision(&agent.model);
    let input = compose(
        text.clone(),
        images.unwrap_or_default(),
        vision,
        &handle.scratch_dir(),
    );
    // "Plan this first" is a *kind* of turn, so the webview sends a flag and the
    // text comes from core — the same instruction `/plan` submits in the
    // terminal. The task itself stays the user's own message, which is what
    // keeps it in the transcript where they wrote it.
    let instructions = match plan.unwrap_or(false) {
        true => vec![tcode_core::commands::plan::planning_instruction("")],
        false => Vec::new(),
    };

    // The queued path carries `instructions` too: core appends them as
    // `Entry::Instruction` immediately before the prompt when it delivers, so
    // "plan this first" means the same thing whether the turn was free or busy.
    Ok(deliver(
        emit,
        agent,
        handle,
        tcode_core::PendingMessage {
            text,
            attachments: Vec::new(),
            blocks: input,
            instructions,
            expects_plan: plan.unwrap_or(false),
        },
    ))
}

/// What this conversation still owes the model, for the strip above the
/// composer. Read rather than pushed: the queue is the backend's, and a webview
/// mirror would be a second answer to "what have I got waiting".
pub fn queued(supervisor: &Arc<Supervisor>, session: String) -> Result<Vec<QueuedView>, String> {
    let handle = supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    Ok(queue_of(&handle))
}

/// Take one queued prompt back, returning the queue as it now stands.
///
/// The text goes with the index and has to match, because both came from the
/// webview and a stale index alone would remove whatever now sits at that
/// position (AGENTS.md rule 3). A mismatch removes nothing and the answer shows
/// the caller why.
pub fn withdraw_queued(
    supervisor: &Arc<Supervisor>,
    session: String,
    index: usize,
    text: String,
) -> Result<Vec<QueuedView>, String> {
    let handle = supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    handle.withdraw_queued(index, &text);
    Ok(queue_of(&handle))
}

/// Stop the running turn and send what is queued straight away.
///
/// Distinct from `interrupt`, which stops and leaves the queue for whenever the
/// turn would have delivered it. This is the "no, do this instead" button, and
/// the follow-up turn is started by `run_turn` itself once the cancelled one has
/// let go of the session.
pub fn interrupt_and_send(
    supervisor: &Arc<Supervisor>,
    session: String,
    turn: u64,
) -> Result<bool, String> {
    Ok(supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?
        .interrupt_and_flush(turn))
}

/// A prompt this conversation can be rewound to.
#[derive(Serialize)]
pub struct RewindTargetView {
    /// Ledger index — the truncation point, and the only thing `rewind` accepts.
    pub index: usize,
    pub text: String,
    pub dirty: bool,
}

/// Every point this conversation can go back to, oldest first.
///
/// The webview matches these onto the prompts in its transcript by text, in
/// order, and draws the control only where one lands. That is deliberately a
/// *lookup* rather than a shared numbering: the transcript is replayed from
/// `history()`, which includes the compacted-away era that holds no valid
/// truncation index at all, so a position in one list is not a position in the
/// other. A prompt this fails to match simply gets no button, and `rewind`
/// re-checks the index against this same list anyway — the failure mode is a
/// missing affordance, never a truncation somewhere nobody asked for.
pub fn rewind_targets(
    supervisor: &Arc<Supervisor>,
    session: String,
) -> Result<Vec<RewindTargetView>, String> {
    let handle = supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    Ok(handle
        .rewind_targets()
        .into_iter()
        .map(|target| RewindTargetView {
            index: target.index,
            text: target.text,
            dirty: target.dirty,
        })
        .collect())
}

/// What rewinding to this point would cost, before anything is done.
///
/// Two round trips instead of one confirm-and-hope, because the second question
/// only exists sometimes: rolling files back is worth offering when that era
/// actually changed some, and is noise when it did not. `dropped` is how many
/// prompts stop existing, which is the part nobody can undo by clicking again.
#[derive(Serialize)]
pub struct RewindPreview {
    pub text: String,
    pub dirty: bool,
    pub dropped: usize,
}

pub fn rewind_preview(
    supervisor: &Arc<Supervisor>,
    session: String,
    entry_index: usize,
) -> Result<RewindPreview, String> {
    let handle = supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    let targets = handle.rewind_targets();
    if targets.is_empty() {
        return Err("this conversation is busy; stop the turn before rewinding it".into());
    }
    let at = targets
        .iter()
        .position(|target| target.index == entry_index)
        .ok_or("that point is no longer in this conversation")?;
    Ok(RewindPreview {
        text: targets[at].text.clone(),
        dirty: targets[at].dirty,
        dropped: targets.len() - at,
    })
}

/// What a file rewind did to one path.
#[derive(Serialize)]
pub struct RestoredFile {
    pub path: String,
    /// `restored` / `deleted` / the failure, in words — the three outcomes are
    /// not interchangeable and a boolean would have to pick two of them.
    pub outcome: String,
}

/// The conversation after a rewind: everything `open_folder` returns, because
/// every derived thing the webview holds — the transcript, the file index, the
/// meter — has to be rebuilt from the ledger that just got shorter.
///
/// Rebuilt rather than truncated on the far side. The webview keeps four
/// separate reductions of the event stream, and hand-truncating each at the
/// right point is four chances to leave one showing work that no longer
/// happened. Replay is a path that already exists and is already correct.
#[derive(Serialize)]
pub struct Rewound {
    pub session: OpenedSession,
    /// The prompt, handed back to be edited and sent again.
    pub text: String,
    pub restored: Vec<RestoredFile>,
}

pub fn rewind(
    supervisor: &Arc<Supervisor>,
    session: String,
    entry_index: usize,
    restore_files: bool,
) -> Result<Rewound, String> {
    let handle = supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    let (text, restored) = handle.rewind(entry_index, restore_files)?;
    eprintln!(
        "tcode-app: rewound session {} to entry {entry_index} (files: {})",
        handle.id,
        if restore_files { "restored" } else { "kept" }
    );
    Ok(Rewound {
        session: OpenedSession::of(&handle, &supervisor.agent()),
        text,
        restored: restored
            .into_iter()
            .map(|(path, outcome)| RestoredFile {
                path: path.display().to_string(),
                outcome: match outcome {
                    tcode_core::checkpoint::Restore::Restored => "restored".into(),
                    tcode_core::checkpoint::Restore::Deleted => "deleted".into(),
                    tcode_core::checkpoint::Restore::Failed(error) => format!("failed: {error}"),
                },
            })
            .collect(),
    })
}

/// One image pasted or dropped into the composer. Base64 because that is what
/// the provider wire wants and what the webview can produce without a file.
#[derive(serde::Deserialize)]
pub struct ImageInput {
    pub media_type: String,
    pub data: String,
}

/// The blocks one prompt turns into.
///
/// Images lead, then the text: a picture followed by "why is this wrong?" reads
/// in that order, and it is the order the TUI already composes.
///
/// When the model cannot see, the image is **saved and named** rather than
/// dropped. The user pasted a thing; telling the model where that thing is
/// leaves them a way forward (switch models, or have a tool read it), while a
/// silent drop leaves a question about a picture nobody has.
pub fn compose(
    text: String,
    images: Vec<ImageInput>,
    vision: bool,
    scratch: &Option<PathBuf>,
) -> Vec<ContentBlock> {
    let mut blocks = Vec::with_capacity(images.len() + 1);
    for (index, image) in images.into_iter().enumerate() {
        if vision {
            blocks.push(ContentBlock::Image {
                media_type: image.media_type,
                data: image.data,
            });
            continue;
        }
        blocks.push(ContentBlock::Text {
            text: match save_pasted(&image, index, scratch) {
                Ok(path) => format!("[pasted image saved to {}]", path.display()),
                Err(reason) => {
                    format!(
                        "[pasted image could not be saved ({reason}); this model cannot view it]"
                    )
                }
            },
        });
    }
    if !text.trim().is_empty() {
        blocks.push(ContentBlock::Text { text });
    }
    blocks
}

fn save_pasted(
    image: &ImageInput,
    index: usize,
    scratch: &Option<PathBuf>,
) -> Result<PathBuf, String> {
    use base64::Engine as _;
    let dir = scratch
        .as_ref()
        .ok_or("this session has no scratch directory")?
        .join("pasted");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .map_err(|error| error.to_string())?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let extension = match image.media_type.as_str() {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    };
    let path = dir.join(format!("paste-{stamp}-{index}.{extension}"));
    std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(&path, bytes))
        .map_err(|error| error.to_string())?;
    Ok(path)
}

/// One file, loaded for an inspect pane opened by `show`.
///
/// `body` is whatever the caller asked for: the file's text, or a `data:` URL
/// when it asked for one. Which of the two a given file needs is the frontend's
/// single extension table (`ui/src/show.ts`) — duplicating that judgement here
/// would be a second table to keep in step with the first, so this command
/// stays a byte server and takes the answer as an argument.
#[derive(Serialize, Debug)]
pub struct ShownFile {
    pub body: String,
    /// The file's real size, so a truncated view can say what it is a prefix of.
    pub bytes: u64,
    /// True when `body` stops short of the file.
    pub truncated: bool,
}

/// An image has to arrive whole to be an image, so it gets a larger allowance
/// than text — but not an unbounded one: the `data:` URL is base64 and lands in
/// the webview's memory in one piece.
const VIEWER_IMAGE_BUDGET: u64 = 4 * tcode_tools::VIEWER_TEXT_BUDGET;

/// Load a file for a `show` pane.
///
/// The path arrives from the webview, so it is data (AGENTS.md rule 3): it is
/// re-checked against the session's own folder here rather than trusted because
/// the tool checked it earlier. The two share one definition of the boundary
/// (`tcode_tools::is_viewable_path`) so they cannot drift into disagreeing.
pub fn shown_file(
    supervisor: &Arc<Supervisor>,
    session: String,
    path: String,
    binary: bool,
) -> Result<ShownFile, String> {
    let handle = supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    load_shown(Path::new(&path), &handle.cwd(), binary)
}

/// The persisted desktop preferences which have a runtime effect.
///
/// Code palettes are browser-local display state; the shell has to cross the
/// sidecar because it chooses the program new terminal tabs execute.
#[derive(Serialize)]
pub struct DesktopSettings {
    pub terminal_shell: String,
}

fn selected_config_file(supervisor: &Arc<Supervisor>) -> Result<PathBuf, String> {
    let menus = supervisor.menus();
    let menus = menus.lock().map_err(|_| "picker state is poisoned")?;
    Ok(menus.config_file.clone())
}

/// Read the terminal shell after its runtime preference has overridden the
/// hand-written default. An empty response deliberately means automatic
/// detection, not a missing setting.
pub fn desktop_settings(supervisor: &Arc<Supervisor>) -> Result<DesktopSettings, String> {
    let config_file = selected_config_file(supervisor)?;
    let config = Config::load_global_at(&config_file).map_err(|error| error.to_string())?;
    Ok(DesktopSettings {
        terminal_shell: config
            .tcode_state
            .terminal_shell
            .or(config.ui.shell)
            .unwrap_or_default(),
    })
}

/// Persist the shell selected in Settings and use it for tabs opened from now
/// on. The empty string is meaningful: it asks the terminal to detect a shell
/// rather than returning to the hand-written `[ui] shell` default.
pub fn set_terminal_shell(
    supervisor: &Arc<Supervisor>,
    terminals: &Arc<crate::terminal::Terminals>,
    shell: String,
) -> Result<DesktopSettings, String> {
    let shell = shell.trim().to_string();
    let config_file = selected_config_file(supervisor)?;
    Config::update_tcode_state_checked(&config_file, |state| {
        state.terminal_shell = Some(shell.clone());
    })
    .map_err(|error| error.to_string())?;
    terminals.set_shell(Some(shell.clone()));
    Ok(DesktopSettings {
        terminal_shell: shell,
    })
}

/// Start a shell for a new terminal tab.
///
/// The folder is data (rule 3) and takes the same canonicalizing path every
/// other folder in this app does, so a tab cannot be opened on `\\?\C:\…` or on
/// a path that no longer exists. A terminal is the *user's*, so there is no
/// permission question here — but there is still a "does this folder exist"
/// question, and answering it here beats a shell that starts in `/` because its
/// `cwd` was rejected silently.
pub fn terminal_open(
    emit: &Arc<dyn Emit>,
    terminals: &Arc<crate::terminal::Terminals>,
    cwd: String,
    cols: u16,
    rows: u16,
) -> Result<String, String> {
    let dir = crate::paths::canonical_dir(Path::new(&cwd))
        .map_err(|error| format!("cannot open a terminal in {cwd}: {error}"))?;
    terminals.open(emit.clone(), &dir, cols, rows)
}

/// Keystrokes, base64. See `terminal.rs` for why they are not a string.
pub fn terminal_write(
    terminals: &Arc<crate::terminal::Terminals>,
    id: String,
    data: String,
) -> Result<(), String> {
    terminals.write(&id, &data)
}

pub fn terminal_resize(
    terminals: &Arc<crate::terminal::Terminals>,
    id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    terminals.resize(&id, cols, rows)
}

pub fn terminal_close(
    terminals: &Arc<crate::terminal::Terminals>,
    id: String,
) -> Result<(), String> {
    terminals.close(&id)
}

/// The URL a frame loads to display a file, instead of its bytes.
///
/// This is the same request as `shown_file` answered a different way, and which
/// files take which route is the frontend's one extension table (`show.ts`),
/// exactly as it is for the text/`data:` split above. A generated HTML report
/// takes this one because the properties it needs — scripts that run, relative
/// references that resolve, `fetch` that works — are properties of the origin
/// it loads from, not of how its bytes are parsed. See `serve.rs`.
///
/// The path is data (AGENTS.md rule 3) and is re-checked here through the same
/// boundary as every other read, inside `Serve::url`.
pub fn serve_url(
    supervisor: &Arc<Supervisor>,
    serve: &crate::boot::ServeHandle,
    session: String,
    path: String,
) -> Result<String, String> {
    let handle = supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    let file = Path::new(&path);
    let file = if file.is_absolute() {
        file.to_path_buf()
    } else {
        handle.cwd().join(file)
    };
    serve.get()?.url(&file, &handle.cwd())
}

/// The command's whole body, reachable without a window (AGENTS.md rule 2).
pub fn load_shown(path: &Path, cwd: &Path, as_data_url: bool) -> Result<ShownFile, String> {
    let file = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    tcode_tools::is_viewable_path(&file, cwd)?;

    let bytes = std::fs::metadata(&file)
        .map_err(|error| format!("cannot read {}: {error}", file.display()))?
        .len();
    let budget = if as_data_url {
        VIEWER_IMAGE_BUDGET
    } else {
        tcode_tools::VIEWER_TEXT_BUDGET
    };
    if as_data_url && bytes > budget {
        return Err(format!(
            "{} is {bytes} bytes — too large to display as an image",
            file.display()
        ));
    }

    let raw =
        std::fs::read(&file).map_err(|error| format!("cannot read {}: {error}", file.display()))?;
    if as_data_url {
        return Ok(ShownFile {
            body: data_url(&file, &raw),
            bytes,
            truncated: false,
        });
    }

    let truncated = raw.len() as u64 > budget;
    let body = if truncated {
        // Cut on a line, not mid-row: a table whose last row is half a line
        // reads as data that is wrong rather than data that is incomplete.
        let clipped = &raw[..budget as usize];
        let cut = clipped
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map(|at| at + 1)
            .unwrap_or(clipped.len());
        String::from_utf8_lossy(&clipped[..cut]).into_owned()
    } else {
        String::from_utf8_lossy(&raw).into_owned()
    };
    Ok(ShownFile {
        body,
        bytes,
        truncated,
    })
}

/// The bytes of a file, addressed the one way this webview can already draw
/// them. Shared by both readers so a `.ico` cannot be one media type when the
/// model shows it and another when it is opened from the tree.
pub(crate) fn data_url(path: &Path, bytes: &[u8]) -> String {
    use base64::Engine as _;
    format!(
        "data:{};base64,{}",
        media_type(path),
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

fn media_type(path: &Path) -> &'static str {
    match path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        _ => "image/png",
    }
}

// ---------------------------------------------------------------- pickers
//
// Thin, like everything here: the menus and their apply-closures come from
// `tcode-frontend` and live in `crate::picker`.

/// Everything the composer's chips show. Read together because a preset moves
/// the model, which moves the effort — three commands would let the strip
/// render a state that never existed.
pub fn picker_state(
    supervisor: &Arc<Supervisor>,
    session: String,
) -> Result<crate::picker::PickerState, String> {
    let handle = supervisor.get(&session);
    let (mode, staged) = handle
        .as_ref()
        .map(|handle| handle.mode())
        // A window with no conversation open still wants the model chip.
        .unwrap_or_default();
    let agent = supervisor.agent();
    let model = handle
        .as_ref()
        .and_then(|handle| handle.model())
        .unwrap_or_else(|| agent.model.clone());
    let menus = supervisor.menus();
    match handle {
        Some(handle) => {
            // Every path needing both locks takes the session-local owner first.
            let main = handle
                .main_pickers
                .lock()
                .map_err(|_| "session picker state is poisoned")?;
            let menus = menus.lock().map_err(|_| "picker state is poisoned")?;
            Ok(crate::picker::state_of(
                &main.models,
                &main.presets,
                &menus,
                &model,
                &agent.models,
                mode,
                staged,
            ))
        }
        None => {
            let menus = menus.lock().map_err(|_| "picker state is poisoned")?;
            Ok(crate::picker::state_of(
                &menus.models,
                &menus.presets,
                &menus,
                &model,
                &agent.models,
                mode,
                staged,
            ))
        }
    }
}

pub fn choose_model(
    supervisor: &Arc<Supervisor>,
    session: String,
    index: usize,
    effort: Option<String>,
) -> Result<(), String> {
    let handle = supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    let model = handle
        .model()
        .ok_or_else(|| format!("session '{session}' has no model"))?;
    let mut main = handle
        .main_pickers
        .lock()
        .map_err(|_| "session picker state is poisoned")?;
    crate::picker::choose_model(&mut main, &model, index, effort.as_deref())?;
    Ok(())
}

pub fn choose_preset(
    supervisor: &Arc<Supervisor>,
    session: String,
    key: String,
) -> Result<String, String> {
    let handle = supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    let model = handle
        .model()
        .ok_or_else(|| format!("session '{session}' has no model"))?;
    let mut main = handle
        .main_pickers
        .lock()
        .map_err(|_| "session picker state is poisoned")?;
    let menus = supervisor.menus();
    let mut menus = menus.lock().map_err(|_| "picker state is poisoned")?;
    crate::picker::choose_preset(&mut main, &mut menus, &model, &key)
}

/// Pin one sub-agent or helper role to a model, to the main model, or off.
pub fn pin_role(
    supervisor: &Arc<Supervisor>,
    kind: String,
    pin: crate::picker::PinChoice,
) -> Result<String, String> {
    let menus = supervisor.menus();
    let mut menus = menus.lock().map_err(|_| "picker state is poisoned")?;
    crate::picker::pin_role(&mut menus, &kind, pin)
}

/// Capture the live line-up as `[presets.<name>]`.
pub fn save_preset(
    supervisor: &Arc<Supervisor>,
    session: String,
    name: String,
) -> Result<(), String> {
    let handle = supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    let model = handle
        .model()
        .ok_or_else(|| format!("session '{session}' has no model"))?;
    let mut main = handle
        .main_pickers
        .lock()
        .map_err(|_| "session picker state is poisoned")?;
    let menus = supervisor.menus();
    let menus = menus.lock().map_err(|_| "picker state is poisoned")?;
    crate::picker::save_preset(&mut main, &menus, &model, name.trim())
}

/// Choose the permission mode for one conversation.
///
/// Per session, unlike the model: the mode is what *this* conversation is
/// allowed to do without asking, and two folders open side by side routinely
/// deserve different answers. It is still remembered as the default for new
/// sessions, which is the part `[tcode_state]` holds.
pub fn choose_mode(
    supervisor: &Arc<Supervisor>,
    session: String,
    mode: String,
) -> Result<(), String> {
    // From the webview, so it is data (AGENTS.md rule 3): an unrecognized mode
    // is refused, never coerced into the most permissive thing it resembles.
    let chosen = crate::picker::mode_from_key(&mode)
        .ok_or_else(|| format!("'{mode}' is not a permission mode"))?;
    let handle = supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    handle.set_mode(chosen);
    let menus = supervisor.menus();
    let config_file = {
        let menus = menus.lock().map_err(|_| "picker state is poisoned")?;
        menus.config_file.clone()
    };
    crate::picker::remember_mode(&config_file, chosen);
    Ok(())
}

/// Answer an approval the agent is parked on.
pub fn respond_approval(
    supervisor: &Arc<Supervisor>,
    session: String,
    answer: ApprovalAnswer,
) -> Result<(), String> {
    let handle = supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    // Two kinds of error come back here, and both are the frontend's to show:
    // an answer that arrived too late (answered twice, or the turn was
    // interrupted while the dialog was open — nothing ran on the strength of it
    // either way), and a plan edit that cannot be applied, which leaves the
    // question standing so the user can fix it.
    handle.pending().answer(answer)
}

/// Stop the running turn. Safe to call when nothing is running.
pub fn interrupt(supervisor: &Arc<Supervisor>, session: String) -> Result<(), String> {
    let handle = supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    handle.interrupt();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> ImageInput {
        ImageInput {
            media_type: "image/png".into(),
            // "hi" — enough to prove the bytes reach the file.
            data: "aGk=".into(),
        }
    }

    #[test]
    fn a_seeing_model_gets_the_image_before_the_question() {
        let blocks = compose("why is this wrong?".into(), vec![image()], true, &None);
        assert!(matches!(blocks[0], ContentBlock::Image { .. }));
        assert!(matches!(&blocks[1], ContentBlock::Text { text } if text.contains("wrong")));
    }

    #[test]
    fn a_blind_model_is_told_where_the_image_went() {
        let scratch = tempfile::tempdir().unwrap();
        let blocks = compose(
            String::new(),
            vec![image()],
            false,
            &Some(scratch.path().to_path_buf()),
        );
        let ContentBlock::Text { text } = &blocks[0] else {
            panic!("expected the fallback note, got {:?}", blocks[0]);
        };
        assert!(text.starts_with("[pasted image saved to "), "{text}");
        let written: Vec<_> = std::fs::read_dir(scratch.path().join("pasted"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(written.len(), 1);
        assert_eq!(std::fs::read(&written[0]).unwrap(), b"hi");
    }

    #[test]
    fn an_unsaveable_image_still_says_so_rather_than_vanishing() {
        let blocks = compose(String::new(), vec![image()], false, &None);
        let ContentBlock::Text { text } = &blocks[0] else {
            panic!("expected a note");
        };
        assert!(text.contains("could not be saved"), "{text}");
    }

    #[test]
    fn a_shown_text_file_arrives_whole_when_it_fits() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("t.csv"), "a,b\n1,2\n").unwrap();
        let file = load_shown(Path::new("t.csv"), dir.path(), false).unwrap();
        assert_eq!(file.body, "a,b\n1,2\n");
        assert!(!file.truncated);
    }

    /// A prefix must end where a row ends, or the last line of a table is a
    /// row that was never in the data.
    #[test]
    fn a_truncated_text_file_stops_on_a_line_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let row = "0123456789012345678901234567890123456789012345678901234567890,x\n";
        let rows = row.repeat((tcode_tools::VIEWER_TEXT_BUDGET as usize / row.len()) + 64);
        std::fs::write(dir.path().join("big.csv"), &rows).unwrap();

        let file = load_shown(Path::new("big.csv"), dir.path(), false).unwrap();
        assert!(file.truncated);
        assert_eq!(file.bytes, rows.len() as u64);
        assert!(file.body.ends_with('\n'), "cut mid-row");
        assert!((file.body.len() as u64) <= tcode_tools::VIEWER_TEXT_BUDGET);
    }

    #[test]
    fn an_image_comes_back_as_a_data_url_with_its_own_media_type() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.jpg"), b"hi").unwrap();
        let file = load_shown(Path::new("p.jpg"), dir.path(), true).unwrap();
        assert_eq!(file.body, "data:image/jpeg;base64,aGk=");
    }

    /// The webview's path is data, not authority: the boundary is re-checked
    /// here even though the tool checked it when the model called `show`.
    #[test]
    fn a_path_outside_the_session_folder_is_refused_at_the_command_too() {
        tcode_core::home::testing::temp_home();
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let outside = elsewhere.path().join("secret.txt");
        std::fs::write(&outside, "s").unwrap();

        let refused = load_shown(&outside, dir.path(), false).unwrap_err();
        assert!(refused.contains("outside"), "{refused}");
    }

    #[test]
    fn an_empty_message_adds_no_empty_text_block() {
        assert!(compose("   ".into(), vec![], true, &None).is_empty());
    }

    #[test]
    fn opened_session_exports_display_history_but_not_project_instructions() {
        use tcode_core::{Entry, PermissionMode, PermissionRules, Session, ToolCtx};

        tcode_core::home::testing::temp_home();
        let cwd = tempfile::tempdir().unwrap();
        let mut session = Session::new(
            ToolCtx::new(cwd.path().to_path_buf(), 25_000),
            PermissionMode::Default,
            PermissionRules::default(),
        );
        session.ledger.append(Entry::User(vec![ContentBlock::Text {
            text: "restore this conversation".into(),
        }]));
        session
            .ledger
            .append(Entry::Instruction("private project rule".into()));
        let handle = crate::state::SessionHandle::new("session".into(), cwd.path().into(), session);

        // `OpenedSession::of` is this call plus an agent read; asserting on the
        // filter directly keeps the test free of a provider it has no use for.
        let history = handle.history();
        assert_eq!(history.len(), 1);
        assert!(matches!(history.as_slice(), [Entry::User(_)]));
    }

    #[test]
    fn create_workspace_entry_delegates_to_the_workspace_with_an_empty_file() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(root.path()).unwrap();

        let file = create_workspace_entry(&workspace, None, "new.txt", "file").unwrap();
        let directory = create_workspace_entry(&workspace, None, "src", "directory").unwrap();

        assert_eq!(file.kind, "file");
        assert_eq!(file.path, "new.txt");
        assert_eq!(
            std::fs::read_to_string(root.path().join("new.txt")).unwrap(),
            ""
        );
        assert_eq!(directory.kind, "directory");
        assert!(root.path().join("src").is_dir());
    }

    #[test]
    fn create_workspace_entry_rejects_unknown_kinds() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(root.path()).unwrap();

        let error = create_workspace_entry(&workspace, None, "new", "symlink").unwrap_err();
        assert_eq!(error, "'symlink' is not a workspace entry kind");
        assert!(!root.path().join("new").exists());
    }
}
