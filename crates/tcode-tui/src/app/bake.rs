//! Baking a tool call into the transcript record.
//!
//! The live path, ledger replay and the approval dialog all go through the
//! same three entry points here — `bake_call_start`, `batch_header_lines` +
//! `batch_item_lines`, and `bake_call_result`. Writing a second formatter for
//! any one of them is how replay lost batch grouping and inter-call spacing
//! in the past.
//!
//! Matching on a tool *name* belongs in `RenderRegistry::from_tools`, never
//! here: this module asks the registry for behaviour and renders the answer.
//!
//! Touches: transcript, renderers, md, cwd, pending_tool, pending_batch,
//! change_prebake, task_runs.

use super::*;

pub(super) struct PendingCall {
    /// Provider-issued id retained until the matching result arrives. Task
    /// trace metadata uses this as the parent-call key.
    pub(super) call_id: String,
    pub(super) detail: String,
    /// Batch items defer their indented summary row (plus any diff) so
    /// `ToolEnd` can bake it directly above this call's own result instead of
    /// baking every item first and every result after. Empty for single calls
    /// (their header is baked at `ToolStart`).
    pub(super) header: Vec<Line<'static>>,
    /// A single bare call's already-baked `●` header block: its result
    /// attaches to that very row at `ToolEnd` instead of opening a
    /// separate `⎿` row. None for batch items and body-carrying calls.
    pub(super) header_index: Option<usize>,
}

/// Where a result's call record lives, shared by live `ToolEnd` and
/// replay. The three cases are mutually exclusive by construction.
pub(super) enum CallRecord {
    /// A batch item: its deferred indented summary lines bake with the result.
    Batch(Vec<Line<'static>>),
    /// A bare single call: its `●` header block is already in the
    /// transcript and the result attaches to that very row.
    HeaderBlock(usize),
    /// Header (plus diff/command body) fully baked; the result stands alone.
    Baked,
}

/// One tool result's rendering, shared by live `ToolEnd` and replay.
pub(super) enum ResultRender {
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

pub(super) enum ResultDetail {
    Lines(Vec<Line<'static>>),
    Markdown(markdown::Document),
}

impl ResultDetail {
    pub(super) fn is_empty(&self) -> bool {
        match self {
            Self::Lines(lines) => lines.is_empty(),
            Self::Markdown(document) => document.is_empty(),
        }
    }
}

/// A result preview riding on its call's own row: ` — preview`, dim on
/// success, red on failure. One format for batch rows and single calls.
pub(super) fn preview_tail(preview: &str, style: ratatui::style::Style) -> Vec<Span<'static>> {
    if preview.is_empty() {
        return Vec::new();
    }
    vec![Span::styled(format!(" — {preview}"), style)]
}

pub(super) fn append_result_preview(
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
pub(super) fn colored_batch_label(label: &str) -> Vec<Span<'static>> {
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

pub(super) fn task_summary_detail(summary: &str) -> Vec<Line<'static>> {
    summary
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| Line::styled(format!("  │ {line}"), theme::dim()))
        .collect()
}

pub(super) fn task_live_detail(summary: &str, steps: &[String]) -> Vec<Line<'static>> {
    let mut lines = task_summary_detail(summary);
    lines.extend(
        steps
            .iter()
            .map(|step| Line::styled(format!("  │ └ {step}"), theme::dim())),
    );
    lines
}

/// The activity is already self-describing ("thinking…", "starting…"), so
/// task cards keep it as a quiet indented status rather than competing with
/// the main window's animated running indicator.
pub(super) fn task_plain_status(activity: &str) -> Vec<Line<'static>> {
    vec![Line::from(vec![Span::styled(
        format!("{TASK_STATUS_INDENT}{activity}"),
        theme::dim(),
    )])]
}

pub(super) fn task_header_summary(summary: &str) -> Vec<Span<'static>> {
    match summary.split_once(" · ") {
        Some((kind, objective)) => vec![
            Span::styled(kind.to_string(), theme::ok()),
            Span::styled(" · ", theme::dim()),
            Span::styled(objective.to_string(), ratatui::style::Style::default()),
        ],
        None => vec![Span::styled(summary.to_string(), theme::ok())],
    }
}

/// Remove task-run accounting from the user-facing report. The complete,
/// unmodified tool result remains in the ledger for the parent agent.
pub(super) fn task_result_text(content: &str) -> String {
    let mut lines = content.lines();
    let first = lines.next().unwrap_or_default();
    let body = if first.starts_with('[') && first.contains("sub-agent on") {
        lines.collect::<Vec<_>>().join("\n")
    } else {
        content.to_string()
    };
    body.trim().to_string()
}

pub(super) fn result_preview(s: &str) -> String {
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

/// A human turn renders as a quoted block: an accent rail down the left, the
/// text otherwise unadorned. A note stays under the same rail — it is the
/// human speaking too — and is told apart by a coloured `Note:` opening its
/// first row. Live echo, ledger replay and approval notes all bake through
/// here, so the three paths cannot drift apart.
/// A multi-line prompt collapsed to one dim row: the queue is a status display,
/// not a second transcript. The real message is rendered in full once it is
/// delivered.
pub(super) fn one_line(text: &str, budget: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(budget) {
        Some((cut, _)) => format!("{}…", &flat[..cut]),
        None => flat,
    }
}

/// Skill descriptions are free text from a SKILL.md front matter, unbounded
/// unlike the `&'static str` help strings hand-written for registry commands.
pub(super) fn clip_description(text: &str, cap: usize) -> String {
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(cap).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

pub(super) fn quote_lines(label: Option<&str>, text: &str) -> Vec<Line<'static>> {
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
            spans.extend(crate::reference_style::user_text_spans(row));
            Line::from(spans)
        })
        .collect()
}

/// Attachments hang off the message they arrived with, under the same rail.
pub(super) fn quote_attachment_line(label: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(theme::USER_GUTTER, theme::user_gutter()),
        Span::styled(format!("⌞ {label}"), theme::dim()),
    ])
}

impl App {
    /// Tool's UI name, resolved from its own `display_name` when it belongs
    /// to this session; falls back to title-case for imported/unknown tools.
    pub(super) fn display_name(&self, name: &str) -> String {
        self.renderers.display_name(name)
    }

    /// Split a tool summary like `shell(cargo test)` into colored spans: the
    /// tool's display name is green, the arguments are dim.
    pub(super) fn colored_tool_summary(&self, summary: &str) -> Vec<Span<'static>> {
        match summary.find('(') {
            Some(paren) => vec![
                Span::styled(self.display_name(&summary[..paren]), theme::ok()),
                Span::styled(summary[paren..].to_string(), theme::dim()),
            ],
            None => vec![Span::styled(self.display_name(summary), theme::ok())],
        }
    }

    /// Task calls foreground the parent-authored objective, not the generic
    /// delegation verb. Every other call keeps the usual verb/argument split.
    pub(super) fn colored_call_summary(&self, name: &str, summary: &str) -> Vec<Span<'static>> {
        match self.renderers.get(name).header_tone() {
            HeaderTone::Tool => self.colored_tool_summary(summary),
            HeaderTone::Task => task_header_summary(summary),
        }
    }

    /// `●` header (+ change body + command block) for one tool call. Shared
    /// by the live `ToolStart` path and transcript replay so they can never
    /// drift apart.
    pub(super) fn call_lines(&self, name: &str, input: &serde_json::Value) -> Vec<Line<'static>> {
        let renderer = self.renderers.get(name);
        let summary = self.display_summary(&renderer.header(name, input, Some(&self.cwd)));
        let mut spans: Vec<Span> = self.colored_call_summary(name, &summary);
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
    pub(super) fn bake_call_start(
        &mut self,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<usize> {
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
    pub(super) fn batch_header_lines(&self, label: &str) -> Vec<Line<'static>> {
        let mut spans = vec![Span::styled("● ", theme::accent())];
        spans.extend(colored_batch_label(label));
        vec![Line::default(), Line::from(spans)]
    }

    /// One batch item's indented summary row (plus any diff). It is baked at
    /// the item's own result so each call stays immediately above its output.
    /// Change bodies retain their trailing separator so adjacent diffs never
    /// visually merge.
    pub(super) fn batch_item_lines(
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
            batch_item_style(renderer.header_tone()),
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
    pub(super) fn bake_call_result(
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
                // Quiet tools (read/grep/glob) and shell commands skip the
                // preview: their fold affordance already exposes the complete
                // result on demand, without competing with the call summary.
                let renderer = self.renderers.get(name);
                let hide_preview = renderer.quiet_output()
                    || input.is_some_and(|input| renderer.folds_result(input));
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

    pub(super) fn attach_result_detail(
        &mut self,
        index: usize,
        detail: ResultDetail,
        append: bool,
    ) {
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

    pub(super) fn push_result_detail(&mut self, head: Vec<Line<'static>>, detail: ResultDetail) {
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
    pub(super) fn result_render(
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
    pub(super) fn output_detail(
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
        if !is_error {
            if let Some(lines) = input.and_then(|input| renderer.syntax_detail(input, content)) {
                return ResultDetail::Lines(lines);
            }
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

    /// Tool inputs are canonical absolute paths, but repeating the current
    /// project root adds noise without adding information in the TUI.
    pub(super) fn display_summary(&self, summary: &str) -> String {
        shorten_summary_path(summary, Some(&self.cwd))
    }
}
