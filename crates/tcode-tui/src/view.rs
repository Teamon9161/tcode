//! Reusable transcript view for a conversation-shaped event stream.
//!
//! The main application still owns turn state, permissions and the editor. A
//! `SessionView` owns only the state necessary to turn ordered agent events or
//! ledger entries into a `Transcript`. Task traces use the same bake methods for
//! their live event stream and their JSONL replay, which is the seam future
//! parallel sessions reuse too.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use tcode_core::{AgentEvent, ContentBlock, Entry};

use crate::live_panel::UiTaskRun;
use crate::markdown;
use crate::render::{
    batch_item_style, shorten_summary_path, CallRoute, HeaderTone, RenderRegistry,
};
use crate::theme;
use crate::transcript::Transcript;

const OUTPUT_VIEW_ROWS: usize = 12;
const BATCH_ITEM_INDENT: &str = "    ";

pub struct BakeCtx<'a> {
    pub renderers: &'a RenderRegistry,
    pub markdown: &'a mut markdown::Renderer,
    pub cwd: &'a Path,
    pub show_reasoning: bool,
}

/// A transcript plus the transient state that determines how the next event
/// attaches to it. There is deliberately no provider ledger here.
pub struct SessionView {
    pub transcript: Transcript,
    pending_tool: Option<PendingCall>,
    pending_batch: VecDeque<PendingCall>,
    /// Runs this view's agent delegated, by run id. A trace view has them for
    /// the same reason the main conversation does: a delegation is a call whose
    /// progress is worth watching, and the alternative — dropping the events —
    /// left a nested sub-agent invisible until its parent call ended.
    task_cards: HashMap<String, UiTaskRun>,
    live_text: String,
    space_before_response: bool,
    live_block: Option<usize>,
    thinking_chars: usize,
    thinking_text: String,
    thinking_started: bool,
}

#[derive(Clone)]
struct PendingCall {
    call_id: String,
    input: serde_json::Value,
    header: Vec<Line<'static>>,
    header_index: Option<usize>,
}

struct ReplayCall {
    name: String,
    input: serde_json::Value,
    batch: Option<ReplayBatch>,
}

struct ReplayBatch {
    header: Option<String>,
    mixed: bool,
}

enum CallRecord {
    Batch(Vec<Line<'static>>),
    HeaderBlock(usize),
    Baked,
}

struct ToolResult<'a> {
    name: &'a str,
    input: Option<&'a serde_json::Value>,
    preview: &'a str,
    content: &'a str,
    is_error: bool,
}

enum ResultRender {
    Nothing,
    Inline(Line<'static>),
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

impl SessionView {
    pub fn new(width: u16) -> Self {
        Self {
            transcript: Transcript::new(width),
            pending_tool: None,
            pending_batch: VecDeque::new(),
            task_cards: HashMap::new(),
            live_text: String::new(),
            space_before_response: false,
            live_block: None,
            thinking_chars: 0,
            thinking_text: String::new(),
            thinking_started: false,
        }
    }

    pub fn bake(&mut self, lines: Vec<Line<'static>>) {
        self.transcript.push(lines);
    }

    pub fn finish(&mut self, ctx: &mut BakeCtx<'_>) {
        self.bake_live_text(ctx);
        self.finish_thinking(ctx);
    }

    /// Bake the transcript-visible portion of an event. Meter/status concerns
    /// stay in `App`; this method is intentionally usable for a trace view.
    pub fn feed_event(&mut self, ev: &AgentEvent, ctx: &mut BakeCtx<'_>) {
        match ev {
            AgentEvent::TextDelta(text) => {
                self.finish_thinking(ctx);
                if self.space_before_response {
                    self.bake(vec![Line::default()]);
                    self.space_before_response = false;
                }
                self.live_text.push_str(text);
                self.refresh_live_text(ctx);
            }
            AgentEvent::ThinkingDelta(text) => {
                self.thinking_started = true;
                self.thinking_chars += text.chars().count();
                self.thinking_text.push_str(text);
            }
            AgentEvent::ToolBatchStart { label, calls } => {
                self.space_before_response = false;
                self.bake_live_text(ctx);
                self.finish_thinking(ctx);
                self.bake(self.batch_header_lines(label));
                self.pending_batch.clear();
                let mixed = calls
                    .iter()
                    .map(|(_, name, _)| name.as_str())
                    .collect::<HashSet<_>>()
                    .len()
                    > 1;
                for (call_id, name, input) in calls {
                    self.pending_batch.push_back(PendingCall {
                        call_id: call_id.clone(),
                        input: input.clone(),
                        header: self.batch_item_lines(name, input, mixed, ctx),
                        header_index: None,
                    });
                }
            }
            AgentEvent::ToolStart {
                call_id,
                name,
                input,
                ..
            } => {
                if !matches!(ctx.renderers.get(name).route(), CallRoute::Transcript) {
                    return;
                }
                self.space_before_response = false;
                self.bake_live_text(ctx);
                self.finish_thinking(ctx);
                if !self.pending_batch.is_empty() {
                    return;
                }
                let header_index = self.bake_call_start(name, input, ctx);
                self.pending_tool = Some(PendingCall {
                    call_id: call_id.clone(),
                    input: input.clone(),
                    header: Vec::new(),
                    header_index,
                });
            }
            AgentEvent::ToolEnd {
                call_id,
                name,
                preview,
                content,
                is_error,
            } => {
                if !matches!(ctx.renderers.get(name).route(), CallRoute::Transcript) {
                    return;
                }
                let entry = self
                    .pending_tool
                    .take()
                    .filter(|call| call.call_id == *call_id)
                    .or_else(|| {
                        let pos = self
                            .pending_batch
                            .iter()
                            .position(|call| call.call_id == *call_id)?;
                        self.pending_batch.remove(pos)
                    });
                let (input, record) = match entry {
                    Some(entry) => {
                        let record = match entry.header_index {
                            Some(index) => CallRecord::HeaderBlock(index),
                            None if !entry.header.is_empty() => CallRecord::Batch(entry.header),
                            None => CallRecord::Baked,
                        };
                        (Some(entry.input), record)
                    }
                    None => (None, CallRecord::Baked),
                };
                self.bake_call_result(
                    ToolResult {
                        name,
                        input: input.as_ref(),
                        preview,
                        content,
                        is_error: *is_error,
                    },
                    record,
                    ctx,
                );
                self.space_before_response = true;
            }
            AgentEvent::Retrying {
                attempt,
                max,
                error,
                partial_output_retained,
                ..
            } => {
                self.bake_live_text(ctx);
                self.finish_thinking(ctx);
                let retained = if *partial_output_retained {
                    " — incomplete response retained; not sent back to model"
                } else {
                    ""
                };
                self.bake(vec![Line::styled(
                    format!("↻ API error ({attempt}/{max}): {error}{retained}"),
                    theme::error_highlight(),
                )]);
            }
            AgentEvent::Compacted(summary) => self.bake_compacted(summary, ctx),
            AgentEvent::AutoClassifierUnavailable(reason) => {
                self.finish(ctx);
                self.bake(vec![Line::styled(
                    format!("⊙ Auto classifier unavailable; asking you instead: {reason}"),
                    ratatui::style::Style::default().fg(theme::WARN),
                )]);
            }
            AgentEvent::TaskRunStarted {
                run,
                parent_call,
                kind,
                model,
                prompt,
                summary,
                ..
            } => {
                let block = self.begin_task_card(run, parent_call, summary);
                let card = UiTaskRun::new(
                    run.clone(),
                    parent_call.clone(),
                    kind.clone(),
                    model.clone(),
                    prompt.clone(),
                    summary.clone(),
                    block,
                );
                self.task_cards.insert(run.clone(), card);
            }
            // One level only: an event from deeper down belongs to a card in
            // *that* run's own trace, which renders it through this same arm.
            AgentEvent::TaskRunEvent { run, event } => {
                let Some(card) = self.task_cards.get_mut(run) else {
                    return;
                };
                card.note_event(event, ctx.renderers, ctx.cwd);
                let Some(block) = card.block else {
                    return;
                };
                let detail = task_live_detail(&card.summary, &card.steps);
                let status = task_status_lines(card, ctx.cwd);
                self.transcript
                    .replace_detail_preserving_open(block, detail, OUTPUT_VIEW_ROWS);
                self.transcript.set_live_status(block, Some(status));
            }
            AgentEvent::TaskRunFinished {
                run,
                status,
                tool_calls,
                usage,
            } => {
                let Some(card) = self.task_cards.get_mut(run) else {
                    return;
                };
                card.status = *status;
                card.tools = *tool_calls;
                card.usage = *usage;
                if let Some(block) = card.block {
                    self.transcript.set_live_status(block, None);
                }
            }
            AgentEvent::Interrupted | AgentEvent::TurnEnd => self.finish(ctx),
            _ => {}
        }
    }

    /// Hang a delegated run's card off the call that spawned it. A parallel
    /// batch item normally waits for its result before baking; a live card
    /// needs its header row now, so that one item bakes early and keeps its
    /// index for the eventual report.
    fn begin_task_card(&mut self, run: &str, parent_call: &str, summary: &str) -> Option<usize> {
        let block = match self
            .pending_tool
            .as_ref()
            .filter(|call| call.call_id == parent_call)
            .map(|call| call.header_index)
        {
            Some(index) => index,
            None => {
                let position = self
                    .pending_batch
                    .iter()
                    .position(|call| call.call_id == parent_call)?;
                let mut call = self.pending_batch.remove(position)?;
                let index = self.transcript.block_count();
                self.bake(std::mem::take(&mut call.header));
                call.header_index = Some(index);
                self.pending_batch.insert(position, call);
                Some(index)
            }
        }?;
        self.transcript.link_task_run(block, run.to_string());
        self.transcript
            .attach_detail(block, task_summary_detail(summary), OUTPUT_VIEW_ROWS);
        self.transcript
            .set_live_status(block, Some(task_plain_status("starting…")));
        Some(block)
    }

    /// Replay an ordinary recorded conversation verbatim. This convenience
    /// wrapper exists solely to assert parity with live rendering; runtime
    /// task traces use `replay_task_ledger` below.
    #[cfg(test)]
    pub fn replay_ledger(
        &mut self,
        entries: &[Entry],
        batch_labels: &[(usize, String)],
        ctx: &mut BakeCtx<'_>,
    ) {
        self.replay_ledger_inner(entries, batch_labels, false, ctx);
    }

    /// Task traces render their spawning prompt in a richer task header. Skip
    /// just the matching first user entry from the ledger so that prompt is not
    /// duplicated; all later user/note entries still replay normally.
    pub fn replay_task_ledger(
        &mut self,
        entries: &[Entry],
        batch_labels: &[(usize, String)],
        ctx: &mut BakeCtx<'_>,
    ) {
        self.replay_ledger_inner(entries, batch_labels, true, ctx);
    }

    fn replay_ledger_inner(
        &mut self,
        entries: &[Entry],
        batch_labels: &[(usize, String)],
        mut skip_initial_user: bool,
        ctx: &mut BakeCtx<'_>,
    ) {
        let mut lines = Vec::new();
        let mut calls: HashMap<String, ReplayCall> = HashMap::new();
        let mut space_before_text = false;
        for (entry_index, entry) in entries.iter().enumerate() {
            match entry {
                Entry::User(blocks) => {
                    if skip_initial_user {
                        skip_initial_user = false;
                        continue;
                    }
                    space_before_text = false;
                    self.transcript.push(std::mem::take(&mut lines));
                    let mut echo = vec![Line::default()];
                    for block in blocks {
                        match block {
                            ContentBlock::Text { text } if !text.starts_with("<tcode-status>") => {
                                match tcode_tools::parse_skill_echo(text) {
                                    Some(skill_echo) => echo.extend(skill_echo_lines(&skill_echo)),
                                    None => echo.extend(quote_lines(text)),
                                }
                            }
                            ContentBlock::Image { .. } => echo.push(attachment_line("[image]")),
                            _ => {}
                        }
                    }
                    echo.push(Line::default());
                    self.transcript.push_tagged(echo, entry_index);
                }
                Entry::Assistant(blocks) => {
                    let mut group = Vec::new();
                    for block in blocks {
                        match block {
                            ContentBlock::Thinking { thinking, .. } => {
                                self.transcript.push(std::mem::take(&mut lines));
                                self.bake_thinking(
                                    &format!("reasoning (~{} tok)", thinking.chars().count() / 3),
                                    thinking,
                                    ctx,
                                );
                            }
                            ContentBlock::Text { text } => {
                                if space_before_text {
                                    self.transcript.push(std::mem::take(&mut lines));
                                    self.bake(vec![Line::default()]);
                                    space_before_text = false;
                                }
                                self.transcript.push(std::mem::take(&mut lines));
                                self.transcript
                                    .push_markdown(ctx.markdown.parse(text).with_trailing_blank());
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                group.push((id.clone(), name.clone(), input.clone()));
                            }
                            _ => {}
                        }
                    }
                    let label = batch_labels
                        .iter()
                        .find(|(after, _)| *after == entry_index + 1)
                        .map(|(_, label)| label.clone());
                    let mixed = group
                        .iter()
                        .map(|(_, name, _)| name.as_str())
                        .collect::<HashSet<_>>()
                        .len()
                        > 1;
                    for (index, (id, name, input)) in group.into_iter().enumerate() {
                        calls.insert(
                            id.clone(),
                            ReplayCall {
                                name,
                                input,
                                batch: label.as_ref().map(|label| ReplayBatch {
                                    header: (index == 0).then(|| label.clone()),
                                    mixed,
                                }),
                            },
                        );
                    }
                }
                Entry::IncompleteAssistant { text, error } => {
                    self.transcript.push(std::mem::take(&mut lines));
                    self.transcript
                        .push_markdown(ctx.markdown.parse(text).with_trailing_blank());
                    lines.push(Line::styled(format!("↻ stream failed: {error} — incomplete response retained; not sent back to model"), theme::error_highlight()));
                    lines.push(Line::default());
                }
                Entry::Summary(summary) => {
                    self.transcript.push(std::mem::take(&mut lines));
                    self.bake_compacted(summary, ctx);
                }
                Entry::ToolResults(blocks) => {
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
                        let name = call.map(|call| call.name.as_str()).unwrap_or("tool");
                        if !matches!(ctx.renderers.get(name).route(), CallRoute::Transcript) {
                            continue;
                        }
                        self.transcript.push(std::mem::take(&mut lines));
                        let record = match call {
                            Some(call) => match &call.batch {
                                Some(batch) => {
                                    if let Some(label) = &batch.header {
                                        self.bake(self.batch_header_lines(label));
                                    }
                                    CallRecord::Batch(self.batch_item_lines(
                                        name,
                                        &call.input,
                                        batch.mixed,
                                        ctx,
                                    ))
                                }
                                None => self
                                    .bake_call_start(name, &call.input, ctx)
                                    .map_or(CallRecord::Baked, CallRecord::HeaderBlock),
                            },
                            None => CallRecord::Baked,
                        };
                        let preview = result_preview(content);
                        self.bake_call_result(
                            ToolResult {
                                name,
                                input: call.map(|call| &call.input),
                                preview: &preview,
                                content,
                                is_error: *is_error,
                            },
                            record,
                            ctx,
                        );
                        space_before_text = true;
                    }
                }
                Entry::UserNote { text, .. } => {
                    self.bake(vec![
                        Line::default(),
                        Line::styled(format!("  │ Note: {text}"), theme::dim()),
                        Line::default(),
                    ]);
                }
                Entry::Note(text) => {
                    lines.push(Line::default());
                    lines.extend(quote_lines(text));
                    lines.push(Line::default());
                }
                Entry::Instruction(_) | Entry::ImportedTool { .. } => {}
            }
        }
        self.bake(lines);
    }

    fn finish_thinking(&mut self, ctx: &mut BakeCtx<'_>) {
        if !self.thinking_started {
            return;
        }
        self.thinking_started = false;
        let text = std::mem::take(&mut self.thinking_text);
        self.bake_thinking(
            &format!("reasoning (~{} tok)", self.thinking_chars / 3),
            &text,
            ctx,
        );
        self.thinking_chars = 0;
    }

    fn bake_thinking(&mut self, title: &str, text: &str, ctx: &mut BakeCtx<'_>) {
        if !ctx.show_reasoning || text.trim().is_empty() {
            return;
        }
        let detail = text
            .lines()
            .map(|line| Line::raw(line.to_string()))
            .collect();
        self.transcript.push_with_detail(
            vec![Line::styled(format!("✻ {title}"), theme::dim())],
            detail,
            false,
            OUTPUT_VIEW_ROWS,
        );
    }

    fn bake_compacted(&mut self, summary: &str, ctx: &mut BakeCtx<'_>) {
        self.transcript.push_with_markdown_detail(
            vec![
                Line::default(),
                Line::styled("── earlier conversation compacted ──", theme::dim()),
            ],
            ctx.markdown.parse(summary),
            Vec::new(),
            false,
            OUTPUT_VIEW_ROWS,
        );
        self.bake(vec![Line::default()]);
    }

    fn refresh_live_text(&mut self, ctx: &mut BakeCtx<'_>) {
        if self.live_text.trim().is_empty() {
            return;
        }
        let document = ctx.markdown.parse(&self.live_text);
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

    fn bake_live_text(&mut self, ctx: &mut BakeCtx<'_>) {
        if self.live_text.trim().is_empty() {
            self.live_text.clear();
            return;
        }
        let document = ctx
            .markdown
            .parse(&std::mem::take(&mut self.live_text))
            .with_trailing_blank();
        if let Some(index) = self.live_block.take() {
            self.transcript.replace_markdown_block(index, document);
        } else {
            self.transcript.push_markdown(document);
        }
    }

    fn call_lines(
        &self,
        name: &str,
        input: &serde_json::Value,
        ctx: &BakeCtx<'_>,
    ) -> Vec<Line<'static>> {
        let renderer = ctx.renderers.get(name);
        let summary =
            shorten_summary_path(&renderer.header(name, input, Some(ctx.cwd)), Some(ctx.cwd));
        let mut spans = colored_call_summary(ctx.renderers, renderer.header_tone(), &summary);
        spans.insert(0, Span::styled("● ", theme::accent()));
        let mut lines = vec![Line::from(spans)];
        lines.extend(renderer.body(input));
        lines
    }

    fn bake_call_start(
        &mut self,
        name: &str,
        input: &serde_json::Value,
        ctx: &mut BakeCtx<'_>,
    ) -> Option<usize> {
        let mut lines = self.call_lines(name, input, ctx);
        self.bake(vec![Line::default()]);
        if lines.len() == 1 {
            let index = self.transcript.block_count();
            self.bake(lines);
            let detail = ctx.renderers.get(name).initial_detail(input);
            if !detail.is_empty() {
                self.transcript
                    .attach_detail(index, detail, OUTPUT_VIEW_ROWS);
            }
            Some(index)
        } else {
            lines.push(Line::default());
            self.bake(lines);
            None
        }
    }

    fn batch_header_lines(&self, label: &str) -> Vec<Line<'static>> {
        let mut spans = vec![Span::styled("● ", theme::accent())];
        spans.extend(colored_batch_label(label));
        vec![Line::default(), Line::from(spans)]
    }

    fn batch_item_lines(
        &self,
        name: &str,
        input: &serde_json::Value,
        mixed: bool,
        ctx: &BakeCtx<'_>,
    ) -> Vec<Line<'static>> {
        let renderer = ctx.renderers.get(name);
        let mut row = vec![Span::styled(BATCH_ITEM_INDENT, theme::dim())];
        if mixed {
            row.push(Span::styled(format!("{name} "), theme::dim()));
        }
        row.push(Span::styled(
            renderer.batch_item(name, input, Some(ctx.cwd)),
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

    fn bake_call_result(
        &mut self,
        result: ToolResult<'_>,
        record: CallRecord,
        ctx: &mut BakeCtx<'_>,
    ) {
        let style = if result.is_error {
            Style::default().fg(theme::ERROR)
        } else {
            theme::dim()
        };
        match self.result_render(&result, ctx) {
            ResultRender::Nothing => {
                if let CallRecord::Batch(header) = record {
                    self.bake(header);
                }
            }
            ResultRender::Inline(line) => match record {
                CallRecord::HeaderBlock(index) => self
                    .transcript
                    .extend_head(index, preview_tail(result.preview, style)),
                CallRecord::Batch(mut header) => {
                    append_result_preview(&mut header, result.preview, style);
                    self.bake(header);
                }
                CallRecord::Baked => self.bake(vec![line]),
            },
            ResultRender::Foldable { head, detail } => {
                let renderer = ctx.renderers.get(result.name);
                let hide_preview = renderer.quiet_output()
                    || result
                        .input
                        .is_some_and(|input| renderer.folds_result(input));
                let label = if result.is_error {
                    renderer.error_label().unwrap_or(result.preview)
                } else {
                    result.preview
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
                            result
                                .input
                                .is_some_and(|input| renderer.folds_result(input)),
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

    fn result_render(&self, result: &ToolResult<'_>, ctx: &mut BakeCtx<'_>) -> ResultRender {
        let renderer = ctx.renderers.get(result.name);
        if !result.is_error && renderer.hide_success_result() {
            return ResultRender::Nothing;
        }
        let style = if result.is_error {
            Style::default().fg(theme::ERROR)
        } else {
            theme::dim()
        };
        if result.is_error {
            if let Some(label) = renderer.error_label() {
                let diagnostic = if result.content.trim().is_empty() {
                    result.preview
                } else {
                    result.content
                };
                return ResultRender::Foldable {
                    head: vec![Line::from(Span::styled(format!("  ⎿ {label}"), style))],
                    detail: self.output_detail(result, diagnostic, false, ctx),
                };
            }
        }
        let folded = result
            .input
            .is_some_and(|input| renderer.folds_result(input));
        let detail = self.output_detail(result, result.content, !folded, ctx);
        if detail.is_empty() {
            ResultRender::Inline(Line::from(Span::styled(
                format!("  ⎿ {}", result.preview),
                style,
            )))
        } else {
            let head = if !result.is_error && (renderer.quiet_output() || folded) {
                Line::from(Span::styled("  ⎿", style))
            } else {
                Line::from(Span::styled(format!("  ⎿ {}", result.preview), style))
            };
            ResultRender::Foldable {
                head: vec![head],
                detail,
            }
        }
    }

    fn output_detail(
        &self,
        result: &ToolResult<'_>,
        content: &str,
        preview_visible: bool,
        ctx: &mut BakeCtx<'_>,
    ) -> ResultDetail {
        let renderer = ctx.renderers.get(result.name);
        let quiet = renderer.quiet_output();
        if preview_visible && content.trim() == result.preview.trim() && (result.is_error || !quiet)
        {
            return ResultDetail::Lines(Vec::new());
        }
        if !result.is_error && renderer.markdown_detail(result.input) {
            return ResultDetail::Markdown(ctx.markdown.parse(content));
        }
        if !result.is_error {
            if let Some(lines) = result
                .input
                .and_then(|input| renderer.syntax_detail(input, content))
            {
                return ResultDetail::Lines(lines);
            }
        }
        let text_style = if result.is_error {
            Style::default().fg(theme::ERROR)
        } else {
            Style::default()
        };
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
                .map(|line| {
                    Line::from(vec![
                        Span::styled("  │ ", theme::dim()),
                        Span::styled(line.to_string(), text_style),
                    ])
                })
                .collect(),
        )
    }
}

fn colored_call_summary(
    renderers: &RenderRegistry,
    tone: HeaderTone,
    summary: &str,
) -> Vec<Span<'static>> {
    match tone {
        HeaderTone::Tool => colored_tool_summary(renderers, summary),
        HeaderTone::Task => task_header_summary(summary),
    }
}

fn colored_tool_summary(renderers: &RenderRegistry, summary: &str) -> Vec<Span<'static>> {
    match summary.find('(') {
        Some(paren) => vec![
            Span::styled(renderers.display_name(&summary[..paren]), theme::ok()),
            Span::styled(summary[paren..].to_string(), theme::dim()),
        ],
        None => vec![Span::styled(renderers.display_name(summary), theme::ok())],
    }
}

fn task_header_summary(summary: &str) -> Vec<Span<'static>> {
    match summary.split_once(" · ") {
        Some((kind, objective)) => vec![
            Span::styled(kind.to_string(), theme::ok()),
            Span::styled(" · ", theme::dim()),
            Span::styled(objective.to_string(), Style::default()),
        ],
        None => vec![Span::styled(summary.to_string(), theme::ok())],
    }
}

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

fn preview_tail(preview: &str, style: Style) -> Vec<Span<'static>> {
    (!preview.is_empty())
        .then(|| Span::styled(format!(" — {preview}"), style))
        .into_iter()
        .collect()
}

fn append_result_preview(lines: &mut Vec<Line<'static>>, preview: &str, style: Style) {
    for span in preview_tail(preview, style) {
        if let Some(last) = lines.last_mut() {
            last.spans.push(span);
        } else {
            lines.push(Line::from(vec![span]));
        }
    }
}

fn result_preview(text: &str) -> String {
    let mut line = text.lines().next().unwrap_or("").to_string();
    if line.chars().count() > 120 {
        line = line.chars().take(120).collect::<String>() + "…";
    }
    let extra = text.lines().count().saturating_sub(1);
    if extra > 0 {
        line.push_str(&format!(" (+{extra} lines)"));
    }
    line
}

fn quote_lines(text: &str) -> Vec<Line<'static>> {
    text.lines()
        .map(|row| {
            let mut spans = vec![Span::styled(theme::USER_GUTTER, theme::user_gutter())];
            // Keep restored and sub-agent transcripts visually identical to a
            // just-sent prompt, without requiring the completion index.
            spans.extend(crate::reference_style::user_text_spans(row));
            Line::from(spans)
        })
        .collect()
}

fn attachment_line(label: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(theme::USER_GUTTER, theme::user_gutter()),
        Span::styled(format!("⌞ {label}"), theme::dim()),
    ])
}

/// `/name args` + a collapsed one-line summary, in place of quoting the whole
/// rendered skill body — the same fold convention read/grep output uses. The
/// single entry point for both the live prompt echo (`app.rs::prompt_echo`)
/// and ledger replay (below), so a `/name` invocation looks identical
/// whichever path baked it.
/// The card lines of a delegated run. They live here rather than in the app
/// because both transcripts bake them: the main conversation for the runs it
/// delegates, and a trace view for the runs *it* delegates in turn. Two copies
/// is how the trace view came to show a bare batch header and nothing else
/// while a nested sub-agent worked.
pub(crate) const TASK_STATUS_INDENT: &str = "      └ ";

pub(crate) fn task_summary_detail(summary: &str) -> Vec<Line<'static>> {
    summary
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| Line::styled(format!("  │ {line}"), theme::dim()))
        .collect()
}

pub(crate) fn task_live_detail(summary: &str, steps: &[String]) -> Vec<Line<'static>> {
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
pub(crate) fn task_plain_status(activity: &str) -> Vec<Line<'static>> {
    vec![Line::from(vec![Span::styled(
        format!("{TASK_STATUS_INDENT}{activity}"),
        theme::dim(),
    )])]
}

/// The card's live status: its parent-authored objective stays the primary
/// label, while the changing sub-agent tool is supporting progress. A parallel
/// batch names its current call and count.
pub(crate) fn task_status_lines(run: &UiTaskRun, cwd: &Path) -> Vec<Line<'static>> {
    let Some(call) = run.current_call() else {
        return task_plain_status(&run.activity);
    };
    let summary = shorten_summary_path(&call.summary, Some(cwd));
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

/// A slash command is a line the user typed, so it is echoed like one. Its
/// answer hangs off this row through `bake::reply_lines`, giving a command the
/// same call/result shape a tool has — without it, consecutive commands stack
/// into one undifferentiated block of dim text with nothing saying which line
/// answered which command.
pub(crate) fn command_echo_lines(cmd: &str) -> Vec<Line<'static>> {
    vec![
        Line::default(),
        Line::from(vec![
            Span::styled(theme::USER_GUTTER, theme::user_gutter()),
            Span::styled(cmd.to_string(), theme::user_message()),
        ]),
    ]
}

pub(crate) fn skill_echo_lines(echo: &tcode_tools::SkillEcho) -> Vec<Line<'static>> {
    let header = if echo.args.is_empty() {
        format!("/{}", echo.name)
    } else {
        format!("/{} {}", echo.name, echo.args)
    };
    vec![
        Line::from(vec![
            Span::styled(theme::USER_GUTTER, theme::user_gutter()),
            Span::styled(header, theme::user_message()),
        ]),
        Line::styled(
            format!("  ⎿ skill loaded ({} lines)", echo.body_line_count),
            theme::dim(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn ctx<'a>(registry: &'a RenderRegistry, markdown: &'a mut markdown::Renderer) -> BakeCtx<'a> {
        BakeCtx {
            renderers: registry,
            markdown,
            cwd: Path::new("."),
            show_reasoning: true,
        }
    }

    #[test]
    fn outer_task_header_highlights_the_kind_and_keeps_the_objective_in_the_foreground() {
        let spans = task_header_summary("Explore · inspect the implementation");
        assert_eq!(spans[0].style.fg, Some(theme::OK));
        assert_eq!(spans[2].style, Style::default());
        assert_eq!(batch_item_style(HeaderTone::Task), Style::default());
    }

    #[test]
    fn edit_errors_keep_their_red_text_through_transcript_wrapping() {
        let registry = RenderRegistry::from_tools(&tcode_tools::builtin_tools(Path::new(".")));
        let mut markdown = markdown::Renderer::default();
        let view = SessionView::new(100);
        let mut bake = ctx(&registry, &mut markdown);
        let result = view.result_render(
            &ToolResult {
                name: "edit",
                input: Some(&serde_json::json!({"path": "src/app.rs"})),
                preview: "edit failed",
                content: "old_string was not found",
                is_error: true,
            },
            &mut bake,
        );

        let ResultRender::Foldable { head, detail } = result else {
            panic!("an edit failure must remain visible with its diagnostic");
        };
        assert_eq!(head[0].style, Style::default());
        assert_eq!(head[0].spans[0].style, Style::default().fg(theme::ERROR));
        let ResultDetail::Lines(lines) = detail else {
            panic!("tool diagnostics render as literal lines");
        };
        assert_eq!(lines[0].spans[1].style, Style::default().fg(theme::ERROR));
    }

    #[test]
    fn replay_keeps_dynamic_instructions_out_of_the_transcript() {
        let registry = RenderRegistry::from_tools(&tcode_tools::builtin_tools(Path::new(".")));
        let mut markdown = markdown::Renderer::default();
        let mut view = SessionView::new(100);
        let mut bake = ctx(&registry, &mut markdown);
        view.replay_ledger(
            &[Entry::Instruction("private project rule".into())],
            &[],
            &mut bake,
        );

        assert!(!rendered_text(&mut view, 100, 10).contains("private project rule"));
    }

    #[test]
    fn task_replay_skips_only_the_prompt_already_rendered_in_the_trace_header() {
        let registry = RenderRegistry::from_tools(&tcode_tools::builtin_tools(Path::new(".")));
        let entries = vec![
            Entry::User(vec![ContentBlock::Text {
                text: "original task prompt".into(),
            }]),
            Entry::User(vec![ContentBlock::Text {
                text: "later user note".into(),
            }]),
        ];
        let mut markdown = markdown::Renderer::default();
        let mut view = SessionView::new(100);
        let mut bake = ctx(&registry, &mut markdown);
        view.replay_task_ledger(&entries, &[], &mut bake);

        assert_eq!(view.transcript.block_count(), 1);
    }

    #[test]
    fn live_tool_events_and_trace_replay_share_the_bake_shape() {
        let registry = RenderRegistry::from_tools(&tcode_tools::builtin_tools(Path::new(".")));
        let input = serde_json::json!({"path": "Cargo.toml"});

        let mut live_markdown = markdown::Renderer::default();
        let mut live = SessionView::new(100);
        let mut live_ctx = ctx(&registry, &mut live_markdown);
        live.feed_event(
            &AgentEvent::ToolStart {
                call_id: "c1".into(),
                name: "read".into(),
                summary: String::new(),
                input: input.clone(),
            },
            &mut live_ctx,
        );
        live.feed_event(
            &AgentEvent::ToolEnd {
                call_id: "c1".into(),
                name: "read".into(),
                preview: "read Cargo.toml".into(),
                content: "[tool output]".into(),
                is_error: false,
            },
            &mut live_ctx,
        );

        let entries = vec![
            Entry::Assistant(vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "read".into(),
                input,
            }]),
            Entry::ToolResults(vec![ContentBlock::ToolResult {
                tool_use_id: "c1".into(),
                content: "[tool output]".into(),
                is_error: false,
                images: Vec::new(),
            }]),
        ];
        let mut replay_markdown = markdown::Renderer::default();
        let mut replay = SessionView::new(100);
        let mut replay_ctx = ctx(&registry, &mut replay_markdown);
        replay.replay_ledger(&entries, &[], &mut replay_ctx);

        assert_eq!(
            live.transcript.block_count(),
            replay.transcript.block_count(),
            "live events and a persisted trace must use the same bake shape"
        );
    }

    #[test]
    fn skill_echo_lines_fold_the_body_into_a_line_count_summary() {
        let long_body: String = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let wrapped = tcode_tools::wrap_skill_echo("init", "", &long_body);
        let echo = tcode_tools::parse_skill_echo(&wrapped).expect("sentinel recognized");
        let lines = skill_echo_lines(&echo);

        // Header + one collapsed summary line, never the 20-line body.
        assert_eq!(lines.len(), 2);
        let header: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header.ends_with("/init"), "header was {header:?}");
        let summary: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(summary.contains("20 lines"), "summary was {summary:?}");
    }

    /// Renders the whole transcript into a plain-text grid so a test can
    /// check what a user would actually see, not just internal block counts.
    fn rendered_text(view: &mut SessionView, width: u16, height: u16) -> String {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        view.transcript.render(&mut buf, area);
        let mut out = String::new();
        for y in 0..height {
            for x in 0..width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn replay_folds_a_skill_echo_but_quotes_a_plain_user_message() {
        let registry = RenderRegistry::from_tools(&tcode_tools::builtin_tools(Path::new(".")));
        let long_body: String = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let wrapped = tcode_tools::wrap_skill_echo("init", "arg", &long_body);

        let mut folded_markdown = markdown::Renderer::default();
        let mut folded = SessionView::new(100);
        let mut folded_ctx = ctx(&registry, &mut folded_markdown);
        folded.replay_ledger(
            &[Entry::User(vec![ContentBlock::Text { text: wrapped }])],
            &[],
            &mut folded_ctx,
        );
        let folded_text = rendered_text(&mut folded, 100, 40);
        assert!(folded_text.contains("/init arg"), "{folded_text}");
        assert!(folded_text.contains("20 lines"), "{folded_text}");
        // The body itself never reaches the screen once folded.
        assert!(!folded_text.contains("line 19"), "{folded_text}");

        let mut plain_markdown = markdown::Renderer::default();
        let mut plain = SessionView::new(100);
        let mut plain_ctx = ctx(&registry, &mut plain_markdown);
        plain.replay_ledger(
            &[Entry::User(vec![ContentBlock::Text { text: long_body }])],
            &[],
            &mut plain_ctx,
        );
        let plain_text = rendered_text(&mut plain, 100, 40);
        // An ordinary long user message is quoted in full, not folded.
        assert!(plain_text.contains("line 19"), "{plain_text}");
    }
}
