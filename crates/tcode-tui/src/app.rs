use std::collections::{HashSet, VecDeque};
use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::{
    cursor::MoveTo,
    execute,
    terminal::{Clear, ClearType},
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use tcode_core::blobs::approx_tokens;
use tcode_core::{
    Agent, AgentError, AgentEvent, Approval, ApprovalDecision, Approver, ContentBlock, Session,
    Usage,
};

use crate::approval::{Dialog, DialogResult};
use crate::editor::Editor;
use crate::model_picker::{self, ModelMenu};
use crate::resume::{self, PickResult as ResumePickResult};
use crate::rewind::{self, PickResult};
use crate::{diff, markdown, theme};

/// Second Esc within this window (while idle) opens the rewind picker.
const DOUBLE_ESC: Duration = Duration::from_millis(1200);

const PASTE_FOLD_LINES: usize = 15;
/// A calm, low-contrast alternative to the legacy sparkle animation.
const CALM_SPINNER: [(&str, ratatui::style::Color); 4] = [
    (".", theme::DIM),
    ("o", theme::DIM),
    ("O", theme::DIM),
    ("o", theme::DIM),
];

const SLASH_COMMANDS: [(&str, &str); 12] = [
    ("/help", "show keys and commands"),
    ("/model", "switch model · adjust reasoning effort"),
    ("/provider", "configure or switch provider"),
    ("/mode", "cycle permission mode"),
    ("/cost", "show last turn token usage"),
    ("/compact", "summarize history to free context"),
    ("/clear", "start a fresh conversation"),
    ("/resume", "resume a session: /resume <id>"),
    ("/note", "add a durable conversation note"),
    ("/memory", "show memory sources · /memory on|off"),
    ("/export", "export transcript: /export [path.md]"),
    ("/exit", "quit tcode"),
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
    Image { png: Vec<u8>, label: String },
    Text { content: String, label: String },
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
    lines: Vec<(bool, String)>,
    cursor_row: usize,
    cursor_col: usize,
}

struct ActivityEntry {
    title: String,
    detail: String,
    expanded: bool,
}

struct PlanStep {
    step: String,
    status: String,
}

pub struct App {
    agent: Arc<Agent>,
    session: Option<Session>,
    /// The TUI retains this while a turn owns `session`, so live tool calls
    /// can still render in-project paths relatively.
    cwd: PathBuf,
    terminal: Terminal<KnownPosBackend>,
    viewport_h: u16,
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
    dialog: Option<(Dialog, oneshot::Sender<Approval>)>,
    /// Change proposals (edit/write) already baked into scrollback before
    /// consent; ToolStart must not render them a second time.
    previewed_changes: HashSet<String>,
    rewind: Option<rewind::Picker>,
    resume_picker: Option<resume::Picker>,
    menu: ModelMenu,
    model_picker: Option<model_picker::Picker>,
    activity: Vec<ActivityEntry>,
    activity_open: bool,
    activity_selected: usize,
    activity_detail_scroll: usize,
    pending_tool: Option<ActivityEntry>,
    /// Entries belonging to a concurrent group, completed in model-call
    /// order. Keeping them queued lets each result retain its own detail.
    pending_batch: VecDeque<ActivityEntry>,
    plan: Vec<PlanStep>,
    last_esc: Option<Instant>,
    popup_index: usize,

    // Live (un-baked) streaming state: rendered only in the viewport,
    // baked into scrollback once finalized.
    live_text: String,
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
}

impl App {
    pub fn new(agent: Arc<Agent>, mut session: Session, menu: ModelMenu) -> anyhow::Result<Self> {
        let (ask_tx, ask_rx) = mpsc::channel(4);
        let mode_label = session.mode.label().to_string();
        let cwd = session.tool_ctx.cwd.clone();
        let context_estimated = session.last_prompt_tokens == 0 && !session.ledger.is_empty();
        let context_tokens = if context_estimated {
            estimate_context_tokens(&agent, &session.ledger)
        } else {
            session.last_prompt_tokens
        };
        // Keep the agent's automatic-compaction guard and status block in
        // step with the UI even when tcode was launched with `--resume`.
        session.last_prompt_tokens = context_tokens;
        let viewport_h = 4;
        let terminal = make_terminal(viewport_h, None)?;
        Ok(Self {
            agent,
            session: Some(session),
            cwd,
            terminal,
            viewport_h,
            md: markdown::Renderer::default(),
            phase: Phase::Idle,
            events_rx: None,
            external_import: None,
            ask_rx,
            approver: Arc::new(ChannelApprover { tx: ask_tx }),
            editor: Editor::new(),
            attachments: Vec::new(),
            dialog: None,
            previewed_changes: HashSet::new(),
            rewind: None,
            resume_picker: None,
            menu,
            model_picker: None,
            activity: Vec::new(),
            activity_open: false,
            activity_selected: 0,
            activity_detail_scroll: 0,
            pending_tool: None,
            pending_batch: VecDeque::new(),
            plan: Vec::new(),
            last_esc: None,
            popup_index: 0,
            live_text: String::new(),
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
            turn_usage: Usage::default(),
            mode_label,
            spinner: 0,
            prev_cache_ratio: None,
            should_exit: false,
            provider_setup_requested: false,
        })
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        self.clear_conversation_screen()?;
        self.bake_transcript();
        let mut term_events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        while !self.should_exit {
            let desired = self.desired_viewport();
            if desired != self.viewport_h {
                self.resize_viewport(desired)?;
            }
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
                    while let Some(rx) = self.events_rx.as_mut() {
                        match rx.try_recv() {
                            Ok(ev) => self.on_agent_event(ev),
                            Err(_) => break,
                        }
                    }
                }
                Some(ask) = self.ask_rx.recv() => {
                    if self.user_wait_started.is_none() {
                        self.user_wait_started = Some(Instant::now());
                    }
                    let dialog = if ask.tool == "ask_user" {
                        let options = ask.input["options"]
                            .as_array()
                            .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect())
                            .unwrap_or_default();
                        Dialog::question(ask.summary, options)
                    } else {
                        // Change proposals (edit/write) go into native
                        // scrollback before consent: the mouse wheel and
                        // terminal search beat any in-dialog pager. The
                        // decision record marks a decline; scrollback
                        // cannot be un-baked.
                        let call_summary = self.display_summary(
                            &tcode_core::agent::summarize_call(&ask.tool, &ask.input),
                        );
                        let change = diff::render_change(&ask.tool, &ask.input);
                        let prebaked = !change.is_empty();
                        if prebaked {
                            // Flush streamed text first so the transcript
                            // keeps its chronological order.
                            self.bake_live_text();
                            self.finish_thinking();
                            let mut spans = colored_tool_summary(&call_summary);
                            spans.insert(0, Span::styled("● ", theme::accent()));
                            let mut lines = vec![Line::default(), Line::from(spans)];
                            lines.extend(change);
                            self.bake(lines);
                            self.previewed_changes.insert(change_key(&ask.tool, &ask.input));
                        }
                        Dialog::new(ask.summary, ask.descriptor, call_summary, prebaked)
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
        self.clear_viewport_on_exit()?;
        Ok(())
    }

    pub fn provider_setup_requested(&self) -> bool {
        self.provider_setup_requested
    }

    fn take_previewed_change(&mut self, name: &str, input: &serde_json::Value) -> bool {
        self.previewed_changes.remove(&change_key(name, input))
    }

    /// Recover the active session when the app intentionally exits to launch
    /// the provider wizard.
    pub fn take_session(&mut self) -> Option<Session> {
        self.session.take()
    }

    fn clear_viewport_on_exit(&mut self) -> anyhow::Result<()> {
        // An inline viewport is not an alternate screen. Clear its transient
        // input/status area before returning control to the shell, while
        // preserving the baked conversation above it.
        self.terminal.clear()?;
        let height = self.terminal.size()?.height;
        execute!(std::io::stdout(), MoveTo(0, height.saturating_sub(1)))?;
        Ok(())
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
        for entry in session.ledger.entries() {
            match entry {
                tcode_core::Entry::User(blocks) => {
                    for b in blocks {
                        match b {
                            ContentBlock::Text { text } if !text.starts_with("<tcode-status>") => {
                                for (i, l) in text.lines().enumerate() {
                                    let prefix = if i == 0 { "› " } else { "  " };
                                    lines.push(Line::from(vec![
                                        Span::styled(
                                            prefix.to_string(),
                                            theme::user_prompt_message(),
                                        ),
                                        Span::styled(l.to_string(), theme::user_message()),
                                    ]));
                                }
                            }
                            ContentBlock::Image { .. } => {
                                lines.push(Line::styled("  ⌞ [image]", theme::dim()));
                            }
                            _ => {}
                        }
                    }
                    // Keep a breathing row between a highlighted human
                    // message and the following assistant/tool activity.
                    lines.push(Line::default());
                }
                tcode_core::Entry::Assistant(blocks) => {
                    for b in blocks {
                        match b {
                            ContentBlock::Text { text } => {
                                lines.extend(self.md.render(text));
                                lines.push(Line::default());
                            }
                            ContentBlock::ToolUse { name, input, .. } => {
                                if name == "update_plan" {
                                    resumed_plan = Some(input.clone());
                                    continue;
                                }
                                let summary = self.display_summary(
                                    &tcode_core::agent::summarize_call(name, input),
                                );
                                let mut spans: Vec<Span> = colored_tool_summary(&summary);
                                spans.insert(0, Span::styled("● ", theme::accent()));
                                lines.push(Line::from(spans));
                            }
                            _ => {}
                        }
                    }
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
                // Tool results and harness notes add noise on replay.
                tcode_core::Entry::ToolResults(_) | tcode_core::Entry::Note(_) => {}
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
        // Until the provider reports authoritative prompt usage, keep the
        // meter useful with a conservative local estimate. Text attachments
        // count here too; image token accounting is provider-specific.
        let attachment_tokens: u64 = self
            .attachments
            .iter()
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
        // Echo the user input into the transcript.
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
            match att {
                Attachment::Image { png, label } => {
                    echo.push(Line::styled(format!("  ⌞ {label}"), theme::dim()));
                    use base64::Engine as _;
                    blocks.push(ContentBlock::Image {
                        media_type: "image/png".into(),
                        data: base64::engine::general_purpose::STANDARD.encode(png),
                    });
                }
                Attachment::Text { content, label } => {
                    echo.push(Line::styled(format!("  ⌞ {label}"), theme::dim()));
                    blocks.push(ContentBlock::Text {
                        text: format!("[pasted content]\n{content}"),
                    });
                }
            }
        }
        self.bake(echo);
        blocks.push(ContentBlock::Text { text: input });

        let (tx, rx) = mpsc::channel(64);
        self.events_rx = Some(rx);
        self.turn_usage = Usage::default();
        self.delegated_usage = Usage::default();
        self.user_wait_started = None;
        self.user_wait_total = Duration::ZERO;
        self.out_tokens = 0;
        self.live_text.clear();
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

    /// `/compact` runs like a turn (spinner, cancel, usage report) but
    /// drives `Agent::compact` instead of the tool loop.
    fn start_compact(&mut self) {
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
            let result = agent.compact(&mut session, &cancel2).await;
            (session, result)
        });
        self.phase = Phase::Running {
            handle,
            cancel,
            started: Instant::now(),
        };
    }

    fn on_turn_done(&mut self, done: (Session, Result<(), AgentError>)) {
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
            session.last_prompt_tokens = estimate_context_tokens(&self.agent, &session.ledger);
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
                ratatui::style::Style::default().fg(theme::ERROR),
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

    // ---------------------------------------------------------- events

    fn on_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::Started => {
                self.context_step_start = self.context_tokens;
                self.state_label = "responding".into();
            }
            AgentEvent::TextDelta(t) => {
                self.finish_thinking();
                let tokens = approx_tokens(&t);
                self.out_tokens += tokens;
                self.context_tokens = self.context_tokens.saturating_add(tokens as u64);
                self.live_text.push_str(&t);
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
            AgentEvent::ToolBatchStart { label, calls } => {
                self.bake_live_text();
                self.finish_thinking();
                self.bake(vec![Line::default()]);
                let mut lines = vec![Line::from(vec![
                    Span::styled("● ", theme::accent()),
                    Span::styled(label.clone(), theme::bold()),
                ])];
                self.pending_batch.clear();
                for (name, input) in calls {
                    let summary =
                        self.display_summary(&tcode_core::agent::summarize_call(&name, &input));
                    self.pending_batch.push_back(ActivityEntry {
                        title: summary.clone(),
                        detail: serde_json::to_string_pretty(&input).unwrap_or_default(),
                        expanded: false,
                    });
                    let mut spans: Vec<Span> = colored_tool_summary(&summary);
                    spans.insert(0, Span::raw("  ├ "));
                    lines.push(Line::from(spans));
                    // Diffs already baked before consent are not repeated;
                    // auto-allowed changes render theirs here.
                    if !self.take_previewed_change(&name, &input) {
                        lines.extend(diff::render_change(&name, &input));
                    }
                    lines.push(Line::default());
                }
                self.bake(lines);
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
                let summary = self.display_summary(&summary);
                // A change baked before consent is this call's display;
                // repeating the header or diff here would duplicate it.
                if self.take_previewed_change(&name, &input) {
                    self.pending_tool = Some(ActivityEntry {
                        title: summary.clone(),
                        detail: serde_json::to_string_pretty(&input).unwrap_or_default(),
                        expanded: false,
                    });
                    self.state_label = format!("running: {summary}");
                    return;
                }
                self.bake_live_text();
                self.finish_thinking();
                self.bake(vec![Line::default()]);
                let approved_change = diff::render_change(&name, &input);
                self.pending_tool = Some(ActivityEntry {
                    title: summary.clone(),
                    detail: serde_json::to_string_pretty(&input).unwrap_or_default(),
                    expanded: false,
                });
                let mut spans: Vec<Span> = colored_tool_summary(&summary);
                spans.insert(0, Span::styled("● ", theme::accent()));
                let mut lines = vec![Line::from(spans)];
                lines.extend(approved_change);
                self.bake(lines);
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
                if let Some(mut entry) = entry {
                    entry.detail.push_str("\n\nresult:\n");
                    entry.detail.push_str(&content);
                    self.activity.push(entry);
                }
                // The gated result is exactly what is appended to the next
                // model request, so it belongs in the in-between estimate.
                self.context_tokens = self
                    .context_tokens
                    .saturating_add(approx_tokens(&content) as u64);
                let style = if is_error {
                    ratatui::style::Style::default().fg(theme::ERROR)
                } else {
                    theme::dim()
                };
                self.bake(vec![Line::styled(format!("  ⎿ {preview}"), style)]);
                self.state_label = "responding".into();
            }
            AgentEvent::Retrying {
                attempt,
                max,
                error,
            } => {
                // Un-baked partial output is simply dropped — that is the
                // whole point of keeping it out of the scrollback.
                self.live_text.clear();
                self.thinking_chars = 0;
                self.thinking_text.clear();
                self.thinking_since = None;
                self.context_tokens = self.context_step_start;
                self.bake(vec![Line::styled(
                    format!("↻ watchdog: {error} — retrying ({attempt}/{max})"),
                    ratatui::style::Style::default().fg(theme::WARN),
                )]);
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

    /// Transcript record of a consent decision. Each tool call appears in
    /// scrollback exactly once, rendered by its ToolStart — so an approval
    /// bakes nothing but the user's note, and only a declined call (which
    /// never emits ToolStart) bakes its own header.
    fn bake_approval_record(&mut self, dialog: &Dialog, approval: &Approval) {
        if dialog.is_question() {
            let answer = approval.comment.clone().unwrap_or_default();
            self.bake(vec![
                Line::default(),
                Line::from(vec![
                    Span::styled("? ", theme::accent()),
                    Span::styled(dialog.summary.clone(), theme::bold()),
                ]),
                Line::styled(format!("  ⎿ {answer}"), theme::dim()),
            ]);
            return;
        }
        match approval.decision {
            ApprovalDecision::Yes | ApprovalDecision::YesAlways => {
                if let Some(note) = approval.comment.as_deref() {
                    let line = Line::styled(format!("  ⊙ note to model — {note}"), theme::dim());
                    // A prebaked diff block is directly above: attach to it.
                    if dialog.prebaked {
                        self.bake(vec![line]);
                    } else {
                        self.bake(vec![Line::default(), line]);
                    }
                }
            }
            ApprovalDecision::No => {
                let reason = approval
                    .comment
                    .as_deref()
                    .map(|c| format!(" — {c}"))
                    .unwrap_or_default();
                let declined = Line::styled(
                    format!("  ⎿ declined{reason}"),
                    ratatui::style::Style::default().fg(theme::ERROR),
                );
                if dialog.prebaked {
                    // The header + diff are already in scrollback and cannot
                    // be un-baked; mark them as not applied.
                    self.previewed_changes.clear();
                    self.bake(vec![declined]);
                } else {
                    let mut spans = colored_tool_summary(&dialog.call_summary);
                    spans.insert(0, Span::styled("● ", theme::accent()));
                    self.bake(vec![Line::default(), Line::from(spans), declined]);
                }
            }
        }
    }

    fn finish_thinking(&mut self) {
        if let Some(since) = self.thinking_since.take() {
            let secs = since.elapsed().as_secs().max(1);
            let title = format!("thought for {secs}s (~{} tok)", self.thinking_chars / 3);
            self.activity.push(ActivityEntry {
                title: title.clone(),
                detail: std::mem::take(&mut self.thinking_text),
                expanded: false,
            });
            self.bake(vec![Line::styled(
                format!("✻ {title} · ctrl+o details"),
                theme::thinking(),
            )]);
            self.thinking_chars = 0;
        }
    }

    fn bake_live_text(&mut self) {
        if self.live_text.trim().is_empty() {
            self.live_text.clear();
            return;
        }
        let text = std::mem::take(&mut self.live_text);
        let mut lines = self.md.render(&text);
        lines.push(Line::default());
        self.bake(lines);
    }

    // ------------------------------------------------------------ keys

    fn on_term_event(&mut self, ev: Event) {
        match ev {
            Event::Key(key) if key.kind != crossterm::event::KeyEventKind::Release => {
                self.on_key(key)
            }
            Event::Paste(text) => self.on_paste_text(text),
            Event::Resize(..) => {}
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if self.activity_open {
            match key.code {
                KeyCode::Esc => self.activity_open = false,
                KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.activity_open = false
                }
                KeyCode::Up => self.activity_selected = self.activity_selected.saturating_sub(1),
                KeyCode::Down => {
                    self.activity_selected =
                        (self.activity_selected + 1).min(self.activity.len().saturating_sub(1));
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    self.activity_detail_scroll = 0;
                    if let Some(entry) = self.activity.get_mut(self.activity_selected) {
                        entry.expanded = !entry.expanded;
                    }
                }
                KeyCode::PageUp => {
                    self.activity_detail_scroll = self.activity_detail_scroll.saturating_sub(12)
                }
                KeyCode::PageDown => self.activity_detail_scroll += 12,
                _ => {}
            }
            return;
        }

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

        // Rewind picker captures everything while open.
        if let Some(picker) = self.rewind.as_mut() {
            match picker.handle_key(key) {
                PickResult::Pending => {}
                PickResult::Cancelled => self.rewind = None,
                PickResult::Rewind {
                    index,
                    restore_files,
                    text,
                } => {
                    self.rewind = None;
                    self.do_rewind(index, restore_files, text);
                }
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
        match key.code {
            KeyCode::Char('o') if ctrl => {
                if self.activity.is_empty() {
                    self.bake(vec![Line::styled("no activity details yet", theme::dim())]);
                } else {
                    self.activity_open = true;
                    self.activity_selected = self.activity.len() - 1;
                }
            }
            KeyCode::Char('c') if ctrl => {
                if running {
                    self.cancel_turn();
                } else if !self.editor.is_empty() || !self.attachments.is_empty() {
                    self.editor.clear();
                    self.attachments.clear();
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
                    self.editor.clear();
                    self.attachments.clear();
                    self.last_esc = Some(Instant::now());
                }
            }
            KeyCode::Char('v') if ctrl || key.modifiers.contains(KeyModifiers::ALT) => {
                self.paste_from_clipboard();
            }
            KeyCode::Char('j') if ctrl => self.editor.newline(),
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => self.editor.newline(),
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
                } else if !self.editor.up() {
                    self.editor.history_prev();
                }
            }
            KeyCode::Down => {
                if self.popup_active() {
                    self.popup_index = (self.popup_index + 1).min(self.popup_matches().len() - 1);
                } else if !self.editor.down() {
                    self.editor.history_next();
                }
            }
            KeyCode::Left => self.editor.left(),
            KeyCode::Right => self.editor.right(),
            KeyCode::Home => self.editor.home(),
            KeyCode::End => self.editor.end(),
            KeyCode::Backspace => self.editor.backspace(),
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
        self.start_turn(input);
    }

    // ---------------------------------------------------------- rewind

    fn open_rewind(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let candidates: Vec<rewind::Candidate> = session
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
                    (!text.is_empty()).then(|| rewind::Candidate {
                        index: i,
                        text,
                        dirty: session.checkpoints.dirty_since(i),
                    })
                }
                _ => None,
            })
            .collect();
        self.rewind = rewind::Picker::new(candidates);
        if self.rewind.is_none() {
            self.bake(vec![Line::styled("nothing to rewind to", theme::dim())]);
        }
    }

    fn do_rewind(&mut self, index: usize, restore_files: bool, text: String) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        session.ledger.truncate_tail(index);
        session.last_prompt_tokens = estimate_context_tokens(&self.agent, &session.ledger);
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

    /// `/export [path]`: write the conversation as a markdown transcript.
    fn export_transcript(&mut self, path_arg: &str) {
        let Some(session) = self.session.as_ref() else {
            self.bake(vec![Line::styled(
                "wait for the current turn before exporting",
                theme::dim(),
            )]);
            return;
        };
        if session.ledger.is_empty() {
            self.bake(vec![Line::styled("nothing to export yet", theme::dim())]);
            return;
        }
        let path = if path_arg.is_empty() {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            std::path::PathBuf::from(format!("tcode-transcript-{secs}.md"))
        } else {
            std::path::PathBuf::from(path_arg)
        };
        let markdown = tcode_core::export_markdown(session.ledger.entries(), "tcode conversation");
        match std::fs::write(&path, markdown) {
            Ok(()) => self.bake(vec![Line::styled(
                format!("transcript exported → {}", path.display()),
                theme::dim(),
            )]),
            Err(e) => self.bake(vec![Line::styled(
                format!("export failed: {e}"),
                ratatui::style::Style::default().fg(theme::ERROR),
            )]),
        }
    }

    fn memory_status(&mut self, arg: &str) {
        let Some(session) = self.session.as_mut() else {
            self.bake(vec![Line::styled(
                "wait for the current turn before inspecting memory",
                theme::dim(),
            )]);
            return;
        };
        let (status, toggle_note) = {
            let mut memory = session.tool_ctx.memory.lock().expect("memory lock");
            memory.restore_from_entries(session.ledger.entries());
            let note = match arg {
                "" => None,
                "on" => Some(memory.set_enabled(true)),
                "off" => Some(memory.set_enabled(false)),
                _ => {
                    drop(memory);
                    self.bake(vec![Line::styled("usage: /memory [on|off]", theme::dim())]);
                    return;
                }
            };
            (memory.status(), note)
        };
        if let Some(note) = toggle_note {
            session.ledger.append(tcode_core::Entry::Note(note));
        }
        let lines = status
            .lines()
            .map(|line| Line::styled(format!("  {line}"), theme::dim()))
            .collect();
        self.bake(lines);
    }

    fn run_slash(&mut self, cmd: &str) {
        if cmd == "/memory" || cmd.starts_with("/memory ") {
            self.memory_status(cmd.strip_prefix("/memory").unwrap_or("").trim());
            return;
        }
        if cmd == "/resume" {
            self.open_resume_picker();
            return;
        }
        if let Some(id) = cmd.strip_prefix("/resume ") {
            self.resume_session(id.trim());
            return;
        }
        if let Some(note) = cmd.strip_prefix("/note ") {
            let note = note.trim();
            if note.is_empty() {
                self.bake(vec![Line::styled("usage: /note <text>", theme::dim())]);
            } else if let Some(session) = self.session.as_mut() {
                session
                    .ledger
                    .append(tcode_core::Entry::Note(note.to_string()));
                self.bake(vec![Line::styled(
                    format!("  ⌞ note: {note}"),
                    theme::dim(),
                )]);
            } else {
                self.bake(vec![Line::styled(
                    "wait for the current turn before adding a note",
                    theme::dim(),
                )]);
            }
            return;
        }
        if cmd == "/export" || cmd.starts_with("/export ") {
            self.export_transcript(cmd.strip_prefix("/export").unwrap_or("").trim());
            return;
        }
        match cmd {
            "/exit" | "/quit" => self.should_exit = true,
            "/provider" => {
                self.provider_setup_requested = true;
                self.should_exit = true;
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
            }
            "/mode" => {
                if let Some(session) = self.session.as_mut() {
                    session.mode = session.mode.cycle();
                    self.mode_label = session.mode.label().to_string();
                    let label = self.mode_label.clone();
                    self.bake(vec![Line::styled(
                        format!("permission mode → {label}"),
                        theme::dim(),
                    )]);
                }
            }
            "/cost" => {
                let u = self.turn_usage;
                self.bake(vec![Line::styled(
                    format!(
                        "last turn: in {} | out {} | cache r {} w {}",
                        u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cache_write_tokens
                    ),
                    theme::dim(),
                )]);
            }
            "/compact" => self.start_compact(),
            "/clear" => {
                if let Some(session) = self.session.as_mut() {
                    session.ledger.truncate_tail(0);
                    session.last_prompt_tokens = 0;
                    self.context_tokens = 0;
                    self.context_step_start = 0;
                    self.context_estimated = false;
                    session
                        .tool_ctx
                        .freshness
                        .lock()
                        .expect("freshness lock")
                        .clear();
                    self.prev_cache_ratio = None;
                    self.activity.clear();
                    self.plan.clear();
                    self.pending_tool = None;
                    self.pending_batch.clear();
                    self.thinking_text.clear();
                    if let Err(e) = self.clear_conversation_screen() {
                        self.bake(vec![Line::styled(
                            format!("could not clear terminal scrollback: {e}"),
                            ratatui::style::Style::default().fg(theme::ERROR),
                        )]);
                    }
                    self.bake(vec![Line::styled("conversation cleared", theme::dim())]);
                }
            }
            "/help" => {
                let mut lines: Vec<Line> =
                    vec![Line::styled("keys:", theme::bold().fg(theme::ACCENT))];
                for (k, d) in [
                    ("enter", "send · alt+enter/ctrl+j newline"),
                    ("esc", "cancel current turn / clear input"),
                    ("shift+tab", "cycle permission mode"),
                    ("ctrl+v / alt+v", "paste (images become attachments)"),
                    ("ctrl+c", "cancel / clear / exit"),
                    ("ctrl+o", "activity details · enter expand"),
                ] {
                    lines.push(Line::styled(format!("  {k:<16} {d}"), theme::dim()));
                }
                lines.push(Line::styled("commands:", theme::bold().fg(theme::ACCENT)));
                for (c, d) in SLASH_COMMANDS {
                    lines.push(Line::styled(format!("  {c:<16} {d}"), theme::dim()));
                }
                self.bake(lines);
            }
            other => {
                self.bake(vec![Line::styled(
                    format!("unknown command {other} — /help lists commands"),
                    theme::dim(),
                )]);
            }
        }
    }

    // ----------------------------------------------------------- paste

    fn paste_from_clipboard(&mut self) {
        let Ok(mut clipboard) = arboard::Clipboard::new() else {
            return;
        };
        if let Ok(img) = clipboard.get_image() {
            if let Some(rgba) = image::RgbaImage::from_raw(
                img.width as u32,
                img.height as u32,
                img.bytes.into_owned(),
            ) {
                let mut png: Vec<u8> = Vec::new();
                let dynimg = image::DynamicImage::ImageRgba8(rgba);
                if dynimg
                    .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                    .is_ok()
                {
                    let label = format!(
                        "image #{} ({}x{}, {}KB)",
                        self.attachments.len() + 1,
                        img.width,
                        img.height,
                        png.len() / 1024
                    );
                    self.attachments.push(Attachment::Image { png, label });
                    return;
                }
            }
        }
        if let Ok(text) = clipboard.get_text() {
            self.on_paste_text(text);
        }
    }

    fn on_paste_text(&mut self, text: String) {
        let lines = text.lines().count();
        if lines > PASTE_FOLD_LINES {
            let label = format!("pasted #{} ({lines} lines)", self.attachments.len() + 1);
            self.attachments.push(Attachment::Text {
                content: text,
                label,
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

    fn popup_matches(&self) -> Vec<(&'static str, &'static str)> {
        let prefix = self.editor.text();
        SLASH_COMMANDS
            .iter()
            .filter(|(c, _)| c.starts_with(&prefix))
            .copied()
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

    fn bake(&mut self, lines: Vec<Line<'static>>) {
        if lines.is_empty() {
            return;
        }
        let width = self.terminal.size().map(|s| s.width).unwrap_or(80).max(20);
        let lines = wrap_baked_lines(lines, width as usize);
        let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        let height = para.line_count(width) as u16;
        let _ = self.terminal.insert_before(height, |buf| {
            para.render(buf.area, buf);
            blank_wide_continuations(buf);
        });
    }

    /// `/clear` starts a genuinely clean visual conversation. Clearing the
    /// ledger alone is insufficient with an inline viewport: prior entries
    /// have already been committed to the terminal's native scrollback.
    fn clear_conversation_screen(&mut self) -> anyhow::Result<()> {
        let _ = self.terminal.clear();
        execute!(std::io::stdout(), Clear(ClearType::Purge), MoveTo(0, 0))?;
        self.terminal = make_terminal(self.viewport_h, Some(0))?;
        let banner = self.banner();
        self.bake(banner);
        Ok(())
    }

    fn resume_session(&mut self, id: &str) {
        if matches!(self.phase, Phase::Running { .. }) {
            self.bake(vec![Line::styled(
                "wait for the current turn before resuming",
                theme::dim(),
            )]);
            return;
        }
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let Some(data_dir) = tcode_core::store::project_data_dir(&session.tool_ctx.cwd) else {
            self.bake(vec![Line::styled(
                "cannot locate tcode session storage",
                theme::dim(),
            )]);
            return;
        };
        match tcode_core::SessionStore::resume(&data_dir, Some(id)) {
            Ok(resumed) => {
                let ckpt_dir = data_dir.join("checkpoints").join(&resumed.store.id);
                session.checkpoints =
                    tcode_core::CheckpointStore::load(ckpt_dir, resumed.checkpoints);
                session.ledger = resumed.ledger;
                session.ledger.attach_sink(Box::new(resumed.store));
                session.last_prompt_tokens = estimate_context_tokens(&self.agent, &session.ledger);
                self.context_tokens = session.last_prompt_tokens;
                self.context_step_start = self.context_tokens;
                self.context_estimated = !session.ledger.is_empty();
                self.activity.clear();
                self.plan.clear();
                self.pending_tool = None;
                self.pending_batch.clear();
                self.thinking_text.clear();
                session
                    .tool_ctx
                    .freshness
                    .lock()
                    .expect("freshness lock")
                    .clear();
                if let Err(e) = self.clear_conversation_screen() {
                    self.bake(vec![Line::styled(
                        format!("could not clear terminal scrollback: {e}"),
                        ratatui::style::Style::default().fg(theme::ERROR),
                    )]);
                }
                self.bake_transcript();
            }
            Err(e) => self.bake(vec![Line::styled(
                format!("cannot resume session {id}: {e}"),
                ratatui::style::Style::default().fg(theme::ERROR),
            )]),
        }
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
                session.last_prompt_tokens = estimate_context_tokens(&self.agent, &session.ledger);
                self.context_tokens = session.last_prompt_tokens;
                self.context_step_start = self.context_tokens;
                self.context_estimated = !session.ledger.is_empty();
                self.activity.clear();
                self.plan.clear();
                self.pending_tool = None;
                self.pending_batch.clear();
                self.thinking_text.clear();
                session
                    .tool_ctx
                    .freshness
                    .lock()
                    .expect("freshness lock")
                    .clear();
                if let Err(e) = self.clear_conversation_screen() {
                    self.bake(vec![Line::styled(
                        format!("could not clear terminal scrollback: {e}"),
                        ratatui::style::Style::default().fg(theme::ERROR),
                    )]);
                }
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

    fn activity_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::styled(
            "activity details",
            theme::bold().fg(theme::ACCENT),
        )];
        let start = self.activity_selected.saturating_sub(4);
        for (index, entry) in self.activity.iter().enumerate().skip(start).take(5) {
            let selected = index == self.activity_selected;
            let marker = if selected { "▸ " } else { "  " };
            let icon = if entry.title.starts_with("thought") {
                "✻"
            } else {
                "●"
            };
            let style = if selected {
                theme::accent()
            } else {
                theme::dim()
            };
            lines.push(Line::styled(
                format!("  {marker}{icon} {}", entry.title),
                style,
            ));
            if selected && entry.expanded {
                let details: Vec<_> = entry.detail.lines().collect();
                for detail in details.iter().skip(self.activity_detail_scroll).take(14) {
                    lines.push(Line::styled(format!("    {detail}"), theme::dim()));
                }
                if details.len() > self.activity_detail_scroll + 14 {
                    lines.push(Line::styled("    … pgdn for more", theme::dim()));
                }
            }
        }
        lines.push(Line::styled(
            "  ↑↓ select · enter/space expand · pgup/pgdn scroll · ctrl+o/esc close",
            theme::dim(),
        ));
        lines
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
            .take(4)
            .collect();
    }

    fn plan_lines(&self) -> Vec<Line<'static>> {
        if self.plan.is_empty() {
            return Vec::new();
        }
        let complete = self
            .plan
            .iter()
            .filter(|item| item.status == "completed")
            .count();
        let mut lines = vec![Line::from(vec![
            Span::styled("  plan ", theme::bold().fg(theme::ACCENT)),
            Span::styled(
                format!("{complete}/{} complete", self.plan.len()),
                theme::dim(),
            ),
        ])];
        lines.extend(self.plan.iter().map(|item| {
            let (marker, style) = match item.status.as_str() {
                "completed" => ("✓ ", ratatui::style::Style::default().fg(theme::OK)),
                "in_progress" => ("● ", theme::accent()),
                _ => ("○ ", theme::dim()),
            };
            Line::from(vec![
                Span::styled(format!("    {marker}"), style),
                Span::styled(
                    item.step.clone(),
                    if item.status == "pending" {
                        theme::dim()
                    } else {
                        ratatui::style::Style::default()
                    },
                ),
            ])
        }));
        lines
    }

    /// Tool inputs are canonical absolute paths, but repeating the current
    /// project root adds noise without adding information in the TUI.
    fn display_summary(&self, summary: &str) -> String {
        shorten_summary_path(summary, Some(&self.cwd))
    }

    fn desired_viewport(&self) -> u16 {
        // Pickers/dialogs replace the whole viewport (bordered box).
        let h = if self.activity_open {
            self.activity_lines().len() as u16 + 2
        } else if let Some(picker) = &self.resume_picker {
            picker.height() + 2
        } else if let Some(picker) = &self.model_picker {
            picker.height() + 2
        } else if let Some(picker) = &self.rewind {
            picker.height() + 2
        } else if let Some((dialog, _)) = &self.dialog {
            dialog.height(area_width(&self.terminal)) + 2
        } else {
            let running = matches!(self.phase, Phase::Running { .. });
            let live = if running {
                (self.live_text.lines().count().min(8) + 1) as u16
            } else {
                0
            };
            let editor_rows = editor_layout(&self.editor, area_width(&self.terminal))
                .lines
                .len() as u16;
            let editor_box = editor_rows.clamp(1, 6) + 2;
            let popup_h = if self.popup_active() {
                self.popup_matches().len() as u16
            } else {
                0
            };
            let attach_h = if self.attachments.is_empty() { 0 } else { 1 };
            // context meter + hint both get their own row below the editor.
            live + self.plan_lines().len() as u16
                + if self.plan.is_empty() { 0 } else { 2 }
                + editor_box
                + popup_h
                + attach_h
                + 2
                + usize::from(self.rate_limits.is_some()) as u16
        };
        h.clamp(4, 18)
    }

    fn resize_viewport(&mut self, desired: u16) -> anyhow::Result<()> {
        // We know where the viewport sits; clearing parks the physical
        // cursor at its top row, and the new terminal is told that row
        // directly — no stdin round-trip, no race (see KnownPosBackend).
        let top = self.terminal.get_frame().area().y;
        let _ = self.terminal.clear();
        self.terminal = make_terminal(desired, Some(top))?;
        self.viewport_h = desired;
        Ok(())
    }

    fn redraw(&mut self) -> anyhow::Result<()> {
        let running = matches!(self.phase, Phase::Running { .. });
        let started = match &self.phase {
            Phase::Running { started, .. } => Some(*started),
            Phase::Idle => None,
        };
        let status = self.status_line(running, started);
        let hint = self.idle_hint();
        let live_tail: Vec<String> = if running {
            let n = self.live_text.lines().count();
            self.live_text
                .lines()
                .skip(n.saturating_sub(8))
                .map(String::from)
                .collect()
        } else {
            Vec::new()
        };
        let dialog_lines = self
            .activity_open
            .then(|| self.activity_lines())
            .or_else(|| self.resume_picker.as_ref().map(|p| p.render()))
            .or_else(|| self.model_picker.as_ref().map(|p| p.render(&self.menu)))
            .or_else(|| self.rewind.as_ref().map(|p| p.render()))
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
        let attach_labels: Vec<String> = self
            .attachments
            .iter()
            .map(|a| match a {
                Attachment::Image { label, .. } | Attachment::Text { label, .. } => label.clone(),
            })
            .collect();
        let plan_lines = self.plan_lines();

        use ratatui::widgets::{Block, BorderType};

        self.terminal.draw(|frame| {
            let area = frame.area();
            let mut y = area.y;
            let row = |y: u16, h: u16| Rect {
                x: area.x,
                y,
                width: area.width,
                height: h.min(area.bottom().saturating_sub(y)),
            };

            // Pickers and approval dialogs own the viewport: a rounded
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

            if !live_tail.is_empty() {
                let lines: Vec<Line> = live_tail
                    .iter()
                    .map(|l| Line::styled(l.clone(), theme::dim()))
                    .collect();
                let h = lines.len() as u16;
                frame.render_widget(Paragraph::new(Text::from(lines)), row(y, h));
                y += h;
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
            let editor_start = editor.cursor_row.saturating_sub(5);
            let visible_editor = editor.lines[editor_start..]
                .iter()
                .take(6)
                .collect::<Vec<_>>();
            let editor_h = (visible_editor.len() as u16).clamp(1, 6);
            let inner: Vec<Line> = visible_editor
                .iter()
                .map(|(first_logical_line, l)| {
                    Line::from(vec![
                        Span::styled(
                            if *first_logical_line { "› " } else { "  " },
                            theme::user_prompt(),
                        ),
                        Span::raw(l.clone()),
                    ])
                })
                .collect();
            let box_y = y;
            frame.render_widget(
                Paragraph::new(Text::from(inner)).block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .border_style(theme::border()),
                ),
                row(y, editor_h + 2),
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

            if !attach_labels.is_empty() {
                frame.render_widget(
                    Paragraph::new(Line::styled(
                        format!("  ⌞ {}", attach_labels.join(" · ")),
                        theme::accent(),
                    )),
                    row(y, 1),
                );
                y += 1;
            }

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
        Ok(())
    }

    /// Spinner line shown above the input while a turn runs. The sparkle
    /// carries the animation; the label stays readable, metadata stays dim.
    fn status_line(&self, running: bool, started: Option<Instant>) -> Line<'static> {
        if !running {
            return Line::default();
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
                format!(" · {elapsed}s · ↓ ~{} tok · esc to cancel", self.out_tokens),
                theme::dim(),
            ),
        ])
    }

    /// Dim one-liner under the input box: mode, model, cache health.
    fn idle_hint(&self) -> String {
        let u = self.turn_usage;
        let cache = if u.total_input() > 0 {
            format!(
                " · cache {}%",
                (u.cache_read_tokens as f64 / u.total_input() as f64 * 100.0).round()
            )
        } else {
            String::new()
        };
        format!(
            "  mode {} · {}{} · /help",
            self.mode_label,
            self.agent.model.snapshot().describe(),
            cache
        )
    }
}

/// Split a tool summary like `shell(cargo test)` into colored spans:
/// the tool name is green, the arguments are dim.
fn colored_tool_summary(summary: &str) -> Vec<Span<'static>> {
    let s = summary.to_string();
    if let Some(paren) = s.find('(') {
        let name = &s[..paren];
        let args = &s[paren..];
        vec![
            Span::styled(name.to_string(), theme::ok()),
            Span::styled(args.to_string(), theme::dim()),
        ]
    } else {
        vec![Span::styled(s, theme::bold())]
    }
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
    let weekly = limits.secondary.filter(|limit| limit.used_percent >= 80.0);
    let (label, limit) = weekly
        .map(|limit| ("week", limit))
        .unwrap_or(("5h", limits.primary));
    let filled = ((limit.used_percent.clamp(0.0, 100.0) / 100.0) * 12.0).round() as usize;
    let color = if limit.used_percent >= 90.0 {
        theme::ERROR
    } else if limit.used_percent >= 75.0 {
        theme::WARN
    } else {
        theme::ACCENT
    };
    Line::from(vec![
        Span::styled(format!("  OpenAI {label} "), theme::dim()),
        Span::styled("▕", ratatui::style::Style::default().fg(color)),
        Span::styled(
            "▰".repeat(filled),
            ratatui::style::Style::default().fg(color),
        ),
        Span::styled("▱".repeat(12 - filled), theme::dim()),
        Span::styled(
            format!("▏ {:.0}%", limit.used_percent),
            ratatui::style::Style::default().fg(color),
        ),
    ])
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
        Span::styled(token_count(usage.input_tokens), theme::accent()),
        Span::styled(" input", theme::dim()),
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
fn estimate_context_tokens(agent: &Agent, ledger: &tcode_core::Ledger) -> u64 {
    let system = approx_tokens(&agent.system) as u64;
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
    let conversation: u64 = ledger
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
            tcode_core::Entry::ImportedTool { .. } => 0,
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

/// Prevent the terminal from silently soft-wrapping baked output. A visible
/// continuation marker makes copied code/text unambiguous: `↪` is display
/// wrapping, whereas a new `Line` is an actual newline from the content.
fn wrap_baked_lines(lines: Vec<Line<'static>>, terminal_width: usize) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthChar;

    let width = terminal_width.saturating_sub(1).max(1);
    let mut out = Vec::new();
    for line in lines {
        let mut current: Vec<Span<'static>> = Vec::new();
        let mut current_width = 0usize;
        for span in line.spans {
            for c in span.content.chars() {
                let char_width = c.width().unwrap_or(0);
                if !current.is_empty() && current_width + char_width > width {
                    out.push(pad_background_line(
                        std::mem::take(&mut current),
                        current_width,
                        width,
                    ));
                    current_width = 0;
                }
                current.push(Span::styled(c.to_string(), span.style));
                current_width += char_width;
            }
        }
        out.push(pad_background_line(current, current_width, width));
    }
    out
}

/// Ratatui backgrounds otherwise stop at the final code character. Extend
/// diff lines to the terminal edge, including every manually wrapped chunk.
fn pad_background_line(mut spans: Vec<Span<'static>>, used: usize, width: usize) -> Line<'static> {
    if let Some(background) = spans.iter().find_map(|span| span.style.bg) {
        spans.push(Span::styled(
            " ".repeat(width.saturating_sub(used)),
            ratatui::style::Style::default().bg(background),
        ));
    }
    Line::from(spans)
}

fn area_width(terminal: &Terminal<KnownPosBackend>) -> u16 {
    terminal.size().map(|s| s.width).unwrap_or(80)
}

/// Wrap logical editor lines ourselves instead of leaving it to the terminal.
/// That keeps soft wraps out of copied text, gives continuation lines a stable
/// prefix, and makes the cursor/viewport agree with what is on screen.
fn editor_layout(editor: &Editor, terminal_width: u16) -> EditorLayout {
    use unicode_width::UnicodeWidthChar;

    // border + two-column prompt + one interior column on the right.
    let width = terminal_width.saturating_sub(4).max(1) as usize;
    let (cursor_line, cursor_col) = editor.cursor();
    let mut lines = Vec::new();
    let mut visual_cursor = (0, 0);

    for (logical_row, text) in editor.lines().iter().enumerate() {
        let mut chunks: Vec<(String, usize, usize)> = Vec::new();
        let mut chunk = String::new();
        let mut start = 0usize;
        let mut end = 0usize;
        for c in text.chars() {
            let char_width = c.width().unwrap_or(0);
            if !chunk.is_empty() && end - start + char_width > width {
                chunks.push((std::mem::take(&mut chunk), start, end));
                start = end;
            }
            chunk.push(c);
            end += char_width;
        }
        if !chunk.is_empty() || chunks.is_empty() {
            chunks.push((chunk, start, end));
        }

        if logical_row == cursor_line {
            let cursor_chunk = chunks
                .iter()
                .position(|(_, start, end)| *start <= cursor_col && cursor_col <= *end)
                .unwrap_or(chunks.len() - 1);
            let (_, start, _) = &chunks[cursor_chunk];
            visual_cursor = (
                lines.len() + cursor_chunk,
                cursor_col.saturating_sub(*start),
            );
        }
        for (i, (chunk, _, _)) in chunks.into_iter().enumerate() {
            lines.push((i == 0, chunk));
        }
    }
    EditorLayout {
        lines,
        cursor_row: visual_cursor.0,
        cursor_col: visual_cursor.1,
    }
}

/// ratatui's `insert_before` writes every buffer cell to the backend,
/// including the placeholder cells after a wide (CJK) character. The
/// backend then prints those placeholder spaces at an already-advanced
/// cursor, shifting the line ("你 是 什 么"). Blanking the placeholder
/// symbols keeps the terminal cursor in sync. (The normal `draw` path
/// diffs the buffer and skips these cells, so it is unaffected.)
fn blank_wide_continuations(buf: &mut ratatui::buffer::Buffer) {
    use unicode_width::UnicodeWidthStr;
    let area = buf.area;
    for y in area.top()..area.bottom() {
        let mut skip = 0usize;
        for x in area.left()..area.right() {
            let Some(cell) = buf.cell_mut((x, y)) else {
                continue;
            };
            if skip > 0 {
                cell.set_symbol("");
                skip -= 1;
            } else {
                skip = cell.symbol().width().saturating_sub(1);
            }
        }
    }
}

/// CrosstermBackend that can answer one cursor-position request from
/// memory instead of querying the terminal over stdin.
///
/// Creating an inline-viewport terminal asks where the cursor is; that
/// round-trip races with the crossterm event reader and with typed-ahead
/// input, and a stale or lost answer places the new viewport on top of
/// already-baked scrollback (text "disappears") or errors out. After
/// startup we always know exactly where the viewport is, so resizes
/// answer from memory and never touch the wire.
struct KnownPosBackend {
    inner: CrosstermBackend<Stdout>,
    forced: Option<ratatui::layout::Position>,
}

impl ratatui::backend::Backend for KnownPosBackend {
    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.inner.draw(content)
    }
    fn append_lines(&mut self, n: u16) -> std::io::Result<()> {
        self.inner.append_lines(n)
    }
    fn hide_cursor(&mut self) -> std::io::Result<()> {
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> std::io::Result<()> {
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
        match self.forced.take() {
            Some(pos) => Ok(pos),
            None => self.inner.get_cursor_position(),
        }
    }
    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        position: P,
    ) -> std::io::Result<()> {
        self.inner.set_cursor_position(position)
    }
    fn clear(&mut self) -> std::io::Result<()> {
        self.inner.clear()
    }
    fn clear_region(&mut self, clear_type: ratatui::backend::ClearType) -> std::io::Result<()> {
        self.inner.clear_region(clear_type)
    }
    fn size(&self) -> std::io::Result<ratatui::layout::Size> {
        self.inner.size()
    }
    fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
        self.inner.window_size()
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// `known_top` = the row the viewport should start at (we track it);
/// None (first creation only) queries the real cursor position.
fn make_terminal(height: u16, known_top: Option<u16>) -> anyhow::Result<Terminal<KnownPosBackend>> {
    let backend = KnownPosBackend {
        inner: CrosstermBackend::new(std::io::stdout()),
        forced: known_top.map(|y| ratatui::layout::Position { x: 0, y }),
    };
    Ok(Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )?)
}

fn change_key(tool: &str, input: &serde_json::Value) -> String {
    format!(
        "{tool}:{}",
        serde_json::to_string(input).unwrap_or_default()
    )
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
    fn editor_layout_wraps_without_losing_cursor_position() {
        let mut editor = Editor::new();
        editor.insert_str("abcdefghi");
        // Width 10 leaves six cells inside the input border and prompt.
        let layout = editor_layout(&editor, 10);
        assert_eq!(
            layout
                .lines
                .iter()
                .map(|(_, line)| line.as_str())
                .collect::<Vec<_>>(),
            ["abcdef", "ghi"]
        );
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 3));
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
                .map(|(first, line)| (*first, line.as_str()))
                .collect::<Vec<_>>(),
            [(true, "abc"), (true, "def")]
        );
    }

    #[test]
    fn baked_lines_mark_visual_wraps() {
        let lines = wrap_baked_lines(vec![Line::raw("abcdefghi")], 7);
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(rendered, ["abcdef", "ghi"]);
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
            "  ╰─ completed 2.5s  ·  ↑ 1.2k input  ·  ↓ 23 output  ·  cache 0%"
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
