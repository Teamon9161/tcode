#[cfg(test)]
mod harness;

mod bake;
mod commands;
mod draw;
mod input;
mod replay;
mod rewind;
mod turn;
mod views;

use bake::*;
use draw::*;
use input::*;
use rewind::*;
use views::*;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use futures::StreamExt;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthStr;

use tcode_core::blobs::approx_tokens;
use tcode_core::commands::{
    CommandCtx, CommandEffect, CommandMessage, CommandRegistry, MessageKind,
};
use tcode_core::{
    Agent, AgentError, AgentEvent, Approval, ApprovalDecision, Approver, BatchApproval, BatchAsk,
    ContentBlock, FolderTrust, PendingMessage, ReferenceCandidate, Session, Usage,
};

use tcode_importers::{
    import_external_session, list_external_sessions, ExternalSessionInfo, ExternalSource,
};

use crate::approval::{Dialog, DialogResult};
use crate::composer::{
    editor_layout, format_bytes, ghost_visual_lines, input_spans, move_editor_visual,
    paste_should_fold, reference_basename, reference_basename_path, reference_boundary,
    reference_candidate_path, reference_marker, reference_match_order, reference_score,
    reference_token_char, VisualMove,
};
use crate::editor::{Editor, Position};
use crate::live_panel::{self, MainAgent, PanelTarget, ProgressPhase, UiTaskRun};
use crate::model_picker::{self, AgentMenu, ModelMenu};
use crate::overlay::{ApprovalReply, Flow, Overlay, OverlayAction, OverlayCtx};
use crate::render::{
    batch_item_style, shorten_summary_path, CallRoute, HeaderTone, RenderRegistry,
};
use crate::resume;
use crate::surface::Surface;
use crate::transcript::Transcript;
use crate::usage::{
    context_progress_line, rate_limit_line, token_count, turn_summary_line, TurnMeter,
};
use crate::view::{BakeCtx, SessionView};
use crate::view_picker::{self, ViewId};
use crate::voice::{Voice, VoiceEvent, VoiceOutcome};
use crate::{diff, markdown, theme, EnvironmentFn, OpeningContextFn};

type Term = Terminal<Surface>;

/// Lines scrolled per mouse-wheel event.
const WHEEL_STEP: usize = 3;
/// Visible rows of an expanded tool-output region.
const OUTPUT_VIEW_ROWS: usize = 12;

/// Second Esc within this window (while idle) opens the rewind picker.
const DOUBLE_ESC: Duration = Duration::from_millis(1200);

/// Opens a note the human slipped to the model mid-turn (approval comment,
/// `/note`), distinguishing it from a full user turn under the same rail.
/// The note's own text already says what it is about — see
/// `Agent`'s approval notes — so the label stays a bare marker.
const NOTE_LABEL: &str = "Note: ";

/// Braille spinner drawn in the accent colour: visibly alive without the
/// bulk or flicker of the legacy sparkle animation.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const LOGO: [&str; 2] = ["▀█▀ █▀▀ █▀█ █▀▄ █▀▀", " █  █▄▄ █▄█ █▄▀ ██▄"];

/// One of these shows per launch, picked at random: a discovery channel
/// for features nobody reads /help for. Every entry must describe real,
/// current behaviour — stale tips are worse than none.
const TIPS: [&str; 11] = [
    "/voice dictates into the prompt: hold the key, speak, let go — enter still sends it",
    "shift+tab cycles permission modes",
    "esc esc rewinds the conversation · ctrl+r also restores files",
    "ctrl+c stops the turn and sends queued prompts right away — it never exits",
    "type while a turn runs: the prompt queues and esc takes it back",
    "→ accepts the dim suggestion in the input box",
    "/model switches model mid-session · /agents pins sub-agent models",
    "/resume picks up an earlier session · /export saves the transcript",
    "/compact squeezes a long conversation back into budget",
    "click a task to expand it · ctrl+click its title to open its trace",
    "/note slips the model an aside without starting a turn",
];

/// Commands whose substance drives frontend-owned objects (key table, model
/// picker, provider wizard). Everything else lives in the shared
/// `CommandRegistry` in tcode-core.
const UI_COMMANDS: [(&str, &str); 6] = [
    ("/help", "show keys and commands"),
    ("/views", "switch concurrent sessions"),
    (
        "/voice",
        "push-to-talk dictation · model picks a recogniser · words biases it · key <name> rebinds",
    ),
    ("/model", "switch model · adjust reasoning effort"),
    (
        "/agents",
        "choose models for sub-agents and Auto Mode safety",
    ),
    ("/provider", "configure or switch provider"),
];

/// One call under review. A combined review carries several of these; every
/// other prompt carries exactly one.
pub struct AskCall {
    pub tool: String,
    pub summary: String,
    pub descriptor: String,
    pub is_edit: bool,
    pub allows_project: bool,
    pub input: serde_json::Value,
}

pub struct AskMsg {
    /// The batch header for a combined review, as the agent loop names it.
    /// Empty for a single prompt.
    pub label: String,
    pub calls: Vec<AskCall>,
    pub reply: ApprovalReply,
}

impl AskMsg {
    /// The one call a single prompt is about.
    pub fn only(&self) -> &AskCall {
        &self.calls[0]
    }

    fn is_batch(&self) -> bool {
        self.calls.len() > 1
    }
}

/// Approver that forwards prompts into the UI loop.
pub struct ChannelApprover {
    pub tx: mpsc::Sender<AskMsg>,
}

#[async_trait]
impl Approver for ChannelApprover {
    async fn ask(
        &self,
        tool: &str,
        summary: &str,
        descriptor: &str,
        is_edit: bool,
        allows_project: bool,
        input: &serde_json::Value,
    ) -> Approval {
        let (reply, rx) = oneshot::channel();
        let msg = AskMsg {
            label: String::new(),
            calls: vec![AskCall {
                tool: tool.to_string(),
                summary: summary.to_string(),
                descriptor: descriptor.to_string(),
                is_edit,
                allows_project,
                input: input.clone(),
            }],
            reply: ApprovalReply::One(reply),
        };
        if self.tx.send(msg).await.is_err() {
            return Approval::simple(ApprovalDecision::No, Some("UI unavailable".into()));
        }
        rx.await
            .unwrap_or_else(|_| Approval::simple(ApprovalDecision::No, None))
    }

    async fn ask_batch(&self, label: &str, calls: &[BatchAsk<'_>]) -> BatchApproval {
        let (reply, rx) = oneshot::channel();
        let msg = AskMsg {
            label: label.to_string(),
            calls: calls
                .iter()
                .map(|call| AskCall {
                    tool: call.tool.to_string(),
                    summary: call.summary.to_string(),
                    descriptor: call.descriptor.to_string(),
                    is_edit: call.is_edit,
                    allows_project: call.allows_project,
                    input: call.input.clone(),
                })
                .collect(),
            reply: ApprovalReply::Batch(reply),
        };
        // A review that never reaches the UI must not decline the batch on the
        // user's behalf: the per-call flow still has its own prompt for each
        // call, and that path already knows how to fail closed.
        if self.tx.send(msg).await.is_err() {
            return BatchApproval::Individually;
        }
        rx.await.unwrap_or(BatchApproval::Individually)
    }
}

enum Phase {
    Idle,
    Running {
        handle: JoinHandle<(Session, Result<(), AgentError>)>,
        cancel: CancellationToken,
        started: Instant,
    },
}

/// Batch items are indented under their shared header without a tree glyph.
const BATCH_ITEM_INDENT: &str = "    ";
/// A sub-agent's live action belongs to its task item, never to the batch
/// header, so it has one extra visible tree level.
const TASK_STATUS_INDENT: &str = "      └ ";

pub struct App {
    agent: Arc<Agent>,
    opening_context: OpeningContextFn,
    environment: EnvironmentFn,
    registry: CommandRegistry,
    /// The same discovery the `skill` tool uses, handed in by the caller so a
    /// `/name` line that misses both `UI_COMMANDS` and the registry can fall
    /// back to loading a skill directly (see `run_slash`) without a second
    /// filesystem scan.
    skills: Vec<tcode_tools::Skill>,
    session: Option<Session>,
    /// The TUI retains this while a turn owns `session`, so live tool calls
    /// can still render in-project paths relatively.
    cwd: PathBuf,
    /// Session-private scratch root mirrored for UI-owned temporary files while
    /// a running turn owns the `Session`.
    scratch_dir: PathBuf,
    /// Per-tool renderers + display names, built from the agent's live tools
    /// so rendering behaviour (quiet output, routes, diffs) can never drift
    /// from core's tool contracts.
    renderers: RenderRegistry,
    terminal: Term,
    transcript: Transcript,
    md: markdown::Renderer,

    phase: Phase,
    events_rx: Option<mpsc::Receiver<AgentEvent>>,
    external_import: Option<
        JoinHandle<(
            ExternalSource,
            Result<tcode_core::Resumed, tcode_core::store::StoreError>,
        )>,
    >,
    ask_rx: mpsc::Receiver<AskMsg>,
    approver: Arc<ChannelApprover>,

    editor: Editor,
    /// Ignored-aware project paths used only for local `@` completion. The
    /// core agent resolves selected markers at send time, so an old index can
    /// never read a stale or outside-project path.
    reference_index: Vec<ReferenceCandidate>,
    reference_tx: mpsc::Sender<(PathBuf, Vec<ReferenceCandidate>)>,
    reference_rx: mpsc::Receiver<(PathBuf, Vec<ReferenceCandidate>)>,
    /// Prompts submitted while a turn was running. The agent loop takes them at
    /// its next safe boundary; until then they show, dimmed, above the input.
    pending: tcode_core::PendingInput,
    /// Fires when a monitor produces an event or exits; re-fetched from the
    /// session each loop iteration so a rebound registry can't leave it stale.
    monitor_signal: Arc<tokio::sync::Notify>,
    /// When to wake an idle session for undelivered monitor events. `None`
    /// while there is nothing to deliver; recomputed from the registry on
    /// signal and at turn end.
    monitor_deadline: Option<tokio::time::Instant>,
    /// A permission-mode switch staged while a turn runs (the running turn owns
    /// the `Session`). The agent loop commits it at the next batch boundary and
    /// reports back with `ModeChanged`; until then the status line shows the
    /// staged target with a pending marker.
    pending_mode: tcode_core::PendingMode,
    /// The mode currently in effect, as the frontend knows it. Kept in step
    /// with `Session::mode`: updated directly when idle, and on `ModeChanged`
    /// when a running turn commits a staged switch. Cycling reads
    /// staged-else-committed so repeated presses collapse to the final target.
    committed_mode: tcode_core::PermissionMode,
    /// The idle guess at the next instruction: ghost text in an empty input,
    /// accepted with →. It belongs to the turn that produced it and survives
    /// typing — the ghost hides while the input has text and comes back when it
    /// is empty again, without a second request. Whether to ask for one at all
    /// is `Session::suggestions` (`/suggest`).
    suggestion: Option<String>,
    suggest_cancel: Option<CancellationToken>,
    /// Which guess is current. A reply carrying an older generation is a guess
    /// about a conversation that no longer exists, and is dropped.
    suggest_gen: u64,
    suggest_tx: mpsc::Sender<(u64, Option<String>)>,
    suggest_rx: mpsc::Receiver<(u64, Option<String>)>,
    /// Push-to-talk dictation (`/voice`). Off unless asked for, and its
    /// transcripts only ever reach the editor.
    voice: Voice,
    voice_rx: mpsc::Receiver<VoiceEvent>,
    /// `/voice keys`: echo every key event to the transcript. The only way to
    /// tell "tcode ignores this key" from "this key never reached tcode".
    voice_probe: bool,
    /// How to fetch the sidecar on a machine that has never had it. Injected,
    /// so the TUI holds no release URLs.
    voice_install: crate::VoiceInstall,
    attachments: Vec<Attachment>,
    /// Monotonic id for the next attachment; keeps inline tokens unique within
    /// a draft. Reset to 1 once the draft is sent or cleared.
    next_attachment_id: u32,
    /// A long-lived system clipboard. Kept alive for the whole session: on
    /// X11, arboard prints a warning to the terminal (corrupting the alternate
    /// screen) if a `Clipboard` is dropped within 100ms of a write, so a fresh
    /// one per copy is not an option. `None` when no local clipboard exists
    /// (headless/SSH) — copy then falls back to OSC 52.
    clipboard: Option<arboard::Clipboard>,
    input_hitbox: Option<InputHitbox>,
    /// Clickable regions for the mode and model values in the idle status row.
    /// They are computed from the final rendered layout every frame.
    status_hitboxes: Option<StatusHitboxes>,
    /// Centered control at the transcript's bottom edge while history is in view.
    /// Captured from the final frame so a click cannot drift from its label.
    jump_to_bottom_hitbox: Option<Rect>,
    /// The jump control uses the same restrained hover lift as other TUI actions.
    jump_to_bottom_hover: bool,
    /// Where the persistent agent tree rendered this frame and which action
    /// target each row owns.
    live_panel_hitbox: Option<PanelHitbox>,
    /// Tree row currently under the pointer; rendered with the shared hover
    /// background so its click behavior is discoverable.
    live_panel_hover: Option<PanelTarget>,
    /// Which clickable status value the pointer currently rests over.
    status_hover: Option<StatusHover>,
    input_mouse_active: bool,
    /// Whether the current prompt press has actually dragged. A plain click
    /// (no Drag event) must not copy, even if the release cell differs slightly.
    input_dragged: bool,
    /// Armed while a transcript selection drag rests at a view edge: `(toward
    /// older, x, y)`. A timer then scrolls and extends the selection, since a
    /// pointer held still at the edge emits no further mouse events. Cleared on
    /// release or when the drag returns inside the view.
    drag_scroll: Option<(bool, u16, u16)>,
    /// The single modal that can own the bottom panel: any picker, or the
    /// approval dialog. They are mutually exclusive, so they share one slot —
    /// see `overlay.rs` for why that matters.
    overlay: Option<Overlay>,
    /// A change diff baked into the transcript while its approval dialog is
    /// open (so the full code is scrollable in the record, not cramped in
    /// the dialog). Holds the block-count mark to retract to on decline or
    /// when a batch supersedes it; on approval it tells the upcoming
    /// `ToolStart` to skip re-baking the diff.
    change_prebake: Option<usize>,
    rewind_nav: Option<RewindNav>,
    menu: ModelMenu,
    agents: AgentMenu,
    /// `/provider`'s two effects: read the user's config, and persist what
    /// the form produced. The binary owns both.
    provider_setup: crate::ProviderSetup,
    /// Mirror of `Session::dogfood` for the status line: a running turn owns
    /// the session, so the hint cannot read it directly.
    dogfood: bool,
    pending_tool: Option<PendingCall>,
    /// Entries belonging to a concurrent group, completed in model-call
    /// order. Keeping them queued lets each result retain its own input.
    pending_batch: VecDeque<PendingCall>,
    progress: Vec<ProgressPhase>,
    /// `task` sub-agent runs of this conversation, fed by `TaskRun*` events.
    /// Running ones show live in the panel above the input; finished ones
    /// stay as the registry the trace viewer will list.
    task_runs: Vec<UiTaskRun>,
    /// Root containing `tN.jsonl` trace files for this selected session. It is
    /// retained while a turn owns `Session`, so task cards can open their
    /// trace during live work.
    task_trace_root: Option<PathBuf>,
    active_view: ViewId,
    trace_view: Option<TraceView>,
    last_esc: Option<Instant>,
    popup_index: usize,
    /// Tab accepted this exact `@` marker. Keep its completion closed until the
    /// user changes the draft, rather than immediately matching it again.
    dismissed_reference: Option<Position>,

    // Live streaming state: kept as a replace-in-place transcript block until
    // the provider finishes this assistant message.
    live_text: String,
    /// A visible tool result must not run into the model's next visible response.
    /// A following tool header or user message clears this before it is rendered.
    space_before_response: bool,
    /// Index of the still-streaming assistant block inside the transcript.
    /// While set, incoming deltas replace that block in place; finalization just
    /// drops the marker so the block becomes ordinary scrollback.
    live_block: Option<usize>,
    thinking_chars: usize,
    thinking_text: String,
    thinking_since: Option<Instant>,
    show_reasoning: bool,
    /// Token accounting for the status row and the turn receipt. These
    /// figures move together, so they live behind one type — see `usage.rs`.
    meter: TurnMeter,
    state_label: String,
    mode_label: String,
    spinner: usize,
    /// Monotonic 100ms shimmer frame for the main running status and in-flight
    /// tool headers. Unlike `spinner` it never wraps, so the sweep stays continuous.
    anim_frame: usize,
    should_exit: bool,
    /// Transient feedback ("copied 3 lines") shown in the hint row.
    notice: Option<(String, Instant)>,
    /// Active retry backoff: countdown deadline plus attempt/max, rendered red
    /// in the status line until the next attempt begins.
    retry_wait: Option<RetryWait>,
}

#[derive(Clone)]
struct RetryWait {
    until: Instant,
    attempt: u32,
    max: u32,
}

impl App {
    pub fn new(
        agent: Arc<Agent>,
        session: Session,
        config: crate::TuiConfig,
    ) -> anyhow::Result<Self> {
        Self::on_surface(agent, session, config, Surface::live())
    }

    /// Build the app painting onto `surface`. Production passes the terminal;
    /// a test passes an in-memory buffer so it can read frames back.
    fn on_surface(
        agent: Arc<Agent>,
        mut session: Session,
        config: crate::TuiConfig,
        surface: Surface,
    ) -> anyhow::Result<Self> {
        let crate::TuiConfig {
            menu,
            agents,
            provider_setup,
            opening_context,
            environment,
            show_reasoning,
            skills,
            voice: voice_config,
            voice_install,
        } = config;
        let (ask_tx, ask_rx) = mpsc::channel(4);
        let (suggest_tx, suggest_rx) = mpsc::channel(1);
        let (reference_tx, reference_rx) = mpsc::channel(1);
        // 16 is generous for a channel whose busiest traffic is a level meter
        // at a few frames a second.
        let (voice_tx, voice_rx) = mpsc::channel(16);
        let voice = Voice::new(voice_config, voice_tx, Voice::default_factory());
        let mode_label = session.mode.label().to_string();
        let committed_mode = session.mode;
        let pending_mode = session.pending_mode.clone();
        let session_dogfood = session.dogfood();
        let cwd = session.tool_ctx.cwd.clone();
        let scratch_dir = session.tool_ctx.scratch_dir.clone();
        let task_trace_root = task_trace_root(&session);
        let overlay = (!session.folder_trust_known())
            .then(|| Overlay::FolderTrust(crate::folder_trust_picker::Picker::new(&cwd)));
        let task_runs = discover_task_runs(task_trace_root.as_deref());
        let context_estimated = session.last_prompt_tokens == 0 && !session.ledger.is_empty();
        let context_tokens = if context_estimated {
            agent.estimate_context_tokens(&session)
        } else {
            session.last_prompt_tokens
        };
        // Keep the agent's automatic-compaction guard and status block in
        // step with the UI even when tcode was launched with `--resume`.
        session.last_prompt_tokens = context_tokens;
        let renderers = RenderRegistry::from_tools(&agent.tools);
        let mut terminal = Terminal::new(surface)?;
        // EnterAlternateScreen does not guarantee a blank physical buffer: on
        // some terminals a prior tcode frame survives a leave/re-enter cycle.
        // Ratatui's first diff sees a blank logical buffer and would otherwise
        // never erase those untouched cells.
        terminal.clear()?;
        let transcript = Transcript::new(terminal.size().map(|s| s.width).unwrap_or(80));
        // The running turn owns the Session; this handle is how input typed
        // meanwhile still reaches it.
        let pending = session.pending.clone();
        let monitor_signal = session
            .tool_ctx
            .background
            .lock()
            .expect("background lock")
            .monitor_signal();
        Ok(Self {
            agent,
            opening_context,
            environment,
            registry: CommandRegistry::builtin(),
            skills,
            session: Some(session),
            pending,
            monitor_signal,
            monitor_deadline: None,
            pending_mode,
            committed_mode,
            cwd,
            scratch_dir,
            renderers,
            terminal,
            transcript,
            md: markdown::Renderer::default(),
            phase: Phase::Idle,
            events_rx: None,
            external_import: None,
            ask_rx,
            approver: Arc::new(ChannelApprover { tx: ask_tx }),
            editor: Editor::new(),
            reference_index: Vec::new(),
            reference_tx,
            reference_rx,
            suggestion: None,
            suggest_cancel: None,
            suggest_gen: 0,
            suggest_tx,
            suggest_rx,
            voice,
            voice_rx,
            voice_probe: false,
            voice_install,
            attachments: Vec::new(),
            next_attachment_id: 1,
            clipboard: arboard::Clipboard::new().ok(),
            input_hitbox: None,
            status_hitboxes: None,
            jump_to_bottom_hitbox: None,
            jump_to_bottom_hover: false,
            live_panel_hitbox: None,
            live_panel_hover: None,
            status_hover: None,
            input_mouse_active: false,
            input_dragged: false,
            drag_scroll: None,
            overlay,
            change_prebake: None,
            rewind_nav: None,
            menu,
            agents,
            provider_setup,
            dogfood: session_dogfood,
            pending_tool: None,
            pending_batch: VecDeque::new(),
            progress: Vec::new(),
            task_runs,
            task_trace_root,
            active_view: ViewId::Main,
            trace_view: None,
            last_esc: None,
            popup_index: 0,
            dismissed_reference: None,
            live_text: String::new(),
            space_before_response: false,
            live_block: None,
            thinking_chars: 0,
            thinking_text: String::new(),
            thinking_since: None,
            show_reasoning,
            meter: TurnMeter::new(context_tokens, context_estimated),
            state_label: String::new(),
            retry_wait: None,
            mode_label,
            spinner: 0,
            anim_frame: 0,
            should_exit: false,
            notice: None,
        })
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        let banner = self.banner();
        self.bake(banner);
        self.bake_transcript();
        self.refresh_reference_index();
        if self.voice.wants_on() {
            self.set_voice(true);
        }
        let mut term_events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Drives selection auto-scroll while the pointer is held at a view edge.
        // Its select arm is gated on `drag_scroll`, so it only wakes the loop
        // while a drag is actually parked at an edge — never when idle.
        let mut drag_tick = tokio::time::interval(Duration::from_millis(50));
        drag_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Drives the shimmer on the main running status and in-flight call
        // headers. Its select arm is gated on `shimmer_active`, so the finer
        // cadence only runs while a turn is actively executing.
        let mut anim_tick = tokio::time::interval(Duration::from_millis(100));
        anim_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Watches an open take: on terminals with no usable key-up, the end of
        // a hold *is* the gap after the last auto-repeat, so it has to be
        // sampled faster than that gap. Gated on recording, so it never wakes
        // the loop otherwise.
        let mut voice_tick = tokio::time::interval(Duration::from_millis(60));
        voice_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        while !self.should_exit {
            self.redraw()?;
            // Re-fetch when idle so a rebound registry (resume/import) can't
            // leave a stale handle; while a turn owns the session, the cached
            // clone stays valid because ToolCtx is never rebound mid-turn.
            if let Some(session) = self.session.as_ref() {
                self.monitor_signal = session
                    .tool_ctx
                    .background
                    .lock()
                    .expect("background lock")
                    .monitor_signal();
            }
            let monitor_signal = self.monitor_signal.clone();
            let monitor_armed =
                self.monitor_deadline.is_some() && matches!(self.phase, Phase::Idle);
            let monitor_deadline = self
                .monitor_deadline
                .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600));
            tokio::select! {
                ev = term_events.next() => {
                    match ev {
                        Some(Ok(ev)) => self.on_term_event(ev),
                        _ => break,
                    }
                }
                Some(ev) = recv_opt(&mut self.events_rx) => {
                    self.on_agent_event(ev);
                    // Drain whatever is already queued to batch redraws.
                    self.drain_agent_events();
                }
                Some((generation, suggestion)) = self.suggest_rx.recv() => {
                    // Keep it even if the user is mid-word: the ghost simply
                    // stays hidden until the input is empty again. Only a guess
                    // about a conversation that has moved on is discarded.
                    if generation == self.suggest_gen {
                        self.suggestion = suggestion;
                    }
                }
                Some((cwd, index)) = self.reference_rx.recv() => {
                    if cwd == self.cwd {
                        self.reference_index = index;
                        self.popup_index = 0;
                    }
                }
                Some(ask) = self.ask_rx.recv() => {
                    self.open_review(ask);
                }
                Some(ev) = self.voice_rx.recv() => {
                    self.on_voice_event(ev);
                }
                done = join_phase(&mut self.phase) => {
                    self.on_turn_done(done);
                }
                _ = monitor_signal.notified() => {
                    self.refresh_monitor_deadline();
                }
                _ = tokio::time::sleep_until(monitor_deadline), if monitor_armed => {
                    self.on_monitor_deadline();
                }
                done = join_external_import(&mut self.external_import) => {
                    self.on_external_import_done(done);
                }
                _ = tick.tick() => {
                    if matches!(self.phase, Phase::Running { .. }) || self.external_import.is_some() {
                        self.spinner = (self.spinner + 1) % SPINNER.len();
                    }
                }
                _ = voice_tick.tick(), if self.voice.is_recording() => {
                    let outcome = self.voice.tick();
                    self.apply_voice(outcome);
                }
                _ = drag_tick.tick(), if self.drag_scroll.is_some() => {
                    self.drag_autoscroll_step();
                }
                _ = anim_tick.tick(), if self.shimmer_active() => {
                    self.anim_frame = self.anim_frame.wrapping_add(1);
                    // Every ~1.5s, parallel batches show their next call.
                    if self.anim_frame.is_multiple_of(15) {
                        self.rotate_task_calls();
                    }
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ turn

    // ------------------------------------------------------------ keys

    fn overlay_ctx(&self) -> OverlayCtx<'_> {
        OverlayCtx {
            menu: &self.menu,
            agents: &self.agents,
            width: area_width(&self.terminal),
            height: area_height(&self.terminal),
        }
    }

    /// Hands one event to the active overlay and applies what it asks for.
    /// The overlay is moved out for the duration so it can be mutated while
    /// `OverlayCtx` still borrows the rest of `App`.
    fn drive_overlay(&mut self, event: impl FnOnce(&mut Overlay, &OverlayCtx) -> Flow) {
        let Some(mut overlay) = self.overlay.take() else {
            return;
        };
        let flow = {
            let ctx = self.overlay_ctx();
            event(&mut overlay, &ctx)
        };
        self.overlay = Some(overlay);
        self.on_overlay_flow(flow);
    }

    /// Open the review pane for a pending authorization: a question form, a
    /// plan, or one or more proposed changes.
    fn open_review(&mut self, ask: AskMsg) {
        self.meter.pause_for_user();
        let dialog = if ask.only().tool == "ask_user" {
            Dialog::questions(ask.only().summary.clone(), &ask.only().input)
        } else if ask.only().tool == "exit_plan" {
            // The plan is the review surface, but it lives inside the
            // pane now (block-navigable, commentable) rather than in
            // the transcript. Split it into blocks and pre-render each
            // so the hot key path never re-parses markdown. On
            // approval the tool runs and its ToolStart bakes the plan
            // into the transcript; on decline `bake_approval_record`
            // bakes it — either way the transcript record matches
            // replay's ExitPlanRenderer output.
            self.bake_live_text();
            self.finish_thinking();
            let source = ask.only().input["plan"].as_str().unwrap_or("").trim();
            let blocks = markdown::split_blocks(source)
                .into_iter()
                .map(|block| {
                    let document = self.md.parse(&block);
                    (block, document)
                })
                .collect();
            Dialog::plan(ask.only().summary.clone(), ask.only().input.clone(), blocks)
        } else {
            self.bake_proposed_changes(&ask)
        };
        self.transcript.clear_hover();
        self.overlay = Some(Overlay::approval(dialog, ask.reply));
    }

    /// Bake every proposed change into the transcript and build the dialog that
    /// asks about them. A change proposal (edit/write) or a long/multi-line
    /// shell command goes into the transcript in full — scrollable as part of
    /// the record — so the reviewer reads the whole thing there rather than in
    /// the cramped dialog. A combined review bakes all of its changes under one
    /// mark, so declining or taking the review apart retracts them together.
    /// On approval the upcoming ToolStart skips re-baking (see
    /// `change_prebake`).
    fn bake_proposed_changes(&mut self, ask: &AskMsg) -> Dialog {
        // The summary is recomputed from name+input through the renderer, not
        // taken as the raw string core put in `summary`: a long/multi-line
        // shell command needs the same capped preview ToolStart uses (see
        // `on_tool_start`), or its full, possibly multi-line text fills the
        // compact dialog with no way to scroll past it to the choices.
        let rendered: Vec<(String, Vec<Line<'static>>)> = ask
            .calls
            .iter()
            .map(|call| {
                let renderer = self.renderers.get(&call.tool);
                let summary = self.display_summary(&renderer.header(
                    &call.tool,
                    &call.input,
                    Some(&self.cwd),
                ));
                (summary, renderer.approval_detail(&call.input))
            })
            .collect();

        if rendered.iter().any(|(_, change)| !change.is_empty()) {
            self.bake_live_text();
            self.finish_thinking();
            self.change_prebake = Some(self.transcript.block_count());
            for (summary, change) in &rendered {
                if change.is_empty() {
                    continue;
                }
                let mut spans: Vec<Span> = self.colored_tool_summary(summary);
                spans.insert(0, Span::styled("● ", theme::accent()));
                let mut lines = vec![Line::default(), Line::from(spans)];
                lines.extend(change.clone());
                lines.push(Line::default());
                self.bake(lines);
            }
        }

        // The diffs live in the transcript; the dialog carries only the choices.
        if ask.is_batch() {
            return Dialog::batch(
                ask.label.clone(),
                ask.calls.len(),
                ask.calls.iter().all(|call| call.is_edit),
            );
        }
        let call = ask.only();
        let summary = rendered[0].0.clone();
        Dialog::new(
            summary.clone(),
            call.descriptor.clone(),
            summary,
            call.is_edit,
            call.allows_project,
        )
    }

    fn on_overlay_flow(&mut self, flow: Flow) {
        match flow {
            Flow::Stay => {}
            Flow::Close => self.overlay = None,
            Flow::ActInPlace(action) => self.apply_overlay_action(action),
            // The approval reply channel lives in the overlay itself, so
            // finishing an approval consumes the overlay rather than routing
            // through `apply_overlay_action`.
            Flow::Act(OverlayAction::Approved(approval)) => {
                if let Some(Overlay::Approval(dialog, reply)) = self.overlay.take() {
                    self.finish_approval(*dialog, reply, approval);
                }
            }
            Flow::Act(OverlayAction::ReviewIndividually) => {
                if let Some(Overlay::Approval(_, reply)) = self.overlay.take() {
                    self.review_individually(reply);
                }
            }
            Flow::Act(action) => {
                self.overlay = None;
                self.apply_overlay_action(action);
            }
        }
    }

    fn apply_overlay_action(&mut self, action: OverlayAction) {
        match action {
            OverlayAction::OpenView(id) => self.open_view(id),
            OverlayAction::ResumeSession(id) => self.resume_session(&id),
            OverlayAction::ShowImportSources => {
                self.overlay = Some(Overlay::Resume(resume::Picker::sources()))
            }
            OverlayAction::OpenExternalSource(source) => self.open_external_resume_picker(source),
            OverlayAction::ImportExternal(external) => self.import_external_session(external),
            OverlayAction::ApplyModel { index, effort } => self.apply_model(index, effort),
            OverlayAction::ApplySetup(done) => self.apply_setup(done),
            OverlayAction::SetMode(mode) => self.set_mode(mode),
            OverlayAction::SetVoiceModel(name) => self.apply_voice_model(name),
            OverlayAction::FolderTrust(choice) => self.apply_folder_trust_choice(choice),
            OverlayAction::ApplyAgentModel { kind, choice } => {
                self.apply_agent_model(&kind, choice)
            }
            // Suspends the terminal, so it cannot run inside the dialog's key
            // handler; the dialog is still open and takes the revision.
            OverlayAction::EditPlan => self.edit_plan_externally(),
            OverlayAction::Approved(_) | OverlayAction::ReviewIndividually => {
                unreachable!("handled by on_overlay_flow")
            }
        }
    }

    /// The reviewer took a combined review apart. Retract every diff it baked —
    /// the per-call flow re-proposes them one at a time — and hand the batch
    /// back to the agent loop unanswered.
    fn review_individually(&mut self, reply: ApprovalReply) {
        if let Some(mark) = self.change_prebake.take() {
            self.transcript.truncate_blocks(mark);
        }
        if let ApprovalReply::Batch(tx) = reply {
            let _ = tx.send(BatchApproval::Individually);
        }
        self.meter.resume_from_user();
    }

    fn finish_approval(&mut self, dialog: Dialog, reply: ApprovalReply, approval: Approval) {
        // A plan approval chose the mode execution runs under. The agent loop
        // applies it to the Session; mirror it into the frontend's own view so
        // the status line updates at once.
        if let Some(mode) = approval.set_mode {
            self.committed_mode = mode;
            self.mode_label = mode.label().to_string();
            self.pending_mode.clear();
        }
        self.bake_approval_record(&dialog, &approval);
        match reply {
            ApprovalReply::One(tx) => {
                let _ = tx.send(approval);
            }
            ApprovalReply::Batch(tx) => {
                let _ = tx.send(BatchApproval::All(approval));
            }
        }
        self.meter.resume_from_user();
    }

    fn on_dialog_mouse(&mut self, mouse: MouseEvent) {
        // A proposed edit/write is deliberately baked into the transcript in
        // full before its compact approval dialog opens. The dialog must not
        // trap the wheel, or a long diff becomes visible only through the few
        // transcript rows left above the panel with no way to inspect it.
        let plan_owns_wheel = self
            .overlay
            .as_ref()
            .and_then(Overlay::as_dialog)
            .is_some_and(Dialog::owns_wheel);
        if !plan_owns_wheel {
            match mouse.kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    self.transcript.wheel(
                        mouse.column,
                        mouse.row,
                        mouse.kind == MouseEventKind::ScrollUp,
                        WHEEL_STEP,
                    );
                    return;
                }
                _ => {}
            }
        }

        let width = area_width(&self.terminal);
        let budget = area_height(&self.terminal).saturating_sub(6);
        // Ask the layout where the panel is rather than reconstructing it: a
        // click must land on the row that was actually painted.
        let Some(row) = self
            .overlay_geometry()
            .and_then(|geometry| geometry.content_row(mouse.row))
        else {
            return;
        };
        let approval = {
            let Some(dialog) = self.overlay.as_mut().and_then(Overlay::as_dialog_mut) else {
                return;
            };
            let col = mouse.column.saturating_sub(1) as usize;
            if !dialog.is_plan() {
                if dialog.is_question() {
                    match mouse.kind {
                        MouseEventKind::Moved => {
                            dialog.question_mouse_moved(row);
                        }
                        MouseEventKind::Down(MouseButton::Left) => {
                            if !dialog.question_mouse_down(row) {
                                dialog.note_mouse_down(row, col, width);
                            }
                        }
                        _ => {}
                    }
                    None
                } else {
                    match mouse.kind {
                        MouseEventKind::Moved => {
                            dialog.approval_mouse_moved(row);
                            None
                        }
                        MouseEventKind::Down(MouseButton::Left) => {
                            if let Some(approval) = dialog.approval_mouse_down(row) {
                                Some(approval)
                            } else {
                                dialog.note_mouse_down(row, col, width);
                                None
                            }
                        }
                        _ => None,
                    }
                }
            } else {
                match mouse.kind {
                    // The plan pane owns its own viewport, including while its feedback
                    // editor has focus. Do not let the modal trap the reviewer at the
                    // current block while they are composing the keep-planning reason.
                    MouseEventKind::ScrollUp => dialog.plan_mouse_wheel(true, width, budget),
                    MouseEventKind::ScrollDown => dialog.plan_mouse_wheel(false, width, budget),
                    MouseEventKind::Down(MouseButton::Left) => {
                        dialog.plan_mouse_down(row, col, width, budget)
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        dialog.plan_mouse_drag(row, col, width, budget)
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        dialog.plan_mouse_up(row, col, width, budget)
                    }
                    _ => {}
                }
                None
            }
        };
        match approval {
            Some(DialogResult::Done(approval)) => {
                self.on_overlay_flow(Flow::Act(OverlayAction::Approved(approval)))
            }
            Some(DialogResult::Individually) => {
                self.on_overlay_flow(Flow::Act(OverlayAction::ReviewIndividually))
            }
            Some(DialogResult::Pending | DialogResult::EditPlan) | None => {}
        }
    }

    /// Mouse input while an overlay is open. Returns whether the overlay
    /// consumed it; only the keyboard-driven resume picker declines, so the
    /// wheel still reaches the transcript behind it.
    fn on_overlay_mouse(&mut self, mouse: MouseEvent) -> bool {
        let Some(overlay) = self.overlay.as_ref() else {
            return false;
        };
        if !overlay.owns_mouse() {
            return false;
        }
        // The approval dialog has its own richer mouse behaviour (plan panes,
        // note carets, drag selection), driven by geometry it owns.
        if overlay.as_dialog().is_some() {
            self.on_dialog_mouse(mouse);
            return true;
        }
        // Ask the layout where the panel is rather than reconstructing it: a
        // click must land on the row that was actually painted.
        let geometry = self.overlay_geometry().expect("overlay present");
        let Some(row) = geometry.content_row(mouse.row) else {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                let flow = self
                    .overlay
                    .as_ref()
                    .expect("overlay present")
                    .on_click_away();
                self.on_overlay_flow(flow);
            } else if matches!(mouse.kind, MouseEventKind::Moved) {
                self.drive_overlay(|overlay, ctx| {
                    overlay.set_hovered_row(None, ctx);
                    Flow::Stay
                });
            }
            return true;
        };
        if matches!(mouse.kind, MouseEventKind::Moved) {
            self.drive_overlay(|overlay, ctx| {
                overlay.set_hovered_row(Some(row), ctx);
                Flow::Stay
            });
            return true;
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            self.drive_overlay(|overlay, ctx| overlay.handle_mouse_row(row, ctx));
        }
        true
    }

    fn on_status_mouse_moved(&mut self, mouse: MouseEvent) -> bool {
        let hover = self
            .status_hitboxes
            .and_then(|hitboxes| status_hover_at(hitboxes, mouse.column, mouse.row));
        if hover == self.status_hover {
            return hover.is_some();
        }
        self.status_hover = hover;
        if hover.is_some() {
            // A status value is an interactive control, not transcript content:
            // clear a prior tool-row highlight before painting its replacement.
            self.active_transcript_mut().clear_hover();
        }
        hover.is_some()
    }

    fn on_status_mouse_down(&mut self, mouse: MouseEvent) -> bool {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return false;
        }
        let Some(hitboxes) = self.status_hitboxes else {
            return false;
        };
        if rect_contains(hitboxes.mode, mouse.column, mouse.row) {
            self.status_hover = None;
            self.open_mode_picker();
            return true;
        }
        if rect_contains(hitboxes.model, mouse.column, mouse.row) {
            self.status_hover = None;
            self.open_model_picker();
            return true;
        }
        false
    }

    fn on_jump_to_bottom_mouse_moved(&mut self, mouse: MouseEvent) -> bool {
        let hover = self
            .jump_to_bottom_hitbox
            .is_some_and(|rect| rect_contains(rect, mouse.column, mouse.row));
        self.jump_to_bottom_hover = hover;
        if hover {
            self.status_hover = None;
            self.active_transcript_mut().clear_hover();
        }
        hover
    }

    fn on_jump_to_bottom_mouse_down(&mut self, mouse: MouseEvent) -> bool {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            || !self
                .jump_to_bottom_hitbox
                .is_some_and(|rect| rect_contains(rect, mouse.column, mouse.row))
        {
            return false;
        }
        self.active_transcript_mut().scroll_to_bottom();
        self.jump_to_bottom_hover = false;
        true
    }

    fn on_live_panel_mouse_moved(&mut self, mouse: MouseEvent) -> bool {
        let target = self.panel_target_at(mouse.column, mouse.row);
        let inside = self.live_panel_hitbox.as_ref().is_some_and(|hitbox| {
            mouse.column >= hitbox.rect.x
                && mouse.column < hitbox.rect.right()
                && mouse.row >= hitbox.rect.y
                && mouse.row < hitbox.rect.bottom()
        });
        if target != self.live_panel_hover {
            self.live_panel_hover = target;
        }
        if inside {
            self.status_hover = None;
            self.active_transcript_mut().clear_hover();
        }
        inside
    }

    fn on_live_panel_mouse_down(&mut self, mouse: MouseEvent) -> bool {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return false;
        }
        let Some(target) = self.panel_target_at(mouse.column, mouse.row) else {
            return false;
        };
        match target {
            PanelTarget::Main => self.open_view(ViewId::Main),
            // The bottom tree is navigation only. Detail belongs to the parent
            // conversation's task card, while Ctrl+clicking its title opens this trace.
            PanelTarget::Task(run) => self.open_view(ViewId::TaskRun(run)),
        }
        true
    }

    fn on_term_event(&mut self, ev: Event) {
        // Before any routing, and before anything can swallow the event:
        // the probe reports what arrived, not what we did with it.
        if self.voice_probe {
            if let Event::Key(key) = &ev {
                let line = crate::voice::describe_key(key);
                self.bake(vec![Line::styled(format!("  {line}"), theme::dim())]);
            }
        }
        match ev {
            // Releases are reported on Windows always, and elsewhere only once
            // voice asks for them (`crate::set_key_release_reporting`). Nothing
            // but push-to-talk has any use for them.
            Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Release => {
                if self.voice.matches_release(key.code) {
                    let outcome = self.voice.on_release();
                    self.apply_voice(outcome);
                }
            }
            Event::Key(key) => self.on_key(key),
            Event::Paste(text) => {
                // An overlay owns interaction while it is on screen. In
                // particular, multiline terminal pastes must not leak into
                // the hidden main editor and then make the restored panel jump.
                if let Some(overlay) = self.overlay.as_mut() {
                    overlay.paste_text(text);
                } else if self.rewind_nav.is_none() {
                    self.on_paste_text(text);
                }
            }
            Event::Mouse(mouse) => {
                // A modal owns mouse input too. Without this guard a plan drag
                // selects and copies the hidden transcript instead of
                // producing a plan comment.
                if self.on_overlay_mouse(mouse) {
                    return;
                }
                if matches!(mouse.kind, MouseEventKind::Moved)
                    && self.on_jump_to_bottom_mouse_moved(mouse)
                {
                    return;
                }
                if self.on_jump_to_bottom_mouse_down(mouse) {
                    return;
                }
                if matches!(mouse.kind, MouseEventKind::Moved)
                    && self.on_live_panel_mouse_moved(mouse)
                {
                    return;
                }
                if self.on_live_panel_mouse_down(mouse) {
                    return;
                }
                if matches!(mouse.kind, MouseEventKind::Moved) && self.on_status_mouse_moved(mouse)
                {
                    return;
                }
                if self.on_status_mouse_down(mouse) {
                    return;
                }
                match mouse.kind {
                    MouseEventKind::Moved => self.active_transcript_mut().mouse_moved(
                        mouse.column,
                        mouse.row,
                        mouse.modifiers.contains(KeyModifiers::CONTROL),
                    ),
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let up = mouse.kind == MouseEventKind::ScrollUp;
                        // Over the input box the wheel scrolls the prompt itself:
                        // a long multi-line paste only shows six rows at a time, so
                        // the wheel walks the visual cursor through it without
                        // clicking or reaching for the arrow keys. The viewport is
                        // cursor-derived, so moving the cursor scrolls the window.
                        // Everywhere else — including while an approval dialog is
                        // open (input hitbox is None then) — the wheel scrolls the
                        // transcript so the reviewer can scroll the pre-baked diff.
                        // Over the input the wheel nudges the prompt one line per
                        // notch (not a page); a short prompt or one already at its
                        // edge can't consume it, so the transcript scrolls instead.
                        let moved_input = self.wheel_over_input(mouse.row)
                            && if up {
                                self.editor_visual_up()
                            } else {
                                self.editor_visual_down()
                            };
                        if !moved_input {
                            self.active_transcript_mut().wheel(
                                mouse.column,
                                mouse.row,
                                up,
                                WHEEL_STEP,
                            );
                        }
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        self.drag_scroll = None;
                        let taken_by_input = self.input_mouse_down(mouse.column, mouse.row);
                        if !taken_by_input {
                            if mouse.modifiers.contains(KeyModifiers::CONTROL) {
                                if let Some(run) = self
                                    .active_transcript()
                                    .task_run_at(mouse.column, mouse.row)
                                {
                                    self.open_view(ViewId::TaskRun(run));
                                    return;
                                }
                                if let Some(url) =
                                    self.active_transcript().link_at(mouse.column, mouse.row)
                                {
                                    self.open_link(&url);
                                    return;
                                }
                            }
                            self.active_transcript_mut()
                                .mouse_down(mouse.column, mouse.row);
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if self.input_mouse_active {
                            self.input_mouse_drag(mouse.column, mouse.row);
                        } else {
                            let drag_edge = {
                                let transcript = self.active_transcript_mut();
                                transcript.mouse_drag(mouse.column, mouse.row);
                                transcript.drag_edge(mouse.row)
                            };
                            // Arm edge auto-scroll when the drag reaches a view
                            // edge; disarm the moment it returns inside.
                            self.drag_scroll = drag_edge.map(|up| (up, mouse.column, mouse.row));
                        }
                    }
                    // Any button release ends the drag (defensive: some terminals
                    // report the release with a non-Left button code).
                    MouseEventKind::Up(_) => self.finish_drag(mouse.column, mouse.row),
                    _ => {}
                }
            }
            // ratatui's autoresize adapts on the next draw; the transcript
            // rewraps lazily from the new area width.
            Event::Resize(..) => {
                self.status_hover = None;
                self.live_panel_hover = None;
                self.active_transcript_mut().clear_hover();
            }
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        // A pending approval keeps its mode status visible. Let its one global
        // shortcut through while all other keys still belong to the dialog.
        if matches!(key.code, KeyCode::BackTab)
            && self
                .overlay
                .as_ref()
                .is_some_and(Overlay::keeps_status_hint)
        {
            self.cycle_mode();
            return;
        }
        // Push-to-talk comes before the editor *and* before the overlay: an
        // approval note or a plan comment is a text field like any other, and
        // dictating into it is the whole point of having the key. Overlays
        // without a text cursor (the pickers) are left alone.
        if self.has_voice_target() {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let at_boundary = self.voice_at_boundary();
            if self.voice.matches_key(key.code, ctrl, at_boundary) {
                // Terminals that label auto-repeat save voice from having to
                // infer it; the rest send a plain `Press` for every repeat.
                let auto_repeat = key.kind == crossterm::event::KeyEventKind::Repeat;
                let outcome = self.voice.on_press(auto_repeat);
                self.apply_voice(outcome);
                return;
            }
        }
        // An overlay — any picker, or the approval dialog — owns the keyboard
        // while it is on screen.
        if self.overlay.is_some() {
            self.drive_overlay(|overlay, ctx| overlay.handle_key(key, ctx));
            return;
        }
        if matches!(key.code, KeyCode::End) && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.active_transcript_mut().scroll_to_bottom();
            return;
        }

        if !matches!(self.active_view, ViewId::Main) {
            match key.code {
                KeyCode::Esc => self.open_view(ViewId::Main),
                KeyCode::PageUp => self.active_transcript_mut().page_up(),
                KeyCode::PageDown => self.active_transcript_mut().page_down(),
                KeyCode::Up => self.active_transcript_mut().scroll_up(1),
                KeyCode::Down => self.active_transcript_mut().scroll_down(1),
                _ => {}
            }
            return;
        }

        // Rewind navigation captures everything while active: the
        // transcript itself shows the target, keys move between inputs.
        if self.rewind_nav.is_some() {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                // Esc keeps the double-Esc rhythm: each press jumps to
                // the next-older input.
                KeyCode::Esc | KeyCode::Up => {
                    let nav = self.rewind_nav.as_mut().expect("nav present");
                    nav.pos = nav.pos.saturating_sub(1);
                    self.apply_rewind_nav();
                }
                KeyCode::Down => {
                    let nav = self.rewind_nav.as_mut().expect("nav present");
                    if nav.pos + 1 < nav.candidates.len() {
                        nav.pos += 1;
                        self.apply_rewind_nav();
                    } else {
                        // Past the newest input: leave navigation.
                        self.exit_rewind_nav();
                    }
                }
                KeyCode::Enter => self.confirm_rewind_nav(false),
                KeyCode::Char('r') if ctrl => self.confirm_rewind_nav(true),
                KeyCode::Char('c') if ctrl => self.exit_rewind_nav(),
                _ => {}
            }
            return;
        }

        let running = matches!(self.phase, Phase::Running { .. });
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        // Esc unwinds the newest thing first, and a take in progress is newer
        // than anything else on screen.
        if matches!(key.code, KeyCode::Esc) && self.voice.is_busy() {
            let outcome = self.voice.cancel();
            self.apply_voice(outcome);
            return;
        }
        // A voice failure holds the hint row until it is acknowledged. Esc is
        // how everything else on that row is dismissed, so it dismisses this
        // too — otherwise the only way out is to know that `/voice off` clears
        // it, which is not something the row ever said.
        if matches!(key.code, KeyCode::Esc) && self.voice.dismiss_failure() {
            return;
        }
        if ctrl && key_char_eq(&key, 'a') {
            self.editor.select_all();
            return;
        }
        if (alt || (ctrl && shift)) && key_char_eq(&key, 'c') {
            self.copy_editor_selection_or_prompt();
            return;
        }
        if (alt || (ctrl && shift)) && key_char_eq(&key, 'x') {
            self.cut_editor_selection_or_prompt();
            return;
        }
        // Paste before the general Char handler: with Shift held the code is
        // 'V', so a bare `Char('v')` arm would miss Ctrl+Shift+V and instead
        // type a literal "V". key_char_eq is case-insensitive.
        if (ctrl || alt) && key_char_eq(&key, 'v') {
            self.paste_from_clipboard();
            return;
        }
        match key.code {
            KeyCode::Char('c') if ctrl => {
                // Ctrl+C interrupts, it never quits: a reflexive ctrl+c to stop
                // a runaway turn must not also throw the session away. `/exit`
                // (or ctrl+d) is the way out, and the footer says so.
                // Copy lives on Ctrl+Shift+C / Alt+C and mouse-release, so this
                // key never has to disambiguate copy vs interrupt.
                if running {
                    // Anything queued was queued to be said *now* — mark it for
                    // the fresh turn before cancelling this worker, so no batch
                    // boundary can attach it to the cancelled turn.
                    self.pending.defer_to_next_turn();
                    self.cancel_turn();
                } else if !self.editor.is_empty() || !self.attachments.is_empty() {
                    self.clear_draft();
                } else {
                    self.bake(vec![Line::styled(
                        "ctrl+c interrupts; /exit or ctrl+d quits",
                        theme::dim(),
                    )]);
                }
            }
            KeyCode::Char('d') if ctrl && self.editor.is_empty() => self.should_exit = true,
            // Esc peels off the newest thing first: a queued prompt, then the
            // turn, then the draft. So esc takes a message back without killing
            // the work in flight, and esc-esc does both — while ctrl+c always
            // means "stop now, and say what I queued".
            KeyCode::Esc if running && !self.pending.is_empty() => self.discard_queued(),
            KeyCode::Esc => {
                if running {
                    self.cancel_turn();
                } else if self
                    .last_esc
                    .take()
                    .is_some_and(|t| t.elapsed() < DOUBLE_ESC)
                {
                    self.open_rewind();
                } else {
                    self.clear_draft();
                    self.last_esc = Some(Instant::now());
                }
            }
            KeyCode::Char('j') if ctrl => self.editor.newline(),
            KeyCode::Enter if alt || shift || ctrl => self.editor.newline(),
            KeyCode::Enter if self.accept_reference_completion() => {}
            KeyCode::Enter => self.submit(running),
            KeyCode::BackTab => self.cycle_mode(),
            KeyCode::Tab => {
                if let Some(completion) = self.popup_selection() {
                    self.accept_completion(completion);
                }
            }
            KeyCode::Up => {
                if self.popup_active() {
                    self.popup_index = self.popup_index.saturating_sub(1);
                } else if !self.editor_visual_up() && self.editor.line_count() == 1 {
                    // History recall is a single-line convenience. In a
                    // multi-line prompt the top edge just stops — pressing up
                    // there must not wipe the draft and jump to a history entry.
                    self.editor.history_prev();
                }
            }
            KeyCode::Down => {
                if self.popup_active() {
                    self.popup_index =
                        (self.popup_index + 1).min(self.completion_matches().len() - 1);
                } else if !self.editor_visual_down() && self.editor.line_count() == 1 {
                    self.editor.history_next();
                }
            }
            KeyCode::Left => self.editor.left(),
            // → on an empty input takes the ghost suggestion; everywhere else
            // it is still just a cursor move. Accepting copies rather than
            // consumes: the ghost then hides because the input is no longer
            // empty, exactly as if the user had typed it — so clearing the
            // input brings it back, and nothing needs to be re-requested.
            KeyCode::Right if self.editor.is_empty() && self.suggestion.is_some() => {
                if let Some(suggestion) = self.suggestion.clone() {
                    self.editor.insert_str(&suggestion);
                }
            }
            KeyCode::Right => self.editor.right(),
            KeyCode::Home => self.editor.home(),
            KeyCode::End => self.editor.end(),
            KeyCode::PageUp => self.transcript.page_up(),
            KeyCode::PageDown => self.transcript.page_down(),
            KeyCode::Backspace => {
                self.dismissed_reference = None;
                let consumed_attachment = self.backspace_attachment_token();
                if !consumed_attachment {
                    self.editor.backspace();
                }
            }
            KeyCode::Delete => {
                self.dismissed_reference = None;
                self.editor.delete();
            }
            KeyCode::Char(c) => {
                self.dismissed_reference = None;
                self.editor.insert_char(c);
                self.popup_index = 0;
            }
            _ => {}
        }
        // Typing does *not* destroy the guess — it only hides it (the ghost
        // draws on an empty input). Type two characters, delete them, and it is
        // back, with no second request: it stays valid until the thing it was
        // predicting actually happens (a submit, a clear, a rewind).
    }

    // ---------------------------------------------------------- rewind

    fn open_link(&mut self, url: &str) {
        match open_http_url(url) {
            Ok(()) => self.notice = Some(("opened link in browser".into(), Instant::now())),
            Err(error) => {
                self.notice = Some((format!("could not open link: {error}"), Instant::now()))
            }
        }
    }

    // ----------------------------------------------------------- input mouse

    // ----------------------------------------------------------- paste/copy

    // ------------------------------------------------------- rendering
}

fn open_http_url(raw: &str) -> Result<(), String> {
    let parsed = url::Url::parse(raw).map_err(|_| "invalid URL")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("only http(s) links can be opened".into());
    }
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("explorer.exe").arg(raw).spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(raw).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(raw).spawn();
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    let result: Result<std::process::Child, std::io::Error> = Err(std::io::Error::other(
        "no browser launcher for this platform",
    ));
    result.map(|_| ()).map_err(|error| error.to_string())
}

/// Build the editor invocation from `$VISUAL`/`$EDITOR` (falling back to `vi`),
/// splitting the spec so a wrapper like `code --wait` keeps its flags, then
/// appending the file to open.
fn editor_command(path: &std::path::Path) -> std::process::Command {
    let spec = std::env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| "vi".to_string());
    let mut parts = spec.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    let mut cmd = std::process::Command::new(program);
    for arg in parts {
        cmd.arg(arg);
    }
    cmd.arg(path);
    cmd
}

fn key_char_eq(key: &KeyEvent, target: char) -> bool {
    matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&target))
}

async fn recv_opt(rx: &mut Option<mpsc::Receiver<AgentEvent>>) -> Option<AgentEvent> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Resolves when the running turn finishes; pends forever when idle.
async fn join_phase(phase: &mut Phase) -> (Session, Result<(), AgentError>) {
    match phase {
        Phase::Running { handle, .. } => match handle.await {
            Ok(done) => done,
            Err(join_err) => std::panic::resume_unwind(join_err.into_panic()),
        },
        Phase::Idle => std::future::pending().await,
    }
}

/// Resolves when a disk-heavy external import finishes; pends otherwise so it
/// composes cleanly with the terminal event loop.
async fn join_external_import(
    import: &mut Option<
        JoinHandle<(
            ExternalSource,
            Result<tcode_core::Resumed, tcode_core::store::StoreError>,
        )>,
    >,
) -> (
    ExternalSource,
    Result<tcode_core::Resumed, tcode_core::store::StoreError>,
) {
    match import {
        Some(handle) => match handle.await {
            Ok(done) => done,
            Err(join_err) => std::panic::resume_unwind(join_err.into_panic()),
        },
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/provider` used to quit the TUI, run a bare-terminal wizard and
    /// rebuild the app around the returned session. It is an overlay now:
    /// the app must stay up, and what it hands the binary to persist must
    /// carry the key that was typed.
    #[test]
    fn provider_setup_is_an_overlay_and_yields_the_edited_config() {
        let dir = tempfile::tempdir().unwrap();
        let saved: Arc<std::sync::Mutex<Option<tcode_core::config::Config>>> = Arc::default();
        let sink = saved.clone();
        let mut app = harness::app_with_provider_setup(
            dir.path(),
            90,
            40,
            crate::ProviderSetup {
                load: Box::new(|| Ok(tcode_core::config::Config::default())),
                apply: Box::new(move |config, _state| {
                    *sink.lock().unwrap() = Some(config);
                    Ok((
                        ModelMenu {
                            options: Vec::new(),
                            current: 0,
                            switch: Box::new(|_, _| Err("rebuilt".into())),
                        },
                        AgentMenu {
                            roles: Vec::new(),
                            pins: Vec::new(),
                            pin: Box::new(|_, _| Err("rebuilt".into())),
                        },
                    ))
                }),
            },
        );

        app.run_slash("/provider");
        let form = app.frame();
        assert!(
            form.contains("providers to configure"),
            "setup is on screen, not a torn-down terminal:\n{form}"
        );
        assert!(!app.should_exit, "the session stays open");

        // Walk to a builtin profile, open its key field, type, confirm.
        fn cursor_on(app: &mut App, name: &str) -> bool {
            // The overlay paints inside a bordered panel, so the cursor mark
            // is mid-line rather than at the start.
            app.frame()
                .lines()
                .any(|line| line.contains('▸') && line.contains(name))
        }
        let mut steps = 0;
        while !cursor_on(&mut app, "deepseek") {
            app.press(KeyCode::Down);
            steps += 1;
            assert!(steps < 20, "deepseek is reachable with the arrow keys");
        }
        app.press(KeyCode::Tab);
        for c in "sk-typed".chars() {
            app.press(KeyCode::Char(c));
        }
        let masked = app.frame();
        assert!(
            masked.contains("••••••••") && !masked.contains("sk-typed"),
            "a key is never echoed in the clear:\n{masked}"
        );
        app.press(KeyCode::Enter); // key → list
        app.press(KeyCode::Enter); // list → model choice
        app.press(KeyCode::Enter); // model → done

        let config = saved.lock().unwrap().take().expect("setup was applied");
        assert!(
            config
                .profiles
                .values()
                .any(|p| p.api_key.as_deref() == Some("sk-typed")),
            "the typed key reaches the binary that persists it"
        );
        let after = app.frame();
        assert!(
            !after.contains("providers to configure"),
            "the overlay closes once applied:\n{after}"
        );
        assert!(after.contains("providers configured"));
    }

    #[test]
    fn shimmer_text_preserves_content_and_animates_the_amber_status() {
        let first = shimmer_text("⠋ responding", 0, theme::WARN);
        let later = shimmer_text("⠋ responding", 5, theme::WARN);
        let text = first
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "⠋ responding");
        assert!(
            first
                .iter()
                .zip(later.iter())
                .any(|(before, after)| before.style.fg != after.style.fg),
            "the main running label has a moving highlight"
        );
    }

    #[test]
    fn live_task_detail_keeps_summary_activity_and_recent_steps() {
        let lines = task_live_detail(
            "Trace session persistence and resume flow",
            &["Read task_trace.rs".into(), "Grep task runs".into()],
        );
        let status = task_plain_status("Search task traces");
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("Trace session persistence and resume flow"));
        assert!(!text.contains("Search task traces"));
        assert_eq!(
            status
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "      └ Search task traces"
        );
        assert_eq!(status[0].spans[0].style, theme::dim());
        assert!(text.contains("└ Read task_trace.rs"));
        assert!(text.contains("└ Grep task runs"));
        assert!(
            lines
                .iter()
                .all(|line| line.style.fg == Some(crate::theme::DIM)),
            "task summary and recent activity stay uniformly dim"
        );
    }

    #[test]
    fn single_task_header_highlights_the_agent_kind() {
        let spans = task_header_summary("Explore · inspect the implementation");
        assert_eq!(spans[0].style.fg, Some(theme::OK));
        assert_eq!(spans[2].style, ratatui::style::Style::default());
    }

    #[test]
    fn status_hitboxes_follow_display_width_and_clip_to_terminal() {
        let hits = status_hitboxes(Rect::new(0, 7, 24, 1), "→ default", "模型 model");
        assert!(rect_contains(hits.mode, 7, 7));
        assert!(!rect_contains(hits.model, 7, 7));
        assert!(rect_contains(hits.model, hits.model.x, 7));
        assert!(hits.model.right() <= 24);

        let clipped = status_hitboxes(Rect::new(0, 0, 8, 1), "accept-edits", "model");
        assert_eq!(clipped.mode.right(), 8);
        assert_eq!(clipped.model.width, 0);
    }

    #[test]
    fn status_hover_targets_only_the_clickable_values() {
        let hits = status_hitboxes(Rect::new(0, 7, 40, 1), "default", "model");
        assert_eq!(status_hover_at(hits, 2, 7), None, "the mode label is inert");
        assert_eq!(
            status_hover_at(hits, hits.mode.x, 7),
            Some(StatusHover::Mode)
        );
        assert_eq!(
            status_hover_at(hits, hits.model.x, 7),
            Some(StatusHover::Model)
        );
        assert_eq!(status_hover_at(hits, 39, 7), None);
    }

    #[test]
    fn tip_source_reflows_without_losing_text() {
        let tip = "ctrl+c stops the turn and sends queued prompts right away — it never exits";
        let expected = format!("  ✻ tip: {tip}");

        let narrow = crate::transcript::wrap_lines(vec![tip_line(tip)], 20);
        assert!(narrow.len() > 1, "the narrow terminal wraps the tip");
        assert_eq!(
            narrow
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            expected
        );

        let wide = crate::transcript::wrap_lines(vec![tip_line(tip)], 120);
        assert_eq!(wide.len(), 1, "a wider terminal restores one row");
        assert_eq!(
            wide[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            expected
        );
    }

    #[test]
    fn attachment_placeholder_is_stable_per_id() {
        let img = Attachment::Image {
            id: 2,
            bytes: Vec::new(),
            media_type: "image/png",
            label: "Image #2".into(),
        };
        let txt = Attachment::Text {
            id: 7,
            content: String::new(),
            label: "Pasted text #7".into(),
        };
        assert_eq!(img.placeholder(), "[Image #2]");
        assert_eq!(txt.placeholder(), "[Pasted text #7]");
    }

    #[test]
    fn inline_token_backspaces_as_a_unit() {
        // Mirrors `backspace_attachment_token`: a backspace right after a
        // token deletes the whole token, leaving unrelated text (and unicode
        // before it) intact.
        let mut e = Editor::new();
        e.insert_str("你好 ");
        let token = "[Image #1]";
        e.insert_str(token);
        let pos = e.position();
        let before: String = e.lines()[pos.row].chars().take(pos.col).collect();
        assert!(before.ends_with(token));
        for _ in 0..token.chars().count() {
            e.backspace();
        }
        assert_eq!(e.text(), "你好 ");
    }

    #[test]
    fn accepted_reference_completion_inserts_a_separator() {
        let mut editor = Editor::new();
        editor.insert_str("review @app");
        App::apply_reference_completion(
            &mut editor,
            Position { row: 0, col: 7 },
            Position { row: 0, col: 11 },
            "@src/app.rs",
        );
        assert_eq!(editor.text(), "review @src/app.rs ");
    }

    #[test]
    fn sent_prompt_echo_accents_reference_paths() {
        let echo = prompt_echo("review @src/app.rs", &[]);
        let accented: Vec<_> = echo[1]
            .spans
            .iter()
            .filter(|span| span.style.fg == Some(theme::ACCENT))
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(accented, ["@src/app.rs"]);
    }

    fn edit_call(path: &str, old: &str, new: &str) -> AskCall {
        AskCall {
            tool: "edit".into(),
            summary: format!("edit({path})"),
            descriptor: format!("edit({path})"),
            is_edit: true,
            allows_project: false,
            input: serde_json::json!({
                "path": path,
                "old_string": old,
                "new_string": new,
            }),
        }
    }

    /// The combined review's whole promise: every diff is on screen *before*
    /// the reviewer answers, and taking the review apart puts the screen back
    /// the way it was so the per-call flow can re-propose them one at a time.
    #[test]
    fn taking_a_combined_review_apart_retracts_every_baked_diff() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("alpha.rs"),
            "fn alpha() {}
",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("beta.rs"),
            "fn beta() {}
",
        )
        .unwrap();
        let mut app = harness::app(dir.path(), 90, 40);
        let (tx, mut rx) = oneshot::channel();
        app.open_review(AskMsg {
            label: "Edit 2 files".into(),
            calls: vec![
                edit_call("alpha.rs", "fn alpha() {}", "fn alpha() -> u8 { 1 }"),
                edit_call("beta.rs", "fn beta() {}", "fn beta() -> u8 { 2 }"),
            ],
            reply: ApprovalReply::Batch(tx),
        });

        let reviewing = app.frame();
        assert!(
            reviewing.contains("fn alpha() -> u8 { 1 }")
                && reviewing.contains("fn beta() -> u8 { 2 }"),
            "both diffs are readable before any answer:
{reviewing}"
        );
        assert!(reviewing.contains("Edit 2 files"));
        assert!(reviewing.contains("Review one at a time"));

        // Options are Yes / Yes, allow all edits / Review one at a time / No.
        app.press(KeyCode::Down);
        app.press(KeyCode::Down);
        app.press(KeyCode::Enter);

        let after = app.frame();
        assert!(
            !after.contains("fn alpha() -> u8 { 1 }") && !after.contains("fn beta() -> u8 { 2 }"),
            "the combined review's diffs are retracted:
{after}"
        );
        assert!(
            !after.contains("Review one at a time"),
            "the pane is closed"
        );
        assert!(
            matches!(rx.try_recv(), Ok(BatchApproval::Individually)),
            "the agent loop is told to prompt per call, not given an answer"
        );
    }

    /// Declining the whole set retracts the diffs too — the batch header and
    /// per-call results become the record instead.
    #[test]
    fn declining_a_combined_review_retracts_its_diffs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("alpha.rs"),
            "fn alpha() {}
",
        )
        .unwrap();
        let mut app = harness::app(dir.path(), 90, 40);
        let (tx, mut rx) = oneshot::channel();
        app.open_review(AskMsg {
            label: "Edit 2 files".into(),
            calls: vec![
                edit_call("alpha.rs", "fn alpha() {}", "fn alpha() -> u8 { 1 }"),
                edit_call("beta.rs", "fn beta() {}", "fn beta() -> u8 { 2 }"),
            ],
            reply: ApprovalReply::Batch(tx),
        });
        assert!(app.frame().contains("fn alpha() -> u8 { 1 }"));

        app.press(KeyCode::Esc);

        let after = app.frame();
        assert!(
            !after.contains("fn alpha() -> u8 { 1 }"),
            "a declined change leaves no diff behind:
{after}"
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(BatchApproval::All(Approval {
                decision: ApprovalDecision::No,
                ..
            }))
        ));
    }

    /// The whole contract of push-to-talk in one pass: the key never types,
    /// the hint says how to end the take, and the transcript lands in the
    /// prompt *without* being sent.
    #[test]
    fn holding_the_voice_key_dictates_into_the_prompt_without_sending_it() {
        use crate::voice::{VoiceCmd, VoiceEvent};

        let dir = tempfile::tempdir().unwrap();
        let mut app = harness::app(dir.path(), 90, 40);
        let sent = app.fake_voice(tcode_core::config::VoiceKey::CtrlSpace);

        // Armed and idle looks like something: which key, before it is pressed.
        let armed = app.frame();
        assert!(
            armed.contains("voice ctrl+space"),
            "the status row says voice is on and what to press:
{armed}"
        );

        app.press_with(KeyCode::Char(' '), KeyModifiers::CONTROL);
        // Auto-repeat while the key is held must not restart the take.
        app.press_with(KeyCode::Char(' '), KeyModifiers::CONTROL);
        let recording = app.frame();
        assert!(
            recording.contains("release ctrl+space to transcribe"),
            "the hint says how to end the take:
{recording}"
        );
        assert!(
            app.editor.is_empty(),
            "the push-to-talk key types nothing: {:?}",
            app.editor.text()
        );

        // Long enough to be a hold rather than a tap, then let go — with ctrl
        // already released, which is how terminals report it.
        app.voice.pretend_recording_started(Duration::from_secs(2));
        app.release(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(app.frame().contains("transcribing"));

        app.on_voice_event(VoiceEvent::Transcript("改一下 editor 的换行".into()));
        assert_eq!(app.editor.text(), "改一下 editor 的换行");
        assert!(
            app.pending.queued().is_empty() && matches!(app.phase, Phase::Idle),
            "dictation fills the prompt; only enter sends it"
        );
        assert_eq!(
            sent.lock().unwrap().as_slice(),
            &[VoiceCmd::Start, VoiceCmd::Stop]
        );
    }

    /// Dictation into a half-written prompt. A space typed mid-word is still a
    /// space; one held at a word boundary is the key. Both have to be true at
    /// once or the space bar is broken.
    #[test]
    fn a_space_mid_sentence_types_and_one_held_after_it_dictates() {
        use crate::voice::VoiceEvent;

        let dir = tempfile::tempdir().unwrap();
        let mut app = harness::app(dir.path(), 90, 40);
        app.fake_voice(tcode_core::config::VoiceKey::Space);
        app.editor.insert_str("把这个");

        // Straight after a character: an ordinary separator, typed through.
        app.press(KeyCode::Char(' '));
        assert_eq!(app.editor.text(), "把这个 ");
        assert!(app.voice.hint().is_none(), "no take from a typed space");

        // Now the caret sits on a boundary, so holding claims the key.
        app.press(KeyCode::Char(' '));
        app.press(KeyCode::Char(' '));
        app.press(KeyCode::Char(' '));
        assert!(app.voice.is_recording(), "the hold was recognised");
        assert_eq!(
            app.editor.text(),
            "把这个 ",
            "every provisional space is taken back, and the typed one is not"
        );

        app.voice.pretend_recording_started(Duration::from_secs(2));
        app.release(KeyCode::Char(' '), KeyModifiers::NONE);
        app.on_voice_event(VoiceEvent::Transcript("改成 spawn_blocking".into()));
        assert_eq!(app.editor.text(), "把这个 改成 spawn_blocking");
    }

    /// An approval note and a plan comment are text fields, so dictation
    /// belongs in them too — and must land where paste would, not in the hidden
    /// prompt box behind the dialog.
    #[test]
    fn dictation_reaches_an_approval_note_rather_than_the_prompt_behind_it() {
        use crate::voice::VoiceEvent;

        let dir = tempfile::tempdir().unwrap();
        let mut app = harness::app(dir.path(), 90, 40);
        app.fake_voice(tcode_core::config::VoiceKey::Function(3));
        let (reply, _rx) = tokio::sync::oneshot::channel();
        app.overlay = Some(Overlay::approval(
            crate::approval::Dialog::new(
                "summary".into(),
                "tool".into(),
                "call".into(),
                false,
                false,
            ),
            crate::overlay::ApprovalReply::One(reply),
        ));

        app.press(KeyCode::F(3));
        assert!(app.voice.is_recording(), "the dialog does not swallow it");
        app.voice.pretend_recording_started(Duration::from_secs(2));
        app.release(KeyCode::F(3), KeyModifiers::NONE);
        app.on_voice_event(VoiceEvent::Transcript("先跑测试".into()));

        assert!(
            app.editor.is_empty(),
            "not into the prompt behind the dialog"
        );
        let note = app
            .overlay
            .as_mut()
            .and_then(Overlay::as_dialog_mut)
            .expect("the dialog is still up")
            .text_target_text();
        assert_eq!(note, "先跑测试", "dictation landed in the note");
    }

    /// The pickers have no text cursor, so the key keeps its own meaning there
    /// rather than starting a take with nowhere to put the words.
    #[test]
    fn a_picker_is_not_a_dictation_target() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = harness::app(dir.path(), 90, 40);
        app.fake_voice(tcode_core::config::VoiceKey::Function(3));
        app.open_mode_picker();

        app.press(KeyCode::F(3));
        assert!(!app.voice.is_recording(), "no target, no take");
        assert!(
            app.overlay.is_some(),
            "and the picker still has the keyboard"
        );
    }

    /// One rule for the word list: a leading `-` removes, anything else adds.
    /// Adding a word twice must not double it — sherpa would weight it twice.
    #[test]
    fn voice_words_add_and_a_leading_dash_removes() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = harness::app(dir.path(), 90, 40);
        app.fake_voice(tcode_core::config::VoiceKey::CtrlSpace);

        app.edit_voice_words("tokio serde tokio");
        assert_eq!(app.voice.words(), ["tokio", "serde"]);

        app.edit_voice_words("-serde spawn_blocking");
        assert_eq!(app.voice.words(), ["tokio", "spawn_blocking"]);

        // The list is the whole point of the command, so it is always shown.
        let frame = app.frame();
        assert!(
            frame.contains("tokio spawn_blocking"),
            "the words are echoed back:
{frame}"
        );
    }

    /// A backend failure parks a warning on the hint row. It has to say how to
    /// get rid of it and Esc has to actually do that, or the only way out the
    /// user can find is restarting tcode.
    #[test]
    fn a_voice_failure_says_how_to_dismiss_it_and_esc_does() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = harness::app(dir.path(), 90, 40);
        app.fake_voice(tcode_core::config::VoiceKey::CtrlSpace);

        app.on_voice_event(crate::voice::VoiceEvent::Failed(
            "unknown argument --model".into(),
        ));
        let warned = app.frame();
        assert!(
            warned.contains("esc dismisses"),
            "the warning states its own exit:
{warned}"
        );

        app.press(KeyCode::Esc);
        let after = app.frame();
        assert!(
            !after.contains("esc dismisses"),
            "esc clears the warning:
{after}"
        );
    }

    /// The picker is built from whatever the installed sidecar reports. With no
    /// sidecar there is nothing to offer, and the reply has to be the build
    /// instructions rather than an empty menu.
    #[test]
    fn the_voice_model_picker_explains_itself_when_there_is_no_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = harness::app(dir.path(), 90, 40);
        // Points at a path that cannot exist, so the test never depends on
        // whether this machine happens to have a sidecar installed.
        let absent = dir.path().join("absent");
        app.voice.set_command(absent.display().to_string());

        app.open_voice_model_picker();
        assert!(app.overlay.is_none(), "no menu without a backend to ask");
        let frame = app.frame();
        assert!(
            frame.contains("[voice] command points at"),
            "the miss names the path it looked at:
{frame}"
        );
    }

    /// Esc belongs to the newest thing on screen, and a take in progress is
    /// newer than the draft it would otherwise clear.
    #[test]
    fn esc_while_recording_cancels_the_take_and_keeps_the_draft() {
        use crate::voice::VoiceCmd;

        let dir = tempfile::tempdir().unwrap();
        let mut app = harness::app(dir.path(), 90, 40);
        let sent = app.fake_voice(tcode_core::config::VoiceKey::CtrlSpace);
        app.editor.insert_str("half a thought");

        app.press_with(KeyCode::Char(' '), KeyModifiers::CONTROL);
        app.press(KeyCode::Esc);

        assert_eq!(app.editor.text(), "half a thought", "the draft survives");
        assert_eq!(
            sent.lock().unwrap().as_slice(),
            &[VoiceCmd::Start, VoiceCmd::Cancel]
        );
        let after = app.frame();
        assert!(
            !after.contains("release ctrl+space"),
            "recording is over:
{after}"
        );
    }
}
