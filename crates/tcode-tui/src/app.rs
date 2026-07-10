use std::io::Stdout;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
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
use crate::rewind::{self, PickResult};
use crate::{diff, markdown, theme};

/// Second Esc within this window (while idle) opens the rewind picker.
const DOUBLE_ESC: Duration = Duration::from_millis(1200);

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const PASTE_FOLD_LINES: usize = 15;

const SLASH_COMMANDS: [(&str, &str); 6] = [
    ("/help", "show keys and commands"),
    ("/mode", "cycle permission mode"),
    ("/cost", "show last turn token usage"),
    ("/compact", "summarize history to free context"),
    ("/clear", "start a fresh conversation"),
    ("/exit", "quit tcode"),
];

pub struct AskMsg {
    pub summary: String,
    pub descriptor: String,
    pub reply: oneshot::Sender<Approval>,
}

/// Approver that forwards prompts into the UI loop.
pub struct ChannelApprover {
    pub tx: mpsc::Sender<AskMsg>,
}

#[async_trait]
impl Approver for ChannelApprover {
    async fn ask(&self, _tool: &str, summary: &str, descriptor: &str) -> Approval {
        let (reply, rx) = oneshot::channel();
        let msg = AskMsg {
            summary: summary.to_string(),
            descriptor: descriptor.to_string(),
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

pub struct App {
    agent: Arc<Agent>,
    session: Option<Session>,
    terminal: Terminal<CrosstermBackend<Stdout>>,
    viewport_h: u16,
    md: markdown::Renderer,

    phase: Phase,
    events_rx: Option<mpsc::Receiver<AgentEvent>>,
    ask_rx: mpsc::Receiver<AskMsg>,
    approver: Arc<ChannelApprover>,

    editor: Editor,
    attachments: Vec<Attachment>,
    dialog: Option<(Dialog, oneshot::Sender<Approval>)>,
    rewind: Option<rewind::Picker>,
    last_esc: Option<Instant>,
    popup_index: usize,

    // Live (un-baked) streaming state: rendered only in the viewport,
    // baked into scrollback once finalized.
    live_text: String,
    thinking_chars: usize,
    thinking_since: Option<Instant>,
    out_tokens: usize,
    state_label: String,
    turn_usage: Usage,
    mode_label: String,
    spinner: usize,
    /// Cache-read share of the previous turn; the regression sentinel
    /// compares against it so cache decay is visible immediately.
    prev_cache_ratio: Option<f64>,
    should_exit: bool,
}

impl App {
    pub fn new(agent: Arc<Agent>, session: Session) -> anyhow::Result<Self> {
        let (ask_tx, ask_rx) = mpsc::channel(4);
        let mode_label = session.mode.label().to_string();
        let viewport_h = 4;
        let terminal = make_terminal(viewport_h)?;
        Ok(Self {
            agent,
            session: Some(session),
            terminal,
            viewport_h,
            md: markdown::Renderer::default(),
            phase: Phase::Idle,
            events_rx: None,
            ask_rx,
            approver: Arc::new(ChannelApprover { tx: ask_tx }),
            editor: Editor::new(),
            attachments: Vec::new(),
            dialog: None,
            rewind: None,
            last_esc: None,
            popup_index: 0,
            live_text: String::new(),
            thinking_chars: 0,
            thinking_since: None,
            out_tokens: 0,
            state_label: String::new(),
            turn_usage: Usage::default(),
            mode_label,
            spinner: 0,
            prev_cache_ratio: None,
            should_exit: false,
        })
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        self.bake(vec![Line::from(vec![
            Span::styled("tcode", theme::user_prompt()),
            Span::styled(
                format!(
                    " v{} · {} · {} · shift+tab mode · /help",
                    env!("CARGO_PKG_VERSION"),
                    self.agent.provider.name(),
                    self.agent.provider.model()
                ),
                theme::dim(),
            ),
        ])]);
        self.bake_transcript();
        let mut term_events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
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
                    while let Some(rx) = self.events_rx.as_mut() {
                        match rx.try_recv() {
                            Ok(ev) => self.on_agent_event(ev),
                            Err(_) => break,
                        }
                    }
                }
                Some(ask) = self.ask_rx.recv() => {
                    self.dialog = Some((
                        Dialog::new(ask.summary.clone(), ask.descriptor.clone()),
                        ask.reply,
                    ));
                }
                done = join_phase(&mut self.phase) => {
                    self.on_turn_done(done);
                }
                _ = tick.tick() => {
                    if matches!(self.phase, Phase::Running { .. }) {
                        self.spinner = (self.spinner + 1) % SPINNER.len();
                    }
                }
            }
        }
        Ok(())
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
        for entry in session.ledger.entries() {
            match entry {
                tcode_core::Entry::User(blocks) => {
                    for b in blocks {
                        match b {
                            ContentBlock::Text { text }
                                if !text.starts_with("<tcode-status>") =>
                            {
                                for (i, l) in text.lines().enumerate() {
                                    let prefix = if i == 0 { "› " } else { "  " };
                                    lines.push(Line::from(vec![
                                        Span::styled(prefix.to_string(), theme::user_prompt()),
                                        Span::raw(l.to_string()),
                                    ]));
                                }
                            }
                            ContentBlock::Image { .. } => {
                                lines.push(Line::styled("  ⌞ [image]", theme::dim()));
                            }
                            _ => {}
                        }
                    }
                }
                tcode_core::Entry::Assistant(blocks) => {
                    for b in blocks {
                        match b {
                            ContentBlock::Text { text } => {
                                lines.extend(self.md.render(text));
                                lines.push(Line::default());
                            }
                            ContentBlock::ToolUse { name, input, .. } => {
                                lines.push(Line::from(vec![
                                    Span::styled("● ", theme::accent()),
                                    Span::styled(
                                        tcode_core::agent::summarize_call(name, input),
                                        theme::bold(),
                                    ),
                                ]));
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
                // Tool results and harness notes add noise on replay.
                tcode_core::Entry::ToolResults(_) | tcode_core::Entry::Note(_) => {}
            }
        }
        lines.push(Line::styled("── resumed ──", theme::dim()));
        self.bake(lines);
    }

    // ------------------------------------------------------------ turn

    fn start_turn(&mut self, input: String) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        // Echo the user input into the transcript.
        let mut echo: Vec<Line> = Vec::new();
        for (i, l) in input.lines().enumerate() {
            let prefix = if i == 0 { "› " } else { "  " };
            echo.push(Line::from(vec![
                Span::styled(prefix.to_string(), theme::user_prompt()),
                Span::raw(l.to_string()),
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
        let (session, result) = done;
        let elapsed = match &self.phase {
            Phase::Running { started, .. } => started.elapsed().as_secs_f32(),
            Phase::Idle => 0.0,
        };
        // The session's per-turn tally is authoritative (it also covers
        // compaction, which streams no Usage events to the UI).
        self.turn_usage = session.turn_usage;
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
        let cache_pct = if u.total_input() > 0 {
            (u.cache_read_tokens as f64 / u.total_input() as f64 * 100.0).round()
        } else {
            0.0
        };
        self.bake(vec![Line::styled(
            format!(
                "· {elapsed:.1}s · in {} | out {} | cache r {} ({cache_pct:.0}%) w {}",
                u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cache_write_tokens
            ),
            theme::dim(),
        )]);
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
            AgentEvent::Started => self.state_label = "responding".into(),
            AgentEvent::TextDelta(t) => {
                self.finish_thinking();
                self.out_tokens += approx_tokens(&t);
                self.live_text.push_str(&t);
                self.state_label = "writing".into();
            }
            AgentEvent::ThinkingDelta(t) => {
                if self.thinking_since.is_none() {
                    self.thinking_since = Some(Instant::now());
                }
                self.out_tokens += approx_tokens(&t);
                self.thinking_chars += t.chars().count();
                self.state_label = "thinking".into();
            }
            AgentEvent::ToolStart {
                name,
                summary,
                input,
            } => {
                self.bake_live_text();
                self.finish_thinking();
                let mut lines = vec![Line::from(vec![
                    Span::styled("● ", theme::accent()),
                    Span::styled(summary.clone(), theme::bold()),
                ])];
                lines.extend(diff::render_change(&name, &input));
                self.bake(lines);
                self.state_label = format!("running: {summary}");
            }
            AgentEvent::ToolEnd {
                preview, is_error, ..
            } => {
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
                self.thinking_since = None;
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

    fn finish_thinking(&mut self) {
        if let Some(since) = self.thinking_since.take() {
            let secs = since.elapsed().as_secs().max(1);
            self.bake(vec![Line::styled(
                format!("✻ thought for {secs}s (~{} tok)", self.thinking_chars / 3),
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
                let verdict = match approval.decision {
                    ApprovalDecision::Yes => "approved",
                    ApprovalDecision::YesAlways => "approved (always)",
                    ApprovalDecision::No => "declined",
                };
                let note = approval
                    .comment
                    .as_deref()
                    .map(|c| format!(" — {c}"))
                    .unwrap_or_default();
                self.bake(vec![
                    Line::from(vec![
                        Span::styled("? ", theme::accent()),
                        Span::styled(dialog.summary, theme::bold()),
                    ]),
                    Line::styled(format!("  ⎿ {verdict}{note}"), theme::dim()),
                ]);
                let _ = reply.send(approval);
            }
            return;
        }

        let running = matches!(self.phase, Phase::Running { .. });
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
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
            let cmd = self
                .popup_selection()
                .unwrap_or_else(|| trimmed.clone());
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

    fn run_slash(&mut self, cmd: &str) {
        match cmd {
            "/exit" | "/quit" => self.should_exit = true,
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
                        u.input_tokens,
                        u.output_tokens,
                        u.cache_read_tokens,
                        u.cache_write_tokens
                    ),
                    theme::dim(),
                )]);
            }
            "/compact" => self.start_compact(),
            "/clear" => {
                if let Some(session) = self.session.as_mut() {
                    session.ledger.truncate_tail(0);
                    session.last_prompt_tokens = 0;
                    session
                        .tool_ctx
                        .freshness
                        .lock()
                        .expect("freshness lock")
                        .clear();
                    self.prev_cache_ratio = None;
                    self.bake(vec![Line::styled("conversation cleared", theme::dim())]);
                }
            }
            "/help" => {
                let mut lines: Vec<Line> = vec![Line::styled("keys:", theme::bold().fg(theme::ACCENT))];
                for (k, d) in [
                    ("enter", "send · alt+enter/ctrl+j newline"),
                    ("esc", "cancel current turn / clear input"),
                    ("shift+tab", "cycle permission mode"),
                    ("ctrl+v / alt+v", "paste (images become attachments)"),
                    ("ctrl+c", "cancel / clear / exit"),
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
        self.dialog.is_none() && self.editor.line_count() == 1 && self.editor.text().starts_with('/')
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
        let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        let height = para.line_count(width) as u16;
        let _ = self.terminal.insert_before(height, |buf| {
            para.render(buf.area, buf);
        });
    }

    fn desired_viewport(&self) -> u16 {
        let h = if let Some(picker) = &self.rewind {
            1 + picker.height()
        } else if let Some((dialog, _)) = &self.dialog {
            1 + dialog.height()
        } else {
            let live = if matches!(self.phase, Phase::Running { .. }) {
                (self.live_text.lines().count().min(8)) as u16
            } else {
                0
            };
            let editor_h = (self.editor.line_count() as u16).clamp(1, 6);
            let popup_h = if self.popup_active() {
                self.popup_matches().len() as u16
            } else {
                0
            };
            let attach_h = if self.attachments.is_empty() { 0 } else { 1 };
            live + 1 + editor_h + popup_h + attach_h + 1
        };
        h.clamp(3, 16)
    }

    fn redraw(&mut self) -> anyhow::Result<()> {
        let desired = self.desired_viewport();
        if desired != self.viewport_h {
            let _ = self.terminal.clear();
            self.terminal = make_terminal(desired)?;
            self.viewport_h = desired;
        }

        let running = matches!(self.phase, Phase::Running { .. });
        let started = match &self.phase {
            Phase::Running { started, .. } => Some(*started),
            Phase::Idle => None,
        };
        let status = self.status_line(running, started);
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
            .rewind
            .as_ref()
            .map(|p| p.render())
            .or_else(|| self.dialog.as_ref().map(|(d, _)| d.render()));
        let editor_lines: Vec<String> = self.editor.lines().to_vec();
        let (cur_row, cur_col) = self.editor.cursor();
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

        self.terminal.draw(|frame| {
            let area = frame.area();
            let mut y = area.y;
            let row = |y: u16, h: u16| Rect {
                x: area.x,
                y,
                width: area.width,
                height: h.min(area.bottom().saturating_sub(y)),
            };

            if let Some(lines) = dialog_lines {
                let h = lines.len() as u16;
                frame.render_widget(Paragraph::new(Text::from(lines)), row(y, h));
                y += h;
                frame.render_widget(
                    Paragraph::new(Line::styled(status, theme::dim())),
                    row(y, 1),
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

            frame.render_widget(
                Paragraph::new(Line::styled(status, theme::dim())),
                row(y, 1),
            );
            y += 1;

            if !attach_labels.is_empty() {
                frame.render_widget(
                    Paragraph::new(Line::styled(
                        format!("attachments: {}", attach_labels.join(" · ")),
                        theme::accent(),
                    )),
                    row(y, 1),
                );
                y += 1;
            }

            let editor_y = y;
            for (i, l) in editor_lines.iter().enumerate().take(6) {
                let prefix = if i == 0 { "› " } else { "  " };
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(prefix, theme::user_prompt()),
                        Span::raw(l.clone()),
                    ])),
                    row(y, 1),
                );
                y += 1;
            }
            frame.set_cursor_position((
                area.x + 2 + cur_col as u16,
                editor_y + cur_row as u16,
            ));

            for (i, (c, d)) in popup.iter().enumerate() {
                let style = if i == popup_index {
                    theme::accent()
                } else {
                    theme::dim()
                };
                frame.render_widget(
                    Paragraph::new(Line::styled(format!("  {c:<10} {d}"), style)),
                    row(y, 1),
                );
                y += 1;
            }
        })?;
        Ok(())
    }

    fn status_line(&self, running: bool, started: Option<Instant>) -> String {
        if running {
            let elapsed = started.map(|s| s.elapsed().as_secs()).unwrap_or(0);
            format!(
                "{} {} · {}s · ↓ ~{} tok · esc to cancel",
                SPINNER[self.spinner], self.state_label, elapsed, self.out_tokens
            )
        } else {
            let u = self.turn_usage;
            let cache = if u.total_input() > 0 {
                format!(
                    " · cache {}%",
                    (u.cache_read_tokens as f64 / u.total_input() as f64 * 100.0).round()
                )
            } else {
                String::new()
            };
            format!("mode {}{} · /help", self.mode_label, cache)
        }
    }
}

fn make_terminal(height: u16) -> anyhow::Result<Terminal<CrosstermBackend<Stdout>>> {
    Ok(Terminal::with_options(
        CrosstermBackend::new(std::io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )?)
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
