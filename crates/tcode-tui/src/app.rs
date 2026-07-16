use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Stdout;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use tcode_core::blobs::approx_tokens;
use tcode_core::commands::{
    CommandCtx, CommandEffect, CommandMessage, CommandRegistry, MessageKind,
};
use tcode_core::{
    Agent, AgentError, AgentEvent, Approval, ApprovalDecision, Approver, ContentBlock,
    PendingMessage, ReferenceCandidate, ReferenceKind, Session, Usage,
};

use crate::approval::{Dialog, DialogResult};
use crate::editor::{Editor, Position};
use crate::model_picker::{self, AgentMenu, ModelMenu};
use crate::render::{shorten_summary_path, CallRoute, RenderRegistry};
use crate::resume::{self, PickResult as ResumePickResult};
use crate::transcript::Transcript;
use crate::{diff, markdown, theme, OpeningContextFn};

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Lines scrolled per mouse-wheel event.
const WHEEL_STEP: usize = 3;
/// Visible rows of an expanded tool-output region.
const OUTPUT_VIEW_ROWS: usize = 12;
/// Progress panel rows should stay small and predictable; long phase lists
/// render as a focused window around the active phase instead of stealing scroll focus.
const PROGRESS_VISIBLE_PHASES: usize = 5;

/// Second Esc within this window (while idle) opens the rewind picker.
const DOUBLE_ESC: Duration = Duration::from_millis(1200);

/// Opens a note the human slipped to the model mid-turn (approval comment,
/// `/note`), distinguishing it from a full user turn under the same rail.
/// The note's own text already says what it is about — see
/// `Agent`'s approval notes — so the label stays a bare marker.
const NOTE_LABEL: &str = "Note: ";

const PASTE_FOLD_LINES: usize = 15;
/// Long one-line pastes should not make the editor visibly type character by
/// character. They are sent as a text attachment instead.
const PASTE_FOLD_CHARS: usize = 1_000;
/// Braille spinner drawn in the accent colour: visibly alive without the
/// bulk or flicker of the legacy sparkle animation.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Half-block wordmark for the welcome banner. Both rows must stay the
/// same character length: the gradient is per-column and the version tag
/// hangs off the second row.
const LOGO: [&str; 2] = ["▀█▀ █▀▀ █▀█ █▀▄ █▀▀", " █  █▄▄ █▄█ █▄▀ ██▄"];

/// One of these shows per launch, picked at random: a discovery channel
/// for features nobody reads /help for. Every entry must describe real,
/// current behaviour — stale tips are worse than none.
const TIPS: [&str; 9] = [
    "shift+tab cycles permission modes",
    "esc esc rewinds the conversation · ctrl+r also restores files",
    "ctrl+c stops the turn and sends queued prompts right away — it never exits",
    "type while a turn runs: the prompt queues and esc takes it back",
    "→ accepts the dim suggestion in the input box",
    "/model switches model mid-session · /agents pins sub-agent models",
    "/resume picks up an earlier session · /export saves the transcript",
    "/compact squeezes a long conversation back into budget",
    "/note slips the model an aside without starting a turn",
];

/// Keep the complete tip in the transcript source. `Transcript` owns wrapping
/// and recomputes it whenever the terminal width changes.
fn tip_line(tip: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("  ✻ ".to_string(), theme::accent()),
        Span::styled(format!("tip: {tip}"), theme::dim()),
    ])
}

/// Commands whose substance drives frontend-owned objects (key table, model
/// picker, provider wizard). Everything else lives in the shared
/// `CommandRegistry` in tcode-core.
const UI_COMMANDS: [(&str, &str); 4] = [
    ("/help", "show keys and commands"),
    ("/model", "switch model · adjust reasoning effort"),
    (
        "/agents",
        "choose models for sub-agents and Auto Mode safety",
    ),
    ("/provider", "configure or switch provider"),
];

pub struct AskMsg {
    pub tool: String,
    pub summary: String,
    pub descriptor: String,
    pub allows_project: bool,
    pub input: serde_json::Value,
    pub reply: oneshot::Sender<Approval>,
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
        allows_project: bool,
        input: &serde_json::Value,
    ) -> Approval {
        let (reply, rx) = oneshot::channel();
        let msg = AskMsg {
            tool: tool.to_string(),
            summary: summary.to_string(),
            descriptor: descriptor.to_string(),
            allows_project,
            input: input.clone(),
            reply,
        };
        if self.tx.send(msg).await.is_err() {
            return Approval::simple(ApprovalDecision::No, Some("UI unavailable".into()));
        }
        rx.await
            .unwrap_or_else(|_| Approval::simple(ApprovalDecision::No, None))
    }
}

enum Attachment {
    Image {
        id: u32,
        bytes: Vec<u8>,
        media_type: &'static str,
        label: String,
    },
    Text {
        id: u32,
        content: String,
        label: String,
    },
}

impl Attachment {
    /// The inline token shown in the editor. Stable per attachment (the id
    /// never renumbers within a draft) so it can be matched back for deletion.
    fn placeholder(&self) -> String {
        match self {
            Attachment::Image { id, .. } => format!("[Image #{id}]"),
            Attachment::Text { id, .. } => format!("[Pasted text #{id}]"),
        }
    }

    fn label(&self) -> &str {
        match self {
            Attachment::Image { label, .. } | Attachment::Text { label, .. } => label,
        }
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

/// The input's visual layout. The editor deliberately stores logical lines
/// only; this is the terminal-width-aware projection used for rendering.
struct EditorLayout {
    lines: Vec<EditorVisualLine>,
    cursor_row: usize,
    cursor_col: usize,
}

struct EditorVisualLine {
    first_logical_line: bool,
    text: String,
    logical_row: usize,
    start_col: usize,
    end_col: usize,
    selection: Option<(usize, usize)>,
}

#[derive(Clone)]
enum CompletionKind {
    Slash,
    Reference { start: Position, end: Position },
}

#[derive(Clone)]
struct CompletionMatch {
    label: String,
    description: String,
    replacement: String,
    kind: CompletionKind,
}

#[derive(Clone, Copy)]
struct InputHitbox {
    rect: Rect,
    editor_start: usize,
}

struct PendingCall {
    detail: String,
    /// Batch items defer their indented summary row (plus any diff) so
    /// `ToolEnd` can bake it directly above this call's own result instead of
    /// baking every item first and every result after. Empty for single calls
    /// (their header is baked at `ToolStart`).
    header: Vec<Line<'static>>,
    /// A single bare call's already-baked `●` header block: its result
    /// attaches to that very row at `ToolEnd` instead of opening a
    /// separate `⎿` row. None for batch items and body-carrying calls.
    header_index: Option<usize>,
}

/// A tool call recovered from the ledger during replay, with the batch it
/// was executed in (asked of core, never re-derived here).
struct ReplayCall {
    name: String,
    input: serde_json::Value,
    batch: Option<ReplayBatch>,
}

struct ReplayBatch {
    /// The `● label` header, carried by the batch's first call only.
    header: Option<String>,
    /// Several tools in one batch: tag each item with its tool name.
    mixed: bool,
}

/// Batch items are indented under their shared header without a tree glyph.
const BATCH_ITEM_INDENT: &str = "    ";

/// Where a result's call record lives, shared by live `ToolEnd` and
/// replay. The three cases are mutually exclusive by construction.
enum CallRecord {
    /// A batch item: its deferred indented summary lines bake with the result.
    Batch(Vec<Line<'static>>),
    /// A bare single call: its `●` header block is already in the
    /// transcript and the result attaches to that very row.
    HeaderBlock(usize),
    /// Header (plus diff/command body) fully baked; the result stands alone.
    Baked,
}

/// One tool result's rendering, shared by live `ToolEnd` and replay.
enum ResultRender {
    /// The call site already told the story (successful edit/write diff).
    Nothing,
    /// A one-line preview and nothing more: it rides on the call's own
    /// row when there is one, or renders as a `⎿ preview` row.
    Inline(Line<'static>),
    /// A head row carrying the fold affordance plus the collapsed body.
    Foldable {
        head: Vec<Line<'static>>,
        detail: ResultDetail,
    },
}

enum ResultDetail {
    Lines(Vec<Line<'static>>),
    Markdown(markdown::Document),
}

impl ResultDetail {
    fn is_empty(&self) -> bool {
        match self {
            Self::Lines(lines) => lines.is_empty(),
            Self::Markdown(document) => document.is_empty(),
        }
    }
}

struct ProgressPhase {
    phase: String,
    status: String,
}

impl ProgressPhase {
    fn is_completed(&self) -> bool {
        self.status == "completed"
    }
}

struct RewindCandidate {
    /// Ledger index of the user entry (truncate target).
    index: usize,
    /// Full original input, prefilled into the editor.
    text: String,
    /// Files changed at/after this point → offer to restore them.
    dirty: bool,
}

/// Double-Esc rewind navigation: the transcript itself jumps to and
/// highlights the chosen user input — no picker dialog.
struct RewindNav {
    candidates: Vec<RewindCandidate>,
    pos: usize,
    /// Editor content before navigation began, restored on exit.
    saved_input: String,
}

pub struct App {
    agent: Arc<Agent>,
    opening_context: OpeningContextFn,
    registry: CommandRegistry,
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
            tcode_core::ExternalSource,
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
    /// is `Session::suggestions` (`/suggestions`).
    suggestion: Option<String>,
    suggest_cancel: Option<CancellationToken>,
    /// Which guess is current. A reply carrying an older generation is a guess
    /// about a conversation that no longer exists, and is dropped.
    suggest_gen: u64,
    suggest_tx: mpsc::Sender<(u64, Option<String>)>,
    suggest_rx: mpsc::Receiver<(u64, Option<String>)>,
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
    input_mouse_active: bool,
    /// Whether the current prompt press has actually dragged. A plain click
    /// (no Drag event) must not copy, even if the release cell differs slightly.
    input_dragged: bool,
    /// Armed while a transcript selection drag rests at a view edge: `(toward
    /// older, x, y)`. A timer then scrolls and extends the selection, since a
    /// pointer held still at the edge emits no further mouse events. Cleared on
    /// release or when the drag returns inside the view.
    drag_scroll: Option<(bool, u16, u16)>,
    dialog: Option<(Dialog, oneshot::Sender<Approval>)>,
    /// A change diff baked into the transcript while its approval dialog is
    /// open (so the full code is scrollable in the record, not cramped in
    /// the dialog). Holds the block-count mark to retract to on decline or
    /// when a batch supersedes it; on approval it tells the upcoming
    /// `ToolStart` to skip re-baking the diff.
    change_prebake: Option<usize>,
    rewind_nav: Option<RewindNav>,
    resume_picker: Option<resume::Picker>,
    menu: ModelMenu,
    model_picker: Option<model_picker::Picker>,
    agents: AgentMenu,
    agent_picker: Option<model_picker::AgentPicker>,
    /// Mirror of `Session::dogfood` for the status line: a running turn owns
    /// the session, so the hint cannot read it directly.
    dogfood: bool,
    pending_tool: Option<PendingCall>,
    /// Entries belonging to a concurrent group, completed in model-call
    /// order. Keeping them queued lets each result retain its own input.
    pending_batch: VecDeque<PendingCall>,
    progress: Vec<ProgressPhase>,
    last_esc: Option<Instant>,
    popup_index: usize,
    /// Tab accepted this exact `@` marker. Keep its completion closed until the
    /// user changes the draft, rather than immediately matching it again.
    dismissed_reference: Option<Position>,

    // Live streaming state: kept as a replace-in-place transcript block until
    // the provider finishes this assistant message.
    live_text: String,
    /// Index of the still-streaming assistant block inside the transcript.
    /// While set, incoming deltas replace that block in place; finalization just
    /// drops the marker so the block becomes ordinary scrollback.
    live_block: Option<usize>,
    thinking_chars: usize,
    thinking_text: String,
    thinking_since: Option<Instant>,
    show_reasoning: bool,
    out_tokens: usize,
    delegated_usage: Usage,
    rate_limits: Option<tcode_core::RateLimits>,
    /// Time the running turn was deliberately paused for a human decision.
    /// Completion receipts report active execution time, not time spent away
    /// from the terminal deciding about a change or answering a question.
    user_wait_started: Option<Instant>,
    user_wait_total: Duration,
    /// Best available estimate of the conversation currently occupying the
    /// model window. A completed provider usage event replaces estimates;
    /// streamed output and tool results keep it moving between those events.
    context_tokens: u64,
    /// Start of the current model request. This lets a retry discard the
    /// speculative streamed-token estimate from the failed attempt.
    context_step_start: u64,
    /// Session JSONL stores messages, not provider token counters. A resumed
    /// conversation starts from a local estimate until its next response
    /// supplies an authoritative usage event.
    context_estimated: bool,
    state_label: String,
    turn_usage: Usage,
    mode_label: String,
    spinner: usize,
    /// Cache-read share of the previous turn; the regression sentinel
    /// compares against it so cache decay is visible immediately.
    prev_cache_ratio: Option<f64>,
    should_exit: bool,
    provider_setup_requested: bool,
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
        mut session: Session,
        menu: ModelMenu,
        agents: AgentMenu,
        opening_context: OpeningContextFn,
        show_reasoning: bool,
    ) -> anyhow::Result<Self> {
        let (ask_tx, ask_rx) = mpsc::channel(4);
        let (suggest_tx, suggest_rx) = mpsc::channel(1);
        let (reference_tx, reference_rx) = mpsc::channel(1);
        let mode_label = session.mode.label().to_string();
        let committed_mode = session.mode;
        let pending_mode = session.pending_mode.clone();
        let session_dogfood = session.dogfood();
        let cwd = session.tool_ctx.cwd.clone();
        let scratch_dir = session.tool_ctx.scratch_dir.clone();
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
        let terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
        let transcript = Transcript::new(terminal.size().map(|s| s.width).unwrap_or(80));
        // The running turn owns the Session; this handle is how input typed
        // meanwhile still reaches it.
        let pending = session.pending.clone();
        Ok(Self {
            agent,
            opening_context,
            registry: CommandRegistry::builtin(),
            session: Some(session),
            pending,
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
            attachments: Vec::new(),
            next_attachment_id: 1,
            clipboard: arboard::Clipboard::new().ok(),
            input_hitbox: None,
            input_mouse_active: false,
            input_dragged: false,
            drag_scroll: None,
            dialog: None,
            change_prebake: None,
            rewind_nav: None,
            resume_picker: None,
            menu,
            model_picker: None,
            agents,
            agent_picker: None,
            dogfood: session_dogfood,
            pending_tool: None,
            pending_batch: VecDeque::new(),
            progress: Vec::new(),
            last_esc: None,
            popup_index: 0,
            dismissed_reference: None,
            live_text: String::new(),
            live_block: None,
            thinking_chars: 0,
            thinking_text: String::new(),
            thinking_since: None,
            show_reasoning,
            out_tokens: 0,
            delegated_usage: Usage::default(),
            rate_limits: None,
            user_wait_started: None,
            user_wait_total: Duration::ZERO,
            context_tokens,
            context_step_start: context_tokens,
            context_estimated,
            state_label: String::new(),
            retry_wait: None,
            turn_usage: Usage::default(),
            mode_label,
            spinner: 0,
            prev_cache_ratio: None,
            should_exit: false,
            provider_setup_requested: false,
            notice: None,
        })
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        let banner = self.banner();
        self.bake(banner);
        self.bake_transcript();
        self.refresh_reference_index();
        let mut term_events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Drives selection auto-scroll while the pointer is held at a view edge.
        // Its select arm is gated on `drag_scroll`, so it only wakes the loop
        // while a drag is actually parked at an edge — never when idle.
        let mut drag_tick = tokio::time::interval(Duration::from_millis(50));
        drag_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        while !self.should_exit {
            self.redraw()?;
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
                    if self.user_wait_started.is_none() {
                        self.user_wait_started = Some(Instant::now());
                    }
                    let dialog = if ask.tool == "ask_user" {
                        Dialog::questions(ask.summary, &ask.input)
                    } else if ask.tool == "exit_plan" {
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
                        let source = ask.input["plan"].as_str().unwrap_or("").trim();
                        let blocks = markdown::split_blocks(source)
                            .into_iter()
                            .map(|block| {
                                let document = self.md.parse(&block);
                                (block, document)
                            })
                            .collect();
                        Dialog::plan(ask.summary, ask.input.clone(), blocks)
                    } else {
                        // A change proposal (edit/write) is baked into the
                        // transcript now — in full, scrollable as part of the
                        // record — so the reviewer reads the whole diff there
                        // rather than in the cramped dialog. On decline it is
                        // retracted; on approval the upcoming ToolStart skips
                        // re-baking it (see `change_prebake`).
                        let call_summary = self.display_summary(
                            &tcode_core::agent::summarize_call(&ask.tool, &ask.input),
                        );
                        let change = self.renderers.get(&ask.tool).body(&ask.input);
                        if !change.is_empty() {
                            self.bake_live_text();
                            self.finish_thinking();
                            self.change_prebake = Some(self.transcript.block_count());
                            let mut spans: Vec<Span> = self.colored_tool_summary(&call_summary);
                            spans.insert(0, Span::styled("● ", theme::accent()));
                            let mut lines = vec![Line::default(), Line::from(spans)];
                            lines.extend(change);
                            lines.push(Line::default());
                            self.bake(lines);
                        }
                        // Diff lives in the transcript; the dialog carries only
                        // the choices.
                        Dialog::new(
                            ask.summary,
                            ask.descriptor,
                            call_summary,
                            ask.allows_project,
                        )
                    };
                    self.transcript.clear_hover();
                    self.dialog = Some((dialog, ask.reply));
                }
                done = join_phase(&mut self.phase) => {
                    self.on_turn_done(done);
                }
                done = join_external_import(&mut self.external_import) => {
                    self.on_external_import_done(done);
                }
                _ = tick.tick() => {
                    if matches!(self.phase, Phase::Running { .. }) || self.external_import.is_some() {
                        self.spinner = (self.spinner + 1) % SPINNER.len();
                    }
                }
                _ = drag_tick.tick(), if self.drag_scroll.is_some() => {
                    self.drag_autoscroll_step();
                }
            }
        }
        Ok(())
    }

    pub fn provider_setup_requested(&self) -> bool {
        self.provider_setup_requested
    }

    /// Recover the active session when the app intentionally exits to launch
    /// the provider wizard.
    pub fn take_session(&mut self) -> Option<Session> {
        self.session.take()
    }

    /// Welcome block: gradient logo, model, cwd, one rotating tip.
    /// Frameless on purpose — whitespace does the framing, and there is
    /// no box-width arithmetic to break on narrow terminals.
    fn banner(&self) -> Vec<Line<'static>> {
        use unicode_width::UnicodeWidthStr;
        let model = self.agent.model.snapshot();
        let version = format!("v{}", env!("CARGO_PKG_VERSION"));
        let term_w = self.terminal.size().map(|s| s.width).unwrap_or(80) as usize;

        let mut out = vec![Line::default()];
        // Half-block wordmark, two rows of equal length so the gradient
        // columns line up. Too-narrow terminals fall back to plain text
        // rather than letting the blocks soft-wrap into noise.
        if term_w > LOGO[0].width() + 4 {
            out.push(Line::from(theme::logo_gradient(&format!(" {}", LOGO[0]))));
            let mut bottom = theme::logo_gradient(&format!(" {}", LOGO[1]));
            bottom.push(Span::styled(format!("  {version}"), theme::dim()));
            out.push(Line::from(bottom));
        } else {
            out.push(Line::from(vec![
                Span::styled(" ✻ ".to_string(), theme::accent()),
                Span::styled("tcode".to_string(), theme::user_prompt()),
                Span::styled(format!(" {version}"), theme::dim()),
            ]));
        }
        out.push(Line::default());

        // Overly long values (deep cwd) keep their tail, which is the
        // informative end of a path.
        let max_content = term_w.saturating_sub(11).max(20);
        let clip = |s: &str| -> String {
            if s.width() <= max_content {
                return s.to_string();
            }
            let tail: String = s
                .chars()
                .rev()
                .scan(0usize, |w, c| {
                    *w += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                    (*w < max_content).then_some(c)
                })
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            format!("…{tail}")
        };
        let cwd = {
            let cwd = self
                .session
                .as_ref()
                .map(|s| s.tool_ctx.cwd.display().to_string())
                .unwrap_or_default();
            let home = std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .unwrap_or_default();
            match (home.is_empty(), cwd.strip_prefix(&home)) {
                (false, Some(rest)) => format!("~{rest}"),
                _ => cwd,
            }
        };
        let rows = [
            (
                "model",
                format!("{} · {}", model.provider.name(), model.describe()),
            ),
            ("cwd", cwd),
        ];
        for (label, value) in rows {
            out.push(Line::from(vec![
                Span::styled(format!("  {label:<6} "), theme::dim()),
                Span::raw(clip(&value)),
            ]));
        }
        out.push(Line::default());

        let tip = TIPS[std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos() as usize)
            % TIPS.len()];
        out.push(tip_line(tip));
        out.push(Line::default());
        out
    }

    /// Replay a resumed conversation into the scrollback so the user
    /// sees where they left off.
    fn bake_transcript(&mut self) {
        // Moved out for the walk so the bake helpers below can take `&mut
        // self`; restored before returning.
        let Some(session) = self.session.take() else {
            return;
        };
        if !session.ledger.is_empty() {
            self.replay_ledger(&session);
        }
        self.session = Some(session);
    }

    fn replay_ledger(&mut self, session: &Session) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut resumed_progress: Option<serde_json::Value> = None;
        let mut calls: HashMap<String, ReplayCall> = HashMap::new();
        for (entry_index, entry) in session.ledger.entries().iter().enumerate() {
            match entry {
                tcode_core::Entry::User(blocks) => {
                    // User echoes are their own entry-tagged blocks so
                    // rewind can jump to and truncate from them.
                    self.transcript.push(std::mem::take(&mut lines));
                    let mut echo: Vec<Line<'static>> = vec![Line::default()];
                    for b in blocks {
                        match b {
                            ContentBlock::Text { text } if !text.starts_with("<tcode-status>") => {
                                echo.extend(quote_lines(None, text));
                            }
                            ContentBlock::Image { .. } => {
                                echo.push(quote_attachment_line("[image]"));
                            }
                            _ => {}
                        }
                    }
                    // Keep a breathing row between a highlighted human
                    // message and the following assistant/tool activity.
                    echo.push(Line::default());
                    self.transcript.push_tagged(echo, entry_index);
                }
                tcode_core::Entry::Assistant(blocks) => {
                    let mut group: Vec<(String, String, serde_json::Value)> = Vec::new();
                    for b in blocks {
                        match b {
                            ContentBlock::Thinking { thinking, .. } => {
                                // Its own foldable block, like live streaming.
                                // The duration is not recorded, so the head
                                // states only the size.
                                self.transcript.push(std::mem::take(&mut lines));
                                let title =
                                    format!("reasoning (~{} tok)", thinking.chars().count() / 3);
                                self.bake_thinking(&title, thinking);
                            }
                            ContentBlock::Text { text } => {
                                self.transcript.push(std::mem::take(&mut lines));
                                self.transcript
                                    .push_markdown(self.md.parse(text).with_trailing_blank());
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                // Defer the header to the matching ToolResults
                                // entry so each call renders directly above its
                                // own result, not all headers then all results.
                                group.push((id.clone(), name.clone(), input.clone()));
                                if matches!(self.renderers.get(name).route(), CallRoute::Progress) {
                                    resumed_progress = Some(input.clone());
                                }
                            }
                            _ => {}
                        }
                    }
                    // Ask core whether these calls ran as a batch: the loop
                    // that made that call is the only place that knows.
                    let label = self.agent.batch_display_label(session, &group);
                    let mixed = group
                        .iter()
                        .map(|(_, name, _)| name.as_str())
                        .collect::<HashSet<_>>()
                        .len()
                        > 1;
                    for (index, (id, name, input)) in group.into_iter().enumerate() {
                        let batch = label.as_ref().map(|label| ReplayBatch {
                            header: (index == 0).then(|| label.clone()),
                            mixed,
                        });
                        calls.insert(id, ReplayCall { name, input, batch });
                    }
                }
                tcode_core::Entry::IncompleteAssistant { text, error } => {
                    self.transcript.push(std::mem::take(&mut lines));
                    self.transcript
                        .push_markdown(self.md.parse(text).with_trailing_blank());
                    lines.push(Line::styled(
                        format!(
                            "↻ stream failed: {error} — incomplete response retained; not sent back to model"
                        ),
                        theme::error_highlight(),
                    ));
                    lines.push(Line::default());
                }
                tcode_core::Entry::Summary(summary) => {
                    // Flush the prose above so the divider bakes as its own
                    // foldable block, exactly as the live path lays it out.
                    self.transcript.push(std::mem::take(&mut lines));
                    let summary = summary.clone();
                    self.bake_compacted(&summary);
                }
                tcode_core::Entry::ImportedTool {
                    name,
                    input,
                    content,
                } => {
                    // These names come from external logs (Codex / Claude
                    // Code) and are never in the render registry; imported
                    // rendering stays hardcoded by design.
                    if name.contains("apply_patch") {
                        lines.push(Line::from(vec![
                            Span::styled("● ", theme::accent()),
                            Span::styled(
                                format!("{name} (imported historical change)"),
                                theme::bold(),
                            ),
                        ]));
                        lines.extend(diff::render_unified_patch(content));
                    } else if name == "output" {
                        for (index, line) in content.lines().enumerate() {
                            let prefix = if index == 0 { "  ⎿ " } else { "    " };
                            lines.push(Line::styled(format!("{prefix}{line}"), theme::dim()));
                        }
                    } else {
                        let summary = if input.is_null() {
                            name.clone()
                        } else {
                            self.display_summary(&tcode_core::agent::summarize_call(name, input))
                        };
                        lines.push(Line::from(vec![
                            Span::styled("● ", theme::accent()),
                            Span::styled(summary, theme::bold()),
                        ]));
                        if !content.is_empty() {
                            self.transcript.push(std::mem::take(&mut lines));
                            self.transcript
                                .push_markdown(self.md.parse(content).with_trailing_blank());
                        }
                    }
                    lines.push(Line::default());
                }
                tcode_core::Entry::ToolResults(blocks) => {
                    for block in blocks {
                        let ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                            ..
                        } = block
                        else {
                            continue;
                        };
                        let call = calls.get(tool_use_id);
                        let name = call.map(|c| c.name.as_str()).unwrap_or("tool");
                        // Plan/Silent calls render via the plan pane or their
                        // approval record, exactly like the live path.
                        if !matches!(self.renderers.get(name).route(), CallRoute::Transcript) {
                            continue;
                        }
                        // Flush the prose above so this call bakes as its own
                        // block, exactly as the live path lays it out.
                        self.transcript.push(std::mem::take(&mut lines));
                        // Bake this call's header (+ diff / command block)
                        // right above its own result.
                        let record = match call {
                            Some(call) => match &call.batch {
                                Some(batch) => {
                                    if let Some(label) = &batch.header {
                                        let header = self.batch_header_lines(label);
                                        self.bake(header);
                                    }
                                    CallRecord::Batch(self.batch_item_lines(
                                        name,
                                        &call.input,
                                        batch.mixed,
                                    ))
                                }
                                None => match self.bake_call_start(name, &call.input) {
                                    Some(index) => CallRecord::HeaderBlock(index),
                                    None => CallRecord::Baked,
                                },
                            },
                            None => CallRecord::Baked,
                        };
                        let preview = result_preview(content);
                        self.bake_call_result(
                            name,
                            call.map(|c| &c.input),
                            &preview,
                            content,
                            *is_error,
                            record,
                        );
                    }
                }
                tcode_core::Entry::UserNote { text, .. } => {
                    // Approval annotations and `ask_user` answers both retain
                    // the person's original wording on resume. Live questions
                    // have a richer Q&A record, but that UI-only shape is not
                    // persisted in the ledger.
                    self.bake_user_note(text);
                }
                tcode_core::Entry::Note(text) => {
                    lines.push(Line::default());
                    lines.extend(quote_lines(Some(NOTE_LABEL), text));
                    lines.push(Line::default());
                }
            }
        }
        lines.push(Line::styled("── resumed ──", theme::dim()));
        if let Some(progress) = resumed_progress {
            self.update_progress(&progress);
        }
        self.bake(lines);
    }

    // ------------------------------------------------------------ turn

    /// Freeze the draft into the message it will stay: the blocks that go on
    /// the wire, plus what the transcript renders it from. Attachments are
    /// consumed here, so a queued prompt keeps the image that was pasted into
    /// it — and one whose inline token the user deleted drops it, exactly as
    /// when sending immediately.
    fn compose_draft(&mut self, input: String) -> PendingMessage {
        let mut attachments: Vec<String> = Vec::new();
        let mut blocks: Vec<ContentBlock> = Vec::new();
        for att in self.attachments.drain(..) {
            let placeholder = att.placeholder();
            if !input.contains(&placeholder) {
                continue;
            }
            match att {
                Attachment::Image {
                    id,
                    bytes,
                    media_type,
                    label,
                } => {
                    attachments.push(label);
                    if self.agent.model.snapshot().provider.supports_vision() {
                        use base64::Engine as _;
                        blocks.push(ContentBlock::Image {
                            media_type: media_type.into(),
                            data: base64::engine::general_purpose::STANDARD.encode(bytes),
                        });
                    } else {
                        let dir = self.scratch_dir.join("pasted");
                        let ext = match media_type {
                            "image/jpeg" => "jpg",
                            "image/png" => "png",
                            _ => "img",
                        };
                        let stamp = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis();
                        let path = dir.join(format!("paste-{stamp}-{id}.{ext}"));
                        match std::fs::create_dir_all(&dir)
                            .and_then(|()| std::fs::write(&path, bytes))
                        {
                            Ok(()) => blocks.push(ContentBlock::Text {
                                text: format!("[image saved to {}]", path.display()),
                            }),
                            Err(error) => {
                                self.notice = Some((
                                    format!("could not save pasted image: {error}"),
                                    Instant::now(),
                                ));
                                blocks.push(ContentBlock::Text {
                                    text: "[pasted image could not be saved; the current model cannot view it]".into(),
                                });
                            }
                        }
                    }
                }
                Attachment::Text { content, .. } => {
                    blocks.push(ContentBlock::Text {
                        text: format!("{placeholder}:\n{content}"),
                    });
                }
            }
        }
        self.next_attachment_id = 1;
        blocks.push(ContentBlock::Text {
            text: input.clone(),
        });
        PendingMessage {
            text: input,
            attachments,
            blocks,
        }
    }

    fn start_turn(&mut self, message: PendingMessage) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        // The user just answered the question the guess was asking.
        self.drop_suggestion();
        self.clear_live_text();
        if !self.progress.is_empty() && self.progress.iter().all(ProgressPhase::is_completed) {
            self.progress.clear();
        }
        // Until the provider reports authoritative prompt usage, keep the meter
        // useful with a conservative local estimate. Pasted text counts here
        // too; image token accounting is provider-specific.
        let prompt_tokens: u64 = message
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(approx_tokens(text.as_str()) as u64),
                _ => None,
            })
            .sum();
        self.context_tokens = session.last_prompt_tokens.saturating_add(prompt_tokens);
        self.context_step_start = self.context_tokens;
        // Echo the user input into the transcript, tagged with the ledger
        // index its User entry is about to occupy (rewind jumps to it).
        let entry_index = session.ledger.entries().len();
        self.transcript.push_tagged(
            prompt_echo(&message.text, &message.attachments),
            entry_index,
        );
        let blocks = message.blocks;

        let (tx, rx) = mpsc::channel(64);
        self.events_rx = Some(rx);
        self.turn_usage = Usage::default();
        self.delegated_usage = Usage::default();
        self.user_wait_started = None;
        self.user_wait_total = Duration::ZERO;
        self.out_tokens = 0;
        self.thinking_chars = 0;
        self.state_label = "sending".into();

        let cancel = CancellationToken::new();
        let agent = self.agent.clone();
        let approver = self.approver.clone();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move {
            let result = agent
                .user_turn(&mut session, blocks, &tx, &*approver, cancel2)
                .await;
            (session, result)
        });
        self.phase = Phase::Running {
            handle,
            cancel,
            started: Instant::now(),
        };
    }

    /// `/compact [focus]` runs like a turn (spinner, cancel, usage report)
    /// but drives `Agent::compact` instead of the tool loop. The optional focus
    /// tells the summarizer which details deserve special attention.
    fn start_compact(&mut self, focus: Option<String>) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        if session.ledger.is_empty() {
            self.session = Some(session);
            self.bake(vec![Line::styled("nothing to compact", theme::dim())]);
            return;
        }
        session.turn_usage = Usage::default();
        self.turn_usage = Usage::default();
        self.delegated_usage = Usage::default();
        self.user_wait_started = None;
        self.user_wait_total = Duration::ZERO;
        self.out_tokens = 0;
        // Legitimate prefix rewrite: don't false-alarm next turn.
        self.prev_cache_ratio = None;
        self.state_label = "compacting".into();
        // Compaction reports through the same event channel a turn does, so
        // its summary is baked by the one `Compacted` handler either way.
        let (tx, rx) = mpsc::channel(64);
        self.events_rx = Some(rx);
        let cancel = CancellationToken::new();
        let agent = self.agent.clone();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move {
            let result = agent
                .compact_with_focus(&mut session, focus.as_deref(), &tx, &cancel2)
                .await;
            (session, result)
        });
        self.phase = Phase::Running {
            handle,
            cancel,
            started: Instant::now(),
        };
    }

    fn on_turn_done(&mut self, done: (Session, Result<(), AgentError>)) {
        // The worker can finish before the UI select loop has received the
        // final queued AgentEvents. Process them before dropping the receiver,
        // otherwise a fast one-shot response (e.g. "hello") can vanish from
        // the transcript even though the model answered.
        self.drain_agent_events();
        self.bake_live_text();
        self.finish_thinking();

        let (mut session, result) = done;
        let elapsed = match &self.phase {
            Phase::Running { started, .. } => started
                .elapsed()
                .saturating_sub(self.user_wait_total)
                .saturating_sub(
                    self.user_wait_started
                        .map(|wait| wait.elapsed())
                        .unwrap_or_default(),
                )
                .as_secs_f32(),
            Phase::Idle => 0.0,
        };
        // The session's per-turn tally is authoritative (it also covers
        // compaction, which streams no Usage events to the UI).
        self.turn_usage = add_usage(session.turn_usage, self.delegated_usage);
        self.context_estimated = session.last_prompt_tokens == 0 && !session.ledger.is_empty();
        if self.context_estimated {
            session.last_prompt_tokens = self.agent.estimate_context_tokens(&session);
        }
        self.context_tokens = session.last_prompt_tokens;
        self.context_step_start = self.context_tokens;
        self.session = Some(session);
        self.phase = Phase::Idle;
        self.events_rx = None;
        self.state_label.clear();
        // A switch staged during the closing (non-tool) output never reached a
        // batch boundary. Now that the turn is over and we are idle again,
        // commit it here. If a queued prompt starts the next turn instead, its
        // own turn-start commit and note injection cover it.
        if let Some(mode) = self.session.as_mut().and_then(|s| s.commit_pending_mode()) {
            self.committed_mode = mode;
            self.mode_label = mode.label().to_string();
            self.bake(vec![Line::styled(
                format!("permission mode → {}", mode.label()),
                theme::dim(),
            )]);
        }
        let landed = result.is_ok();
        if let Err(e) = result {
            self.bake(vec![Line::styled(
                format!("error: {e}"),
                theme::error_highlight(),
            )]);
        }
        let u = self.turn_usage;
        self.bake(vec![turn_summary_line(elapsed, u)]);
        // Cache regression sentinel: an append-only ledger should keep
        // the hit share high; a sharp drop means something rewrote the
        // prefix and deserves attention now, not on the monthly bill.
        if u.total_input() > 0 {
            let ratio = u.cache_read_tokens as f64 / u.total_input() as f64;
            if let Some(prev) = self.prev_cache_ratio {
                if prev >= 0.5 && ratio < prev * 0.5 {
                    self.bake(vec![Line::styled(
                        format!(
                            "⚠ cache hit fell {:.0}% → {:.0}% — prompt prefix changed unexpectedly",
                            prev * 100.0,
                            ratio * 100.0
                        ),
                        ratatui::style::Style::default().fg(theme::WARN),
                    )]);
                }
            }
            self.prev_cache_ratio = Some(ratio);
        }
        // Whatever the loop never reached a boundary to deliver — a message
        // queued during the closing answer, or one queued right before ctrl+c —
        // becomes the next turn immediately. The user already pressed enter on
        // it; they should not have to press it again.
        if let Some(message) = merge(self.pending.take()) {
            self.start_turn(message);
            return;
        }
        // A turn that errored out leaves the user reading a failure, not
        // choosing a next step. `suggest_request` refuses interrupted turns on
        // the same principle; this catches the broken-stream case too.
        if landed {
            self.start_suggestion();
        }
    }

    fn drain_agent_events(&mut self) {
        while let Some(rx) = self.events_rx.as_mut() {
            let ev = rx.try_recv();
            match ev {
                Ok(ev) => self.on_agent_event(ev),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
    }

    fn on_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::Started => {
                // The retry succeeded (or this is the first attempt): drop the
                // countdown.
                self.retry_wait = None;
                self.context_step_start = self.context_tokens;
                self.state_label = "responding".into();
            }
            AgentEvent::TextDelta(t) => {
                self.finish_thinking();
                let tokens = approx_tokens(&t);
                self.out_tokens += tokens;
                self.context_tokens = self.context_tokens.saturating_add(tokens as u64);
                self.live_text.push_str(&t);
                self.refresh_live_text();
                self.state_label = "writing".into();
            }
            AgentEvent::ThinkingDelta(t) => {
                if self.thinking_since.is_none() {
                    self.thinking_since = Some(Instant::now());
                }
                let tokens = approx_tokens(&t);
                self.out_tokens += tokens;
                self.context_tokens = self.context_tokens.saturating_add(tokens as u64);
                self.thinking_chars += t.chars().count();
                self.thinking_text.push_str(&t);
                self.state_label = "thinking".into();
            }
            AgentEvent::ToolInputDelta(t) => {
                // Tool arguments are output tokens too; count them so the meter
                // moves while the model assembles a call. The call header itself
                // is rendered later by ToolStart, so nothing is baked here.
                let tokens = approx_tokens(&t);
                self.out_tokens += tokens;
                self.context_tokens = self.context_tokens.saturating_add(tokens as u64);
                self.state_label = "calling tool".into();
            }
            AgentEvent::ToolBatchStart { label, calls } => {
                // A batch supersedes any single diff pre-baked for its
                // (once) approval — retract it so the batch renders in full.
                if let Some(mark) = self.change_prebake.take() {
                    self.transcript.truncate_blocks(mark);
                }
                self.bake_live_text();
                self.finish_thinking();
                let header = self.batch_header_lines(&label);
                self.bake(header);
                self.pending_batch.clear();
                // A batch spanning several tools tags each item with its tool
                // name so the reader can tell the calls apart; a single-tool
                // batch (e.g. "Read 5 files") needs no per-item prefix.
                let mixed = calls
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<HashSet<_>>()
                    .len()
                    > 1;
                for (name, input) in calls {
                    self.pending_batch.push_back(PendingCall {
                        detail: serde_json::to_string_pretty(&input).unwrap_or_default(),
                        header: self.batch_item_lines(&name, &input, mixed),
                        header_index: None,
                    });
                }
                self.state_label = format!("running: {label}");
            }
            AgentEvent::ToolStart {
                name,
                summary,
                input,
            } => {
                match self.renderers.get(&name).route() {
                    CallRoute::Progress => {
                        self.update_progress(&input);
                        self.state_label = "updating progress".into();
                        return;
                    }
                    // The question and its answer are already baked by the
                    // approval dialog; a second header is noise.
                    CallRoute::Silent => return,
                    CallRoute::Transcript => {}
                }
                // Recompute the header from name+input so a long/multi-line
                // shell command collapses to `Shell` and renders as a block,
                // instead of the raw command string core put in `summary`.
                let _ = summary;
                let summary = self.display_summary(&self.renderers.get(&name).header(
                    &name,
                    &input,
                    Some(&self.cwd),
                ));
                self.bake_live_text();
                self.finish_thinking();
                if !self.pending_batch.is_empty() {
                    self.state_label = format!("running: {summary}");
                    return;
                }
                // If this call's diff was already baked in full while its
                // approval dialog was open, keep that block — don't render a
                // second, capped copy.
                let header_index = if self.change_prebake.take().is_none() {
                    self.bake_call_start(&name, &input)
                } else {
                    None
                };
                self.pending_tool = Some(PendingCall {
                    detail: serde_json::to_string_pretty(&input).unwrap_or_default(),
                    header: Vec::new(),
                    header_index,
                });
                self.state_label = format!("running: {summary}");
            }
            AgentEvent::ToolEnd {
                name,
                preview,
                content,
                is_error,
                ..
            } => {
                if !is_error
                    && self
                        .agent
                        .tools
                        .iter()
                        .any(|tool| tool.name() == name && tool.is_mutating())
                {
                    self.refresh_reference_index();
                }
                if !matches!(self.renderers.get(&name).route(), CallRoute::Transcript) {
                    self.state_label = "responding".into();
                    return;
                }
                let entry = self
                    .pending_tool
                    .take()
                    .or_else(|| self.pending_batch.pop_front());
                // Recover the call's input (stashed as JSON) to decide whether
                // the output is markdown before the result is appended to it.
                let (input, record) = match entry {
                    Some(entry) => {
                        let record = match entry.header_index {
                            Some(index) => CallRecord::HeaderBlock(index),
                            None if !entry.header.is_empty() => {
                                let header = entry.header;
                                CallRecord::Batch(header)
                            }
                            None => CallRecord::Baked,
                        };
                        (
                            serde_json::from_str::<serde_json::Value>(&entry.detail).ok(),
                            record,
                        )
                    }
                    None => (None, CallRecord::Baked),
                };
                // The gated result is exactly what is appended to the next
                // model request, so it belongs in the in-between estimate.
                self.context_tokens = self
                    .context_tokens
                    .saturating_add(approx_tokens(&content) as u64);
                self.bake_call_result(&name, input.as_ref(), &preview, &content, is_error, record);
                self.state_label = "responding".into();
            }
            AgentEvent::ReferencesExpanded {
                labels,
                added_tokens,
            } => {
                self.context_tokens = self.context_tokens.saturating_add(added_tokens as u64);
                let count = labels.len();
                let summary = labels.into_iter().take(2).collect::<Vec<_>>().join(", ");
                let more = (count > 2)
                    .then(|| format!(" +{}", count - 2))
                    .unwrap_or_default();
                self.notice = Some((format!("referenced {summary}{more}"), Instant::now()));
            }
            AgentEvent::QueuedInput {
                text,
                attachments,
                entry_index,
            } => {
                // It has left the queue and is now history: same renderer as a
                // prompt sent at the start of a turn, tagged with its ledger
                // index so rewind can jump to it. The dim waiting row goes away
                // on its own — it was only ever a view of the queue.
                self.bake_live_text();
                self.finish_thinking();
                self.transcript
                    .push_tagged(prompt_echo(&text, &attachments), entry_index);
            }
            AgentEvent::UserNote { text, answer } => {
                // `ask_user` already has a dedicated Q&A record. Approval
                // annotations arrive after ToolEnd and use the exact helper
                // replay calls for Entry::UserNote.
                if !answer {
                    self.bake_user_note(&text);
                }
            }
            AgentEvent::Retrying {
                attempt,
                max,
                error,
                partial_output_retained,
                delay_ms,
            } => {
                // A failed attempt is valid human-facing history, but never
                // provider history. Bake its live text before the retry starts;
                // the core ledger persists the same text as IncompleteAssistant.
                self.bake_live_text();
                self.finish_thinking();
                self.context_tokens = self.context_step_start;
                // Record the failure in red scrollback, then show a live
                // countdown in the status line until the next attempt fires.
                let retained = if partial_output_retained {
                    " — incomplete response retained; not sent back to model"
                } else {
                    ""
                };
                self.bake(vec![Line::styled(
                    format!("↻ API error ({attempt}/{max}): {error}{retained}"),
                    theme::error_highlight(),
                )]);
                self.retry_wait = Some(RetryWait {
                    until: Instant::now() + Duration::from_millis(delay_ms),
                    attempt,
                    max,
                });
                self.state_label = format!("retrying ({attempt}/{max})");
            }
            AgentEvent::Usage(u) => {
                self.turn_usage.input_tokens += u.input_tokens;
                self.turn_usage.output_tokens += u.output_tokens;
                self.turn_usage.cache_read_tokens += u.cache_read_tokens;
                self.turn_usage.cache_write_tokens += u.cache_write_tokens;
                // Providers report the full prompt (cached tokens included)
                // plus this response; this is the most accurate context
                // figure available to the TUI.
                self.context_tokens = u.total_input().saturating_add(u.output_tokens);
                self.context_step_start = self.context_tokens;
                self.context_estimated = false;
            }
            AgentEvent::RateLimits(limits) => self.rate_limits = Some(limits),
            AgentEvent::DelegatedUsage(u) => {
                // Sub-agent requests are billable and should animate the
                // turn's token counter, but run in an isolated context.
                self.delegated_usage = add_usage(self.delegated_usage, u);
                self.turn_usage = add_usage(self.turn_usage, u);
                self.out_tokens = self.out_tokens.saturating_add(u.output_tokens as usize);
                self.state_label = "sub-agent working".into();
            }
            AgentEvent::Compacting => {
                self.bake(vec![Line::styled(
                    "✦ context near limit — compacting earlier history",
                    ratatui::style::Style::default().fg(theme::WARN),
                )]);
                self.state_label = "compacting".into();
                // Legitimate prefix rewrite: don't false-alarm next turn.
                self.prev_cache_ratio = None;
            }
            AgentEvent::Compacted(summary) => self.bake_compacted(&summary),
            AgentEvent::ModeChanged(mode) => {
                // A staged switch just committed at a batch boundary. Promote
                // the pending marker to the real mode and record where in the
                // transcript it took effect.
                self.committed_mode = mode;
                self.mode_label = mode.label().to_string();
                self.bake(vec![Line::styled(
                    format!("permission mode → {}", mode.label()),
                    theme::dim(),
                )]);
            }
            AgentEvent::AutoModePaused(notice) => {
                self.bake(vec![Line::styled(
                    format!("⊙ {notice}"),
                    ratatui::style::Style::default().fg(theme::WARN),
                )]);
                self.state_label = "manual approvals required".into();
            }
            AgentEvent::AwaitingUserInput => {
                self.bake(vec![Line::styled(
                    "⊙ change declined — add guidance in the input to continue",
                    ratatui::style::Style::default().fg(theme::WARN),
                )]);
                self.state_label = "waiting for your instruction".into();
            }
            AgentEvent::StepLimitReached { max } => {
                self.bake(vec![Line::styled(
                    format!("⊙ step limit reached ({max} steps) — say \"continue\" to keep going"),
                    ratatui::style::Style::default().fg(theme::WARN),
                )]);
                self.state_label = "waiting for your instruction".into();
            }
            AgentEvent::Interrupted => {
                self.bake_live_text();
                self.finish_thinking();
                self.bake(vec![Line::styled(
                    "⨯ interrupted",
                    ratatui::style::Style::default().fg(theme::WARN),
                )]);
            }
            AgentEvent::TurnEnd => {
                self.bake_live_text();
                self.finish_thinking();
            }
        }
    }

    /// Bake the original human wording of an approved annotation. Both the
    /// post-result core event and resumed `Entry::UserNote` reach this path.
    fn bake_user_note(&mut self, text: &str) {
        let mut lines = vec![Line::default()];
        lines.extend(quote_lines(Some(NOTE_LABEL), text));
        lines.push(Line::default());
        self.bake(lines);
    }

    /// Transcript record of a consent decision. An approved call renders via
    /// its ToolStart (header + diff); its annotation arrives only after the
    /// result through `AgentEvent::UserNote`. A declined call (which never
    /// emits ToolStart) leaves a one-line record — the proposed diff never
    /// reaches the transcript.
    fn bake_approval_record(&mut self, dialog: &Dialog, approval: &Approval) {
        // Flush streamed text so the record keeps chronological order.
        self.bake_live_text();
        self.finish_thinking();
        if dialog.is_plan() {
            match approval.decision {
                // Approved: the tool runs, so its ToolStart bakes the plan and
                // its ToolEnd result + the ModeChanged event carry the record.
                // Nothing to add here.
                ApprovalDecision::Yes
                | ApprovalDecision::YesSession
                | ApprovalDecision::YesProject => {}
                ApprovalDecision::No => {
                    // Keep-planning: no ToolStart/ToolEnd will fire, so bake the
                    // plan here (through the same ExitPlanRenderer path replay
                    // uses) followed by the decision + feedback.
                    if let Some(input) = dialog.plan_input() {
                        self.bake_call_start("exit_plan", &input);
                    }
                    let reason = approval
                        .comment
                        .as_deref()
                        .map(|c| format!(" — {c}"))
                        .unwrap_or_default();
                    self.bake(vec![Line::styled(
                        format!("  ⎿ keep planning{reason}"),
                        ratatui::style::Style::default().fg(theme::WARN),
                    )]);
                }
            }
            return;
        }
        if dialog.is_question() {
            let answer = approval.comment.clone().unwrap_or_default();
            let mut lines = vec![Line::default()];
            for (i, row) in dialog.summary.lines().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled("? ", theme::accent()),
                        Span::styled(row.to_string(), theme::bold()),
                    ]));
                } else {
                    lines.push(Line::styled(format!("  {row}"), theme::dim()));
                }
            }
            for row in answer.lines() {
                lines.push(Line::styled(format!("  ⎿ {row}"), theme::dim()));
            }
            self.bake(lines);
            return;
        }
        match approval.decision {
            // The agent emits `UserNote` only after its tool result commits;
            // drawing it here would put live scrollback ahead of replay.
            ApprovalDecision::Yes | ApprovalDecision::YesSession | ApprovalDecision::YesProject => {
            }
            ApprovalDecision::No => {
                // Retract the diff baked while the dialog was open — a
                // declined change leaves only its one-line record.
                if let Some(mark) = self.change_prebake.take() {
                    self.transcript.truncate_blocks(mark);
                }
                let reason = approval
                    .comment
                    .as_deref()
                    .map(|c| format!(" — {c}"))
                    .unwrap_or_default();
                let mut spans = self.colored_tool_summary(&dialog.call_summary);
                spans.insert(0, Span::styled("● ", theme::accent()));
                self.bake(vec![
                    Line::default(),
                    Line::from(spans),
                    Line::styled(
                        format!("  ⎿ declined{reason}"),
                        ratatui::style::Style::default().fg(theme::ERROR),
                    ),
                ]);
            }
        }
    }

    /// Open the plan under review in `$EDITOR`, then feed any change back into
    /// the pane. The TUI owns the terminal, so it is suspended (leave the
    /// alternate screen, drop raw mode and the input hooks) around the child
    /// process, then restored and fully redrawn. A revision equal to the
    /// original is a no-op inside `revise_plan`.
    fn edit_plan_externally(&mut self) {
        let Some(source) = self.dialog.as_ref().and_then(|(d, _)| d.plan_source()) else {
            return;
        };

        // Stage the plan in a unique project-scratchpad file. A fixed name
        // would let simultaneous tcode sessions overwrite each other's review.
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let path = self
            .scratch_dir
            .join(format!("plan-review-{}-{nonce}.md", std::process::id()));
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&path, &source).is_err() {
            self.notice = Some((
                "could not stage the plan for editing".into(),
                Instant::now(),
            ));
            return;
        }

        // Suspend the TUI around the editor, then restore it and force a full
        // repaint (the child scribbled over the alternate screen).
        use crossterm::event::{
            DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        };
        use crossterm::terminal::{
            disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
        };
        let mut out = std::io::stdout();
        let _ = crossterm::execute!(
            out,
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();

        let launched = editor_command(&path).status();

        let _ = enable_raw_mode();
        let _ = crossterm::execute!(
            out,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        );
        let _ = self.terminal.clear();

        if launched.is_err() {
            let _ = std::fs::remove_file(&path);
            self.notice = Some(("could not launch $EDITOR".into(), Instant::now()));
            return;
        }

        let edited = std::fs::read_to_string(&path);
        // This is review-only staging, never a durable plan artifact. The
        // approved tool writes the permanent copy under `plans/`.
        let _ = std::fs::remove_file(&path);
        let Ok(edited) = edited else {
            self.notice = Some(("could not read the edited plan".into(), Instant::now()));
            return;
        };
        // Re-split and pre-render the revised plan for the pane (which has no
        // markdown renderer of its own).
        let blocks = markdown::split_blocks(edited.trim())
            .into_iter()
            .map(|block| {
                let document = self.md.parse(&block);
                (block, document)
            })
            .collect();
        if let Some((dialog, _)) = self.dialog.as_mut() {
            dialog.revise_plan(edited, blocks);
        }
    }

    fn finish_thinking(&mut self) {
        if let Some(since) = self.thinking_since.take() {
            let secs = since.elapsed().as_secs().max(1);
            let text = std::mem::take(&mut self.thinking_text);
            self.bake_thinking(
                &format!("reasoning for {secs}s (~{} tok)", self.thinking_chars / 3),
                &text,
            );
            self.thinking_chars = 0;
        }
    }

    /// Provider reasoning remains in the ledger for replay, but is opt-in in the
    /// transcript. When shown it is deliberately plain text rather than
    /// Markdown, so an expanded detail does not compete with the answer.
    fn bake_thinking(&mut self, title: &str, text: &str) {
        if !self.show_reasoning || text.trim().is_empty() {
            return;
        }
        let head = vec![Line::styled(format!("✻ {title}"), theme::dim())];
        let detail = text
            .lines()
            .map(|line| Line::raw(line.to_string()))
            .collect();
        self.transcript
            .push_with_detail(head, detail, false, OUTPUT_VIEW_ROWS);
    }

    /// The `── earlier conversation compacted ──` divider, folding open to the
    /// summary that replaced the history. Announcing the compaction is not
    /// enough: that summary is now the model's entire record of everything
    /// before it, so the user has to be able to read what it is working from.
    /// Live and replay share this entry point.
    fn bake_compacted(&mut self, summary: &str) {
        // The blank belongs to the head so the fold affordance still lands on
        // the divider — `display_head` appends it to the last head line.
        let head = vec![
            Line::default(),
            Line::styled("── earlier conversation compacted ──", theme::dim()),
        ];
        self.transcript.push_with_markdown_detail(
            head,
            self.md.parse(summary),
            Vec::new(),
            false,
            OUTPUT_VIEW_ROWS,
        );
        self.bake(vec![Line::default()]);
    }

    fn refresh_live_text(&mut self) {
        if self.live_text.trim().is_empty() {
            return;
        }
        let document = self.md.parse(&self.live_text);
        if document.is_empty() {
            return;
        }
        if let Some(index) = self.live_block {
            self.transcript.replace_markdown_block(index, document);
        } else {
            let index = self.transcript.block_count();
            self.transcript.push_markdown(document);
            self.live_block = Some(index);
        }
    }

    fn clear_live_text(&mut self) {
        self.live_text.clear();
        if let Some(index) = self.live_block.take() {
            self.transcript.truncate_blocks(index);
        }
    }

    fn bake_live_text(&mut self) {
        if self.live_text.trim().is_empty() {
            self.clear_live_text();
            return;
        }
        let text = std::mem::take(&mut self.live_text);
        let document = self.md.parse(&text).with_trailing_blank();
        if let Some(index) = self.live_block.take() {
            self.transcript.replace_markdown_block(index, document);
        } else {
            self.transcript.push_markdown(document);
        }
    }

    /// Tool's UI name, resolved from its own `display_name` when it belongs
    /// to this session; falls back to title-case for imported/unknown tools.
    fn display_name(&self, name: &str) -> String {
        self.renderers.display_name(name)
    }

    /// Split a tool summary like `shell(cargo test)` into colored spans: the
    /// tool's display name is green, the arguments are dim.
    fn colored_tool_summary(&self, summary: &str) -> Vec<Span<'static>> {
        // The tool name is always the accent-colored part, argument or not: a
        // long shell command collapses to a bare `Run`, and an argument-less
        // summary (MCP tools) must not read as a different kind of record.
        match summary.find('(') {
            Some(paren) => vec![
                Span::styled(self.display_name(&summary[..paren]), theme::ok()),
                Span::styled(summary[paren..].to_string(), theme::dim()),
            ],
            None => vec![Span::styled(self.display_name(summary), theme::ok())],
        }
    }

    /// `●` header (+ change body + command block) for one tool call. Shared
    /// by the live `ToolStart` path and transcript replay so they can never
    /// drift apart.
    fn call_lines(&self, name: &str, input: &serde_json::Value) -> Vec<Line<'static>> {
        let renderer = self.renderers.get(name);
        let summary = self.display_summary(&renderer.header(name, input, Some(&self.cwd)));
        let mut spans: Vec<Span> = self.colored_tool_summary(&summary);
        spans.insert(0, Span::styled("● ", theme::accent()));
        let mut lines = vec![Line::from(spans)];
        lines.extend(renderer.body(input));
        lines
    }

    /// Bake a single call's `●` block. The blank row above is what keeps
    /// consecutive calls from running together — live and replayed alike.
    /// Long shell commands attach their command text as a closed detail here,
    /// then append their result to that same detail at `ToolEnd`.
    ///
    /// A bare header (no diff, no command block) becomes its own transcript
    /// block and its index is returned: the result attaches to that very
    /// row at `ToolEnd`, so hover and fold live on the tool line itself
    /// instead of a separate `⎿` row beneath it.
    fn bake_call_start(&mut self, name: &str, input: &serde_json::Value) -> Option<usize> {
        let mut lines = self.call_lines(name, input);
        self.bake(vec![Line::default()]);
        if lines.len() == 1 {
            let index = self.transcript.block_count();
            self.bake(lines);
            let detail = self.renderers.get(name).initial_detail(input);
            if !detail.is_empty() {
                self.transcript
                    .attach_detail(index, detail, OUTPUT_VIEW_ROWS);
            }
            return Some(index);
        }
        lines.push(Line::default());
        self.bake(lines);
        None
    }

    /// The `● label` row opening a batch, with its separating blank above.
    fn batch_header_lines(&self, label: &str) -> Vec<Line<'static>> {
        let mut spans = vec![Span::styled("● ", theme::accent())];
        spans.extend(colored_batch_label(label));
        vec![Line::default(), Line::from(spans)]
    }

    /// One batch item's indented summary row (plus any diff). It is baked at
    /// the item's own result so each call stays immediately above its output.
    /// Change bodies retain their trailing separator so adjacent diffs never
    /// visually merge.
    fn batch_item_lines(
        &self,
        name: &str,
        input: &serde_json::Value,
        mixed: bool,
    ) -> Vec<Line<'static>> {
        let renderer = self.renderers.get(name);
        let mut row = vec![Span::styled(BATCH_ITEM_INDENT, theme::dim())];
        if mixed {
            // Keep per-item tool tags subdued: the batch header is where
            // display names get title-cased and highlighted.
            row.push(Span::styled(format!("{name} "), theme::dim()));
        }
        row.push(Span::styled(
            renderer.batch_item(name, input, Some(&self.cwd)),
            theme::dim(),
        ));
        let mut lines = vec![Line::from(row)];
        let body = renderer.body(input);
        if !body.is_empty() {
            lines.extend(body);
            lines.push(Line::default());
        }
        lines
    }

    /// Bake one call's result, shared by live `ToolEnd` and replay. The
    /// result lands on the call's own row whenever there is one — preview
    /// appended, fold affordance on hover — so no record spends an extra
    /// line on a separate result marker. Only `Baked` records (a diff or
    /// command block between header and output) keep the `⎿ preview` row.
    fn bake_call_result(
        &mut self,
        name: &str,
        input: Option<&serde_json::Value>,
        preview: &str,
        content: &str,
        is_error: bool,
        record: CallRecord,
    ) {
        let style = if is_error {
            ratatui::style::Style::default().fg(theme::ERROR)
        } else {
            theme::dim()
        };
        match self.result_render(name, input, preview, content, is_error) {
            ResultRender::Nothing => {
                if let CallRecord::Batch(header) = record {
                    self.bake(header);
                }
            }
            ResultRender::Inline(line) => match record {
                CallRecord::HeaderBlock(index) => self
                    .transcript
                    .extend_head(index, preview_tail(preview, style)),
                CallRecord::Batch(mut header) => {
                    append_result_preview(&mut header, preview, style);
                    self.bake(header);
                }
                CallRecord::Baked => self.bake(vec![line]),
            },
            ResultRender::Foldable { head, detail } => {
                // Quiet tools (read/grep/glob) skip the preview: the fold
                // affordance already states the line count on hover. Long
                // single shell calls likewise keep both their command and
                // output inside the foldout.
                let renderer = self.renderers.get(name);
                let hide_preview = !is_error
                    && (renderer.quiet_output()
                        || input.is_some_and(|input| renderer.folds_result(input)));
                let label = if is_error {
                    renderer.error_label().unwrap_or(preview)
                } else {
                    preview
                };
                match record {
                    CallRecord::HeaderBlock(index) => {
                        if !hide_preview {
                            self.transcript
                                .extend_head(index, preview_tail(label, style));
                        }
                        self.attach_result_detail(
                            index,
                            detail,
                            input.is_some_and(|input| renderer.folds_result(input)),
                        );
                    }
                    CallRecord::Batch(mut header) => {
                        if !hide_preview {
                            append_result_preview(&mut header, label, style);
                        }
                        self.push_result_detail(header, detail);
                    }
                    CallRecord::Baked => self.push_result_detail(head, detail),
                }
            }
        }
    }

    fn attach_result_detail(&mut self, index: usize, detail: ResultDetail, append: bool) {
        match detail {
            ResultDetail::Lines(lines) if append => {
                self.transcript
                    .append_detail(index, lines, OUTPUT_VIEW_ROWS)
            }
            ResultDetail::Lines(lines) => {
                self.transcript
                    .attach_detail(index, lines, OUTPUT_VIEW_ROWS)
            }
            ResultDetail::Markdown(document) => self.transcript.attach_markdown_detail(
                index,
                document,
                vec![Span::styled("  │ ", theme::dim())],
                OUTPUT_VIEW_ROWS,
            ),
        }
    }

    fn push_result_detail(&mut self, head: Vec<Line<'static>>, detail: ResultDetail) {
        match detail {
            ResultDetail::Lines(lines) => {
                self.transcript
                    .push_with_detail(head, lines, false, OUTPUT_VIEW_ROWS)
            }
            ResultDetail::Markdown(document) => self.transcript.push_with_markdown_detail(
                head,
                document,
                vec![Span::styled("  │ ", theme::dim())],
                false,
                OUTPUT_VIEW_ROWS,
            ),
        }
    }

    /// How one tool result renders, shared by live `ToolEnd` and replay.
    /// `Nothing`: the call site already told the story (successful edit/write
    /// diffs). `Inline`: a single `⎿ preview` row. `Foldable`: a head row
    /// carrying the fold affordance plus the collapsed body.
    fn result_render(
        &self,
        name: &str,
        input: Option<&serde_json::Value>,
        preview: &str,
        content: &str,
        is_error: bool,
    ) -> ResultRender {
        let renderer = self.renderers.get(name);
        if !is_error && renderer.hide_success_result() {
            return ResultRender::Nothing;
        }
        let style = if is_error {
            ratatui::style::Style::default().fg(theme::ERROR)
        } else {
            theme::dim()
        };
        if is_error {
            if let Some(label) = renderer.error_label() {
                let diagnostic = if content.trim().is_empty() {
                    preview
                } else {
                    content
                };
                return ResultRender::Foldable {
                    head: vec![Line::styled(format!("  {label}"), style)],
                    detail: self.output_detail(name, input, preview, diagnostic, true, false),
                };
            }
        }
        let folded_result = input.is_some_and(|input| renderer.folds_result(input));
        let detail = self.output_detail(name, input, preview, content, is_error, !folded_result);
        if detail.is_empty() {
            ResultRender::Inline(Line::styled(format!("  ⎿ {preview}"), style))
        } else {
            let quiet = !is_error && renderer.quiet_output();
            // The fold affordance already carries the line count ("▸ N
            // lines"); a quiet tool's head only marks the result.
            let head = if quiet || folded_result {
                Line::styled("  ⎿", style)
            } else {
                Line::styled(format!("  ⎿ {preview}"), style)
            };
            ResultRender::Foldable {
                head: vec![head],
                detail,
            }
        }
    }

    /// The foldable body of a tool result. Markdown-shaped output (a
    /// `web_fetch`, or a `read` of a `.md` file) is rendered; everything
    /// else stays literal. Either way a left gutter bar delineates the
    /// expanded region, and the first line is dropped only when the head has
    /// already shown it as a preview.
    fn output_detail(
        &self,
        name: &str,
        input: Option<&serde_json::Value>,
        preview: &str,
        content: &str,
        is_error: bool,
        preview_visible: bool,
    ) -> ResultDetail {
        let renderer = self.renderers.get(name);
        let quiet = renderer.quiet_output();
        if preview_visible && content.trim() == preview.trim() && (is_error || !quiet) {
            return ResultDetail::Lines(Vec::new()); // nothing beyond the preview
        }
        let gutter = || Span::styled("  │ ", theme::dim());
        if !is_error && renderer.markdown_detail(input) {
            return ResultDetail::Markdown(self.md.parse(content));
        }
        let text_style = if is_error {
            ratatui::style::Style::default().fg(theme::ERROR)
        } else {
            ratatui::style::Style::default()
        };
        // When the head shows the real preview, skip that first line in the
        // foldout; quiet tools such as read/grep use a generic head, so the
        // expanded body must include line 1.
        let first = content.lines().next().unwrap_or("");
        let skip = usize::from(
            preview_visible
                && !quiet
                && first.chars().count() <= 120
                && content.lines().count() > 1,
        );
        ResultDetail::Lines(
            content
                .lines()
                .skip(skip)
                .map(|line| Line::from(vec![gutter(), Span::styled(line.to_string(), text_style)]))
                .collect(),
        )
    }

    fn editor_visual_up(&mut self) -> bool {
        let layout = editor_layout(&self.editor, area_width(&self.terminal));
        move_editor_visual(&mut self.editor, &layout, VisualMove::Up)
    }

    fn editor_visual_down(&mut self) -> bool {
        let layout = editor_layout(&self.editor, area_width(&self.terminal));
        move_editor_visual(&mut self.editor, &layout, VisualMove::Down)
    }

    // ------------------------------------------------------------ keys

    fn on_dialog_mouse(&mut self, mouse: MouseEvent) {
        // A proposed edit/write is deliberately baked into the transcript in
        // full before its compact approval dialog opens. The dialog must not
        // trap the wheel, or a long diff becomes visible only through the few
        // transcript rows left above the panel with no way to inspect it.
        let plan_owns_wheel = self
            .dialog
            .as_ref()
            .is_some_and(|(dialog, _)| dialog.owns_wheel());
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
        let height = area_height(&self.terminal);
        let budget = height.saturating_sub(6);
        let Some((dialog, _)) = self.dialog.as_mut() else {
            return;
        };
        // Match `redraw`'s panel geometry so a screen coordinate is translated
        // to the panel's border-free content row, not the transcript behind
        // the modal.
        let lines = dialog.render(width, budget);
        let panel_h = (lines.len() as u16 + 2)
            .min(height.saturating_sub(4))
            .max(1);
        let panel_top = height.saturating_sub(panel_h);
        if mouse.row <= panel_top || mouse.row >= panel_top + panel_h.saturating_sub(1) {
            return;
        }
        let row = mouse.row.saturating_sub(panel_top + 1) as usize;
        let col = mouse.column.saturating_sub(1) as usize;
        if !dialog.is_plan() {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                dialog.note_mouse_down(row, col, width);
            }
            return;
        }
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
            MouseEventKind::Up(MouseButton::Left) => dialog.plan_mouse_up(row, col, width, budget),
            _ => {}
        }
    }

    fn on_term_event(&mut self, ev: Event) {
        match ev {
            Event::Key(key) if key.kind != crossterm::event::KeyEventKind::Release => {
                self.on_key(key)
            }
            Event::Paste(text) => {
                // A dialog owns interaction while it is on screen. In
                // particular, multiline terminal pastes must not leak into
                // the hidden main editor and then make the restored panel jump.
                if let Some((dialog, _)) = self.dialog.as_mut() {
                    dialog.paste_text(text);
                } else if self.resume_picker.is_none()
                    && self.model_picker.is_none()
                    && self.agent_picker.is_none()
                    && self.rewind_nav.is_none()
                {
                    self.on_paste_text(text);
                }
            }
            Event::Mouse(mouse) => {
                // A modal approval owns mouse input too. Without this guard a
                // plan drag selects and copies the hidden transcript instead of
                // producing a plan comment.
                if self.dialog.is_some() {
                    self.on_dialog_mouse(mouse);
                    return;
                }
                match mouse.kind {
                    MouseEventKind::Moved => self.transcript.mouse_moved(mouse.column, mouse.row),
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
                            self.transcript
                                .wheel(mouse.column, mouse.row, up, WHEEL_STEP);
                        }
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        self.drag_scroll = None;
                        let taken_by_input = self.input_mouse_down(mouse.column, mouse.row);
                        if !taken_by_input {
                            self.transcript.mouse_down(mouse.column, mouse.row);
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if self.input_mouse_active {
                            self.input_mouse_drag(mouse.column, mouse.row);
                        } else {
                            self.transcript.mouse_drag(mouse.column, mouse.row);
                            // Arm edge auto-scroll when the drag reaches a view
                            // edge; disarm the moment it returns inside.
                            self.drag_scroll = self
                                .transcript
                                .drag_edge(mouse.row)
                                .map(|up| (up, mouse.column, mouse.row));
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
            Event::Resize(..) => self.transcript.clear_hover(),
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        // Model picker captures everything while open.
        if let Some(picker) = self.resume_picker.as_mut() {
            match picker.handle_key(key) {
                ResumePickResult::Pending => {}
                ResumePickResult::Cancelled => self.resume_picker = None,
                ResumePickResult::Import => self.resume_picker = Some(resume::Picker::sources()),
                ResumePickResult::Current(id) => {
                    self.resume_picker = None;
                    self.resume_session(&id);
                }
                ResumePickResult::Source(source) => self.open_external_resume_picker(source),
                ResumePickResult::External(external) => {
                    self.resume_picker = None;
                    self.import_external_session(external);
                }
            }
            return;
        }

        if let Some(picker) = self.model_picker.as_mut() {
            match picker.handle_key(key) {
                model_picker::PickResult::Pending => {}
                model_picker::PickResult::Cancelled => self.model_picker = None,
                model_picker::PickResult::Picked { option, effort } => {
                    self.model_picker = None;
                    // `/model` has no inherit row, so the option is always set.
                    if let Some(index) = option {
                        self.apply_model(index, effort);
                    }
                }
            }
            return;
        }

        if let Some(picker) = self.agent_picker.as_mut() {
            match picker.handle_key(key, &self.menu, &self.agents) {
                model_picker::AgentPick::Pending => {}
                model_picker::AgentPick::Cancelled => self.agent_picker = None,
                model_picker::AgentPick::Picked {
                    kind,
                    option,
                    effort,
                } => {
                    self.agent_picker = None;
                    self.apply_agent_model(&kind, option, effort);
                }
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

        // Approval dialog captures everything while open.
        if let Some((dialog, _)) = self.dialog.as_mut() {
            match dialog.handle_key(key) {
                DialogResult::Pending => {}
                // Open the plan in `$EDITOR`, then feed any revision back into
                // the pane. Handled out here because it suspends the terminal.
                DialogResult::EditPlan => self.edit_plan_externally(),
                DialogResult::Done(approval) => {
                    let (dialog, reply) = self.dialog.take().expect("dialog present");
                    // A plan approval chose the mode execution runs under. The
                    // agent loop applies it to the Session; mirror it into the
                    // frontend's own view so the status line updates at once.
                    if let Some(mode) = approval.set_mode {
                        self.committed_mode = mode;
                        self.mode_label = mode.label().to_string();
                        self.pending_mode.clear();
                    }
                    self.bake_approval_record(&dialog, &approval);
                    let _ = reply.send(approval);
                    if let Some(wait_started) = self.user_wait_started.take() {
                        self.user_wait_total += wait_started.elapsed();
                    }
                }
            }
            return;
        }

        let running = matches!(self.phase, Phase::Running { .. });
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
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
                    // Anything queued was queued to be said *now* — cancelling
                    // hands it to the turn that starts on the way out.
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
            KeyCode::Enter => self.submit(running),
            KeyCode::BackTab => self.cycle_mode(),
            KeyCode::Tab => {
                if let Some(completion) = self.popup_selection() {
                    match completion.kind {
                        CompletionKind::Slash => {
                            self.dismissed_reference = None;
                            self.editor.clear();
                            self.editor.insert_str(&completion.replacement);
                        }
                        CompletionKind::Reference { start, end } => {
                            self.editor
                                .replace_range(start, end, &completion.replacement);
                            self.dismissed_reference = Some(start);
                        }
                    }
                    self.popup_index = 0;
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

    fn submit(&mut self, running: bool) {
        let text = self.editor.text();
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() && self.attachments.is_empty() {
            return;
        }
        if trimmed.starts_with('/') {
            let cmd = self
                .popup_selection()
                .filter(|completion| matches!(&completion.kind, CompletionKind::Slash))
                .map(|completion| completion.replacement)
                .unwrap_or_else(|| trimmed.clone());
            self.editor.take();
            self.run_slash(&cmd);
            return;
        }
        if running {
            // The turn owns the Session, and a user entry cannot be spliced
            // between a tool call and its result anyway. Queue the finished
            // message — attachments and all — for the loop's next boundary;
            // ctrl+c sends it right away.
            self.editor.take();
            let message = self.compose_draft(trimmed);
            self.pending.push(message);
            return;
        }
        let input = self.editor.take();
        // Sending a message means the user is done reading history.
        self.transcript.scroll_to_bottom();
        let message = self.compose_draft(input);
        self.start_turn(message);
    }

    /// shift+tab cycles the permission mode. Idle, it takes effect at once;
    /// while a turn runs, the running turn owns the `Session`, so the switch is
    /// staged and the agent loop commits it at the next batch boundary. Either
    /// way the cycle steps from staged-else-committed, so repeated presses
    /// collapse to the final target rather than restacking.
    fn cycle_mode(&mut self) {
        let base = self.pending_mode.get().unwrap_or(self.committed_mode);
        let next = base.cycle();
        self.persist_mode(next);
        match self.session.as_mut() {
            Some(session) => {
                session.mode = next;
                self.committed_mode = next;
                self.pending_mode.clear();
                self.mode_label = next.label().to_string();
            }
            None => {
                self.pending_mode.set(next);
                // A staged target shows with an arrow so the user is never
                // misled into thinking the plan gate is already active while
                // the current batch still runs under the old mode.
                self.mode_label = format!("→ {}", next.label());
            }
        }
    }

    /// Persist the chosen mode as the default for new sessions — except Unsafe:
    /// a one-off flip to it must not silently arm every future session, so
    /// landing there clears the stored choice instead.
    fn persist_mode(&self, mode: tcode_core::PermissionMode) {
        tcode_core::config::ModelState::update(|state| {
            state.mode = (mode != tcode_core::PermissionMode::Unsafe).then_some(mode);
        });
    }

    // ---------------------------------------------------------- rewind

    fn open_rewind(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let candidates: Vec<RewindCandidate> = session
            .ledger
            .entries()
            .iter()
            .enumerate()
            .filter_map(|(i, e)| match e {
                tcode_core::Entry::User(blocks) => {
                    let text = blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text }
                                if !text.starts_with("<tcode-status>")
                                    && !text.starts_with("[pasted content]") =>
                            {
                                Some(text.as_str())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    (!text.is_empty()).then(|| RewindCandidate {
                        index: i,
                        text,
                        dirty: session.checkpoints.dirty_since(i),
                    })
                }
                _ => None,
            })
            .collect();
        if candidates.is_empty() {
            self.bake(vec![Line::styled("nothing to rewind to", theme::dim())]);
            return;
        }
        self.rewind_nav = Some(RewindNav {
            pos: candidates.len() - 1,
            candidates,
            saved_input: self.editor.text(),
        });
        self.apply_rewind_nav();
    }

    /// Show the current navigation target: highlight + scroll its echo
    /// into view, prefill the editor with the original input.
    fn apply_rewind_nav(&mut self) {
        let Some(nav) = &self.rewind_nav else {
            return;
        };
        let candidate = &nav.candidates[nav.pos];
        let text = candidate.text.clone();
        self.transcript.highlight_entry(candidate.index);
        self.editor.clear();
        self.editor.insert_str(&text);
    }

    fn exit_rewind_nav(&mut self) {
        if let Some(nav) = self.rewind_nav.take() {
            self.transcript.clear_highlight();
            self.transcript.scroll_to_bottom();
            self.editor.clear();
            self.editor.insert_str(&nav.saved_input);
        }
    }

    fn confirm_rewind_nav(&mut self, restore_files: bool) {
        let Some(nav) = self.rewind_nav.take() else {
            return;
        };
        self.transcript.clear_highlight();
        let candidate = &nav.candidates[nav.pos];
        self.do_rewind(candidate.index, restore_files, candidate.text.clone());
    }

    fn do_rewind(&mut self, index: usize, restore_files: bool, text: String) {
        // The turn it was predicting from is about to stop existing.
        self.drop_suggestion();
        let Some(session) = self.session.as_mut() else {
            return;
        };
        // Visual truncation first: the transcript forgets the rewound tail
        // exactly like the ledger does. (False only for history without an
        // echo, e.g. compacted or imported conversations.)
        self.transcript.truncate_from_entry(index);
        session.ledger.truncate_tail(index);
        session.last_prompt_tokens = self.agent.estimate_context_tokens(session);
        self.context_tokens = session.last_prompt_tokens;
        self.context_step_start = self.context_tokens;
        self.context_estimated = !session.ledger.is_empty();
        // Earlier reads are gone from the model's context: freshness
        // stubs would point at nothing. Reset it wholesale.
        session
            .tool_ctx
            .freshness
            .lock()
            .expect("freshness lock")
            .clear();
        let mut lines = vec![Line::styled(
            format!("↺ rewound conversation to entry {index}"),
            ratatui::style::Style::default().fg(theme::WARN),
        )];
        if restore_files {
            for (path, outcome) in session.checkpoints.restore_to(index) {
                use tcode_core::checkpoint::Restore;
                let what = match outcome {
                    Restore::Restored => "restored".to_string(),
                    Restore::Deleted => "deleted (did not exist yet)".to_string(),
                    Restore::Failed(e) => format!("FAILED: {e}"),
                };
                lines.push(Line::styled(
                    format!("  ⎿ {} — {what}", path.display()),
                    theme::dim(),
                ));
            }
        } else if session.checkpoints.dirty_since(index) {
            lines.push(Line::styled(
                "  ⎿ files keep their current content (not rolled back)",
                theme::dim(),
            ));
        }
        self.bake(lines);
        // The original input comes back for editing and resending.
        self.editor.clear();
        self.editor.insert_str(&text);
    }

    fn cancel_turn(&mut self) {
        if let Phase::Running { cancel, .. } = &self.phase {
            cancel.cancel();
            self.state_label = "cancelling".into();
        }
    }

    /// Hot-swap the shared ModelCell; a running turn finishes on its
    /// snapshot, the next request uses the new model.
    fn apply_model(&mut self, index: usize, effort: Option<String>) {
        let Some(opt) = self.menu.options.get(index) else {
            return;
        };
        match (self.menu.switch)(opt, effort.as_deref()) {
            Ok(active) => {
                let label = active.describe();
                let name = active.provider.name().to_string();
                self.agent.model.swap(active);
                self.menu.current = index;
                self.bake(vec![Line::styled(
                    format!("model → {name} · {label}"),
                    theme::dim(),
                )]);
            }
            Err(e) => self.bake(vec![Line::styled(
                format!("cannot switch model: {e}"),
                ratatui::style::Style::default().fg(theme::ERROR),
            )]),
        }
    }

    /// `/agents`: pin a sub-agent kind to a model, or (option = None) let it
    /// go back to following the main one. The binary applies and persists it;
    /// we only mirror the result so the kind list stays truthful.
    fn apply_agent_model(&mut self, kind: &str, option: Option<usize>, effort: Option<String>) {
        let choice = option.and_then(|index| {
            self.menu
                .options
                .get(index)
                .map(|opt| (opt, effort.as_deref()))
        });
        match (self.agents.pin)(kind, choice) {
            Ok(label) => {
                if let Some(slot) = self
                    .agents
                    .kinds
                    .iter()
                    .position(|k| k == kind)
                    .map(|i| &mut self.agents.pins[i])
                {
                    *slot = option.map(|index| (index, effort.clone()));
                }
                self.bake(vec![Line::styled(
                    format!("{kind} → {label}"),
                    theme::dim(),
                )]);
            }
            Err(e) => self.bake(vec![Line::styled(
                format!("cannot pin {kind}: {e}"),
                ratatui::style::Style::default().fg(theme::ERROR),
            )]),
        }
    }

    fn run_slash(&mut self, cmd: &str) {
        // UI-only commands: their substance drives frontend-owned objects
        // (key table, model picker, provider wizard), so they never reach
        // the shared registry.
        match cmd {
            "/help" => {
                self.show_help();
                return;
            }
            "/provider" => {
                self.provider_setup_requested = true;
                self.should_exit = true;
                return;
            }
            "/model" => {
                let effort = self.agent.model.snapshot().effort;
                self.model_picker = model_picker::Picker::new(&self.menu, effort.as_deref());
                if self.model_picker.is_none() {
                    self.bake(vec![Line::styled(
                        "no models configured — edit ~/.tcode/config.toml",
                        theme::dim(),
                    )]);
                }
                return;
            }
            "/agents" => {
                self.agent_picker = model_picker::AgentPicker::new(&self.agents);
                return;
            }
            _ => {}
        }
        let Some(command) = self.registry.find(cmd) else {
            self.bake(vec![Line::styled(
                format!("unknown command {cmd} — /help lists commands"),
                theme::dim(),
            )]);
            return;
        };
        if self.session.is_none() {
            // A running turn owns the session. /cost stays answerable from
            // the UI's own tally; everything else waits.
            if command.name() == "cost" {
                let u = self.turn_usage;
                self.bake(vec![Line::styled(
                    format!(
                        "last turn: in {} | out {} | cache r {} w {}",
                        u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cache_write_tokens
                    ),
                    theme::dim(),
                )]);
            } else {
                self.bake(vec![Line::styled(
                    "wait for the current turn to finish",
                    theme::dim(),
                )]);
            }
            return;
        }
        let session = self.session.as_mut().expect("checked above");
        let mut ctx = CommandCtx {
            session,
            opening_context: &self.opening_context,
            turn_usage: self.turn_usage,
        };
        let outcome = self
            .registry
            .dispatch(&mut ctx, cmd)
            .expect("command found above");
        self.apply_command_outcome(outcome);
    }

    fn show_help(&mut self) {
        let mut lines: Vec<Line> = vec![Line::styled("keys:", theme::bold().fg(theme::ACCENT))];
        for (k, d) in [
            ("enter", "send · during a turn: queue · shift+enter newline"),
            (
                "esc",
                "take back a queued prompt / cancel turn / clear input",
            ),
            ("shift+tab", "cycle permission mode"),
            (
                "ctrl+v / alt+v",
                "paste (images/long text become inline tokens)",
            ),
            ("ctrl+a", "select prompt · ctrl+c copy selection"),
            ("alt+c / alt+x", "copy / cut prompt"),
            ("mouse", "click prompt to move cursor · drag to copy"),
            ("backspace", "delete · after an [attachment] token drops it"),
            (
                "ctrl+c",
                "interrupt turn (sends anything queued) / clear input",
            ),
            ("ctrl+d", "quit · /exit also works"),
        ] {
            lines.push(Line::styled(format!("  {k:<16} {d}"), theme::dim()));
        }
        lines.push(Line::styled("commands:", theme::bold().fg(theme::ACCENT)));
        for (c, d) in UI_COMMANDS {
            lines.push(Line::styled(format!("  {c:<16} {d}"), theme::dim()));
        }
        for (c, d) in self.registry.entries() {
            lines.push(Line::styled(format!("  {c:<16} {d}"), theme::dim()));
        }
        self.bake(lines);
    }

    /// Interpret a command's effects, then bake its messages. Effects run
    /// first: /clear must wipe the screen before "conversation cleared"
    /// appears in the fresh transcript.
    fn apply_command_outcome(&mut self, outcome: tcode_core::commands::CommandOutcome) {
        for effect in outcome.effects {
            match effect {
                CommandEffect::Exit => self.should_exit = true,
                CommandEffect::Compact { focus } => self.start_compact(focus),
                CommandEffect::ConversationCleared => self.reset_conversation_ui(),
                CommandEffect::ConversationReplaced => {
                    self.reset_conversation_ui();
                    self.bake_transcript();
                }
                CommandEffect::OpenResumePicker => self.open_resume_picker(),
                CommandEffect::PersistDogfood(on) => {
                    tcode_core::config::ModelState::update(|state| state.dogfood = on)
                }
                CommandEffect::PersistSuggestions(on) => {
                    tcode_core::config::ModelState::update(|state| state.suggestions = Some(on));
                    // Off means the pending guess is stale; on means the next
                    // turn's end starts one.
                    self.drop_suggestion();
                }
            }
        }
        for message in outcome.messages {
            self.bake_command_message(message);
        }
        // Cheap mirror sync instead of per-command effects: a command may
        // have moved the cwd (/cd) or cycled the permission mode (/mode).
        let old_cwd = self.cwd.clone();
        if let Some(session) = self.session.as_ref() {
            self.cwd = session.tool_ctx.cwd.clone();
            self.scratch_dir = session.tool_ctx.scratch_dir.clone();
            self.mode_label = session.mode.label().to_string();
            self.committed_mode = session.mode;
            self.pending_mode.clear();
            self.dogfood = session.dogfood();
        }
        if self.cwd != old_cwd {
            self.reference_index.clear();
            self.refresh_reference_index();
        }
    }

    fn bake_command_message(&mut self, message: CommandMessage) {
        let lines = match message.kind {
            MessageKind::Info => message
                .text
                .lines()
                .map(|line| Line::styled(line.to_string(), theme::dim()))
                .collect(),
            MessageKind::Error => vec![Line::styled(
                message.text,
                ratatui::style::Style::default().fg(theme::ERROR),
            )],
            MessageKind::Note => quote_lines(Some(NOTE_LABEL), &message.text),
        };
        self.bake(lines);
    }

    // ----------------------------------------------------------- input mouse

    /// Is the mouse row inside the input box? Used to route the wheel to the
    /// prompt instead of the transcript. None hitbox (a dialog/picker owns the
    /// panel) means the wheel keeps scrolling the transcript.
    fn wheel_over_input(&self, y: u16) -> bool {
        self.input_hitbox
            .is_some_and(|hit| y >= hit.rect.y && y < hit.rect.bottom())
    }

    fn input_mouse_down(&mut self, x: u16, y: u16) -> bool {
        let Some((row, col)) = self.input_position_at(x, y) else {
            self.input_mouse_active = false;
            return false;
        };
        self.input_mouse_active = true;
        self.input_dragged = false;
        self.editor.start_selection_by_display_col(row, col);
        self.popup_index = 0;
        true
    }

    fn input_mouse_drag(&mut self, x: u16, y: u16) {
        if let Some((row, col)) = self.input_position_at(x, y) {
            self.input_dragged = true;
            self.editor.extend_selection_by_display_col(row, col);
        }
    }

    /// End a selection drag: stop any auto-scroll, then copy like a normal
    /// mouse-up. Reached from a real `Up`; a click anywhere always lands here
    /// (or in `mouse_down`), so a stuck auto-scroll is one click away from
    /// clearing — the timer never traps input.
    fn finish_drag(&mut self, x: u16, y: u16) {
        self.drag_scroll = None;
        if self.input_mouse_active {
            self.input_mouse_up(x, y);
        } else if let Some(text) = self.transcript.mouse_up() {
            self.copy_selection(text);
        }
    }

    /// One timer step of edge auto-scroll: scroll the transcript a line in the
    /// armed direction, then re-extend the selection to the (now different)
    /// edge row. Self-terminates once the view reaches the top or bottom of the
    /// content — nothing more to reveal — so a release the terminal never
    /// reported (button let go outside the window) cannot scroll forever. A
    /// dialog opening mid-drag takes over the mouse, so disarm.
    fn drag_autoscroll_step(&mut self) {
        let Some((toward_older, x, y)) = self.drag_scroll else {
            return;
        };
        if self.dialog.is_some() {
            self.drag_scroll = None;
            return;
        }
        let before = self.transcript.scroll_offset();
        if toward_older {
            self.transcript.scroll_up(1);
        } else {
            self.transcript.scroll_down(1);
        }
        if self.transcript.scroll_offset() == before {
            // Hit the content edge: stop and copy what is now selected, so the
            // gesture completes even if its release was lost outside the window.
            self.drag_scroll = None;
            if let Some(text) = self.transcript.mouse_up() {
                self.copy_selection(text);
            }
            return;
        }
        self.transcript.mouse_drag(x, y);
    }

    fn input_mouse_up(&mut self, x: u16, y: u16) {
        self.input_mouse_active = false;
        // A plain click (no drag) only repositions the cursor — never copies,
        // even if the release cell rounds to a neighbour of the press cell.
        if !self.input_dragged {
            return;
        }
        self.input_mouse_drag(x, y);
        if let Some(text) = self.editor.selected_text() {
            self.copy_input_text(text);
        }
    }

    fn input_position_at(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        let hit = self.input_hitbox?;
        if x <= hit.rect.x
            || x >= hit.rect.right().saturating_sub(1)
            || y <= hit.rect.y
            || y >= hit.rect.bottom().saturating_sub(1)
        {
            return None;
        }
        let layout = editor_layout(&self.editor, hit.rect.width);
        if layout.lines.is_empty() {
            return Some((0, 0));
        }
        let visual_row = hit.editor_start + (y - hit.rect.y - 1) as usize;
        let visual_row = visual_row.min(layout.lines.len() - 1);
        let line = &layout.lines[visual_row];
        let content_x = x.saturating_sub(hit.rect.x + 3) as usize;
        let display_col =
            line.start_col + content_x.min(line.end_col.saturating_sub(line.start_col));
        Some((line.logical_row, display_col))
    }

    // ----------------------------------------------------------- paste/copy

    fn copy_input_text(&mut self, text: String) {
        let lines = text.lines().count().max(1);
        let what = if lines <= 1 {
            "input".to_string()
        } else {
            format!("input {lines} lines")
        };
        self.copy_text(text, what);
    }

    fn copy_editor_selection_or_prompt(&mut self) {
        if let Some(text) = self.editor.selected_text() {
            self.copy_input_text(text);
        } else if !self.editor.is_empty() {
            self.copy_input_text(self.editor.text());
        }
    }

    fn cut_editor_selection_or_prompt(&mut self) {
        self.dismissed_reference = None;
        if let Some(text) = self.editor.selected_text() {
            self.copy_input_text(text);
            self.editor.delete_selection();
        } else if !self.editor.is_empty() {
            self.copy_input_text(self.editor.text());
            self.editor.clear();
        }
    }

    /// Mouse-selection copy: system clipboard first (arboard), OSC 52 as
    /// the remote/SSH fallback where no local clipboard exists.
    fn copy_selection(&mut self, text: String) {
        let lines = text.lines().count();
        let what = if lines <= 1 {
            "selection".to_string()
        } else {
            format!("{lines} lines")
        };
        self.copy_text(text, what);
    }

    fn copy_text(&mut self, text: String, what: String) {
        let copied = self
            .clipboard
            .as_mut()
            .is_some_and(|clipboard| clipboard.set_text(text.clone()).is_ok());
        if !copied {
            use base64::Engine as _;
            use std::io::Write as _;
            let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
            let mut out = std::io::stdout();
            let _ = write!(out, "\x1b]52;c;{encoded}\x07");
            let _ = out.flush();
        }
        self.notice = Some((format!("copied {what}"), Instant::now()));
    }

    /// Schedule a fresh ignored-aware index without blocking terminal input.
    /// Results carry their cwd, so a slow scan that finishes after `/cd` is
    /// harmlessly discarded.
    fn refresh_reference_index(&self) {
        let cwd = self.cwd.clone();
        let tx = self.reference_tx.clone();
        tokio::spawn(async move {
            let scan_cwd = cwd.clone();
            let index = tokio::task::spawn_blocking(move || tcode_core::index_project(&scan_cwd))
                .await
                .unwrap_or_default();
            let _ = tx.send((cwd, index)).await;
        });
    }

    fn paste_from_clipboard(&mut self) {
        let Some(clipboard) = self.clipboard.as_mut() else {
            return;
        };
        // Pull owned data out while the clipboard borrow is held, then release
        // it before touching other `self` fields.
        let image = clipboard.get_image().ok().and_then(|img| {
            let width = u32::try_from(img.width).ok()?;
            let height = u32::try_from(img.height).ok()?;
            tcode_core::images::normalize_rgba(width, height, img.bytes.into_owned()).ok()
        });
        if let Some(image) = image {
            let kb = image.bytes.len() / 1024;
            self.add_attachment(|id| Attachment::Image {
                id,
                bytes: image.bytes,
                media_type: image.media_type,
                label: format!("Image #{id} ({}x{}, {kb}KB)", image.width, image.height),
            });
            return;
        }
        if let Some(text) = self.clipboard.as_mut().and_then(|c| c.get_text().ok()) {
            self.on_paste_text(text);
        }
    }

    /// Discard the whole draft: editor text, attachments, and the token
    /// numbering that ties them together.
    fn clear_draft(&mut self) {
        self.dismissed_reference = None;
        self.editor.clear();
        self.attachments.clear();
        self.next_attachment_id = 1;
    }

    /// Register an attachment and drop its inline token into the editor at the
    /// cursor. The token is how the user sees, moves, and deletes it — pressing
    /// backspace right after it removes the whole thing (see `on_key`).
    fn add_attachment(&mut self, make: impl FnOnce(u32) -> Attachment) {
        self.dismissed_reference = None;
        let id = self.next_attachment_id;
        self.next_attachment_id += 1;
        let att = make(id);
        let placeholder = att.placeholder();
        self.attachments.push(att);
        self.editor.insert_str(&placeholder);
    }

    /// If the cursor sits immediately after an attachment's inline token,
    /// delete the whole token and drop that attachment in one keystroke.
    fn backspace_attachment_token(&mut self) -> bool {
        let pos = self.editor.position();
        let before: String = self.editor.lines()[pos.row].chars().take(pos.col).collect();
        let Some(idx) = self
            .attachments
            .iter()
            .position(|a| before.ends_with(&a.placeholder()))
        else {
            return false;
        };
        let token_len = self.attachments[idx].placeholder().chars().count();
        for _ in 0..token_len {
            self.editor.backspace();
        }
        let label = self.attachments.remove(idx).label().to_string();
        self.notice = Some((format!("removed {label}"), Instant::now()));
        true
    }

    fn on_paste_text(&mut self, text: String) {
        self.dismissed_reference = None;
        let lines = text.lines().count().max(1);
        let chars = text.chars().count();
        if paste_should_fold(chars, lines) {
            self.add_attachment(|id| Attachment::Text {
                id,
                content: text,
                label: format!("Pasted text #{id} ({chars} chars · {lines} lines)"),
            });
        } else {
            self.editor.insert_str(&text);
        }
    }

    // ------------------------------------------------------- rendering

    fn popup_active(&self) -> bool {
        self.dialog.is_none() && !self.completion_matches().is_empty()
    }

    fn completion_matches(&self) -> Vec<CompletionMatch> {
        if self.dialog.is_some() {
            return Vec::new();
        }
        if self.editor.line_count() == 1 && self.editor.text().starts_with('/') {
            let prefix = self.editor.text();
            return UI_COMMANDS
                .iter()
                .copied()
                .chain(self.registry.entries())
                .filter(|(command, _)| command.starts_with(&prefix))
                .map(|(command, description)| CompletionMatch {
                    label: command.to_string(),
                    description: description.to_string(),
                    replacement: command.to_string(),
                    kind: CompletionKind::Slash,
                })
                .collect();
        }
        let Some((start, end, query)) = self.active_reference() else {
            return Vec::new();
        };
        if self.dismissed_reference == Some(start) {
            return Vec::new();
        }
        let mut matches: Vec<(usize, &ReferenceCandidate)> = self
            .reference_index
            .iter()
            .filter_map(|candidate| {
                reference_score(&candidate.path, &query).map(|score| (score, candidate))
            })
            .collect();
        matches.sort_by(|(left_score, left), (right_score, right)| {
            left_score
                .cmp(right_score)
                .then_with(|| left.path.cmp(&right.path))
        });
        let mut basename_counts = HashMap::new();
        for (_, candidate) in &matches {
            *basename_counts
                .entry(reference_basename(&candidate.path))
                .or_insert(0usize) += 1;
        }
        matches
            .into_iter()
            .take(8)
            .map(|(_, candidate)| {
                let path = reference_candidate_path(candidate);
                let label_path = if basename_counts
                    .get(reference_basename(&candidate.path))
                    .copied()
                    .unwrap_or_default()
                    > 1
                {
                    path.clone()
                } else {
                    reference_basename_path(candidate)
                };
                let replacement = reference_marker(&path);
                let size = candidate
                    .bytes
                    .map(format_bytes)
                    .map(|size| format!(" · {size}"))
                    .unwrap_or_default();
                CompletionMatch {
                    label: reference_marker(&label_path),
                    description: format!("{}{}", candidate.display_kind(), size),
                    replacement,
                    kind: CompletionKind::Reference { start, end },
                }
            })
            .collect()
    }

    fn active_reference(&self) -> Option<(Position, Position, String)> {
        let cursor = self.editor.position();
        let line = self.editor.lines().get(cursor.row)?;
        let chars: Vec<char> = line.chars().collect();
        let at = (0..cursor.col)
            .rev()
            .find(|&index| chars[index] == '@' && reference_boundary(&chars, index))?;
        let quoted = chars.get(at + 1) == Some(&'"');
        let content_start = at + 1 + usize::from(quoted);
        let mut token_end = content_start;
        if quoted {
            while token_end < chars.len() && chars[token_end] != '"' {
                token_end += 1;
            }
            if token_end < chars.len() {
                token_end += 1;
            }
        } else {
            while token_end < chars.len() && reference_token_char(chars[token_end]) {
                token_end += 1;
            }
        }
        if cursor.col < content_start || cursor.col > token_end {
            return None;
        }
        let query: String = chars[content_start..cursor.col.min(token_end)]
            .iter()
            .collect();
        Some((
            Position {
                row: cursor.row,
                col: at,
            },
            Position {
                row: cursor.row,
                col: token_end,
            },
            query,
        ))
    }

    fn popup_selection(&self) -> Option<CompletionMatch> {
        let matches = self.completion_matches();
        matches
            .get(self.popup_index.min(matches.len().saturating_sub(1)))
            .cloned()
    }

    /// Finalize content into the transcript. Name kept from the inline era;
    /// unlike native scrollback, transcript content can still be truncated
    /// (rewind) or cleared later.
    fn bake(&mut self, lines: Vec<Line<'static>>) {
        self.transcript.push(lines);
    }

    /// `/clear`, resume and import restart the visual conversation. The
    /// transcript is ours, so this is a plain reset — no terminal purge.
    fn clear_conversation_screen(&mut self) {
        self.transcript.clear();
        self.live_text.clear();
        self.live_block = None;
        let banner = self.banner();
        self.bake(banner);
    }

    /// The ledger was cleared or replaced: drop turn-scoped UI state and
    /// restart the visual conversation. Shared by /clear, /resume and
    /// external import.
    fn reset_conversation_ui(&mut self) {
        self.drop_suggestion();
        if let Some(session) = self.session.as_mut() {
            if session.last_prompt_tokens == 0 && !session.ledger.is_empty() {
                session.last_prompt_tokens = self.agent.estimate_context_tokens(session);
            }
            self.context_tokens = session.last_prompt_tokens;
            self.context_estimated = !session.ledger.is_empty();
        } else {
            self.context_tokens = 0;
            self.context_estimated = false;
        }
        self.context_step_start = self.context_tokens;
        self.prev_cache_ratio = None;
        self.progress.clear();
        self.pending_tool = None;
        self.pending_batch.clear();
        self.thinking_text.clear();
        self.clear_conversation_screen();
    }

    /// Resume picker selections route through the same registry command as
    /// a typed `/resume <id>`.
    fn resume_session(&mut self, id: &str) {
        self.run_slash(&format!("/resume {}", id.trim()));
    }

    fn open_resume_picker(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let Some(data_dir) = tcode_core::store::project_data_dir(&session.tool_ctx.cwd) else {
            self.bake(vec![Line::styled(
                "cannot locate tcode session storage",
                theme::dim(),
            )]);
            return;
        };
        match tcode_core::SessionStore::list(&data_dir) {
            Ok(sessions) => self.resume_picker = Some(resume::Picker::new(sessions)),
            // External import is useful even before tcode itself has stored a
            // prior conversation in this project.
            Err(tcode_core::store::StoreError::NoSession) => {
                self.resume_picker = Some(resume::Picker::new(Vec::new()))
            }
            Err(e) => self.bake(vec![Line::styled(
                format!("cannot list resumable sessions: {e}"),
                ratatui::style::Style::default().fg(theme::ERROR),
            )]),
        }
    }

    fn open_external_resume_picker(&mut self, source: tcode_core::ExternalSource) {
        let sessions = tcode_core::list_external_sessions(&self.cwd, source);
        match resume::Picker::external(source, sessions) {
            Some(picker) => self.resume_picker = Some(picker),
            None => {
                self.resume_picker = None;
                self.bake(vec![Line::styled(
                    format!("no {} conversations found for this project", source.label()),
                    theme::dim(),
                )]);
            }
        }
    }

    fn import_external_session(&mut self, external: tcode_core::ExternalSessionInfo) {
        if matches!(self.phase, Phase::Running { .. }) || self.external_import.is_some() {
            self.bake(vec![Line::styled(
                "wait for the current turn before importing",
                theme::dim(),
            )]);
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let Some(data_dir) = tcode_core::store::project_data_dir(&session.tool_ctx.cwd) else {
            self.bake(vec![Line::styled(
                "cannot locate tcode session storage",
                theme::dim(),
            )]);
            return;
        };
        let cwd = session.tool_ctx.cwd.clone();
        let source = external.source;
        self.external_import = Some(tokio::task::spawn_blocking(move || {
            let result = tcode_core::import_external_session(&data_dir, &cwd, &external);
            (source, result)
        }));
        self.state_label = format!("importing {} conversation", source.label());
    }

    fn on_external_import_done(
        &mut self,
        (source, result): (
            tcode_core::ExternalSource,
            Result<tcode_core::Resumed, tcode_core::store::StoreError>,
        ),
    ) {
        self.external_import = None;
        self.state_label.clear();
        let opening_context = self.opening_context.clone();
        let Some(session) = self.session.as_mut() else {
            return;
        };
        match result {
            Ok(resumed) => {
                let imported_id = resumed.store.id.clone();
                session.checkpoints = tcode_core::CheckpointStore::default();
                session.ledger = resumed.ledger;
                session.ledger.attach_sink(Box::new(resumed.store));
                session.bind_scratch_session(&imported_id);
                let opening = opening_context(&session.tool_ctx.cwd, &session.tool_ctx.scratch_dir);
                session.replace_opening_context_for_resume(opening);
                self.scratch_dir = session.tool_ctx.scratch_dir.clone();
                session.last_prompt_tokens = 0;
                session
                    .tool_ctx
                    .freshness
                    .lock()
                    .expect("freshness lock")
                    .clear();
                self.reset_conversation_ui();
                self.bake(vec![Line::styled(
                    format!("imported {} as tcode session {imported_id}", source.label()),
                    theme::dim(),
                )]);
                self.bake_transcript();
            }
            Err(e) => self.bake(vec![Line::styled(
                format!("cannot import external session: {e}"),
                ratatui::style::Style::default().fg(theme::ERROR),
            )]),
        }
    }

    fn update_progress(&mut self, input: &serde_json::Value) {
        // `plan` / `step` keep resumed sessions created before the rename
        // readable; live calls use `phases` / `phase` exclusively.
        let Some(items) = input["phases"]
            .as_array()
            .or_else(|| input["plan"].as_array())
        else {
            return;
        };
        self.progress = items
            .iter()
            .filter_map(|item| {
                let phase = item["phase"]
                    .as_str()
                    .or_else(|| item["step"].as_str())?
                    .trim();
                let status = item["status"].as_str()?;
                (!phase.is_empty() && matches!(status, "pending" | "in_progress" | "completed"))
                    .then(|| ProgressPhase {
                        phase: phase.to_string(),
                        status: status.to_string(),
                    })
            })
            .collect();
    }

    fn progress_lines(&self) -> Vec<Line<'static>> {
        if self.progress.is_empty() {
            return Vec::new();
        }
        let complete = self
            .progress
            .iter()
            .filter(|item| item.is_completed())
            .count();
        let (start, end) = visible_phase_range(&self.progress, PROGRESS_VISIBLE_PHASES);
        let hidden_before = start;
        let hidden_after = self.progress.len().saturating_sub(end);
        let mut lines = vec![Line::from(vec![
            Span::styled("  progress ", theme::bold().fg(theme::ACCENT)),
            Span::styled(
                format!("{complete}/{} phases complete", self.progress.len()),
                theme::dim(),
            ),
            if hidden_before + hidden_after > 0 {
                Span::styled(format!(" · showing {}-{}", start + 1, end), theme::dim())
            } else {
                Span::raw("")
            },
        ])];
        if hidden_before > 0 {
            lines.push(Line::styled(
                format!("    … {hidden_before} earlier"),
                theme::dim(),
            ));
        }
        lines.extend(self.progress[start..end].iter().map(|item| {
            let (marker, style) = match item.status.as_str() {
                "completed" => ("✓ ", ratatui::style::Style::default().fg(theme::OK)),
                "in_progress" => ("● ", theme::accent()),
                _ => ("○ ", theme::dim()),
            };
            Line::from(vec![
                Span::styled(format!("    {marker}"), style),
                Span::styled(
                    item.phase.clone(),
                    if item.status == "completed" {
                        ratatui::style::Style::default()
                            .fg(theme::OK)
                            .add_modifier(ratatui::style::Modifier::CROSSED_OUT)
                    } else if item.status == "pending" {
                        theme::dim()
                    } else {
                        ratatui::style::Style::default()
                    },
                ),
            ])
        }));
        if hidden_after > 0 {
            lines.push(Line::styled(
                format!("    … {hidden_after} later"),
                theme::dim(),
            ));
        }
        lines
    }

    /// Ask, off-thread, what the user probably wants next. It runs on its own
    /// small prose conversation and its own model role (see `Agent::suggest`),
    /// so a turn only pays for its newest pair — but it is still a request,
    /// hence `[ui] suggest_next` and `/suggestions`.
    fn start_suggestion(&mut self) {
        self.drop_suggestion();
        if !self.editor.is_empty() {
            return;
        }
        let Some(session) = self.session.as_ref().filter(|s| s.suggestions()) else {
            return;
        };
        let Some(request) = self.agent.suggest_request(session) else {
            return;
        };
        let agent = self.agent.clone();
        let tx = self.suggest_tx.clone();
        let cancel = CancellationToken::new();
        let generation = self.suggest_gen;
        self.suggest_cancel = Some(cancel.clone());
        tokio::spawn(async move {
            let suggestion = agent.suggest(request, cancel).await;
            let _ = tx.send((generation, suggestion)).await;
        });
    }

    /// Submitting, clearing, compacting or rewinding makes a guess stale — the
    /// conversation it was predicting from is no longer the one on screen.
    /// Typing does not: see `on_key`.
    ///
    /// Bumping the generation retires any request still in flight, so a reply
    /// that lands after this point cannot resurrect a guess about a past turn.
    fn drop_suggestion(&mut self) {
        if let Some(cancel) = self.suggest_cancel.take() {
            cancel.cancel();
        }
        self.suggest_gen += 1;
        self.suggestion = None;
    }

    /// Take the queued prompts back. The turn keeps running: this cancels what
    /// the user said, not what the agent is doing.
    ///
    /// The newest message returns to the input box rather than evaporating —
    /// "take it back" should not mean "retype it". Anything that cannot be put
    /// back (an older message, or the draft's attachments, which were already
    /// encoded into blocks) leaves a dim record instead of vanishing silently.
    fn discard_queued(&mut self) {
        let mut queued = self.pending.take();
        let Some(last) = queued.pop() else {
            return;
        };
        let mut lines: Vec<Line> = queued
            .iter()
            .map(|message| Line::styled(format!("⏳ discarded: {}", message.text), theme::dim()))
            .collect();
        if self.editor.is_empty() {
            self.editor.insert_str(&last.text);
            if !last.attachments.is_empty() {
                lines.push(Line::styled(
                    format!("re-paste to restore: {}", last.attachments.join(", ")),
                    theme::dim(),
                ));
            }
        } else {
            lines.push(Line::styled(
                format!("⏳ discarded: {}", last.text),
                theme::dim(),
            ));
        }
        if !lines.is_empty() {
            self.bake(lines);
        }
    }

    /// What the user has already sent but the model has not yet seen: the
    /// prompt itself, dimmed, waiting above the input box it came from. It is a
    /// view of the queue, not a copy — delivery drains the queue and the row
    /// disappears by itself, replaced by the real prompt in the transcript.
    fn queued_lines(&self, width: u16) -> Vec<Line<'static>> {
        let queued = self.pending.queued();
        if queued.is_empty() {
            return Vec::new();
        }
        let budget = (width as usize).saturating_sub(6).max(16);
        let mut lines: Vec<Line> = queued
            .iter()
            .map(|message| {
                let mut text = one_line(&message.text, budget);
                for label in &message.attachments {
                    text.push_str(&format!(" ⌞ {label}"));
                }
                Line::from(vec![
                    Span::styled("⏳ ", theme::dim()),
                    Span::styled(text, theme::dim()),
                ])
            })
            .collect();
        // The two ways out of the wait, where the waiting is.
        lines.push(Line::styled(
            "   ctrl+c to send now · esc to take it back",
            theme::dim(),
        ));
        lines
    }

    /// Tool inputs are canonical absolute paths, but repeating the current
    /// project root adds noise without adding information in the TUI.
    fn display_summary(&self, summary: &str) -> String {
        shorten_summary_path(summary, Some(&self.cwd))
    }

    fn redraw(&mut self) -> anyhow::Result<()> {
        let running = matches!(self.phase, Phase::Running { .. });
        let started = match &self.phase {
            Phase::Running { started, .. } => Some(*started),
            Phase::Idle => None,
        };
        let status = self.status_line(running, started);
        let hint = self.idle_hint();
        let dialog_lines = self
            .resume_picker
            .as_ref()
            .map(|p| p.render())
            .or_else(|| self.model_picker.as_ref().map(|p| p.render(&self.menu)))
            .or_else(|| {
                self.agent_picker
                    .as_ref()
                    .map(|p| p.render(&self.menu, &self.agents))
            })
            .or_else(|| {
                self.dialog.as_ref().map(|(d, _)| {
                    // The plan pane scrolls its own body: cap it to the space the
                    // panel can occupy (the transcript keeps a few rows) so a long
                    // plan does not push the options and hint off screen.
                    let budget = area_height(&self.terminal).saturating_sub(6);
                    d.render(area_width(&self.terminal), budget)
                })
            });
        // A focused note in the approval dialog exposes its caret cell (set by
        // the `render` just above). Anchoring the real terminal cursor there
        // keeps the OS IME composition window tracking the caret. Only valid
        // when the dialog — not a picker — produced the lines this frame.
        let dialog_owns_panel = dialog_lines.is_some()
            && self.resume_picker.is_none()
            && self.model_picker.is_none()
            && self.agent_picker.is_none();
        let dialog_cursor = dialog_owns_panel
            .then(|| self.dialog.as_ref().and_then(|(d, _)| d.cursor_cell()))
            .flatten();
        let editor = editor_layout(&self.editor, area_width(&self.terminal));
        // Ghost text only appears while the input is empty and idle. Its visual
        // rows use the editor's own width calculation below.
        let ghost = self
            .suggestion
            .clone()
            .filter(|_| self.editor.is_empty() && !running);
        // Ghost text uses the same width calculation as actual input. Its rows
        // remain read-only, but a longer suggestion must not overrun the box.
        let ghost_lines = ghost.as_deref().map(|text| {
            ghost_visual_lines(&format!("{text}  → to accept"), area_width(&self.terminal))
        });
        let popup: Vec<(String, String)> = if self.popup_active() {
            self.completion_matches()
                .into_iter()
                .map(|completion| (completion.label, completion.description))
                .collect()
        } else {
            Vec::new()
        };
        let popup_index = self.popup_index.min(popup.len().saturating_sub(1));
        let progress_lines = self.progress_lines();
        let queued_lines = self.queued_lines(area_width(&self.terminal));

        use ratatui::widgets::{Block, BorderType, Clear};

        // The input box geometry is only known during layout; capture it so
        // mouse hit-testing (selection/copy in the prompt) can map screen
        // coordinates back to editor positions. None when a dialog/picker
        // replaces the input box.
        let mut captured_input: Option<InputHitbox> = None;
        let transcript = &mut self.transcript;
        self.terminal.draw(|frame| {
            let area = frame.area();
            // The lower panel changes height when a dialog opens, a paste is
            // folded, or the editor wraps. Clear the entire frame first:
            // widgets paint only their own cells, so otherwise letters from a
            // previous, taller panel survive after the layout moves.
            frame.render_widget(Clear, area);

            // ------- bottom panel height (transcript gets the rest) -------
            let editor_start = editor.cursor_row.saturating_sub(5);
            let editor_h = ghost_lines
                .as_ref()
                .map(|lines| lines.len().min(6))
                .unwrap_or_else(|| editor.lines.len() - editor_start)
                .clamp(1, 6) as u16;
            let panel_h = if let Some(lines) = &dialog_lines {
                lines.len() as u16 + 2
            } else {
                let mut h = editor_h + 2 + 2; // input box + context meter + hint
                if running {
                    // Leave a breathing row after the transcript before the
                    // live status line, rather than pinning "responding" to
                    // the last rendered transcript row.
                    h += 2; // separator + spinner/status line above the input box
                }
                h += queued_lines.len() as u16;
                if !progress_lines.is_empty() {
                    h += progress_lines.len() as u16 + 2;
                }
                if self.rate_limits.is_some() {
                    h += 1;
                }
                h + popup.len() as u16
            };
            // The transcript keeps at least a few visible rows.
            let panel_h = panel_h.min(area.height.saturating_sub(4)).max(1);
            let split = area.height.saturating_sub(panel_h);
            transcript.render(
                frame.buffer_mut(),
                Rect {
                    height: split,
                    ..area
                },
            );

            let mut y = area.y + split;
            let row = |y: u16, h: u16| Rect {
                x: area.x,
                y,
                width: area.width,
                height: h.min(area.bottom().saturating_sub(y)),
            };

            // Pickers and approval dialogs own the panel: a rounded
            // accent-bordered box signals "keys go here now".
            if let Some(lines) = dialog_lines {
                let h = lines.len() as u16 + 2;
                frame.render_widget(
                    Paragraph::new(Text::from(lines)).block(
                        Block::bordered()
                            .border_type(BorderType::Rounded)
                            .border_style(theme::border_active()),
                    ),
                    row(y, h),
                );
                // Place the terminal cursor on the focused note caret (+1 for
                // the block border) so the IME follows it. Without this the
                // hardware cursor stays hidden and IME composition detaches.
                if let Some((crow, ccol)) = dialog_cursor {
                    frame.set_cursor_position((area.x + 1 + ccol, y + 1 + crow));
                }
                return;
            }

            if !progress_lines.is_empty() {
                let h = progress_lines.len() as u16 + 2;
                frame.render_widget(
                    Paragraph::new(Text::from(progress_lines)).block(
                        Block::bordered()
                            .border_type(BorderType::Rounded)
                            .border_style(theme::border()),
                    ),
                    row(y, h),
                );
                y += h;
            }

            if running {
                // The reserved row separates completed transcript content from
                // the live turn indicator.
                y += 1;
                frame.render_widget(Paragraph::new(status), row(y, 1));
                y += 1;
            }

            // Prompts already sent by the user but not yet by us: they sit
            // between the spinner and the input box, where the next thing to
            // reach the model belongs.
            if !queued_lines.is_empty() {
                let h = queued_lines.len() as u16;
                frame.render_widget(Paragraph::new(Text::from(queued_lines)), row(y, h));
                y += h;
            }

            // Input inside a rounded box, Claude Code style.
            // Show the cursor even when a long multi-line prompt exceeds
            // the six-row input box.
            let inner: Vec<Line> = if let Some(lines) = &ghost_lines {
                lines
                    .iter()
                    .take(6)
                    .enumerate()
                    .map(|(index, line)| {
                        Line::from(vec![
                            Span::styled(
                                if index == 0 { "› " } else { "  " },
                                theme::user_prompt(),
                            ),
                            Span::styled(line.clone(), theme::dim()),
                        ])
                    })
                    .collect()
            } else {
                editor.lines[editor_start..]
                    .iter()
                    .take(6)
                    .map(|vl| {
                        let mut spans = vec![Span::styled(
                            if vl.first_logical_line { "› " } else { "  " },
                            theme::user_prompt(),
                        )];
                        match vl.selection {
                            Some(selection) => spans.extend(input_spans(
                                &vl.text,
                                Some(selection),
                                &self.reference_index,
                            )),
                            None => {
                                spans.extend(input_spans(&vl.text, None, &self.reference_index))
                            }
                        }
                        Line::from(spans)
                    })
                    .collect()
            };
            let box_y = y;
            let input_rect = row(y, editor_h + 2);
            captured_input = Some(InputHitbox {
                rect: input_rect,
                editor_start,
            });
            frame.render_widget(
                Paragraph::new(Text::from(inner)).block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .border_style(theme::border()),
                ),
                input_rect,
            );
            y += editor_h + 2;
            frame.set_cursor_position((
                area.x + 3 + editor.cursor_col as u16,
                box_y + 1 + (editor.cursor_row - editor_start) as u16,
            ));

            frame.render_widget(
                Paragraph::new(context_progress_line(
                    self.context_tokens,
                    self.agent.model.snapshot().context_window,
                    area.width,
                    self.context_estimated,
                )),
                row(y, 1),
            );
            y += 1;

            if let Some(limits) = self.rate_limits {
                frame.render_widget(Paragraph::new(rate_limit_line(limits)), row(y, 1));
                y += 1;
            }

            for (i, (c, d)) in popup.iter().enumerate() {
                let line = if i == popup_index {
                    Line::from(vec![
                        Span::styled("  ▸ ".to_string(), theme::accent()),
                        Span::styled(format!("{c:<10}"), theme::user_prompt()),
                        Span::styled(format!(" {d}"), theme::accent()),
                    ])
                } else {
                    Line::styled(format!("    {c:<10} {d}"), theme::dim())
                };
                frame.render_widget(Paragraph::new(line), row(y, 1));
                y += 1;
            }

            frame.render_widget(Paragraph::new(hint), row(y, 1));
        })?;
        self.input_hitbox = captured_input;
        Ok(())
    }

    /// Spinner line shown above the input while a turn runs. The sparkle
    /// carries the animation; the label stays readable, metadata stays dim.
    fn status_line(&self, running: bool, started: Option<Instant>) -> Line<'static> {
        if !running {
            return Line::default();
        }
        // A pending retry takes over the status line with a red live countdown.
        if let Some(retry) = &self.retry_wait {
            let remaining = retry.until.saturating_duration_since(Instant::now());
            let secs = remaining.as_secs() + u64::from(remaining.subsec_millis() > 0);
            let red = ratatui::style::Style::default().fg(theme::ERROR);
            return Line::from(vec![
                Span::styled(
                    format!("↻ retrying ({}/{}) ", retry.attempt, retry.max),
                    red,
                ),
                Span::styled(
                    if secs > 0 {
                        format!("in {secs}s")
                    } else {
                        "now…".to_string()
                    },
                    red,
                ),
                Span::styled(" · esc to cancel", theme::dim()),
            ]);
        }
        let elapsed = started.map(|s| s.elapsed().as_secs()).unwrap_or(0);
        let frame = SPINNER[self.spinner % SPINNER.len()];
        Line::from(vec![
            Span::styled(format!("{frame} "), theme::warn()),
            Span::styled(self.state_label.clone(), theme::warn()),
            Span::styled(
                format!(
                    " · {elapsed}s · ↓ ~{} tok · esc to cancel",
                    token_count(self.out_tokens as u64)
                ),
                theme::dim(),
            ),
        ])
    }

    /// One-liner under the input box: mode, model, cache health. Mostly
    /// dim; the mode value carries the accent because it decides what the
    /// agent may do without asking, and a transient notice keeps the
    /// default foreground so it reads as news rather than furniture.
    fn idle_hint(&self) -> Line<'static> {
        if let Some(nav) = &self.rewind_nav {
            let files = if nav.candidates[nav.pos].dirty {
                " · ctrl+r rewind + restore files"
            } else {
                ""
            };
            return Line::from(vec![
                Span::styled("  ↺ rewind:".to_string(), theme::accent()),
                Span::styled(
                    format!(" enter confirm{files} · esc/↑ older · ↓ newer/exit"),
                    theme::dim(),
                ),
            ]);
        }
        let u = self.turn_usage;
        let cache = if u.total_input() > 0 {
            format!(
                " · cache {}%",
                (u.cache_read_tokens as f64 / u.total_input() as f64 * 100.0).round()
            )
        } else {
            String::new()
        };
        let scrolled = if self.transcript.is_following() {
            ""
        } else {
            " · ↑ viewing history"
        };
        let mut spans = vec![
            Span::styled("  mode ".to_string(), theme::dim()),
            Span::styled(self.mode_label.clone(), theme::accent()),
            Span::styled(
                format!(" · {}", self.agent.model.snapshot().describe()),
                theme::dim(),
            ),
        ];
        // A mode that silently changes what the model does must be visible
        // while it is on, not only in the line that switched it on.
        if self.dogfood {
            spans.push(Span::styled(" · dogfood".to_string(), theme::warn()));
        }
        spans.push(Span::styled(format!("{cache}{scrolled}"), theme::dim()));
        if let Some((text, _)) = self
            .notice
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(3))
        {
            spans.push(Span::styled(" · ".to_string(), theme::dim()));
            spans.push(Span::raw(text.clone()));
        }
        spans.push(Span::styled(" · /help".to_string(), theme::dim()));
        Line::from(spans)
    }
}

fn visible_phase_range(phases: &[ProgressPhase], max_visible: usize) -> (usize, usize) {
    if phases.len() <= max_visible || max_visible == 0 {
        return (0, phases.len());
    }
    let focus = phases
        .iter()
        .position(|item| item.status == "in_progress")
        .or_else(|| phases.iter().position(|item| item.status == "pending"))
        .unwrap_or(phases.len() - 1);
    let mut start = focus.saturating_sub(max_visible / 2);
    start = start.min(phases.len().saturating_sub(max_visible));
    (start, start + max_visible)
}

/// A result preview riding on its call's own row: ` — preview`, dim on
/// success, red on failure. One format for batch rows and single calls.
fn preview_tail(preview: &str, style: ratatui::style::Style) -> Vec<Span<'static>> {
    if preview.is_empty() {
        return Vec::new();
    }
    vec![Span::styled(format!(" — {preview}"), style)]
}

fn append_result_preview(
    lines: &mut Vec<Line<'static>>,
    preview: &str,
    style: ratatui::style::Style,
) {
    for span in preview_tail(preview, style) {
        if let Some(last) = lines.last_mut() {
            last.spans.push(span);
        } else {
            lines.push(Line::from(vec![span]));
        }
    }
}

/// Color a batch label such as "Read 5 files · Search 2 patterns": each
/// fragment's leading tool name is green, matching a single call's header.
fn colored_batch_label(label: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, fragment) in label.split(" · ").enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", theme::dim()));
        }
        match fragment.split_once(' ') {
            Some((name, rest)) => {
                spans.push(Span::styled(name.to_string(), theme::ok()));
                spans.push(Span::styled(format!(" {rest}"), theme::bold()));
            }
            None => spans.push(Span::styled(fragment.to_string(), theme::ok())),
        }
    }
    spans
}

fn result_preview(s: &str) -> String {
    let mut line = s.lines().next().unwrap_or("").to_string();
    if line.chars().count() > 120 {
        line = line.chars().take(120).collect::<String>() + "…";
    }
    let extra = s.lines().count().saturating_sub(1);
    if extra > 0 {
        line.push_str(&format!(" (+{extra} lines)"));
    }
    line
}

/// One compact row below the editor. The meter intentionally reports the
/// current conversation, rather than cumulative billable tokens: cached input
/// still occupies context and must count toward the model window.
fn context_progress_line(
    used: u64,
    window: u64,
    terminal_width: u16,
    estimated: bool,
) -> Line<'static> {
    let window = window.max(1);
    let pct = used.saturating_mul(100).saturating_div(window).min(100);
    let estimate_mark = if estimated { "≈" } else { "" };
    let color = if pct >= 95 {
        theme::ERROR
    } else if pct >= 85 {
        theme::WARN
    } else {
        theme::OK
    };
    let (label, bar_width) = if terminal_width < 42 {
        ("  ctx ", 8usize)
    } else {
        ("  context ", 12usize)
    };
    let filled = if used == 0 {
        0
    } else {
        ((bar_width as u64 * pct).div_ceil(100) as usize).min(bar_width)
    };
    let mut spans = vec![Span::styled(label, theme::dim())];
    spans.extend(slim_bar(filled, bar_width, color));
    spans.push(Span::styled(
        format!(" {estimate_mark}{pct}%"),
        ratatui::style::Style::default().fg(color),
    ));
    if terminal_width >= 42 {
        spans.push(Span::styled(
            format!(" · {}/{}", token_count(used), token_count(window)),
            theme::dim(),
        ));
    }
    Line::from(spans)
}

/// The slim gauge shared by the context meter and the rate-limit row:
/// a heavy coloured run over a dim dashed track.
fn slim_bar(filled: usize, width: usize, color: ratatui::style::Color) -> [Span<'static>; 2] {
    [
        Span::styled(
            "━".repeat(filled),
            ratatui::style::Style::default().fg(color),
        ),
        Span::styled("╌".repeat(width.saturating_sub(filled)), theme::dim()),
    ]
}

fn token_count(tokens: u64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 10_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        format!("{}k", tokens.div_ceil(1_000))
    }
}

fn rate_limit_line(limits: tcode_core::RateLimits) -> Line<'static> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    rate_limit_line_at(limits, now)
}

fn rate_limit_line_at(limits: tcode_core::RateLimits, now: u64) -> Line<'static> {
    let primary_used = limits.primary.used_percent.clamp(0.0, 100.0);
    let filled = ((primary_used / 100.0) * 12.0).round() as usize;
    let color = usage_color(primary_used);
    let mut spans = vec![Span::styled("  Codex 5h ", theme::dim())];
    spans.extend(slim_bar(filled, 12, color));
    spans.push(Span::styled(
        format!(" {primary_used:.0}%"),
        ratatui::style::Style::default().fg(color),
    ));
    append_reset_countdown(&mut spans, limits.primary.resets_at, now);

    if let Some(weekly) = limits.secondary.filter(|limit| limit.used_percent >= 65.0) {
        let weekly_used = weekly.used_percent.clamp(0.0, 100.0);
        let weekly_filled = ((weekly_used / 100.0) * 12.0).round() as usize;
        let weekly_color = usage_color(weekly_used);
        spans.push(Span::styled(" · week ", theme::dim()));
        spans.extend(slim_bar(weekly_filled, 12, weekly_color));
        spans.push(Span::styled(
            format!(" {weekly_used:.0}%"),
            ratatui::style::Style::default().fg(weekly_color),
        ));
        append_reset_countdown(&mut spans, weekly.resets_at, now);
    }
    Line::from(spans)
}

fn usage_color(used_percent: f64) -> ratatui::style::Color {
    if used_percent >= 90.0 {
        theme::ERROR
    } else if used_percent >= 75.0 {
        theme::WARN
    } else {
        theme::OK
    }
}

fn append_reset_countdown(spans: &mut Vec<Span<'static>>, resets_at: u64, now: u64) {
    let Some(remaining) = resets_at.checked_sub(now).filter(|&seconds| seconds > 0) else {
        return;
    };
    spans.push(Span::styled(
        format!(" ↻ {}", brief_duration(remaining)),
        theme::dim(),
    ));
}

/// Compact countdown for the status line: enough precision for a human to
/// decide whether to wait, without turning the meter into a timestamp.
fn brief_duration(seconds: u64) -> String {
    if seconds < 60 {
        "<1m".into()
    } else if seconds < 3_600 {
        format!("{}m", seconds.div_ceil(60))
    } else if seconds < 86_400 {
        format!("{}h{}m", seconds / 3_600, (seconds % 3_600).div_ceil(60))
    } else {
        format!("{}d", seconds.div_ceil(86_400))
    }
}

fn add_usage(left: Usage, right: Usage) -> Usage {
    Usage {
        input_tokens: left.input_tokens.saturating_add(right.input_tokens),
        output_tokens: left.output_tokens.saturating_add(right.output_tokens),
        cache_read_tokens: left
            .cache_read_tokens
            .saturating_add(right.cache_read_tokens),
        cache_write_tokens: left
            .cache_write_tokens
            .saturating_add(right.cache_write_tokens),
    }
}

/// A human turn renders as a quoted block: an accent rail down the left, the
/// text otherwise unadorned. A note stays under the same rail — it is the
/// human speaking too — and is told apart by a coloured `Note:` opening its
/// first row. Live echo, ledger replay and approval notes all bake through
/// here, so the three paths cannot drift apart.
/// A multi-line prompt collapsed to one dim row: the queue is a status display,
/// not a second transcript. The real message is rendered in full once it is
/// delivered.
fn one_line(text: &str, budget: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(budget) {
        Some((cut, _)) => format!("{}…", &flat[..cut]),
        None => flat,
    }
}

/// Several prompts queued behind one turn become one prompt when that turn ends
/// — starting a turn per queued line would make the model answer the first one
/// with the rest still unsaid.
fn merge(queued: Vec<PendingMessage>) -> Option<PendingMessage> {
    let mut queued = queued.into_iter();
    let mut merged = queued.next()?;
    for next in queued {
        merged.text.push('\n');
        merged.text.push_str(&next.text);
        merged.attachments.extend(next.attachments);
        merged.blocks.extend(next.blocks);
    }
    Some(merged)
}

/// How a prompt appears in the transcript. The single renderer for both paths:
/// a prompt sent immediately and one that waited in the queue must be
/// indistinguishable once they land.
fn prompt_echo(text: &str, attachments: &[String]) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = vec![Line::default()];
    lines.extend(quote_lines(None, text));
    lines.extend(attachments.iter().map(|label| quote_attachment_line(label)));
    lines.push(Line::default());
    lines
}

fn quote_lines(label: Option<&str>, text: &str) -> Vec<Line<'static>> {
    text.lines()
        .enumerate()
        .map(|(i, row)| {
            let mut spans = vec![Span::styled(theme::USER_GUTTER, theme::user_gutter())];
            match label {
                Some(label) if i == 0 => {
                    spans.push(Span::styled(label.to_string(), theme::note_label()));
                }
                _ => {}
            }
            spans.push(Span::styled(row.to_string(), theme::user_message()));
            Line::from(spans)
        })
        .collect()
}

/// Attachments hang off the message they arrived with, under the same rail.
fn quote_attachment_line(label: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(theme::USER_GUTTER, theme::user_gutter()),
        Span::styled(format!("⌞ {label}"), theme::dim()),
    ])
}

/// A turn boundary should read as a small receipt, not as an unstructured
/// diagnostic log line. The numbers stay selectable/copyable terminal text,
/// while colour and arrows make input, output and cache scannable.
fn turn_summary_line(elapsed: f32, usage: Usage) -> Line<'static> {
    let cache_pct = if usage.total_input() > 0 {
        (usage.cache_read_tokens as f64 / usage.total_input() as f64 * 100.0).round()
    } else {
        0.0
    };
    let cache_style = if cache_pct > 0.0 {
        theme::accent()
    } else {
        theme::dim()
    };
    Line::from(vec![
        Span::styled("✓ completed ", theme::ok()),
        Span::styled(format!("{elapsed:.1}s"), theme::bold()),
        Span::styled(" · ↑", theme::dim()),
        // Uncached input only: the tokens this turn actually paid full price
        // for. Summing total_input() across a multi-step turn would recount
        // the cached prefix on every request; the cache figure below shows
        // how much of the full prompt was reused. This is a turn receipt, not
        // the window-occupancy figure the context meter reports.
        Span::styled(token_count(usage.input_tokens), theme::accent()),
        Span::styled(" · ↓", theme::dim()),
        Span::styled(
            token_count(usage.output_tokens),
            ratatui::style::Style::default().fg(theme::OK),
        ),
        Span::styled(" · cache ", theme::dim()),
        Span::styled(format!("{cache_pct:.0}%"), cache_style),
    ])
}

fn area_width(terminal: &Term) -> u16 {
    terminal.size().map(|s| s.width).unwrap_or(80)
}

fn area_height(terminal: &Term) -> u16 {
    terminal.size().map(|s| s.height).unwrap_or(24)
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

/// Wrap logical editor lines ourselves instead of leaving it to the terminal.
/// That keeps soft wraps out of copied text, gives continuation lines a stable
/// prefix, and makes the cursor/viewport agree with what is on screen.
/// A display-width-bounded slice of a logical line. Tracks display columns
/// (for mapping mouse clicks back to a cursor position) and char offsets
/// (for slicing the selection highlight).
struct LayoutChunk {
    text: String,
    start_col: usize,
    end_col: usize,
    char_start: usize,
    char_end: usize,
}

enum VisualMove {
    Up,
    Down,
}

fn move_editor_visual(editor: &mut Editor, layout: &EditorLayout, direction: VisualMove) -> bool {
    let target_row = match direction {
        VisualMove::Up => match layout.cursor_row.checked_sub(1) {
            Some(row) => row,
            None => return false,
        },
        VisualMove::Down => {
            let row = layout.cursor_row + 1;
            if row >= layout.lines.len() {
                return false;
            }
            row
        }
    };

    let target = &layout.lines[target_row];
    let display_col = (target.start_col + layout.cursor_col).min(target.end_col);
    editor.set_cursor_by_display_col(target.logical_row, display_col);
    true
}

fn editor_layout(editor: &Editor, terminal_width: u16) -> EditorLayout {
    use unicode_width::UnicodeWidthChar;

    // border + two-column prompt + one interior column on the right.
    let width = terminal_width.saturating_sub(4).max(1) as usize;
    let (cursor_line, cursor_col) = editor.cursor();
    let selection = editor.selection_bounds();
    let mut lines = Vec::new();
    let mut visual_cursor = (0, 0);

    for (logical_row, text) in editor.lines().iter().enumerate() {
        let mut chunks: Vec<LayoutChunk> = Vec::new();
        let mut chunk = String::new();
        let mut start_col = 0usize;
        let mut end_col = 0usize;
        let mut char_start = 0usize;
        let mut char_index = 0usize;
        for c in text.chars() {
            let char_width = c.width().unwrap_or(0);
            if !chunk.is_empty() && end_col - start_col + char_width > width {
                chunks.push(LayoutChunk {
                    text: std::mem::take(&mut chunk),
                    start_col,
                    end_col,
                    char_start,
                    char_end: char_index,
                });
                start_col = end_col;
                char_start = char_index;
            }
            chunk.push(c);
            end_col += char_width;
            char_index += 1;
        }
        if !chunk.is_empty() || chunks.is_empty() {
            chunks.push(LayoutChunk {
                text: chunk,
                start_col,
                end_col,
                char_start,
                char_end: char_index,
            });
        }

        if logical_row == cursor_line {
            let cursor_chunk = chunks
                .iter()
                .rposition(|c| c.start_col <= cursor_col && cursor_col <= c.end_col)
                .unwrap_or(chunks.len() - 1);
            let start = chunks[cursor_chunk].start_col;
            visual_cursor = (lines.len() + cursor_chunk, cursor_col.saturating_sub(start));
        }
        for (i, chunk) in chunks.into_iter().enumerate() {
            let selection = selection.and_then(|(s, e)| {
                selection_span(logical_row, chunk.char_start, chunk.char_end, s, e)
            });
            lines.push(EditorVisualLine {
                first_logical_line: i == 0,
                text: chunk.text,
                logical_row,
                start_col: chunk.start_col,
                end_col: chunk.end_col,
                selection,
            });
        }
    }
    EditorLayout {
        lines,
        cursor_row: visual_cursor.0,
        cursor_col: visual_cursor.1,
    }
}

/// Build read-only ghost rows with the exact same wrapping rules as the editor.
/// Keeping this route shared is what makes a two-line suggestion grow the input
/// box instead of overflowing its first row.
fn ghost_visual_lines(text: &str, terminal_width: u16) -> Vec<String> {
    let mut ghost = Editor::new();
    ghost.insert_str(text);
    editor_layout(&ghost, terminal_width)
        .lines
        .into_iter()
        .map(|line| line.text)
        .collect()
}

/// Char range within a wrapped chunk `[char_start, char_end)` that falls
/// inside the selection `[start, end]` (both in logical row/char coords).
/// Returns offsets relative to the chunk, or `None` if disjoint.
fn selection_span(
    row: usize,
    char_start: usize,
    char_end: usize,
    start: Position,
    end: Position,
) -> Option<(usize, usize)> {
    if row < start.row || row > end.row {
        return None;
    }
    let sel_from = if row == start.row { start.col } else { 0 };
    let sel_to = if row == end.row { end.col } else { usize::MAX };
    let from = sel_from.max(char_start);
    let to = sel_to.min(char_end);
    (from < to).then(|| (from - char_start, to - char_start))
}

fn paste_should_fold(chars: usize, lines: usize) -> bool {
    chars > PASTE_FOLD_CHARS || lines > PASTE_FOLD_LINES
}

fn reference_boundary(chars: &[char], at: usize) -> bool {
    at == 0 || (!chars[at - 1].is_alphanumeric() && chars[at - 1] != '_')
}

fn reference_token_char(c: char) -> bool {
    !c.is_whitespace()
        && !matches!(
            c,
            '@' | '`' | '"' | '\'' | ')' | '(' | '[' | ']' | '{' | '}' | ',' | ';' | ':'
        )
}

fn reference_score(path: &str, query: &str) -> Option<usize> {
    let path = path.to_lowercase();
    let query = query.to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    let basename = path.rsplit('/').next().unwrap_or(&path);
    if basename.starts_with(&query) {
        return Some(0);
    }
    if path.starts_with(&query) {
        return Some(1);
    }
    let mut next = 0;
    let mut gaps = 0;
    for wanted in query.chars() {
        let found = path[next..].find(wanted)?;
        gaps += found;
        next += found + wanted.len_utf8();
    }
    Some(10 + gaps)
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn reference_candidate_path(candidate: &ReferenceCandidate) -> String {
    match candidate.kind {
        ReferenceKind::Directory => format!("{}/", candidate.path),
        ReferenceKind::File => candidate.path.clone(),
    }
}

fn reference_basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn reference_basename_path(candidate: &ReferenceCandidate) -> String {
    let mut name = reference_basename(&candidate.path).to_string();
    if matches!(candidate.kind, ReferenceKind::Directory) {
        name.push('/');
    }
    name
}

fn reference_marker(path: &str) -> String {
    if path.chars().any(char::is_whitespace) {
        format!("@\"{path}\"")
    } else {
        format!("@{path}")
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum InputSpanStyle {
    Plain,
    Token,
    Selection,
}

/// Style exact project references and attachment placeholders directly in the
/// input, while preserving selection highlighting as the higher-priority
/// interaction state. An unrecognized `@token` remains ordinary prose.
fn input_spans(
    text: &str,
    selection: Option<(usize, usize)>,
    references: &[ReferenceCandidate],
) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let token_ranges = input_token_ranges(&chars, references);
    let style_at = |index| {
        if selection.is_some_and(|(from, to)| from <= index && index < to) {
            InputSpanStyle::Selection
        } else if token_ranges
            .iter()
            .any(|&(from, to)| from <= index && index < to)
        {
            InputSpanStyle::Token
        } else {
            InputSpanStyle::Plain
        }
    };

    let mut spans = Vec::new();
    let mut start = 0;
    let mut style = style_at(0);
    for index in 1..=chars.len() {
        let next_style = (index < chars.len()).then(|| style_at(index));
        if next_style == Some(style) {
            continue;
        }
        let segment: String = chars[start..index].iter().collect();
        let span = match style {
            InputSpanStyle::Plain => Span::raw(segment),
            InputSpanStyle::Token => Span::styled(segment, theme::accent()),
            InputSpanStyle::Selection => Span::styled(segment, theme::selection()),
        };
        spans.push(span);
        start = index;
        if let Some(next_style) = next_style {
            style = next_style;
        }
    }
    spans
}

fn input_token_ranges(chars: &[char], references: &[ReferenceCandidate]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '@' && reference_boundary(chars, index) {
            let mut end = index + 1;
            if chars.get(end) == Some(&'"') {
                end += 1;
                while end < chars.len() && chars[end] != '"' {
                    end += 1;
                }
                if end < chars.len() {
                    end += 1;
                }
            } else {
                while end < chars.len() && reference_token_char(chars[end]) {
                    end += 1;
                }
            }
            if known_reference_marker(chars, index, end, references) {
                ranges.push((index, end));
            }
            index = end;
            continue;
        }
        let attachment_end = ["[Image #", "[Pasted text #"].iter().find_map(|prefix| {
            let prefix_len = prefix.chars().count();
            (chars[index..]
                .iter()
                .take(prefix_len)
                .copied()
                .eq(prefix.chars()))
            .then(|| {
                let mut end = index + prefix_len;
                while end < chars.len() && chars[end].is_ascii_digit() {
                    end += 1;
                }
                (end > index + prefix_len && chars.get(end) == Some(&']')).then_some(end + 1)
            })
            .flatten()
        });
        if let Some(end) = attachment_end {
            ranges.push((index, end));
            index = end;
        } else {
            index += 1;
        }
    }
    ranges
}

fn known_reference_marker(
    chars: &[char],
    start: usize,
    end: usize,
    references: &[ReferenceCandidate],
) -> bool {
    let marker: String = chars[start..end].iter().collect();
    let Some(raw) = marker
        .strip_prefix("@\"")
        .and_then(|quoted| quoted.strip_suffix('"'))
        .or_else(|| marker.strip_prefix('@'))
    else {
        return false;
    };
    references.iter().any(|candidate| match candidate.kind {
        ReferenceKind::File => raw == candidate.path,
        ReferenceKind::Directory => raw.trim_end_matches('/') == candidate.path,
    })
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
            tcode_core::ExternalSource,
            Result<tcode_core::Resumed, tcode_core::store::StoreError>,
        )>,
    >,
) -> (
    tcode_core::ExternalSource,
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

    #[test]
    fn long_or_multiline_pastes_fold_into_attachments() {
        assert!(!paste_should_fold(PASTE_FOLD_CHARS, 1));
        assert!(paste_should_fold(PASTE_FOLD_CHARS + 1, 1));
        assert!(!paste_should_fold(1, PASTE_FOLD_LINES));
        assert!(paste_should_fold(1, PASTE_FOLD_LINES + 1));
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
    fn ghost_suggestion_uses_the_editor_wrap_width() {
        // Width 10 leaves six input cells after the border and prompt gutter.
        assert_eq!(ghost_visual_lines("abcdefghi", 10), ["abcdef", "ghi"]);
    }

    #[test]
    fn editor_layout_wraps_without_losing_cursor_position() {
        let mut editor = Editor::new();
        editor.insert_str("abcdefghi");
        // Width 10 leaves six cells inside the input border and prompt.
        let layout = editor_layout(&editor, 10);
        assert_eq!(
            layout
                .lines
                .iter()
                .map(|vl| vl.text.as_str())
                .collect::<Vec<_>>(),
            ["abcdef", "ghi"]
        );
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 3));
    }

    #[test]
    fn editor_layout_places_boundary_cursor_on_next_soft_wrap() {
        let mut editor = Editor::new();
        editor.insert_str("abcdefghi");
        editor.set_cursor(0, 6);
        let layout = editor_layout(&editor, 10);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 0));
    }

    #[test]
    fn editor_visual_move_crosses_soft_wrapped_lines() {
        let mut editor = Editor::new();
        editor.insert_str("abcdefghi");
        // Width 10 leaves six cells inside the input border and prompt:
        // visual rows are "abcdef" and "ghi".
        let layout = editor_layout(&editor, 10);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 3));

        assert!(move_editor_visual(&mut editor, &layout, VisualMove::Up));
        assert_eq!(editor.position(), Position { row: 0, col: 3 });

        let layout = editor_layout(&editor, 10);
        assert!(move_editor_visual(&mut editor, &layout, VisualMove::Down));
        assert_eq!(editor.position(), Position { row: 0, col: 9 });
    }

    #[test]
    fn editor_layout_marks_selection_across_a_soft_wrap() {
        let mut editor = Editor::new();
        editor.insert_str("abcdefghi");
        // Select chars 4..8 ("efgh"), which straddles the wrap at 6.
        editor.set_cursor(0, 4);
        editor.start_selection_by_display_col(0, 4);
        editor.extend_selection_by_display_col(0, 8);
        let layout = editor_layout(&editor, 10);
        // First visual line "abcdef": tail "ef" (offsets 4..6) selected.
        assert_eq!(layout.lines[0].selection, Some((4, 6)));
        // Second visual line "ghi": head "gh" (offsets 0..2) selected.
        assert_eq!(layout.lines[1].selection, Some((0, 2)));
    }

    #[test]
    fn editor_layout_keeps_explicit_newlines_distinct_from_soft_wraps() {
        let mut editor = Editor::new();
        editor.insert_str("abc\ndef");
        let layout = editor_layout(&editor, 10);
        assert_eq!(
            layout
                .lines
                .iter()
                .map(|vl| (vl.first_logical_line, vl.text.as_str()))
                .collect::<Vec<_>>(),
            [(true, "abc"), (true, "def")]
        );
    }

    #[test]
    fn visible_phase_range_focuses_in_progress_item() {
        let phases = (0..8)
            .map(|i| ProgressPhase {
                phase: format!("Phase {i}"),
                status: if i == 5 { "in_progress" } else { "pending" }.to_string(),
            })
            .collect::<Vec<_>>();
        assert_eq!(visible_phase_range(&phases, 5), (3, 8));
    }

    #[test]
    fn visible_phase_range_falls_back_to_first_pending() {
        let phases = (0..8)
            .map(|i| ProgressPhase {
                phase: format!("Phase {i}"),
                status: if i < 4 { "completed" } else { "pending" }.to_string(),
            })
            .collect::<Vec<_>>();
        assert_eq!(visible_phase_range(&phases, 5), (2, 7));
    }

    #[test]
    fn input_tokens_accent_only_known_references_and_attachment_placeholders() {
        let references = [ReferenceCandidate {
            path: "src/app.rs".into(),
            kind: ReferenceKind::File,
            bytes: Some(1),
        }];
        let spans = input_spans(
            "see @src/app.rs, but @not-a-file [Image #2] and me@example.com",
            None,
            &references,
        );
        let accented: Vec<_> = spans
            .iter()
            .filter(|span| span.style.fg == Some(theme::ACCENT))
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(accented, ["@src/app.rs", "[Image #2]"]);
        let plain: String = spans
            .iter()
            .filter(|span| span.style.fg.is_none())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            plain.contains("@not-a-file"),
            "unmatched at-sign text is ordinary prose"
        );
    }

    #[test]
    fn selection_overrides_input_token_accent() {
        let references = [ReferenceCandidate {
            path: "src/app.rs".into(),
            kind: ReferenceKind::File,
            bytes: Some(1),
        }];
        let spans = input_spans("@src/app.rs", Some((0, 11)), &references);
        assert_eq!(spans.len(), 1);
        assert_ne!(spans[0].style.fg, Some(theme::ACCENT));
    }

    #[test]
    fn reference_labels_use_basenames_unless_they_conflict() {
        let file = ReferenceCandidate {
            path: "crates/tcode-tui/src/app.rs".into(),
            kind: ReferenceKind::File,
            bytes: Some(1),
        };
        let directory = ReferenceCandidate {
            path: "crates/tcode-tui/src".into(),
            kind: ReferenceKind::Directory,
            bytes: None,
        };
        assert_eq!(reference_basename_path(&file), "app.rs");
        assert_eq!(reference_basename_path(&directory), "src/");
        assert_eq!(
            reference_marker(&reference_candidate_path(&file)),
            "@crates/tcode-tui/src/app.rs"
        );
    }

    #[test]
    fn reference_matching_prefers_basenames_then_fuzzy_paths() {
        assert_eq!(
            reference_score("crates/tcode-tui/src/app.rs", "app"),
            Some(0)
        );
        assert_eq!(
            reference_score("crates/tcode-tui/src/app.rs", "crates"),
            Some(1)
        );
        assert!(reference_score("crates/tcode-tui/src/app.rs", "tuiapp").is_some());
        assert!(reference_score("crates/tcode-tui/src/app.rs", "xyz").is_none());
    }

    #[test]
    fn reference_token_avoids_email_addresses() {
        let email: Vec<char> = "me@example.com".chars().collect();
        assert!(!reference_boundary(&email, 2));
        let mention: Vec<char> = "read @src".chars().collect();
        assert!(reference_boundary(&mention, 5));
    }

    #[test]
    fn visible_phase_range_shows_tail_when_all_complete() {
        let phases = (0..8)
            .map(|i| ProgressPhase {
                phase: format!("Phase {i}"),
                status: "completed".to_string(),
            })
            .collect::<Vec<_>>();
        assert_eq!(visible_phase_range(&phases, 5), (3, 8));
    }

    #[test]
    fn normal_usage_meters_use_the_tool_green() {
        let context = context_progress_line(20_000, 200_000, 80, false);
        assert!(context
            .spans
            .iter()
            .any(|span| span.style.fg == Some(theme::OK)));

        let limits = tcode_core::RateLimits {
            primary: tcode_core::RateLimit {
                used_percent: 30.0,
                window_minutes: 300,
                resets_at: 14_800,
            },
            secondary: None,
        };
        let rate_limit = rate_limit_line_at(limits, 10_000);
        assert!(rate_limit
            .spans
            .iter()
            .any(|span| span.style.fg == Some(theme::OK)));
    }

    #[test]
    fn context_meter_reports_percent_and_warning_color() {
        let line = context_progress_line(170_000, 200_000, 80, false);
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("context"));
        assert!(text.contains("85% · 170k/200k"));
        assert!(line
            .spans
            .iter()
            .any(|span| span.style.fg == Some(theme::WARN)));
    }

    #[test]
    fn codex_rate_limit_line_shows_used_percent_and_reset_countdowns() {
        let limits = tcode_core::RateLimits {
            primary: tcode_core::RateLimit {
                used_percent: 30.0,
                window_minutes: 300,
                resets_at: 14_800,
            },
            secondary: Some(tcode_core::RateLimit {
                used_percent: 65.0,
                window_minutes: 10_080,
                resets_at: 269_200,
            }),
        };
        let text = rate_limit_line_at(limits, 10_000)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("Codex 5h"));
        assert!(text.contains(" 30% ↻ 1h20m"));
        assert!(text.contains("week "));
        assert!(text.contains(" 65% ↻ 3d"));
    }

    #[test]
    fn brief_duration_stays_compact_at_unit_boundaries() {
        assert_eq!(brief_duration(59), "<1m");
        assert_eq!(brief_duration(60), "1m");
        assert_eq!(brief_duration(3_601), "1h1m");
        assert_eq!(brief_duration(86_401), "2d");
    }

    #[test]
    fn codex_rate_limit_line_hides_week_below_65_percent() {
        let limits = tcode_core::RateLimits {
            primary: tcode_core::RateLimit {
                used_percent: 30.0,
                window_minutes: 300,
                resets_at: 0,
            },
            secondary: Some(tcode_core::RateLimit {
                used_percent: 64.9,
                window_minutes: 10_080,
                resets_at: 0,
            }),
        };
        let text = rate_limit_line(limits)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(!text.contains("week"));
    }

    #[test]
    fn turn_summary_is_a_scannable_receipt() {
        let line = turn_summary_line(
            2.5,
            Usage {
                input_tokens: 1_178,
                output_tokens: 23,
                ..Usage::default()
            },
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "✓ completed 2.5s · ↑1.2k · ↓23 · cache 0%");
    }

    #[test]
    fn delegated_usage_is_added_without_losing_cache_fields() {
        let total = add_usage(
            Usage {
                input_tokens: 10,
                output_tokens: 2,
                cache_read_tokens: 3,
                cache_write_tokens: 4,
            },
            Usage {
                input_tokens: 20,
                output_tokens: 5,
                cache_read_tokens: 6,
                cache_write_tokens: 7,
            },
        );
        assert_eq!(total.input_tokens, 30);
        assert_eq!(total.output_tokens, 7);
        assert_eq!(total.cache_read_tokens, 9);
        assert_eq!(total.cache_write_tokens, 11);
    }
}
