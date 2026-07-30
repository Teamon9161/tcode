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
/// four tools whose body is a diff, not a capability core has any opinion about.
#[derive(Serialize)]
pub struct ToolViewMeta {
    pub name: String,
    pub route: &'static str,
    pub quiet_output: bool,
    pub hide_success_result: bool,
}

/// Tools whose call body already showed the change, so a success line under it
/// only repeats what the diff said.
const BODY_IS_THE_RESULT: &[&str] = &["edit", "write", "multi_edit", "notebook_edit"];

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

#[tauri::command]
pub fn tool_views(supervisor: State<'_, Arc<Supervisor>>) -> Vec<ToolViewMeta> {
    tool_view_metas(&supervisor.agent().tools)
}

/// The command's whole body, reachable without a window (AGENTS.md rule 2).
pub fn tool_view_metas(tools: &[Arc<dyn tcode_core::Tool>]) -> Vec<ToolViewMeta> {
    let meta = |name: String, tool: &Arc<dyn tcode_core::Tool>| ToolViewMeta {
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
#[tauri::command]
pub fn plan(supervisor: State<'_, Arc<Supervisor>>, session: String) -> Option<PlanView> {
    let handle = supervisor.get(&session)?;
    handle.plan().as_ref().map(PlanView::of)
}

/// Apply a breakdown the user edited by hand, outside any review.
///
/// This is the one write to a progress file that does not come from the model,
/// and it is a legal one: the file is the user's. The model finds out the same
/// way it finds out about an edit made in `$EDITOR` — its next `progress` call
/// is handed their version.
#[tauri::command]
pub fn write_plan(
    supervisor: State<'_, Arc<Supervisor>>,
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
#[tauri::command]
pub fn execute_plan_elsewhere(
    app: AppHandle,
    supervisor: State<'_, Arc<Supervisor>>,
    session: String,
) -> Result<OpenedSession, String> {
    let (handle, instructions) = crate::state::hand_off_plan(&supervisor, &session)?;
    let agent = supervisor.agent();
    let opened = OpenedSession::of(&handle, &agent);
    let emit: Arc<dyn Emit> = Arc::new(app);
    tauri::async_runtime::spawn(async move {
        if let Err(error) = run_turn(
            agent,
            handle.clone(),
            emit.clone(),
            Vec::new(),
            instructions,
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
) -> Result<OpenedSession, String> {
    // Canonicalize before anything else: the session id, the project data
    // directory and the launchpad's grouping all key on the path, and two
    // spellings of one folder would otherwise become two projects.
    let cwd = crate::paths::canonical_dir(Path::new(&path))
        .map_err(|error| format!("cannot open {path}: {error}"))?;
    let handle = supervisor
        .open_folder(&cwd, resume)
        .map_err(|error| error.to_string())?;
    eprintln!(
        "tcode-app: session {} open on {}",
        handle.id,
        handle.cwd.display()
    );
    Ok(OpenedSession::of(&handle, &supervisor.agent()))
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
    images: Option<Vec<ImageInput>>,
    plan: Option<bool>,
) -> Result<(), String> {
    let handle = supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    let agent = supervisor.agent();
    let vision = agent.model.snapshot().provider.supports_vision();
    let input = compose(
        text,
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
    let emit: Arc<dyn Emit> = Arc::new(app);
    tauri::async_runtime::spawn(async move {
        if let Err(error) = run_turn(agent, handle.clone(), emit.clone(), input, instructions).await
        {
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
#[tauri::command]
pub fn shown_file(
    supervisor: State<'_, Arc<Supervisor>>,
    session: String,
    path: String,
    binary: bool,
) -> Result<ShownFile, String> {
    let handle = supervisor
        .get(&session)
        .ok_or_else(|| format!("session '{session}' is not open"))?;
    load_shown(Path::new(&path), &handle.cwd, binary)
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
            body: format!("data:{};base64,{}", media_type(&file), {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD.encode(&raw)
            }),
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
#[tauri::command]
pub fn picker_state(
    supervisor: State<'_, Arc<Supervisor>>,
    session: String,
) -> Result<crate::picker::PickerState, String> {
    let (mode, staged) = match supervisor.get(&session) {
        Some(handle) => handle.mode(),
        // The launchpad has no session yet and still wants the model chip.
        None => (tcode_core::PermissionMode::default(), false),
    };
    let menus = supervisor.menus();
    let menus = menus.lock().map_err(|_| "picker state is poisoned")?;
    Ok(crate::picker::state_of(
        &menus,
        &supervisor.agent().model,
        mode,
        staged,
    ))
}

#[tauri::command]
pub fn choose_model(
    supervisor: State<'_, Arc<Supervisor>>,
    index: usize,
    effort: Option<String>,
) -> Result<(), String> {
    let menus = supervisor.menus();
    let mut menus = menus.lock().map_err(|_| "picker state is poisoned")?;
    // The agent's own cell, not a copy: every open session reads through it, so
    // one swap moves the whole window (see AGENTS.md rule 15 — mode is per
    // session, model is per process).
    crate::picker::choose_model(
        &mut menus,
        &supervisor.agent().model,
        index,
        effort.as_deref(),
    )
}

#[tauri::command]
pub fn choose_preset(
    supervisor: State<'_, Arc<Supervisor>>,
    key: String,
) -> Result<String, String> {
    let menus = supervisor.menus();
    let mut menus = menus.lock().map_err(|_| "picker state is poisoned")?;
    crate::picker::choose_preset(&mut menus, &key)
}

/// Pin one sub-agent or helper role to a model, to the main model, or off.
#[tauri::command]
pub fn pin_role(
    supervisor: State<'_, Arc<Supervisor>>,
    kind: String,
    pin: crate::picker::PinChoice,
) -> Result<String, String> {
    let menus = supervisor.menus();
    let mut menus = menus.lock().map_err(|_| "picker state is poisoned")?;
    crate::picker::pin_role(&mut menus, &kind, pin)
}

/// Capture the live line-up as `[presets.<name>]`.
#[tauri::command]
pub fn save_preset(supervisor: State<'_, Arc<Supervisor>>, name: String) -> Result<(), String> {
    let menus = supervisor.menus();
    let mut menus = menus.lock().map_err(|_| "picker state is poisoned")?;
    crate::picker::save_preset(&mut menus, &supervisor.agent().model, name.trim())
}

/// Choose the permission mode for one conversation.
///
/// Per session, unlike the model: the mode is what *this* conversation is
/// allowed to do without asking, and two folders open side by side routinely
/// deserve different answers. It is still remembered as the default for new
/// sessions, which is the part `[tcode_state]` holds.
#[tauri::command]
pub fn choose_mode(
    supervisor: State<'_, Arc<Supervisor>>,
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
#[tauri::command]
pub fn respond_approval(
    supervisor: State<'_, Arc<Supervisor>>,
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
#[tauri::command]
pub fn interrupt(supervisor: State<'_, Arc<Supervisor>>, session: String) -> Result<(), String> {
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
}
