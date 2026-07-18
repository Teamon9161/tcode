//! Rebuilding the transcript from a stored ledger.
//!
//! Replay must produce exactly what the live path produced, so it goes
//! through the same `bake_*` entry points rather than formatting anything
//! itself — batch grouping, blank-line separators and diff bodies have all
//! drifted apart here before.
//!
//! Touches: transcript, renderers, md, pending_tool, pending_batch,
//! task_runs, space_before_response.

use super::*;

/// A tool call recovered from the ledger during replay, with the batch it
/// was executed in (asked of core, never re-derived here).
pub(super) struct ReplayCall {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) input: serde_json::Value,
    pub(super) batch: Option<ReplayBatch>,
}

pub(super) struct ReplayBatch {
    /// The `● label` header, carried by the batch's first call only.
    pub(super) header: Option<String>,
    /// Several tools in one batch: tag each item with its tool name.
    pub(super) mixed: bool,
}

impl App {
    /// Replay a resumed conversation into the scrollback so the user
    /// sees where they left off.
    pub(super) fn bake_transcript(&mut self) {
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

    pub(super) fn replay_ledger(&mut self, session: &Session) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut resumed_progress: Option<serde_json::Value> = None;
        let mut calls: HashMap<String, ReplayCall> = HashMap::new();
        let mut space_before_assistant_text = false;
        for (entry_index, entry) in session.ledger.entries().iter().enumerate() {
            match entry {
                tcode_core::Entry::User(blocks) => {
                    space_before_assistant_text = false;
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
                                if space_before_assistant_text {
                                    self.transcript.push(std::mem::take(&mut lines));
                                    self.bake(vec![Line::default()]);
                                    space_before_assistant_text = false;
                                }
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
                        calls.insert(
                            id.clone(),
                            ReplayCall {
                                id,
                                name,
                                input,
                                batch,
                            },
                        );
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
                        let run_id = call.and_then(|call| {
                            self.task_runs
                                .iter()
                                .find(|run| run.parent_call == call.id)
                                .map(|run| run.id.clone())
                        });
                        let blocks_before = self.transcript.block_count();
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
                        let report = run_id.as_ref().map(|_| task_result_text(content));
                        self.bake_call_result(
                            name,
                            call.map(|c| &c.input),
                            &preview,
                            report.as_deref().unwrap_or(content),
                            *is_error,
                            record,
                        );
                        if let Some(run) =
                            run_id.filter(|_| self.transcript.block_count() > blocks_before)
                        {
                            if let Some(index) = self.transcript.last_block_index() {
                                self.transcript.link_task_run(index, run.clone());
                                if let Some(entry) =
                                    self.task_runs.iter_mut().find(|entry| entry.id == run)
                                {
                                    entry.block = Some(index);
                                }
                            }
                        }
                        space_before_assistant_text = true;
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
}
