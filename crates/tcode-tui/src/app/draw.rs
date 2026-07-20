//! Painting a frame, and the status/hint rows below the transcript.
//!
//! Only the visible slice is rendered: the transcript locates its viewport by
//! binary search over cached per-block wraps, so a frame costs O(viewport)
//! regardless of how long the conversation is. Nothing here may start walking
//! the whole transcript.
//!
//! Touches: terminal, transcript, overlay, editor, meter, state_label,
//! mode_label, spinner, anim_frame, notice, retry_wait, hitboxes.

use super::*;
use crate::composer::EditorLayout;

/// What `redraw` measured before laying the panel out. Passed as one struct
/// because every field is needed by both the height calculation and the paint.
struct PanelInput<'a> {
    running: bool,
    viewing_trace: bool,
    width: u16,
    editor: &'a EditorLayout,
    editor_start: usize,
    ghost_lines: Option<Vec<String>>,
    panel_lines: Vec<Line<'static>>,
    status: Line<'static>,
    hint: Line<'static>,
}

/// One horizontal band of the bottom panel, in paint order.
///
/// A section knows its own height. That is the point: the panel's total height
/// is the sum, so a section can never be drawn at a size the layout did not
/// reserve for it.
enum Section {
    /// A picker or approval dialog owns the panel's primary content. An
    /// approval additionally leaves the status hint below it visible.
    Overlay(Vec<Line<'static>>),
    /// The persistent agent tree, in its own bordered box.
    LivePanel(Vec<Line<'static>>),
    /// Breathing room between the transcript and the live status line.
    Gap,
    Status(Line<'static>),
    Queued(Vec<Line<'static>>),
    Input(Vec<Line<'static>>),
    ContextMeter {
        used: u64,
        window: u64,
        estimated: bool,
    },
    RateLimit(tcode_core::RateLimits),
    Popup(Vec<Line<'static>>),
    Hint(Line<'static>),
}

impl Section {
    /// Rows this section occupies, borders included.
    fn height(&self) -> u16 {
        match self {
            Section::Overlay(lines) | Section::LivePanel(lines) => lines.len() as u16 + 2,
            Section::Input(lines) => lines.len().clamp(1, 6) as u16 + 2,
            Section::Queued(lines) | Section::Popup(lines) => lines.len() as u16,
            Section::Gap
            | Section::Status(_)
            | Section::ContextMeter { .. }
            | Section::RateLimit(_)
            | Section::Hint(_) => 1,
        }
    }
}

/// Where the bottom panel sits, and how tall it is.
///
/// Painting and mouse hit-testing both ask this. When each computed it
/// separately, a change to the panel's height silently moved clicks a row off
/// whatever they appeared to land on.
#[derive(Clone, Copy)]
pub(super) struct PanelGeometry {
    height: u16,
    pub(super) top: u16,
}

impl PanelGeometry {
    fn new(desired: u16, area_height: u16) -> Self {
        // The transcript keeps at least a few visible rows.
        let height = desired.min(area_height.saturating_sub(4)).max(1);
        Self {
            height,
            top: area_height.saturating_sub(height),
        }
    }

    /// The border-free content row a screen row falls on, or `None` when the
    /// pointer is outside the panel or on its border.
    pub(super) fn content_row(&self, row: u16) -> Option<usize> {
        (row > self.top && row < self.top + self.height.saturating_sub(1))
            .then(|| row.saturating_sub(self.top + 1) as usize)
    }
}

/// The rounded box shared by the overlay, the agent tree and the input.
fn bordered(lines: Vec<Line<'static>>, border: ratatui::style::Style) -> Paragraph<'static> {
    use ratatui::widgets::{Block, BorderType};
    Paragraph::new(Text::from(lines)).block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(border),
    )
}

/// A trace has no editor or status line. Keep its navigation visible even
/// after the sub-agent has finished and dropped out of the live tree.
fn trace_navigation_lines(mut lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    if !lines.is_empty() {
        lines.push(Line::default());
    }
    lines.push(Line::styled("  esc to return to Main agent", theme::dim()));
    lines
}

/// The agent tree's rendered area plus the action target each inner row owns
/// (border rows excluded, indexes parallel the panel's content lines).
pub(super) struct PanelHitbox {
    pub(super) rect: Rect,
    pub(super) targets: Vec<Option<PanelTarget>>,
}

#[derive(Clone, Copy)]
pub(super) struct StatusHitboxes {
    pub(super) mode: Rect,
    pub(super) model: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StatusHover {
    Mode,
    Model,
}

/// Keep the complete tip in the transcript source. `Transcript` owns wrapping
/// and recomputes it whenever the terminal width changes.
pub(super) fn tip_line(tip: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("  ✻ ".to_string(), theme::accent()),
        Span::styled(format!("tip: {tip}"), theme::dim()),
    ])
}

/// Build an amber activity label with the same soft sweep used by in-flight
/// tool headers. Per-character spans keep the terminal's normal wrapping while
/// allowing the sweep to preserve the status line's warning hue at rest.
pub(super) fn shimmer_text(
    text: &str,
    frame: usize,
    base: ratatui::style::Color,
) -> Vec<Span<'static>> {
    let width = text.width().max(1);
    let mut column = 0;
    text.chars()
        .map(|ch| {
            let content = ch.to_string();
            let span = Span::styled(
                content.clone(),
                ratatui::style::Style::default()
                    .fg(theme::shimmer_color(frame, column, width, base)),
            );
            column += content.width();
            span
        })
        .collect()
}

pub(super) fn status_icon(status: tcode_core::TaskRunStatus) -> &'static str {
    match status {
        tcode_core::TaskRunStatus::Running => "●",
        tcode_core::TaskRunStatus::Done => "✓",
        tcode_core::TaskRunStatus::Failed => "!",
        tcode_core::TaskRunStatus::Cancelled => "⨯",
        tcode_core::TaskRunStatus::Interrupted => "⊘",
    }
}

pub(super) fn status_hitboxes(row: Rect, mode: &str, model: &str) -> StatusHitboxes {
    // The hint begins with `  mode `, then `mode`, ` · `, then the model
    // description. Compute terminal-cell widths rather than byte offsets: both
    // labels may contain Unicode glyphs.
    let mode_start = "  mode ".width();
    let model_start = mode_start + mode.width() + " · ".width();
    StatusHitboxes {
        mode: clipped_hitbox(row, mode_start, mode.width()),
        model: clipped_hitbox(row, model_start, model.width()),
    }
}

pub(super) fn clipped_hitbox(row: Rect, offset: usize, width: usize) -> Rect {
    let right = row.right();
    let x = row
        .x
        .saturating_add(offset.min(u16::MAX as usize) as u16)
        .min(right);
    Rect {
        x,
        y: row.y,
        width: right
            .saturating_sub(x)
            .min(width.min(u16::MAX as usize) as u16),
        height: row.height,
    }
}

pub(super) fn rect_contains(rect: Rect, x: u16, y: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && x >= rect.x
        && x < rect.right()
        && y >= rect.y
        && y < rect.bottom()
}

/// A compact, centered affordance over the transcript's bottom edge. It keeps
/// the full shortcut when space permits, then degrades to a small arrow-only
/// target rather than wrapping into the input panel.
pub(super) fn jump_to_bottom_control(
    row: Rect,
    unseen_blocks: usize,
    hovered: bool,
) -> (Rect, Line<'static>) {
    let full = if unseen_blocks == 0 {
        " Jump to bottom  ↓ ".to_string()
    } else {
        format!(
            " {unseen_blocks} new message{}  (ctrl+End)  ↓ ",
            if unseen_blocks == 1 { "" } else { "s" }
        )
    };
    let compact = if unseen_blocks == 0 {
        " ↓ bottom ".to_string()
    } else {
        format!(" {unseen_blocks} new  ↓ ")
    };
    let label = if full.width() <= row.width as usize {
        full
    } else if compact.width() <= row.width as usize {
        compact
    } else {
        "↓".to_string()
    };
    let width = label.width().min(row.width as usize) as u16;
    let rect = Rect {
        x: row.x + row.width.saturating_sub(width) / 2,
        y: row.y,
        width,
        height: 1,
    };
    let base = ratatui::style::Style::default()
        .fg(theme::DIM)
        .bg(ratatui::style::Color::Rgb(43, 46, 52));
    let style = if hovered {
        theme::hover_style(base)
    } else {
        base
    };
    (rect, Line::styled(label, style))
}

pub(super) fn status_hover_at(hitboxes: StatusHitboxes, x: u16, y: u16) -> Option<StatusHover> {
    if rect_contains(hitboxes.mode, x, y) {
        Some(StatusHover::Mode)
    } else if rect_contains(hitboxes.model, x, y) {
        Some(StatusHover::Model)
    } else {
        None
    }
}

/// A turn boundary should read as a small receipt, not as an unstructured
/// diagnostic log line. The numbers stay selectable/copyable terminal text,
/// while colour and arrows make input, output and cache scannable.
pub(super) fn area_width(terminal: &Term) -> u16 {
    terminal.size().map(|s| s.width).unwrap_or(80)
}

pub(super) fn area_height(terminal: &Term) -> u16 {
    terminal.size().map(|s| s.height).unwrap_or(24)
}

impl App {
    /// True while the main running status or an in-flight tool header needs a
    /// shimmer frame. The 100ms animation tick stays asleep while idle and
    /// during a retry countdown, whose red status is intentionally static.
    pub(super) fn shimmer_active(&self) -> bool {
        matches!(self.phase, Phase::Running { .. }) && self.retry_wait.is_none()
    }

    /// Welcome block: gradient logo, model, cwd, one rotating tip.
    /// Frameless on purpose — whitespace does the framing, and there is
    /// no box-width arithmetic to break on narrow terminals.
    pub(super) fn banner(&self) -> Vec<Line<'static>> {
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

    pub(super) fn panel_target_at(&self, x: u16, y: u16) -> Option<PanelTarget> {
        let hitbox = self.live_panel_hitbox.as_ref()?;
        if y <= hitbox.rect.y || y >= hitbox.rect.bottom().saturating_sub(1) || x < hitbox.rect.x {
            return None;
        }
        let row = y.saturating_sub(hitbox.rect.y + 1) as usize;
        hitbox.targets.get(row).and_then(Clone::clone)
    }

    /// Finalize content into the transcript. Name kept from the inline era;
    /// unlike native scrollback, transcript content can still be truncated
    /// (rewind) or cleared later.
    pub(super) fn bake(&mut self, lines: Vec<Line<'static>>) {
        self.transcript.push(lines);
    }

    /// The task card's live status is intentionally muted: its parent-authored
    /// objective is the primary label, while the changing sub-agent tool is
    /// supporting progress. A parallel batch names its current task and count.
    pub(super) fn task_status_lines(&self, run: &UiTaskRun) -> Vec<Line<'static>> {
        let Some(call) = run.current_call() else {
            return task_plain_status(&run.activity);
        };
        let summary = self.display_summary(&call.summary);
        let mut spans = vec![Span::styled(
            format!("{TASK_STATUS_INDENT}{summary}"),
            theme::dim(),
        )];
        if run.calls.len() > 1 {
            spans.push(Span::styled(
                format!(
                    " · task {}/{}",
                    run.rotation % run.calls.len() + 1,
                    run.calls.len()
                ),
                theme::dim(),
            ));
        }
        vec![Line::from(spans)]
    }

    /// Advance which call of a parallel batch each running task's status line
    /// shows. Driven by the animation tick at a slow multiple of its cadence.
    pub(super) fn rotate_task_calls(&mut self) {
        for index in 0..self.task_runs.len() {
            let run = &self.task_runs[index];
            if run.status != tcode_core::TaskRunStatus::Running || run.calls.len() < 2 {
                continue;
            }
            self.task_runs[index].rotation = self.task_runs[index].rotation.wrapping_add(1);
            let Some(block) = self.task_runs[index].block else {
                continue;
            };
            let status = self.task_status_lines(&self.task_runs[index]);
            self.transcript.set_live_status(block, Some(status));
        }
    }

    /// The persistent agent tree: progress phases plus the root conversation
    /// and only currently working task runs. Completed traces remain reachable
    /// from their parent task headers, not as stale tree children.
    pub(super) fn live_panel_lines(&self) -> (Vec<Line<'static>>, Vec<Option<PanelTarget>>) {
        let running: Vec<&UiTaskRun> = self
            .task_runs
            .iter()
            .filter(|run| run.status == tcode_core::TaskRunStatus::Running)
            .collect();
        let started = match &self.phase {
            Phase::Running { started, .. } => Some(*started),
            Phase::Idle => None,
        };
        let current = match &self.active_view {
            ViewId::Main => PanelTarget::Main,
            ViewId::TaskRun(id) => PanelTarget::Task(id.clone()),
        };
        // The bottom status line keeps the "running:" prefix; inside the tree
        // the dot already says the row is live, so the label alone reads better.
        let activity = self
            .state_label
            .strip_prefix("running: ")
            .unwrap_or(&self.state_label);
        live_panel::lines(
            &self.progress,
            &running,
            MainAgent {
                running: started.is_some(),
                activity: if activity.is_empty() {
                    "working…"
                } else {
                    activity
                },
                elapsed_secs: started
                    .map(|started| started.elapsed().as_secs())
                    .unwrap_or(0),
                output_tokens: self.meter.out_tokens,
            },
            area_width(&self.terminal),
            self.live_panel_hover.as_ref(),
            &current,
        )
    }

    /// What the user has already sent but the model has not yet seen: the
    /// prompt itself, dimmed, waiting above the input box it came from. It is a
    /// view of the queue, not a copy — delivery drains the queue and the row
    /// disappears by itself, replaced by the real prompt in the transcript.
    pub(super) fn queued_lines(&self, width: u16) -> Vec<Line<'static>> {
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

    pub(super) fn redraw(&mut self) -> anyhow::Result<()> {
        let running = matches!(self.phase, Phase::Running { .. });
        let started = match &self.phase {
            Phase::Running { started, .. } => Some(*started),
            Phase::Idle => None,
        };
        let width = area_width(&self.terminal);
        let status_model_label = self.agent.model.snapshot().describe();
        let mode_label = self.mode_label.clone();
        // Rewind navigation and an overlay take over interaction, so status
        // values remain visible but are not clickable while either is active.
        let status_clickable = self.rewind_nav.is_none() && self.overlay.is_none();
        let viewing_trace = !matches!(self.active_view, ViewId::Main);
        let editor = editor_layout(&self.editor, width);
        let editor_start = editor.cursor_row.saturating_sub(5);
        // Ghost text only appears while the input is empty and idle, and uses
        // the same width calculation as real input so a long suggestion cannot
        // overrun the box.
        let ghost_lines = self
            .suggestion
            .clone()
            .filter(|_| self.editor.is_empty() && !running)
            .map(|text| ghost_visual_lines(&format!("{text}  \u{2192} to accept"), width));
        let (mut panel_lines, mut panel_targets) = self.live_panel_lines();
        if viewing_trace {
            panel_lines = trace_navigation_lines(panel_lines);
            panel_targets.resize(panel_lines.len(), None);
        }

        let sections = self.panel_sections(PanelInput {
            running,
            viewing_trace,
            width,
            editor: &editor,
            editor_start,
            ghost_lines,
            panel_lines,
            status: self.status_line(running, started),
            hint: self.idle_hint(),
        });

        // Overlay rendering calculates a focused note's caret cell. Read it
        // after `panel_sections` renders the overlay so keyboard movement and
        // the terminal cursor update in the same frame.
        let dialog_cursor = self.overlay.as_ref().and_then(Overlay::cursor_cell);

        use ratatui::widgets::Clear;

        // Hitboxes are only known once the layout is placed; capture them so
        // mouse handling can map screen coordinates back to what was drawn.
        let mut captured_input: Option<InputHitbox> = None;
        let mut captured_status: Option<StatusHitboxes> = None;
        let mut captured_jump_to_bottom: Option<Rect> = None;
        let mut captured_panel: Option<Rect> = None;
        let animation_frame = self.anim_frame;
        let jump_to_bottom_hover = self.jump_to_bottom_hover;
        let transcript = match &self.active_view {
            ViewId::Main => &mut self.transcript,
            ViewId::TaskRun(_) => {
                &mut self
                    .trace_view
                    .as_mut()
                    .expect("active task view has trace")
                    .view
                    .transcript
            }
        };
        transcript.set_animation_frame(animation_frame);
        self.terminal.draw(|frame| {
            let area = frame.area();
            // The lower panel changes height when a dialog opens, a paste is
            // folded, or the editor wraps. Clear the entire frame first:
            // widgets paint only their own cells, so otherwise letters from a
            // previous, taller panel survive after the layout moves.
            frame.render_widget(Clear, area);

            // The panel is exactly as tall as its sections say; the transcript
            // gets the rest.
            let geometry =
                PanelGeometry::new(sections.iter().map(Section::height).sum(), area.height);
            transcript.render(
                frame.buffer_mut(),
                Rect {
                    height: geometry.top,
                    ..area
                },
            );
            if geometry.top > 0 && !transcript.is_following() {
                let row = Rect {
                    x: area.x,
                    y: area.y + geometry.top - 1,
                    width: area.width,
                    height: 1,
                };
                let (rect, line) =
                    jump_to_bottom_control(row, transcript.unseen_blocks(), jump_to_bottom_hover);
                frame.render_widget(Paragraph::new(line), rect);
                captured_jump_to_bottom = Some(rect);
            }

            let mut y = area.y + geometry.top;
            let row = |y: u16, h: u16| Rect {
                x: area.x,
                y,
                width: area.width,
                height: h.min(area.bottom().saturating_sub(y)),
            };

            for section in sections {
                let height = section.height();
                let rect = row(y, height);
                match section {
                    // Pickers and approval dialogs own the panel's main
                    // content: a rounded accent border signals where most
                    // keys go. Approval keeps the mode hint below this box.
                    Section::Overlay(lines) => {
                        frame.render_widget(bordered(lines, theme::border_active()), rect);
                        // Place the terminal cursor on the focused note caret
                        // (+1 for the block border) so the IME follows it.
                        // Without this the hardware cursor stays hidden and IME
                        // composition detaches.
                        if let Some((crow, ccol)) = dialog_cursor {
                            frame.set_cursor_position((area.x + 1 + ccol, y + 1 + crow));
                        }
                    }
                    Section::LivePanel(lines) => {
                        frame.render_widget(bordered(lines, theme::border()), rect);
                        captured_panel = Some(rect);
                    }
                    // The reserved row separates completed transcript content
                    // from the live turn indicator.
                    Section::Gap => {}
                    Section::Status(line) => frame.render_widget(Paragraph::new(line), rect),
                    Section::Queued(lines) => {
                        frame.render_widget(Paragraph::new(Text::from(lines)), rect)
                    }
                    Section::Input(lines) => {
                        captured_input = Some(InputHitbox { rect, editor_start });
                        frame.render_widget(bordered(lines, theme::border()), rect);
                        // Show the cursor even when a long multi-line prompt
                        // exceeds the six-row input box.
                        frame.set_cursor_position((
                            area.x + 3 + editor.cursor_col as u16,
                            y + 1 + (editor.cursor_row - editor_start) as u16,
                        ));
                    }
                    Section::ContextMeter {
                        used,
                        window,
                        estimated,
                    } => frame.render_widget(
                        Paragraph::new(context_progress_line(used, window, area.width, estimated)),
                        rect,
                    ),
                    Section::RateLimit(limits) => {
                        frame.render_widget(Paragraph::new(rate_limit_line(limits)), rect)
                    }
                    Section::Popup(lines) => {
                        frame.render_widget(Paragraph::new(Text::from(lines)), rect)
                    }
                    Section::Hint(line) => {
                        if status_clickable {
                            captured_status =
                                Some(status_hitboxes(rect, &mode_label, &status_model_label));
                        }
                        frame.render_widget(Paragraph::new(line), rect);
                    }
                }
                y += height;
            }
        })?;
        self.input_hitbox = captured_input;
        self.status_hitboxes = captured_status;
        self.jump_to_bottom_hitbox = captured_jump_to_bottom;
        self.live_panel_hitbox = captured_panel.map(|rect| PanelHitbox {
            rect,
            targets: panel_targets,
        });
        Ok(())
    }

    /// The bottom panel, top to bottom. An overlay replaces the whole panel; a
    /// trace view shows only the agent tree; otherwise the full stack.
    ///
    /// This is the only place the panel's composition is decided, so `redraw`
    /// never has to keep a running height in step with what it later paints.
    fn panel_sections(&self, input: PanelInput<'_>) -> Vec<Section> {
        if let Some(overlay) = self.overlay.as_ref() {
            let mut sections = vec![Section::Overlay(overlay.render(&self.overlay_ctx()))];
            if overlay.keeps_status_hint() {
                sections.push(Section::Hint(input.hint));
            }
            return sections;
        }
        if input.viewing_trace {
            return vec![Section::LivePanel(input.panel_lines)];
        }

        let input_lines = self.input_lines(&input);
        let mut sections = Vec::new();
        if !input.panel_lines.is_empty() {
            sections.push(Section::LivePanel(input.panel_lines));
        }
        if input.running {
            sections.push(Section::Gap);
            sections.push(Section::Status(input.status));
        }
        // Prompts already sent by the user but not yet by us: they sit between
        // the spinner and the input box, where the next thing to reach the
        // model belongs.
        let queued = self.queued_lines(input.width);
        if !queued.is_empty() {
            sections.push(Section::Queued(queued));
        }
        sections.push(Section::Input(input_lines));
        sections.push(Section::ContextMeter {
            used: self.meter.context_tokens,
            window: self.agent.model.snapshot().context_window,
            estimated: self.meter.context_estimated,
        });
        if let Some(limits) = self.meter.rate_limits {
            sections.push(Section::RateLimit(limits));
        }
        if self.popup_active() {
            sections.push(Section::Popup(self.popup_lines()));
        }
        sections.push(Section::Hint(input.hint));
        sections
    }

    /// Inner rows of the input box: either the ghost suggestion or the real
    /// prompt, both under the same rail.
    fn input_lines(&self, input: &PanelInput<'_>) -> Vec<Line<'static>> {
        if let Some(lines) = &input.ghost_lines {
            return lines
                .iter()
                .take(6)
                .enumerate()
                .map(|(index, line)| {
                    Line::from(vec![
                        Span::styled(
                            if index == 0 { "\u{203a} " } else { "  " },
                            theme::user_prompt(),
                        ),
                        Span::styled(line.clone(), theme::dim()),
                    ])
                })
                .collect();
        }
        input.editor.lines[input.editor_start..]
            .iter()
            .take(6)
            .map(|visual| {
                let mut spans = vec![Span::styled(
                    if visual.first_logical_line {
                        "\u{203a} "
                    } else {
                        "  "
                    },
                    theme::user_prompt(),
                )];
                spans.extend(input_spans(
                    &visual.text,
                    visual.selection,
                    &self.reference_index,
                ));
                Line::from(spans)
            })
            .collect()
    }

    fn popup_lines(&self) -> Vec<Line<'static>> {
        let matches = self.completion_matches();
        let selected = self.popup_index.min(matches.len().saturating_sub(1));
        matches
            .into_iter()
            .enumerate()
            .map(|(index, completion)| {
                let (label, description) = (completion.label, completion.description);
                if index == selected {
                    Line::from(vec![
                        Span::styled("  \u{25b8} ".to_string(), theme::accent()),
                        Span::styled(format!("{label:<10}"), theme::user_prompt()),
                        Span::styled(format!(" {description}"), theme::accent()),
                    ])
                } else {
                    Line::styled(format!("    {label:<10} {description}"), theme::dim())
                }
            })
            .collect()
    }

    /// The panel an open overlay produces. Mouse hit-testing calls this, so it
    /// agrees with what `redraw` painted by construction rather than by a
    /// duplicated formula.
    pub(super) fn overlay_geometry(&self) -> Option<PanelGeometry> {
        let overlay = self.overlay.as_ref()?;
        let section = Section::Overlay(overlay.render(&self.overlay_ctx()));
        let status_height = u16::from(overlay.keeps_status_hint());
        Some(PanelGeometry::new(
            section.height() + status_height,
            area_height(&self.terminal),
        ))
    }

    /// Spinner line shown above the input while a turn runs. The sparkle
    /// carries the animation; the label stays readable, metadata stays dim.
    pub(super) fn status_line(&self, running: bool, started: Option<Instant>) -> Line<'static> {
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
        let activity = format!("{frame} {}", self.state_label);
        let mut spans = shimmer_text(&activity, self.anim_frame, theme::WARN);
        spans.push(Span::styled(
            format!(
                " · {elapsed}s · ↓ ~{} tok · esc to cancel",
                token_count(self.meter.out_tokens as u64)
            ),
            theme::dim(),
        ));
        Line::from(spans)
    }

    /// One-liner under the input box: mode, model, cache health. Mostly
    /// dim; the mode value carries the accent because it decides what the
    /// agent may do without asking. The active model and cache hit rate use a
    /// modest neutral lift, and a transient notice keeps the default foreground
    /// so it reads as news rather than furniture.
    pub(super) fn idle_hint(&self) -> Line<'static> {
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
        let u = self.meter.turn;
        let cache = (u.total_input() > 0).then(|| {
            format!(
                "{}%",
                (u.cache_read_tokens as f64 / u.total_input() as f64 * 100.0).round()
            )
        });
        let mode_style = if self.status_hover == Some(StatusHover::Mode) {
            theme::hover_style(theme::accent())
        } else {
            theme::accent()
        };
        let model_style = if self.status_hover == Some(StatusHover::Model) {
            theme::hover_style(theme::metadata())
        } else {
            theme::metadata()
        };
        let mut spans = vec![
            Span::styled("  mode ".to_string(), theme::dim()),
            Span::styled(self.mode_label.clone(), mode_style),
            Span::styled(" · ".to_string(), theme::dim()),
            Span::styled(self.agent.model.snapshot().describe(), model_style),
        ];
        // A mode that silently changes what the model does must be visible
        // while it is on, not only in the line that switched it on.
        if self.dogfood {
            spans.push(Span::styled(" · dogfood".to_string(), theme::warn()));
        }
        if let Some(cache) = cache {
            spans.push(Span::styled(" · cache ", theme::dim()));
            spans.push(Span::styled(cache, theme::metadata()));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(n: usize) -> Vec<Line<'static>> {
        vec![Line::raw("x"); n]
    }

    /// The panel is exactly the sum of its sections. That is what lets
    /// painting and hit-testing share one geometry instead of each keeping a
    /// running height in step by hand.
    #[test]
    fn panel_height_is_the_sum_of_its_sections() {
        let sections = [
            Section::LivePanel(rows(3)),
            Section::Gap,
            Section::Status(Line::raw("")),
            Section::Input(rows(2)),
            Section::ContextMeter {
                used: 0,
                window: 1,
                estimated: false,
            },
            Section::Hint(Line::raw("")),
        ];
        let total: u16 = sections.iter().map(Section::height).sum();
        assert_eq!(total, 5 + 1 + 1 + 4 + 1 + 1);
    }

    /// However long the prompt, the input box shows at most six rows, and it
    /// never collapses to nothing when the draft is empty.
    #[test]
    fn the_input_box_stays_between_one_and_six_rows() {
        assert_eq!(Section::Input(rows(0)).height(), 3);
        assert_eq!(Section::Input(rows(1)).height(), 3);
        assert_eq!(Section::Input(rows(20)).height(), 8);
    }

    /// A panel that wants the whole screen still leaves the transcript
    /// readable behind it.
    #[test]
    fn a_tall_panel_still_leaves_the_transcript_visible() {
        let geometry = PanelGeometry::new(100, 20);
        assert_eq!(geometry.height, 16);
        assert_eq!(geometry.top, 4);
    }

    /// Border rows are not content. A click on the box edge must not be
    /// reported as the first or last row inside it.
    #[test]
    fn content_rows_exclude_the_panel_border() {
        let geometry = PanelGeometry::new(6, 20);
        assert_eq!(geometry.top, 14);
        assert_eq!(geometry.content_row(14), None, "top border");
        assert_eq!(geometry.content_row(15), Some(0));
        assert_eq!(geometry.content_row(18), Some(3));
        assert_eq!(geometry.content_row(19), None, "bottom border");
        assert_eq!(geometry.content_row(2), None, "transcript above the panel");
    }

    #[test]
    fn trace_navigation_remains_visible_without_live_agent_rows() {
        let lines = trace_navigation_lines(Vec::new());
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "  esc to return to Main agent"
        );
        assert_eq!(lines[0].style, theme::dim());
    }

    #[test]
    fn jump_control_centers_the_unseen_message_count_and_preserves_shortcut() {
        let (rect, line) = jump_to_bottom_control(Rect::new(0, 9, 80, 1), 1, false);
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text, " 1 new message  (ctrl+End)  ↓ ");
        assert_eq!(rect.x, (80 - text.width() as u16) / 2);
        assert_eq!(rect.width, text.width() as u16);
    }

    #[test]
    fn jump_control_falls_back_without_wrapping_on_narrow_terminal() {
        let (rect, line) = jump_to_bottom_control(Rect::new(0, 0, 4, 1), 0, false);
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "↓");
        assert_eq!(rect, Rect::new(1, 0, 1, 1));
    }
}
