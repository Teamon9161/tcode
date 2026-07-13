use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind,
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
    Agent, AgentError, AgentEvent, Approval, ApprovalDecision, Approver, ContentBlock, Session,
    Usage,
};

use crate::approval::{Dialog, DialogResult};
use crate::editor::{Editor, Position};
use crate::model_picker::{self, ModelMenu};
use crate::resume::{self, PickResult as ResumePickResult};
use crate::transcript::Transcript;
use crate::{diff, markdown, theme, OpeningContextFn};

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Lines scrolled per mouse-wheel event.
const WHEEL_STEP: usize = 3;
/// Visible rows of an expanded tool-output region.
const OUTPUT_VIEW_ROWS: usize = 12;
/// Plan panel rows should stay small and predictable; long plans render as a
/// focused window around the active step instead of stealing scroll focus.
const PLAN_VISIBLE_STEPS: usize = 5;

/// Second Esc within this window (while idle) opens the rewind picker.
const DOUBLE_ESC: Duration = Duration::from_millis(1200);

const PASTE_FOLD_LINES: usize = 15;
/// Long one-line pastes should not make the editor visibly type character by
/// character. They are sent as a text attachment instead.
const PASTE_FOLD_CHARS: usize = 1_000;
/// A calm, low-contrast alternative to the legacy sparkle animation.
const CALM_SPINNER: [(&str, ratatui::style::Color); 4] = [
    (".", theme::DIM),
    ("o", theme::DIM),
    ("O", theme::DIM),
    ("o", theme::DIM),
];

/// Commands whose substance drives frontend-owned objects (key table, model
/// picker, provider wizard). Everything else lives in the shared
/// `CommandRegistry` in tcode-core.
const UI_COMMANDS: [(&str, &str); 3] = [
    ("/help", "show keys and commands"),
    ("/model", "switch model · adjust reasoning effort"),
    ("/provider", "configure or switch provider"),
];

pub struct AskMsg {
    pub tool: String,
    pub summary: String,
    pub descriptor: String,
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
        input: &serde_json::Value,
    ) -> Approval {
        let (reply, rx) = oneshot::channel();
        let msg = AskMsg {
            tool: tool.to_string(),
            summary: summary.to_string(),
            descriptor: descriptor.to_string(),
            input: input.clone(),
            reply,
        };
        if self.tx.send(msg).await.is_err() {
            return Approval {
                decision: ApprovalDecision::No,
                comment: Some("UI unavailable".into()),
            };
        }
        rx.await.unwrap_or(Approval {
            decision: ApprovalDecision::No,
            comment: None,
        })
    }
}

enum Attachment {
    Image { id: u32, png: Vec<u8>, label: String },
    Text { id: u32, content: String, label: String },
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

#[derive(Clone, Copy)]
struct InputHitbox {
    rect: Rect,
    editor_start: usize,
}

struct PendingCall {
    detail: String,
    /// Batch items defer their `├ summary` line (plus any diff) to here, so
    /// `ToolEnd` can bake it directly above this call's own result instead of
    /// baking every item first and every result after. Empty for single calls
    /// (their header is baked at `ToolStart`).
    header: Vec<Line<'static>>,
}

struct PlanStep {
    step: String,
    status: String,
}

impl PlanStep {
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
    /// Tool name → its UI display name, snapshotted from the agent's tools
    /// so headers can label calls without a live `Tool` handle.
    display_names: HashMap<String, String>,
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
    pending_tool: Option<PendingCall>,
    /// Entries belonging to a concurrent group, completed in model-call
    /// order. Keeping them queued lets each result retain its own input.
    pending_batch: VecDeque<PendingCall>,
    plan: Vec<PlanStep>,
    last_esc: Option<Instant>,
    popup_index: usize,

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
        opening_context: OpeningContextFn,
    ) -> anyhow::Result<Self> {
        let (ask_tx, ask_rx) = mpsc::channel(4);
        let mode_label = session.mode.label().to_string();
        let cwd = session.tool_ctx.cwd.clone();
        let context_estimated = session.last_prompt_tokens == 0 && !session.ledger.is_empty();
        let context_tokens = if context_estimated {
            estimate_context_tokens(&agent, &session)
        } else {
            session.last_prompt_tokens
        };
        // Keep the agent's automatic-compaction guard and status block in
        // step with the UI even when tcode was launched with `--resume`.
        session.last_prompt_tokens = context_tokens;
        let display_names = agent
            .tools
            .iter()
            .map(|t| (t.name().to_string(), t.display_name()))
            .collect();
        let terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
        let transcript = Transcript::new(terminal.size().map(|s| s.width).unwrap_or(80));
        Ok(Self {
            agent,
            opening_context,
            registry: CommandRegistry::builtin(),
            session: Some(session),
            cwd,
            display_names,
            terminal,
            transcript,
            md: markdown::Renderer::default(),
            phase: Phase::Idle,
            events_rx: None,
            external_import: None,
            ask_rx,
            approver: Arc::new(ChannelApprover { tx: ask_tx }),
            editor: Editor::new(),
            attachments: Vec::new(),
            next_attachment_id: 1,
            clipboard: arboard::Clipboard::new().ok(),
            input_hitbox: None,
            input_mouse_active: false,
            input_dragged: false,
            dialog: None,
            change_prebake: None,
            rewind_nav: None,
            resume_picker: None,
            menu,
            model_picker: None,
            pending_tool: None,
            pending_batch: VecDeque::new(),
            plan: Vec::new(),
            last_esc: None,
            popup_index: 0,
            live_text: String::new(),
            live_block: None,
            thinking_chars: 0,
            thinking_text: String::new(),
            thinking_since: None,
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
        let mut term_events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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
                Some(ask) = self.ask_rx.recv() => {
                    if self.user_wait_started.is_none() {
                        self.user_wait_started = Some(Instant::now());
                    }
                    let dialog = if ask.tool == "ask_user" {
                        Dialog::questions(ask.summary, &ask.input)
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
                        let change = diff::render_change(&ask.tool, &ask.input);
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
                        Dialog::new(ask.summary, ask.descriptor, call_summary)
                    };
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
                        self.spinner = (self.spinner + 1) % CALM_SPINNER.len();
                    }
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

    /// Welcome box, Claude Code style: identity, model, cwd.
    fn banner(&self) -> Vec<Line<'static>> {
        use unicode_width::UnicodeWidthStr;
        let model = self.agent.model.snapshot();
        let title = format!("✻ tcode v{}", env!("CARGO_PKG_VERSION"));
        let rows = [
            format!("model  {} · {}", model.provider.name(), model.describe()),
            {
                let cwd = self
                    .session
                    .as_ref()
                    .map(|s| s.tool_ctx.cwd.display().to_string())
                    .unwrap_or_default();
                let home = std::env::var("HOME")
                    .or_else(|_| std::env::var("USERPROFILE"))
                    .unwrap_or_default();
                let cwd = match (home.is_empty(), cwd.strip_prefix(&home)) {
                    (false, Some(rest)) => format!("~{rest}"),
                    _ => cwd,
                };
                format!("cwd    {cwd}")
            },
        ];
        // Fit inside the terminal; overly long rows (deep cwd) keep
        // their tail, which is the informative end of a path.
        let term_w = self.terminal.size().map(|s| s.width).unwrap_or(80) as usize;
        let max_content = term_w.saturating_sub(6).max(20);
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
        let title = clip(&title);
        let rows: Vec<String> = rows.iter().map(|r| clip(r)).collect();
        let width = rows
            .iter()
            .map(|r| r.width())
            .chain([title.width()])
            .max()
            .unwrap_or(0);
        let pad = |s: &str| " ".repeat(width.saturating_sub(s.width()));
        let mut out = vec![Line::from(vec![Span::styled(
            format!("╭{}╮", "─".repeat(width + 2)),
            theme::border(),
        )])];
        out.push(Line::from(vec![
            Span::styled("│ ".to_string(), theme::border()),
            Span::styled(title.clone(), theme::user_prompt()),
            Span::raw(format!("{} ", pad(&title))),
            Span::styled("│".to_string(), theme::border()),
        ]));
        for row in &rows {
            out.push(Line::from(vec![
                Span::styled("│ ".to_string(), theme::border()),
                Span::styled(format!("{row}{} ", pad(row)), theme::dim()),
                Span::styled("│".to_string(), theme::border()),
            ]));
        }
        out.push(Line::styled(
            format!("╰{}╯", "─".repeat(width + 2)),
            theme::border(),
        ));
        out.push(Line::styled(
            "  /help commands · /model switch model · shift+tab permission mode",
            theme::dim(),
        ));
        out.push(Line::default());
        out
    }

    /// Replay a resumed conversation into the scrollback so the user
    /// sees where they left off.
    fn bake_transcript(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        if session.ledger.is_empty() {
            return;
        }
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut resumed_plan: Option<serde_json::Value> = None;
        let mut tool_calls: HashMap<String, (String, serde_json::Value)> = HashMap::new();
        for (entry_index, entry) in session.ledger.entries().iter().enumerate() {
            match entry {
                tcode_core::Entry::User(blocks) => {
                    // User echoes are their own entry-tagged blocks so
                    // rewind can jump to and truncate from them.
                    self.transcript.push(std::mem::take(&mut lines));
                    let mut echo: Vec<Line<'static>> = Vec::new();
                    for b in blocks {
                        match b {
                            ContentBlock::Text { text } if !text.starts_with("<tcode-status>") => {
                                for (i, l) in text.lines().enumerate() {
                                    let prefix = if i == 0 { "› " } else { "  " };
                                    echo.push(Line::from(vec![
                                        Span::styled(
                                            prefix.to_string(),
                                            theme::user_prompt_message(),
                                        ),
                                        Span::styled(l.to_string(), theme::user_message()),
                                    ]));
                                }
                            }
                            ContentBlock::Image { .. } => {
                                echo.push(Line::styled("  ⌞ [image]", theme::dim()));
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
                    for b in blocks {
                        match b {
                            ContentBlock::Text { text } => {
                                lines.extend(self.md.render(text));
                                lines.push(Line::default());
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                // Defer the header to the matching ToolResults
                                // entry so each call renders directly above its
                                // own result, not all headers then all results.
                                tool_calls.insert(id.clone(), (name.clone(), input.clone()));
                                if name == "update_plan" {
                                    resumed_plan = Some(input.clone());
                                }
                            }
                            _ => {}
                        }
                    }
                }
                tcode_core::Entry::IncompleteAssistant { text, error } => {
                    lines.extend(self.md.render(text));
                    lines.push(Line::styled(
                        format!(
                            "↻ stream failed: {error} — incomplete response retained; not sent back to model"
                        ),
                        theme::error_highlight(),
                    ));
                    lines.push(Line::default());
                }
                tcode_core::Entry::Summary(_) => {
                    lines.push(Line::styled(
                        "── earlier conversation compacted ──",
                        theme::dim(),
                    ));
                }
                tcode_core::Entry::ImportedTool {
                    name,
                    input,
                    content,
                } => {
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
                            lines.extend(self.md.render(content));
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
                        let (name, input) = tool_calls
                            .get(tool_use_id)
                            .map(|(name, input)| (name.as_str(), Some(input)))
                            .unwrap_or(("tool", None));
                        // Bake this call's header (+ diff / command block) right
                        // above its result. update_plan renders via the plan
                        // pane and ask_user via its own record — no tool header.
                        if let Some(input) = input {
                            if name != "update_plan" && name != "ask_user" {
                                let summary = self.display_summary(&call_header(name, input));
                                let mut spans: Vec<Span> = self.colored_tool_summary(&summary);
                                spans.insert(0, Span::styled("● ", theme::accent()));
                                lines.push(Line::from(spans));
                                lines.extend(diff::render_change(name, input));
                                lines.extend(diff::render_command(name, input));
                            }
                        }
                        let preview = result_preview(content);
                        let style = if *is_error {
                            ratatui::style::Style::default().fg(theme::ERROR)
                        } else {
                            theme::dim()
                        };
                        let detail = self.output_detail(name, input, &preview, content, *is_error);
                        if detail.is_empty() {
                            lines.push(Line::styled(format!("  ⎿ {preview}"), style));
                        } else {
                            let head = vec![self.output_head(name, &preview, content, *is_error)];
                            self.transcript.push(std::mem::take(&mut lines));
                            self.transcript
                                .push_with_detail(head, detail, false, OUTPUT_VIEW_ROWS);
                        }
                    }
                }
                tcode_core::Entry::Note(text) => {
                    for (i, line) in text.lines().enumerate() {
                        let prefix = if i == 0 {
                            "› note to model — "
                        } else {
                            "  "
                        };
                        lines.push(Line::from(vec![
                            Span::styled(prefix.to_string(), theme::user_prompt_message()),
                            Span::styled(line.to_string(), theme::user_message()),
                        ]));
                    }
                    lines.push(Line::default());
                }
            }
        }
        lines.push(Line::styled("── resumed ──", theme::dim()));
        if let Some(plan) = resumed_plan {
            self.update_plan(&plan);
        }
        self.bake(lines);
    }

    // ------------------------------------------------------------ turn

    fn start_turn(&mut self, input: String) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        self.clear_live_text();
        if !self.plan.is_empty() && self.plan.iter().all(PlanStep::is_completed) {
            self.plan.clear();
        }
        // Until the provider reports authoritative prompt usage, keep the
        // meter useful with a conservative local estimate. Text attachments
        // count here too; image token accounting is provider-specific.
        let attachment_tokens: u64 = self
            .attachments
            .iter()
            .filter(|a| input.contains(&a.placeholder()))
            .filter_map(|attachment| match attachment {
                Attachment::Text { content, .. } => Some(approx_tokens(content) as u64),
                Attachment::Image { .. } => None,
            })
            .sum();
        self.context_tokens = session
            .last_prompt_tokens
            .saturating_add(approx_tokens(&input) as u64)
            .saturating_add(attachment_tokens);
        self.context_step_start = self.context_tokens;
        // Echo the user input into the transcript, tagged with the ledger
        // index its User entry is about to occupy (rewind jumps to it).
        let entry_index = session.ledger.entries().len();
        let mut echo: Vec<Line> = Vec::new();
        for (i, l) in input.lines().enumerate() {
            let prefix = if i == 0 { "› " } else { "  " };
            echo.push(Line::from(vec![
                Span::styled(prefix.to_string(), theme::user_prompt_message()),
                Span::styled(l.to_string(), theme::user_message()),
            ]));
        }
        let mut blocks: Vec<ContentBlock> = Vec::new();
        for att in self.attachments.drain(..) {
            let placeholder = att.placeholder();
            // Dedup: if the user deleted the inline token from the draft, the
            // attachment goes with it — don't smuggle orphaned content along.
            if !input.contains(&placeholder) {
                continue;
            }
            match att {
                Attachment::Image { png, label, .. } => {
                    echo.push(Line::styled(format!("  ⌞ {label}"), theme::dim()));
                    use base64::Engine as _;
                    blocks.push(ContentBlock::Image {
                        media_type: "image/png".into(),
                        data: base64::engine::general_purpose::STANDARD.encode(png),
                    });
                }
                Attachment::Text { content, .. } => {
                    blocks.push(ContentBlock::Text {
                        text: format!("{placeholder}:\n{content}"),
                    });
                }
            }
        }
        self.next_attachment_id = 1;
        echo.push(Line::default());
        self.transcript.push_tagged(echo, entry_index);
        blocks.push(ContentBlock::Text { text: input });

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
        self.prev_cache_ratio = None;
        self.state_label = "compacting".into();
        let cancel = CancellationToken::new();
        let agent = self.agent.clone();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move {
            let result = agent
                .compact_with_focus(&mut session, focus.as_deref(), &cancel2)
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
            session.last_prompt_tokens = estimate_context_tokens(&self.agent, &session);
        }
        self.context_tokens = session.last_prompt_tokens;
        self.context_step_start = self.context_tokens;
        self.session = Some(session);
        self.phase = Phase::Idle;
        self.events_rx = None;
        self.state_label.clear();
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
    }

    fn drain_agent_events(&mut self) {
        loop {
            let ev = match self.events_rx.as_mut() {
                Some(rx) => rx.try_recv(),
                None => break,
            };
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
                let mut header_spans = vec![Span::styled("● ", theme::accent())];
                header_spans.extend(colored_batch_label(&label));
                self.bake(vec![Line::default(), Line::from(header_spans)]);
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
                    let item = batch_item_summary(&name, &input, Some(&self.cwd));
                    let mut row = vec![Span::raw("  ├ ")];
                    if mixed {
                        // Keep per-item tool tags subdued: the batch header is
                        // where display names get title-cased and highlighted.
                        row.push(Span::styled(format!("{name} "), theme::dim()));
                    }
                    row.push(Span::styled(item, theme::dim()));
                    let mut header = vec![Line::from(row)];
                    header.extend(diff::render_change(&name, &input));
                    self.pending_batch.push_back(PendingCall {
                        detail: serde_json::to_string_pretty(&input).unwrap_or_default(),
                        header,
                    });
                }
                self.state_label = format!("running: {label}");
            }
            AgentEvent::ToolStart {
                name,
                summary,
                input,
            } => {
                if name == "update_plan" {
                    self.update_plan(&input);
                    self.state_label = "updating plan".into();
                    return;
                }
                // The question and its answer are already baked by the
                // approval dialog; a second ask_user(...) header is noise.
                if name == "ask_user" {
                    return;
                }
                // Recompute the header from name+input so a long/multi-line
                // shell command collapses to `Shell` and renders as a block,
                // instead of the raw command string core put in `summary`.
                let _ = summary;
                let summary = self.display_summary(&call_header(&name, &input));
                self.bake_live_text();
                self.finish_thinking();
                if !self.pending_batch.is_empty() {
                    self.state_label = format!("running: {summary}");
                    return;
                }
                self.pending_tool = Some(PendingCall {
                    detail: serde_json::to_string_pretty(&input).unwrap_or_default(),
                    header: Vec::new(),
                });
                // If this call's diff was already baked in full while its
                // approval dialog was open, keep that block — don't render a
                // second, capped copy.
                if self.change_prebake.take().is_none() {
                    let mut spans: Vec<Span> = self.colored_tool_summary(&summary);
                    spans.insert(0, Span::styled("● ", theme::accent()));
                    let mut lines = vec![Line::default(), Line::from(spans)];
                    lines.extend(diff::render_change(&name, &input));
                    lines.extend(diff::render_command(&name, &input));
                    if lines.len() > 2 {
                        lines.push(Line::default());
                    }
                    self.bake(lines);
                }
                self.state_label = format!("running: {summary}");
            }
            AgentEvent::ToolEnd {
                name,
                preview,
                content,
                is_error,
                ..
            } => {
                if name == "update_plan" || name == "ask_user" {
                    self.state_label = "responding".into();
                    return;
                }
                let entry = self
                    .pending_tool
                    .take()
                    .or_else(|| self.pending_batch.pop_front());
                // Recover the call's input (stashed as JSON) to decide whether
                // the output is markdown before the result is appended to it.
                let (input, mut batch_header): (Option<serde_json::Value>, Vec<Line<'static>>) =
                    if let Some(entry) = entry {
                        (serde_json::from_str(&entry.detail).ok(), entry.header)
                    } else {
                        (None, Vec::new())
                    };
                // The gated result is exactly what is appended to the next
                // model request, so it belongs in the in-between estimate.
                self.context_tokens = self
                    .context_tokens
                    .saturating_add(approx_tokens(&content) as u64);
                // edit/write already rendered their diff at the call site; the
                // textual "edited … Result:" only repeats it, so a successful
                // one shows nothing further. (Errors still surface below.)
                if !is_error && matches!(name.as_str(), "edit" | "write") {
                    if !batch_header.is_empty() {
                        self.bake(batch_header);
                    }
                    self.state_label = "responding".into();
                    return;
                }
                let style = if is_error {
                    ratatui::style::Style::default().fg(theme::ERROR)
                } else {
                    theme::dim()
                };
                // The head row carries the fold affordance (added by the
                // transcript). For batch items that head is the `├ summary`
                // row; for single calls it is the `⎿ preview` row.
                let detail =
                    self.output_detail(&name, input.as_ref(), &preview, &content, is_error);
                if detail.is_empty() {
                    if batch_header.is_empty() {
                        self.bake(vec![Line::styled(format!("  ⎿ {preview}"), style)]);
                    } else {
                        append_result_preview(&mut batch_header, &preview, style);
                        self.bake(batch_header);
                    }
                } else if batch_header.is_empty() {
                    let head = vec![self.output_head(&name, &preview, &content, is_error)];
                    self.transcript
                        .push_with_detail(head, detail, false, OUTPUT_VIEW_ROWS);
                } else {
                    // Batch items already have a compact `├ summary` row. Hang
                    // the fold affordance off that row instead of adding a
                    // separate `⎿  ▸ N lines` line beneath every item.
                    if is_error || !suppress_output_preview(&name) {
                        append_result_preview(&mut batch_header, &preview, style);
                    }
                    self.transcript
                        .push_with_detail(batch_header, detail, false, OUTPUT_VIEW_ROWS);
                }
                self.state_label = "responding".into();
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
                let retained = partial_output_retained
                    .then_some(" — incomplete response retained; not sent back to model")
                    .unwrap_or("");
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

    /// Transcript record of a consent decision. An approved call renders
    /// via its ToolStart (header + diff), so approval bakes nothing but the
    /// user's note; a declined call (which never emits ToolStart) leaves a
    /// one-line record — the proposed diff never reaches the transcript.
    fn bake_approval_record(&mut self, dialog: &Dialog, approval: &Approval) {
        // Flush streamed text so the record keeps chronological order.
        self.bake_live_text();
        self.finish_thinking();
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
            ApprovalDecision::Yes | ApprovalDecision::YesAlways => {
                if let Some(note) = approval.comment.as_deref() {
                    self.bake(vec![
                        Line::default(),
                        Line::from(vec![
                            Span::styled("› note to model — ", theme::user_prompt_message()),
                            Span::styled(note.to_string(), theme::user_message()),
                        ]),
                    ]);
                }
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

    fn finish_thinking(&mut self) {
        if let Some(since) = self.thinking_since.take() {
            let secs = since.elapsed().as_secs().max(1);
            let title = format!("thought for {secs}s (~{} tok)", self.thinking_chars / 3);
            self.bake(vec![Line::styled(format!("✻ {title}"), theme::thinking())]);
            self.thinking_text.clear();
            self.thinking_chars = 0;
        }
    }

    fn refresh_live_text(&mut self) {
        if self.live_text.trim().is_empty() {
            return;
        }
        let lines = self.md.render(&self.live_text);
        if lines.is_empty() {
            return;
        }
        if let Some(index) = self.live_block {
            self.transcript.replace_block(index, lines);
        } else {
            let index = self.transcript.block_count();
            self.transcript.push(lines);
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
        let mut lines = self.md.render(&text);
        lines.push(Line::default());
        if let Some(index) = self.live_block.take() {
            self.transcript.replace_block(index, lines);
        } else {
            self.bake(lines);
        }
    }

    /// Tool's UI name, resolved from its own `display_name` when it belongs
    /// to this session; falls back to title-case for imported/unknown tools.
    fn display_name(&self, name: &str) -> String {
        self.display_names
            .get(name)
            .cloned()
            .unwrap_or_else(|| title_case_tool_name(name))
    }

    /// Split a tool summary like `shell(cargo test)` into colored spans: the
    /// tool's display name is green, the arguments are dim.
    fn colored_tool_summary(&self, summary: &str) -> Vec<Span<'static>> {
        if let Some(paren) = summary.find('(') {
            vec![
                Span::styled(self.display_name(&summary[..paren]), theme::ok()),
                Span::styled(summary[paren..].to_string(), theme::dim()),
            ]
        } else {
            vec![Span::styled(self.display_name(summary), theme::bold())]
        }
    }

    fn output_head(
        &self,
        name: &str,
        preview: &str,
        content: &str,
        is_error: bool,
    ) -> Line<'static> {
        let style = if is_error {
            ratatui::style::Style::default().fg(theme::ERROR)
        } else {
            theme::dim()
        };
        if !is_error && suppress_output_preview(name) {
            // The fold affordance already carries the line count ("▸ N lines"),
            // so the head only marks the result — no duplicate count here.
            let _ = content;
            Line::styled("  ⎿", style)
        } else {
            Line::styled(format!("  ⎿ {preview}"), style)
        }
    }

    /// The foldable body of a tool result. Markdown-shaped output (a
    /// `web_fetch`, or a `read` of a `.md` file) is rendered; everything
    /// else stays literal. Either way a left gutter bar delineates the
    /// expanded region, and the first line — already shown untruncated in
    /// the preview head — is dropped from plain output to avoid a duplicate.
    fn output_detail(
        &self,
        name: &str,
        input: Option<&serde_json::Value>,
        preview: &str,
        content: &str,
        is_error: bool,
    ) -> Vec<Line<'static>> {
        if content.trim() == preview.trim() && (is_error || !suppress_output_preview(name)) {
            return Vec::new(); // nothing beyond the preview
        }
        let gutter = || Span::styled("  │ ", theme::dim());
        let is_markdown = !is_error
            && (name == "web_fetch" || (name == "read" && input.is_some_and(path_is_markdown)));
        if is_markdown {
            return self
                .md
                .render(content)
                .into_iter()
                .map(|line| {
                    let mut spans = vec![gutter()];
                    spans.extend(line.spans);
                    Line::from(spans)
                })
                .collect();
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
            !suppress_output_preview(name)
                && first.chars().count() <= 120
                && content.lines().count() > 1,
        );
        content
            .lines()
            .skip(skip)
            .map(|line| Line::from(vec![gutter(), Span::styled(line.to_string(), text_style)]))
            .collect()
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
                    && self.rewind_nav.is_none()
                {
                    self.on_paste_text(text);
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
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
                    if !self.input_mouse_down(mouse.column, mouse.row) {
                        self.transcript.mouse_down(mouse.column, mouse.row);
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if self.input_mouse_active {
                        self.input_mouse_drag(mouse.column, mouse.row);
                    } else {
                        self.transcript.mouse_drag(mouse.column, mouse.row);
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    if self.input_mouse_active {
                        self.input_mouse_up(mouse.column, mouse.row);
                    } else if let Some(text) = self.transcript.mouse_up() {
                        self.copy_selection(text);
                    }
                }
                _ => {}
            },
            // ratatui's autoresize adapts on the next draw; the transcript
            // rewraps lazily from the new area width.
            Event::Resize(..) => {}
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        // Model picker captures everything while open.
        if let Some(picker) = self.resume_picker.as_mut() {
            match picker.handle_key(key) {
                ResumePickResult::Pending => {}
                ResumePickResult::Cancelled => self.resume_picker = None,
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
                model_picker::PickResult::Picked { index, effort } => {
                    self.model_picker = None;
                    self.apply_model(index, effort);
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
            if let DialogResult::Done(approval) = dialog.handle_key(key) {
                let (dialog, reply) = self.dialog.take().expect("dialog present");
                self.bake_approval_record(&dialog, &approval);
                let _ = reply.send(approval);
                if let Some(wait_started) = self.user_wait_started.take() {
                    self.user_wait_total += wait_started.elapsed();
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
                // Ctrl+C is the single terminal-standard interrupt ladder:
                // cancel a running turn, else clear the input, else exit.
                // Copy lives on Ctrl+Shift+C / Alt+C and mouse-release, so
                // this key never has to disambiguate copy vs interrupt.
                if running {
                    self.cancel_turn();
                } else if !self.editor.is_empty() || !self.attachments.is_empty() {
                    self.clear_draft();
                } else {
                    self.should_exit = true;
                }
            }
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
            KeyCode::BackTab => {
                if let Some(session) = self.session.as_mut() {
                    session.mode = session.mode.cycle();
                    self.mode_label = session.mode.label().to_string();
                } else {
                    self.bake(vec![Line::styled(
                        "mode can be changed when idle",
                        theme::dim(),
                    )]);
                }
            }
            KeyCode::Tab => {
                if let Some(cmd) = self.popup_selection() {
                    self.editor.clear();
                    self.editor.insert_str(&cmd);
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
                    self.popup_index = (self.popup_index + 1).min(self.popup_matches().len() - 1);
                } else if !self.editor_visual_down() && self.editor.line_count() == 1 {
                    self.editor.history_next();
                }
            }
            KeyCode::Left => self.editor.left(),
            KeyCode::Right => self.editor.right(),
            KeyCode::Home => self.editor.home(),
            KeyCode::End => self.editor.end(),
            KeyCode::PageUp => self.transcript.page_up(),
            KeyCode::PageDown => self.transcript.page_down(),
            KeyCode::Backspace => {
                if !self.backspace_attachment_token() {
                    self.editor.backspace();
                }
            }
            KeyCode::Delete => self.editor.delete(),
            KeyCode::Char(c) => {
                self.editor.insert_char(c);
                self.popup_index = 0;
            }
            _ => {}
        }
    }

    fn submit(&mut self, running: bool) {
        let text = self.editor.text();
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() && self.attachments.is_empty() {
            return;
        }
        if trimmed.starts_with('/') {
            let cmd = self.popup_selection().unwrap_or_else(|| trimmed.clone());
            self.editor.take();
            self.run_slash(&cmd);
            return;
        }
        if running {
            return; // M3: queued follow-up messages
        }
        let input = self.editor.take();
        // Sending a message means the user is done reading history.
        self.transcript.scroll_to_bottom();
        self.start_turn(input);
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
        let Some(session) = self.session.as_mut() else {
            return;
        };
        // Visual truncation first: the transcript forgets the rewound tail
        // exactly like the ledger does. (False only for history without an
        // echo, e.g. compacted or imported conversations.)
        self.transcript.truncate_from_entry(index);
        session.ledger.truncate_tail(index);
        session.last_prompt_tokens = estimate_context_tokens(&self.agent, &session);
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
            ("enter", "send · shift/ctrl/alt+enter newline"),
            ("esc", "cancel current turn / clear input"),
            ("shift+tab", "cycle permission mode"),
            ("ctrl+v / alt+v", "paste (images/long text become inline tokens)"),
            ("ctrl+a", "select prompt · ctrl+c copy selection"),
            ("alt+c / alt+x", "copy / cut prompt"),
            ("mouse", "click prompt to move cursor · drag to copy"),
            ("backspace", "delete · after an [attachment] token drops it"),
            ("ctrl+c", "cancel / clear / exit"),
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
            }
        }
        for message in outcome.messages {
            self.bake_command_message(message);
        }
        // Cheap mirror sync instead of per-command effects: a command may
        // have moved the cwd (/cd) or cycled the permission mode (/mode).
        if let Some(session) = self.session.as_ref() {
            self.cwd = session.tool_ctx.cwd.clone();
            self.mode_label = session.mode.label().to_string();
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
            MessageKind::Note => vec![Line::from(vec![
                Span::styled("› note to model — ", theme::user_prompt_message()),
                Span::styled(message.text, theme::user_message()),
            ])],
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

    fn paste_from_clipboard(&mut self) {
        let Some(clipboard) = self.clipboard.as_mut() else {
            return;
        };
        // Pull owned data out while the clipboard borrow is held, then release
        // it before touching other `self` fields.
        let image = clipboard.get_image().ok().and_then(|img| {
            let (w, h) = (img.width, img.height);
            let rgba = image::RgbaImage::from_raw(w as u32, h as u32, img.bytes.into_owned())?;
            let mut png: Vec<u8> = Vec::new();
            image::DynamicImage::ImageRgba8(rgba)
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .ok()?;
            Some((png, w, h))
        });
        if let Some((png, w, h)) = image {
            let kb = png.len() / 1024;
            self.add_attachment(|id| Attachment::Image {
                id,
                png,
                label: format!("Image #{id} ({w}x{h}, {kb}KB)"),
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
        self.editor.clear();
        self.attachments.clear();
        self.next_attachment_id = 1;
    }

    /// Register an attachment and drop its inline token into the editor at the
    /// cursor. The token is how the user sees, moves, and deletes it — pressing
    /// backspace right after it removes the whole thing (see `on_key`).
    fn add_attachment(&mut self, make: impl FnOnce(u32) -> Attachment) {
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
        let before: String = self.editor.lines()[pos.row]
            .chars()
            .take(pos.col)
            .collect();
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
        self.dialog.is_none()
            && self.editor.line_count() == 1
            && self.editor.text().starts_with('/')
    }

    fn popup_matches(&self) -> Vec<(&str, &str)> {
        let prefix = self.editor.text();
        UI_COMMANDS
            .iter()
            .copied()
            .chain(self.registry.entries())
            .filter(|(c, _)| c.starts_with(&prefix))
            .collect()
    }

    fn popup_selection(&self) -> Option<String> {
        if !self.popup_active() {
            return None;
        }
        let matches = self.popup_matches();
        matches
            .get(self.popup_index.min(matches.len().saturating_sub(1)))
            .map(|(c, _)| (*c).to_string())
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
        if let Some(session) = self.session.as_mut() {
            if session.last_prompt_tokens == 0 && !session.ledger.is_empty() {
                session.last_prompt_tokens = estimate_context_tokens(&self.agent, session);
            }
            self.context_tokens = session.last_prompt_tokens;
            self.context_estimated = !session.ledger.is_empty();
        } else {
            self.context_tokens = 0;
            self.context_estimated = false;
        }
        self.context_step_start = self.context_tokens;
        self.prev_cache_ratio = None;
        self.plan.clear();
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
        let Some(session) = self.session.as_mut() else {
            return;
        };
        match result {
            Ok(resumed) => {
                let imported_id = resumed.store.id.clone();
                session.checkpoints = tcode_core::CheckpointStore::default();
                session.ledger = resumed.ledger;
                session.ledger.attach_sink(Box::new(resumed.store));
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

    fn update_plan(&mut self, input: &serde_json::Value) {
        let Some(items) = input["plan"].as_array() else {
            return;
        };
        self.plan = items
            .iter()
            .filter_map(|item| {
                let step = item["step"].as_str()?.trim();
                let status = item["status"].as_str()?;
                (!step.is_empty() && matches!(status, "pending" | "in_progress" | "completed"))
                    .then(|| PlanStep {
                        step: step.to_string(),
                        status: status.to_string(),
                    })
            })
            .collect();
    }

    fn plan_lines(&self) -> Vec<Line<'static>> {
        if self.plan.is_empty() {
            return Vec::new();
        }
        let complete = self.plan.iter().filter(|item| item.is_completed()).count();
        let (start, end) = visible_plan_range(&self.plan, PLAN_VISIBLE_STEPS);
        let hidden_before = start;
        let hidden_after = self.plan.len().saturating_sub(end);
        let mut lines = vec![Line::from(vec![
            Span::styled("  plan ", theme::bold().fg(theme::ACCENT)),
            Span::styled(
                format!("{complete}/{} complete", self.plan.len()),
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
        lines.extend(self.plan[start..end].iter().map(|item| {
            let (marker, style) = match item.status.as_str() {
                "completed" => ("✓ ", ratatui::style::Style::default().fg(theme::OK)),
                "in_progress" => ("● ", theme::accent()),
                _ => ("○ ", theme::dim()),
            };
            Line::from(vec![
                Span::styled(format!("    {marker}"), style),
                Span::styled(
                    item.step.clone(),
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
                self.dialog
                    .as_ref()
                    .map(|(d, _)| d.render(area_width(&self.terminal)))
            });
        let editor = editor_layout(&self.editor, area_width(&self.terminal));
        let popup: Vec<(String, String)> = if self.popup_active() {
            self.popup_matches()
                .into_iter()
                .map(|(c, d)| (c.to_string(), d.to_string()))
                .collect()
        } else {
            Vec::new()
        };
        let popup_index = self.popup_index.min(popup.len().saturating_sub(1));
        let plan_lines = self.plan_lines();

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
            let editor_h = ((editor.lines.len() - editor_start) as u16).clamp(1, 6);
            let panel_h = if let Some(lines) = &dialog_lines {
                lines.len() as u16 + 2
            } else {
                let mut h = editor_h + 2 + 2; // input box + context meter + hint
                if running {
                    h += 1; // spinner/status line above the input box
                }
                if !plan_lines.is_empty() {
                    h += plan_lines.len() as u16 + 2;
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
                return;
            }

            if !plan_lines.is_empty() {
                let h = plan_lines.len() as u16 + 2;
                frame.render_widget(
                    Paragraph::new(Text::from(plan_lines)).block(
                        Block::bordered()
                            .border_type(BorderType::Rounded)
                            .border_style(theme::border()),
                    ),
                    row(y, h),
                );
                y += h;
            }

            if running {
                frame.render_widget(Paragraph::new(status), row(y, 1));
                y += 1;
            }

            // Input inside a rounded box, Claude Code style.
            // Show the cursor even when a long multi-line prompt exceeds
            // the six-row input box.
            let inner: Vec<Line> = editor.lines[editor_start..]
                .iter()
                .take(6)
                .map(|vl| {
                    let mut spans = vec![Span::styled(
                        if vl.first_logical_line { "› " } else { "  " },
                        theme::user_prompt(),
                    )];
                    match vl.selection {
                        Some((from, to)) => {
                            let chars: Vec<char> = vl.text.chars().collect();
                            let pre: String = chars[..from].iter().collect();
                            let mid: String = chars[from..to].iter().collect();
                            let post: String = chars[to..].iter().collect();
                            if !pre.is_empty() {
                                spans.push(Span::raw(pre));
                            }
                            spans.push(Span::styled(mid, theme::selection()));
                            if !post.is_empty() {
                                spans.push(Span::raw(post));
                            }
                        }
                        None => spans.push(Span::raw(vl.text.clone())),
                    }
                    Line::from(spans)
                })
                .collect();
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

            frame.render_widget(Paragraph::new(Line::styled(hint, theme::dim())), row(y, 1));
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
        let (frame, color) = CALM_SPINNER[self.spinner % CALM_SPINNER.len()];
        Line::from(vec![
            Span::styled(
                format!("{frame} "),
                ratatui::style::Style::default().fg(color),
            ),
            Span::styled(self.state_label.clone(), theme::accent()),
            Span::styled(
                format!(
                    " · {elapsed}s · ↓ ~{} tok · esc to cancel",
                    token_count(self.out_tokens as u64)
                ),
                theme::dim(),
            ),
        ])
    }

    /// Dim one-liner under the input box: mode, model, cache health.
    fn idle_hint(&self) -> String {
        if let Some(nav) = &self.rewind_nav {
            let files = if nav.candidates[nav.pos].dirty {
                " · ctrl+r rewind + restore files"
            } else {
                ""
            };
            return format!("  ↺ rewind: enter confirm{files} · esc/↑ older · ↓ newer/exit");
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
        let notice = self
            .notice
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(3))
            .map(|(text, _)| format!(" · {text}"))
            .unwrap_or_default();
        format!(
            "  mode {} · {}{}{}{} · /help",
            self.mode_label,
            self.agent.model.snapshot().describe(),
            cache,
            scrolled,
            notice
        )
    }
}

fn visible_plan_range(plan: &[PlanStep], max_visible: usize) -> (usize, usize) {
    if plan.len() <= max_visible || max_visible == 0 {
        return (0, plan.len());
    }
    let focus = plan
        .iter()
        .position(|item| item.status == "in_progress")
        .or_else(|| plan.iter().position(|item| item.status == "pending"))
        .unwrap_or(plan.len() - 1);
    let mut start = focus.saturating_sub(max_visible / 2);
    start = start.min(plan.len().saturating_sub(max_visible));
    (start, start + max_visible)
}

fn append_result_preview(
    lines: &mut Vec<Line<'static>>,
    preview: &str,
    style: ratatui::style::Style,
) {
    if preview.is_empty() {
        return;
    }
    let span = Span::styled(format!("  ⎿ {preview}"), style);
    if let Some(last) = lines.last_mut() {
        last.spans.push(span);
    } else {
        lines.push(Line::from(vec![span]));
    }
}

/// Whether a `read` call targets a Markdown file (so its output is worth
/// rendering rather than showing raw). Keyed on the tool's `path`/`file_path`.
fn path_is_markdown(input: &serde_json::Value) -> bool {
    input["path"]
        .as_str()
        .or_else(|| input["file_path"].as_str())
        .map(|p| p.rsplit('.').next().unwrap_or("").to_ascii_lowercase())
        .is_some_and(|ext| matches!(ext.as_str(), "md" | "markdown" | "mdx"))
}

/// Fallback UI name for a tool whose handle we no longer hold: title-case.
/// The authoritative name is `Tool::display_name`, resolved via
/// `App::display_name` when the tool is in this session's set.
fn title_case_tool_name(name: &str) -> String {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    first.to_uppercase().collect::<String>() + chars.as_str()
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

// These are the parallel-read-only tools (core's `BatchPolicy::ParallelReadOnly`):
// their output is a precise, self-explanatory listing, so the transcript hides
// the preview. The TUI only sees the tool name (no `Tool` handle), so this list
// must track that set by hand — keep them in sync.
fn suppress_output_preview(name: &str) -> bool {
    matches!(name, "read" | "grep" | "glob")
}

/// Header text for a tool call. A long or multi-line shell command collapses
/// to the bare tool name; its full command renders below via
/// `diff::render_command`, so the header line stays intact.
fn call_header(name: &str, input: &serde_json::Value) -> String {
    if diff::command_is_block(name, input) {
        name.to_string()
    } else {
        tcode_core::agent::summarize_call(name, input)
    }
}

/// First line of a command, capped, with a note when more lines follow. Keeps
/// a multi-line command from corrupting a compact one-line batch row.
fn command_first_line(cmd: &str) -> String {
    let mut line = cmd.lines().next().unwrap_or("").to_string();
    if line.chars().count() > 120 {
        line = line.chars().take(120).collect::<String>() + "…";
    }
    let extra = cmd.lines().count().saturating_sub(1);
    if extra > 0 {
        line.push_str(&format!(" (+{extra} lines)"));
    }
    line
}

fn batch_item_summary(name: &str, input: &serde_json::Value, cwd: Option<&Path>) -> String {
    match name {
        "shell" | "bash" => command_first_line(input["command"].as_str().unwrap_or(name)),
        "read" => {
            let path = input_path(input)
                .map(|path| shorten_path(path, cwd))
                .unwrap_or_else(|| "<missing path>".into());
            let offset = input["offset"].as_u64().unwrap_or(1);
            match input["limit"].as_u64() {
                Some(limit) => format!("{path}:{offset}-{}", offset + limit - 1),
                None if offset > 1 => format!("{path}:{offset}-"),
                None => path,
            }
        }
        "edit" | "write" => input_path(input)
            .map(|path| shorten_path(path, cwd))
            .unwrap_or_else(|| name.to_string()),
        "grep" => input["pattern"].as_str().unwrap_or("grep").to_string(),
        "glob" => input["pattern"].as_str().unwrap_or("glob").to_string(),
        _ => shorten_summary_path(&tcode_core::agent::summarize_call(name, input), cwd),
    }
}

fn input_path(input: &serde_json::Value) -> Option<&str> {
    input["path"]
        .as_str()
        .or_else(|| input["file_path"].as_str())
}

fn shorten_path(path: &str, cwd: Option<&Path>) -> String {
    let Some(cwd) = cwd else {
        return path.to_string();
    };
    Path::new(path)
        .strip_prefix(cwd)
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|_| path.to_string())
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
    let label = if terminal_width < 42 {
        format!("  ctx {estimate_mark}{pct}% ")
    } else {
        format!(
            "  context {estimate_mark}{pct}% · {}/{} ",
            token_count(used),
            token_count(window)
        )
    };
    let bar_width = (terminal_width as usize)
        .saturating_sub(label.len() + 2)
        .clamp(6, 22);
    let filled = if used == 0 {
        0
    } else {
        ((bar_width as u64 * pct).div_ceil(100) as usize).min(bar_width)
    };
    let color = if pct >= 95 {
        theme::ERROR
    } else if pct >= 85 {
        theme::WARN
    } else {
        theme::ACCENT
    };
    Line::from(vec![
        Span::styled(label, theme::dim()),
        Span::styled("▕", ratatui::style::Style::default().fg(color)),
        Span::styled(
            "▰".repeat(filled),
            ratatui::style::Style::default().fg(color),
        ),
        Span::styled("▱".repeat(bar_width - filled), theme::dim()),
        Span::styled("▏", ratatui::style::Style::default().fg(color)),
    ])
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
    let mut spans = vec![
        Span::styled("  Codex 5h used ", theme::dim()),
        Span::styled("▕", ratatui::style::Style::default().fg(color)),
        Span::styled(
            "▰".repeat(filled),
            ratatui::style::Style::default().fg(color),
        ),
        Span::styled("▱".repeat(12 - filled), theme::dim()),
        Span::styled(
            format!("▏ {primary_used:.0}%"),
            ratatui::style::Style::default().fg(color),
        ),
    ];
    append_reset_countdown(&mut spans, limits.primary.resets_at, now);

    if let Some(weekly) = limits.secondary.filter(|limit| limit.used_percent >= 65.0) {
        let weekly_used = weekly.used_percent.clamp(0.0, 100.0);
        let weekly_filled = ((weekly_used / 100.0) * 12.0).round() as usize;
        let weekly_color = usage_color(weekly_used);
        spans.push(Span::styled(" · week used ", theme::dim()));
        spans.push(Span::styled(
            "▕",
            ratatui::style::Style::default().fg(weekly_color),
        ));
        spans.push(Span::styled(
            "▰".repeat(weekly_filled),
            ratatui::style::Style::default().fg(weekly_color),
        ));
        spans.push(Span::styled("▱".repeat(12 - weekly_filled), theme::dim()));
        spans.push(Span::styled(
            format!("▏ {weekly_used:.0}%"),
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
        theme::ACCENT
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
        Span::styled("  ╰─ ", theme::border()),
        Span::styled("completed ", theme::dim()),
        Span::styled(format!("{elapsed:.1}s"), theme::bold()),
        Span::styled("  ·  ↑ ", theme::dim()),
        // Uncached input only: the tokens this turn actually paid full price
        // for. Summing total_input() across a multi-step turn would recount
        // the cached prefix on every request; the cache figure below shows
        // how much of the full prompt was reused. This is a turn receipt, not
        // the window-occupancy figure the context meter reports.
        Span::styled(token_count(usage.input_tokens), theme::accent()),
        Span::styled(" new input", theme::dim()),
        Span::styled("  ·  ↓ ", theme::dim()),
        Span::styled(
            token_count(usage.output_tokens),
            ratatui::style::Style::default().fg(theme::OK),
        ),
        Span::styled(" output", theme::dim()),
        Span::styled("  ·  cache ", theme::dim()),
        Span::styled(format!("{cache_pct:.0}%"), cache_style),
    ])
}

/// JSONL persists the ledger but provider usage counters are deliberately
/// ephemeral. On resume, estimate the request shape from the same pieces the
/// provider receives (system prompt, tool definitions, and conversation).
/// Image accounting varies by provider, so use a modest fixed placeholder
/// until the first provider usage event corrects it.
fn estimate_context_tokens(agent: &Agent, session: &Session) -> u64 {
    let system = (approx_tokens(&agent.system) + approx_tokens(session.opening_context())) as u64;
    let tool_defs: u64 = agent
        .tools
        .iter()
        .map(|tool| {
            let schema = serde_json::to_string(&tool.input_schema()).unwrap_or_default();
            (approx_tokens(tool.name())
                + approx_tokens(tool.description())
                + approx_tokens(&schema)) as u64
        })
        .sum();
    let conversation: u64 = session
        .ledger
        .entries()
        .iter()
        .map(|entry| match entry {
            tcode_core::Entry::User(blocks)
            | tcode_core::Entry::Assistant(blocks)
            | tcode_core::Entry::ToolResults(blocks) => blocks
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text } => approx_tokens(text) as u64,
                    ContentBlock::Thinking {
                        thinking,
                        signature,
                    } => {
                        (approx_tokens(thinking)
                            + signature.as_deref().map(approx_tokens).unwrap_or_default())
                            as u64
                    }
                    ContentBlock::Image { .. } => 1_000,
                    ContentBlock::ToolUse { id, name, input } => {
                        (approx_tokens(id)
                            + approx_tokens(name)
                            + serde_json::to_string(input)
                                .map(|json| approx_tokens(&json))
                                .unwrap_or_default()) as u64
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => (approx_tokens(tool_use_id) + approx_tokens(content)) as u64,
                })
                .sum(),
            // These variants grow into small XML-like wrappers before the
            // provider sees them, so reserve a little structural overhead.
            tcode_core::Entry::Note(text) => approx_tokens(text) as u64 + 12,
            tcode_core::Entry::Summary(text) => approx_tokens(text) as u64 + 24,
            tcode_core::Entry::ImportedTool { .. }
            | tcode_core::Entry::IncompleteAssistant { .. } => 0,
        })
        .sum();
    system
        .saturating_add(tool_defs)
        .saturating_add(conversation)
}

fn shorten_summary_path(summary: &str, cwd: Option<&Path>) -> String {
    let Some(cwd) = cwd else {
        return summary.to_string();
    };
    let Some((tool, argument)) = summary.split_once('(') else {
        return summary.to_string();
    };
    let Some(argument) = argument.strip_suffix(')') else {
        return summary.to_string();
    };
    let Ok(relative) = Path::new(argument).strip_prefix(cwd) else {
        return summary.to_string();
    };
    format!("{tool}({})", relative.display())
}

fn area_width(terminal: &Term) -> u16 {
    terminal.size().map(|s| s.width).unwrap_or(80)
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
    fn attachment_placeholder_is_stable_per_id() {
        let img = Attachment::Image {
            id: 2,
            png: Vec::new(),
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
    fn project_paths_are_shortened_but_other_arguments_are_unchanged() {
        let cwd = Path::new("/work/tcode");
        assert_eq!(
            shorten_summary_path("read(/work/tcode/crates/core.rs)", Some(cwd)),
            "read(crates/core.rs)"
        );
        assert_eq!(
            shorten_summary_path("shell(cargo test)", Some(cwd)),
            "shell(cargo test)"
        );
        assert_eq!(
            shorten_summary_path("read(/tmp/other.rs)", Some(cwd)),
            "read(/tmp/other.rs)"
        );
    }

    #[test]
    fn visible_plan_range_focuses_in_progress_item() {
        let plan = (0..8)
            .map(|i| PlanStep {
                step: format!("step {i}"),
                status: if i == 5 { "in_progress" } else { "pending" }.to_string(),
            })
            .collect::<Vec<_>>();
        assert_eq!(visible_plan_range(&plan, 5), (3, 8));
    }

    #[test]
    fn visible_plan_range_falls_back_to_first_pending() {
        let plan = (0..8)
            .map(|i| PlanStep {
                step: format!("step {i}"),
                status: if i < 4 { "completed" } else { "pending" }.to_string(),
            })
            .collect::<Vec<_>>();
        assert_eq!(visible_plan_range(&plan, 5), (2, 7));
    }

    #[test]
    fn visible_plan_range_shows_tail_when_all_complete() {
        let plan = (0..8)
            .map(|i| PlanStep {
                step: format!("step {i}"),
                status: "completed".to_string(),
            })
            .collect::<Vec<_>>();
        assert_eq!(visible_plan_range(&plan, 5), (3, 8));
    }

    #[test]
    fn context_meter_reports_percent_and_warning_color() {
        let line = context_progress_line(170_000, 200_000, 80, false);
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("context 85% · 170k/200k"));
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

        assert!(text.contains("Codex 5h used"));
        assert!(text.contains("▏ 30% ↻ 1h20m"));
        assert!(text.contains("week used ▕"));
        assert!(text.contains("▏ 65% ↻ 3d"));
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

        assert!(!text.contains("week used"));
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
        assert_eq!(
            text,
            "  ╰─ completed 2.5s  ·  ↑ 1.2k new input  ·  ↓ 23 output  ·  cache 0%"
        );
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
