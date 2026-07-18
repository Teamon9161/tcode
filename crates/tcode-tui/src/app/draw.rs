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
        let status = self.status_line(running, started);
        let hint = self.idle_hint();
        let status_model_label = self.agent.model.snapshot().describe();
        let viewing_trace = !matches!(self.active_view, ViewId::Main);
        let dialog_lines = self
            .overlay
            .as_ref()
            .map(|overlay| overlay.render(&self.overlay_ctx()));
        // A focused note in the approval dialog exposes its caret cell (set by
        // the `render` just above). Anchoring the real terminal cursor there
        // keeps the OS IME composition window tracking the caret. Pickers have
        // no caret, so `cursor_cell` is `None` for them by construction.
        let dialog_cursor = self.overlay.as_ref().and_then(Overlay::cursor_cell);
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
        let (panel_lines, panel_targets) = self.live_panel_lines();
        let queued_lines = if viewing_trace {
            Vec::new()
        } else {
            self.queued_lines(area_width(&self.terminal))
        };

        use ratatui::widgets::{Block, BorderType, Clear};

        // The input box geometry is only known during layout; capture it so
        // mouse hit-testing (selection/copy in the prompt) can map screen
        // coordinates back to editor positions. None when a dialog/picker
        // replaces the input box.
        let mut captured_input: Option<InputHitbox> = None;
        let mut captured_status: Option<StatusHitboxes> = None;
        let mut captured_panel: Option<Rect> = None;
        let animation_frame = self.anim_frame;
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

            // ------- bottom panel height (transcript gets the rest) -------
            let editor_start = editor.cursor_row.saturating_sub(5);
            let editor_h = ghost_lines
                .as_ref()
                .map(|lines| lines.len().min(6))
                .unwrap_or_else(|| editor.lines.len() - editor_start)
                .clamp(1, 6) as u16;
            let panel_h = if let Some(lines) = &dialog_lines {
                lines.len() as u16 + 2
            } else if viewing_trace {
                (!panel_lines.is_empty() as u16) * (panel_lines.len() as u16 + 2)
            } else {
                let mut h = editor_h + 2 + 2; // input box + context meter + hint
                if running {
                    // Leave a breathing row after the transcript before the
                    // live status line, rather than pinning "responding" to
                    // the last rendered transcript row.
                    h += 2; // separator + spinner/status line above the input box
                }
                h += queued_lines.len() as u16;
                if !panel_lines.is_empty() {
                    h += panel_lines.len() as u16 + 2;
                }
                if self.meter.rate_limits.is_some() {
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

            if viewing_trace {
                if !panel_lines.is_empty() {
                    let h = panel_lines.len() as u16 + 2;
                    let rect = row(y, h);
                    frame.render_widget(
                        Paragraph::new(Text::from(panel_lines)).block(
                            Block::bordered()
                                .border_type(BorderType::Rounded)
                                .border_style(theme::border()),
                        ),
                        rect,
                    );
                    captured_panel = Some(rect);
                }
                return;
            }

            if !panel_lines.is_empty() {
                let h = panel_lines.len() as u16 + 2;
                let rect = row(y, h);
                frame.render_widget(
                    Paragraph::new(Text::from(panel_lines)).block(
                        Block::bordered()
                            .border_type(BorderType::Rounded)
                            .border_style(theme::border()),
                    ),
                    rect,
                );
                captured_panel = Some(rect);
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
                    self.meter.context_tokens,
                    self.agent.model.snapshot().context_window,
                    area.width,
                    self.meter.context_estimated,
                )),
                row(y, 1),
            );
            y += 1;

            if let Some(limits) = self.meter.rate_limits {
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

            if self.rewind_nav.is_none() {
                captured_status = Some(status_hitboxes(
                    row(y, 1),
                    &self.mode_label,
                    &status_model_label,
                ));
            }
            frame.render_widget(Paragraph::new(hint), row(y, 1));
        })?;
        self.input_hitbox = captured_input;
        self.status_hitboxes = captured_status;
        self.live_panel_hitbox = captured_panel.map(|rect| PanelHitbox {
            rect,
            targets: panel_targets,
        });
        Ok(())
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
        let scrolled = if self.transcript.is_following() {
            ""
        } else {
            " · ↑ viewing history"
        };
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
        spans.push(Span::styled(scrolled, theme::dim()));
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
