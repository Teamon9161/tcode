//! Running a turn: starting one, consuming its `AgentEvent` stream, and
//! settling back to idle.
//!
//! `on_agent_event` stays a single match on purpose. `AgentEvent` is a closed
//! core enum, not a pluggable capability, so a dispatch registry would only
//! make "who handles this event" harder to find; oversized arms delegate to
//! named methods instead.
//!
//! Touches: phase, events_rx, session, meter, state_label, pending_tool,
//! pending_batch, live_text, live_block, thinking_*, task_runs, progress,
//! suggestion, suggest_*, monitor_deadline, retry_wait.

use super::*;

/// Several prompts queued behind one turn become one prompt when that turn ends
/// — starting a turn per queued line would make the model answer the first one
/// with the rest still unsaid.
pub(super) fn merge(queued: Vec<PendingMessage>) -> Option<PendingMessage> {
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

impl App {
    pub(super) fn start_turn(&mut self, message: PendingMessage) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        // The user just answered the question the guess was asking.
        self.drop_suggestion();
        self.clear_live_text();
        self.space_before_response = false;
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
        self.meter.set_context(
            session.last_prompt_tokens.saturating_add(prompt_tokens),
            self.meter.context_estimated,
        );
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
        self.meter.start_turn();
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

    /// Recompute when an idle session should wake for monitor activity.
    /// While a turn is running the session is owned by the worker, so the
    /// deadline stays unset; `on_turn_done` recomputes it.
    pub(super) fn refresh_monitor_deadline(&mut self) {
        self.monitor_deadline = self
            .session
            .as_ref()
            .and_then(|session| {
                session
                    .tool_ctx
                    .background
                    .lock()
                    .expect("background lock")
                    .monitor_wake_deadline()
            })
            .map(tokio::time::Instant::from_std);
    }

    pub(super) fn on_monitor_deadline(&mut self) {
        self.monitor_deadline = None;
        // Don't start a turn underneath a modal (approval dialog, pickers,
        // rewind navigation); retry shortly instead of losing the events.
        let modal_open = self.overlay.is_some() || self.rewind_nav.is_some();
        if modal_open {
            self.monitor_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(1));
            return;
        }
        self.start_monitor_wake();
    }

    /// A harness-started turn delivering monitor events that arrived while
    /// idle. There is no user prompt to echo; the injected notes bake via
    /// `AgentEvent::Note` and the model's reaction streams as usual.
    pub(super) fn start_monitor_wake(&mut self) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        self.drop_suggestion();
        self.clear_live_text();
        self.space_before_response = false;

        let (tx, rx) = mpsc::channel(64);
        self.events_rx = Some(rx);
        self.meter.start_turn();
        self.thinking_chars = 0;
        self.state_label = "monitor event".into();

        let cancel = CancellationToken::new();
        let agent = self.agent.clone();
        let approver = self.approver.clone();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move {
            let result = agent
                .monitor_turn(&mut session, &tx, &*approver, cancel2)
                .await
                .map(|_| ());
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
    pub(super) fn start_compact(&mut self, focus: Option<String>) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        if session.ledger.is_empty() {
            self.session = Some(session);
            self.bake(vec![Line::styled("nothing to compact", theme::dim())]);
            return;
        }
        session.turn_usage = Usage::default();
        self.meter.start_turn();
        // Legitimate prefix rewrite: don't false-alarm next turn.
        self.meter.forget_cache_baseline();
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

    pub(super) fn on_turn_done(&mut self, done: (Session, Result<(), AgentError>)) {
        // The worker can finish before the UI select loop has received the
        // final queued AgentEvents. Process them before dropping the receiver,
        // otherwise a fast one-shot response (e.g. "hello") can vanish from
        // the transcript even though the model answered.
        self.drain_agent_events();
        self.bake_live_text();
        self.finish_thinking();
        // An interrupted turn can leave a call in flight; its header must not
        // keep shimmering over a turn that is over.
        self.transcript.set_live_head(None);
        self.pending_tool = None;
        self.pending_batch.clear();

        let (mut session, result) = done;
        let elapsed = match &self.phase {
            Phase::Running { started, .. } => self.meter.active_elapsed(*started),
            Phase::Idle => 0.0,
        };
        // The session's per-turn tally is authoritative (it also covers
        // compaction, which streams no Usage events to the UI).
        self.meter.finish_turn(session.turn_usage);
        let estimated = session.last_prompt_tokens == 0 && !session.ledger.is_empty();
        if estimated {
            session.last_prompt_tokens = self.agent.estimate_context_tokens(&session);
        }
        self.meter
            .set_context(session.last_prompt_tokens, estimated);
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
        let u = self.meter.turn;
        self.bake(vec![turn_summary_line(elapsed, u)]);
        if let Some((prev, ratio)) = self.meter.take_cache_regression() {
            self.bake(vec![Line::styled(
                format!(
                    "⚠ cache hit fell {:.0}% → {:.0}% — prompt prefix changed unexpectedly",
                    prev * 100.0,
                    ratio * 100.0
                ),
                ratatui::style::Style::default().fg(theme::WARN),
            )]);
        }
        // Whatever the loop never reached a boundary to deliver — a message
        // queued during the closing answer, or one queued right before ctrl+c —
        // becomes the next turn immediately. The user already pressed enter on
        // it; they should not have to press it again.
        if let Some(message) = merge(self.pending.take_for_next_turn()) {
            self.start_turn(message);
            return;
        }
        // A turn that errored out leaves the user reading a failure, not
        // choosing a next step. `suggest_request` refuses interrupted turns on
        // the same principle; this catches the broken-stream case too.
        if landed {
            self.start_suggestion();
        }
        // Monitor events that arrived during the closing words never reached
        // a boundary; now that the session is back, re-arm the idle wake.
        self.refresh_monitor_deadline();
    }

    pub(super) fn drain_agent_events(&mut self) {
        while let Some(rx) = self.events_rx.as_mut() {
            let ev = rx.try_recv();
            match ev {
                Ok(ev) => self.on_agent_event(ev),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
    }

    pub(super) fn on_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::Started => {
                // The retry succeeded (or this is the first attempt): drop the
                // countdown.
                self.retry_wait = None;
                self.meter.begin_step();
                self.state_label = "responding".into();
            }
            AgentEvent::TextDelta(t) => {
                self.finish_thinking();
                if self.space_before_response {
                    self.bake(vec![Line::default()]);
                    self.space_before_response = false;
                }
                let tokens = approx_tokens(&t);
                self.meter.on_streamed_tokens(tokens);
                self.live_text.push_str(&t);
                self.refresh_live_text();
                self.state_label = "writing".into();
            }
            AgentEvent::ThinkingDelta(t) => {
                if self.thinking_since.is_none() {
                    self.thinking_since = Some(Instant::now());
                }
                let tokens = approx_tokens(&t);
                self.meter.on_streamed_tokens(tokens);
                self.thinking_chars += t.chars().count();
                self.thinking_text.push_str(&t);
                self.state_label = "thinking".into();
            }
            AgentEvent::ToolInputDelta(t) => {
                // Tool arguments are output tokens too; count them so the meter
                // moves while the model assembles a call. The call header itself
                // is rendered later by ToolStart, so nothing is baked here.
                let tokens = approx_tokens(&t);
                self.meter.on_streamed_tokens(tokens);
                self.state_label = "calling tool".into();
            }
            AgentEvent::ToolBatchStart { label, calls } => {
                self.space_before_response = false;
                // A batch supersedes any single diff pre-baked for its
                // (once) approval — retract it so the batch renders in full.
                if let Some(mark) = self.change_prebake.take() {
                    self.transcript.truncate_blocks(mark);
                }
                self.bake_live_text();
                self.finish_thinking();
                let header = self.batch_header_lines(&label);
                let header_block = self.transcript.block_count();
                self.bake(header);
                // The batch header shimmers until its last result lands.
                self.transcript.set_live_head(Some(header_block));
                self.pending_batch.clear();
                // A batch spanning several tools tags each item with its tool
                // name so the reader can tell the calls apart; a single-tool
                // batch (e.g. "Read 5 files") needs no per-item prefix.
                let mixed = calls
                    .iter()
                    .map(|(_, n, _)| n.as_str())
                    .collect::<HashSet<_>>()
                    .len()
                    > 1;
                for (call_id, name, input) in calls {
                    self.pending_batch.push_back(PendingCall {
                        call_id,
                        detail: serde_json::to_string_pretty(&input).unwrap_or_default(),
                        header: self.batch_item_lines(&name, &input, mixed),
                        header_index: None,
                    });
                }
                self.state_label = format!("running: {label}");
            }
            AgentEvent::ToolStart {
                call_id,
                name,
                summary,
                input,
            } => self.on_tool_start(call_id, name, summary, input),
            AgentEvent::ToolEnd {
                call_id,
                name,
                preview,
                content,
                is_error,
            } => self.on_tool_end(call_id, name, preview, content, is_error),
            AgentEvent::ReferencesExpanded {
                labels,
                added_tokens,
            } => {
                self.meter.add_context(added_tokens as u64);
                let count = labels.len();
                let summary = labels.into_iter().take(2).collect::<Vec<_>>().join(", ");
                let more = if count > 2 {
                    format!(" +{}", count - 2)
                } else {
                    String::new()
                };
                self.notice = Some((format!("referenced {summary}{more}"), Instant::now()));
            }
            AgentEvent::Note(text) => {
                // Same record replay bakes from `Entry::Note`, shown live so
                // background completions and monitor events are visible the
                // moment they land in the ledger.
                self.bake_live_text();
                self.finish_thinking();
                let mut lines = vec![Line::default()];
                lines.extend(quote_lines(Some(NOTE_LABEL), &text));
                lines.push(Line::default());
                self.bake(lines);
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
                self.space_before_response = false;
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
                self.meter.rewind_step();
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
                self.meter.on_usage(u);
                // Providers report the full prompt (cached tokens included)
                // plus this response; this is the most accurate context
                // figure available to the TUI.
                // `on_usage` also re-anchors the retry rewind point.
            }
            AgentEvent::RateLimits(limits) => self.meter.rate_limits = Some(limits),
            AgentEvent::DelegatedUsage(u) => {
                // Sub-agent requests are billable and should animate the
                // turn's token counter, but run in an isolated context.
                self.meter.on_delegated_usage(u);
                self.state_label = "sub-agent working".into();
            }
            AgentEvent::TaskRunEvent { run, event } => {
                let trace_event = (*event).clone();
                // Usage inside a run carries delegated semantics: billable,
                // animates the counter, never the parent's context meter.
                if let AgentEvent::Usage(u) | AgentEvent::DelegatedUsage(u) = event.as_ref() {
                    self.meter.on_delegated_usage(*u);
                }
                if let Some(position) = self.task_runs.iter().position(|entry| entry.id == run) {
                    let entry = &mut self.task_runs[position];
                    entry.note_event(&event, &self.renderers, &self.cwd);
                    entry.events.push(trace_event);
                    let entry = &self.task_runs[position];
                    if let Some(block) = entry.block {
                        let detail = task_live_detail(&entry.summary, &entry.steps);
                        let status = self.task_status_lines(entry);
                        self.transcript.replace_detail_preserving_open(
                            block,
                            detail,
                            OUTPUT_VIEW_ROWS,
                        );
                        self.transcript.set_live_status(block, Some(status));
                    }
                }
                self.refresh_open_trace(&run);
            }
            AgentEvent::TaskRunStarted {
                run,
                parent_call,
                kind,
                model,
                prompt,
                summary,
            } => self.on_task_run_started(run, parent_call, kind, model, prompt, summary),
            AgentEvent::TaskRunFinished {
                run,
                status,
                tool_calls,
                usage,
            } => {
                let block = self
                    .task_runs
                    .iter_mut()
                    .find(|r| r.id == run)
                    .map(|entry| {
                        entry.status = status;
                        entry.tools = tool_calls;
                        entry.usage = usage;
                        entry.block
                    });
                if let Some(Some(block)) = block {
                    self.transcript.set_live_status(block, None);
                }
                self.finish_open_trace(&run);
            }
            AgentEvent::Compacting => {
                self.bake(vec![Line::styled(
                    "✦ context near limit — compacting earlier history",
                    ratatui::style::Style::default().fg(theme::WARN),
                )]);
                self.state_label = "compacting".into();
                // Legitimate prefix rewrite: don't false-alarm next turn.
                self.meter.forget_cache_baseline();
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
            AgentEvent::AutoClassifierUnavailable(reason) => {
                self.bake(vec![Line::styled(
                    format!("⊙ Auto classifier unavailable; asking you instead: {reason}"),
                    ratatui::style::Style::default().fg(theme::WARN),
                )]);
                self.state_label = "classifier unavailable — approval required".into();
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
    /// A tool call started: route it, bake its header, and mark it in flight.
    pub(super) fn on_tool_start(
        &mut self,
        call_id: String,
        name: String,
        summary: String,
        input: serde_json::Value,
    ) {
        self.space_before_response = false;
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
        // shell command gets a capped preview and folded detail,
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
        // A bare call's header shimmers while the tool runs; calls
        // whose record is a baked body (diff/preview) stay static.
        self.transcript.set_live_head(header_index);
        self.pending_tool = Some(PendingCall {
            call_id,
            detail: serde_json::to_string_pretty(&input).unwrap_or_default(),
            header: Vec::new(),
            header_index,
        });
        self.state_label = format!("running: {summary}");
    }

    /// A tool call finished: bake its result into the record its header owns,
    /// and link it back to a sub-agent run when one produced it.
    pub(super) fn on_tool_end(
        &mut self,
        call_id: String,
        name: String,
        preview: String,
        content: String,
        is_error: bool,
    ) {
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
        let task_run = self
            .task_runs
            .iter()
            .find(|run| run.parent_call == call_id)
            .map(|run| run.id.clone());
        let linked_run = task_run
            .as_ref()
            .and_then(|run| {
                self.task_runs
                    .iter()
                    .find(|entry| entry.id == *run && entry.block.is_none())
            })
            .map(|run| run.id.clone());
        let blocks_before = self.transcript.block_count();
        let entry = self
            .pending_tool
            .take()
            .filter(|entry| entry.call_id == call_id)
            .or_else(|| {
                let position = self
                    .pending_batch
                    .iter()
                    .position(|entry| entry.call_id == call_id)?;
                self.pending_batch.remove(position)
            });
        // Last in-flight call finished: stop the header shimmer.
        if self.pending_tool.is_none() && self.pending_batch.is_empty() {
            self.transcript.set_live_head(None);
        }
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
        self.meter.add_context(approx_tokens(&content) as u64);
        let report = task_run.as_ref().map(|_| task_result_text(&content));
        self.bake_call_result(
            &name,
            input.as_ref(),
            &preview,
            report.as_deref().unwrap_or(&content),
            is_error,
            record,
        );
        if let Some(run) = linked_run.filter(|_| self.transcript.block_count() > blocks_before) {
            if let Some(index) = self.transcript.last_block_index() {
                self.transcript.link_task_run(index, run.clone());
                if let Some(entry) = self.task_runs.iter_mut().find(|entry| entry.id == run) {
                    entry.block = Some(index);
                }
            }
        }
        self.space_before_response = true;
        self.state_label = "responding".into();
    }

    /// A sub-agent run began. Its live trace card hangs off the parent call's
    /// header block, which a batch item has to bake early to obtain.
    pub(super) fn on_task_run_started(
        &mut self,
        run: String,
        parent_call: String,
        kind: String,
        model: String,
        prompt: String,
        summary: String,
    ) {
        // ToolStart always precedes a task's delegated start. Single
        // calls already have a header block; batch calls receive their
        // link when their item bakes at ToolEnd.
        let mut block = self
            .pending_tool
            .as_ref()
            .filter(|call| call.call_id == parent_call)
            .and_then(|call| call.header_index);
        if let Some(index) = block {
            self.transcript.link_task_run(index, run.clone());
            self.transcript
                .attach_detail(index, task_summary_detail(&summary), OUTPUT_VIEW_ROWS);
            self.transcript
                .set_live_status(index, Some(task_plain_status("starting…")));
        } else if let Some(position) = self
            .pending_batch
            .iter()
            .position(|call| call.call_id == parent_call)
        {
            // A parallel task's batch item normally waits for its
            // result. It is a live trace card, though, so bake it now
            // and retain the header index for the eventual report.
            let mut call = self
                .pending_batch
                .remove(position)
                .expect("position checked");
            let index = self.transcript.block_count();
            self.bake(std::mem::take(&mut call.header));
            call.header_index = Some(index);
            self.pending_batch.insert(position, call);
            block = Some(index);
            self.transcript.link_task_run(index, run.clone());
            self.transcript
                .attach_detail(index, task_summary_detail(&summary), OUTPUT_VIEW_ROWS);
            self.transcript
                .set_live_status(index, Some(task_plain_status("starting…")));
        }
        self.task_runs.push(UiTaskRun::new(
            run,
            parent_call,
            kind,
            model,
            prompt,
            summary,
            block,
        ));
    }

    /// Dedicated transcript record for a completed `ask_user` form. It keeps
    /// the question adjacent to its answer and deliberately bypasses the normal
    /// silent tool renderer, which would otherwise duplicate the interaction.
    pub(super) fn bake_question_record(&mut self, pairs: &[(String, String)]) {
        let mut lines = vec![Line::default()];
        let count = pairs.len();
        let mut header = vec![
            Span::styled("● ", theme::ok()),
            Span::styled(
                "Ask user",
                theme::ok().add_modifier(ratatui::style::Modifier::BOLD),
            ),
        ];
        if count > 1 {
            header.push(Span::styled(format!(" · {count} questions"), theme::dim()));
        }
        lines.push(Line::from(header));
        for (index, (question, answer)) in pairs.iter().enumerate() {
            let last = index + 1 == count;
            let branch = if last { "└" } else { "├" };
            let continuation = if last { " " } else { "│" };
            let label = if count > 1 {
                format!("{}. {question}", index + 1)
            } else {
                question.clone()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {branch} "), theme::dim()),
                Span::styled(label, theme::bold()),
            ]));
            for (answer_row, row) in answer.lines().enumerate() {
                let marker = if answer_row == 0 { "└" } else { " " };
                lines.push(Line::from(vec![
                    Span::styled(format!("  {continuation}   {marker} "), theme::dim()),
                    Span::styled(row.to_string(), theme::accent()),
                ]));
            }
        }
        self.bake(lines);
    }

    pub(super) fn bake_user_note(&mut self, text: &str) {
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
    pub(super) fn bake_approval_record(&mut self, dialog: &Dialog, approval: &Approval) {
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
            let answer = approval.comment.as_deref().unwrap_or_default();
            self.bake_question_record(&dialog.question_answer_pairs(answer));
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
    pub(super) fn edit_plan_externally(&mut self) {
        let Some(source) = self
            .overlay
            .as_ref()
            .and_then(Overlay::as_dialog)
            .and_then(Dialog::plan_source)
        else {
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
        if let Some(dialog) = self.overlay.as_mut().and_then(Overlay::as_dialog_mut) {
            dialog.revise_plan(edited, blocks);
        }
    }

    pub(super) fn finish_thinking(&mut self) {
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
    pub(super) fn bake_thinking(&mut self, title: &str, text: &str) {
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
    pub(super) fn bake_compacted(&mut self, summary: &str) {
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

    pub(super) fn refresh_live_text(&mut self) {
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

    pub(super) fn clear_live_text(&mut self) {
        self.live_text.clear();
        if let Some(index) = self.live_block.take() {
            self.transcript.truncate_blocks(index);
        }
    }

    pub(super) fn bake_live_text(&mut self) {
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

    pub(super) fn cancel_turn(&mut self) {
        if let Phase::Running { cancel, .. } = &self.phase {
            cancel.cancel();
            self.state_label = "cancelling".into();
        }
    }

    pub(super) fn update_progress(&mut self, input: &serde_json::Value) {
        // `plan` / `step` keep resumed sessions created before the rename
        // readable; live calls use `phases` / `phase` exclusively.
        let Some(items) = input["phases"]
            .as_array()
            .or_else(|| input["plan"].as_array())
        else {
            return;
        };
        let parsed: Vec<ProgressPhase> = items
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
        // `[]` deliberately clears progress. A non-empty malformed payload is
        // not a meaningful update and must not erase the existing snapshot.
        if items.is_empty() || !parsed.is_empty() {
            self.progress = parsed;
        }
    }

    /// Ask, off-thread, what the user probably wants next. It runs on its own
    /// small prose conversation and its own model role (see `Agent::suggest`),
    /// so a turn only pays for its newest pair — but it is still a request,
    /// hence `[ui] suggest_next` and `/suggestions`.
    pub(super) fn start_suggestion(&mut self) {
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
    pub(super) fn drop_suggestion(&mut self) {
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
    pub(super) fn discard_queued(&mut self) {
        let mut queued = self.pending.take_for_next_turn();
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
}
