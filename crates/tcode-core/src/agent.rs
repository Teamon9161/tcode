use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use futures::{future::join_all, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::accumulate::ResponseAccumulator;
use crate::config::WatchdogConfig;
use crate::ledger::{Entry, Ledger};
use crate::permission::{
    ApprovalDecision, Approver, Decision, PermissionMode, PermissionRules,
};
use crate::provider::{ProviderError, Request, StreamEvent};
use crate::tool::{PermissionRequest, Tool, ToolCtx, ToolOutput};
use crate::types::{ContentBlock, RateLimits, StopReason, Usage};

/// Hard ceiling on model round-trips per user turn; a runaway loop
/// should never bill unbounded.
const MAX_STEPS: usize = 100;

/// One-way events for the UI. Approval prompts go the other way through
/// the `Approver` trait.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Model accepted the request and is responding.
    Started,
    TextDelta(String),
    ThinkingDelta(String),
    /// Streaming failed mid-turn; the request is being re-sent.
    /// UI must discard un-baked partial output for this step.
    Retrying {
        attempt: u32,
        max: u32,
        error: String,
    },
    ToolStart {
        name: String,
        summary: String,
        /// Raw call input, e.g. for rendering edit diffs in the UI.
        input: Value,
    },
    /// A concurrently-dispatched group. Individual results still arrive as
    /// `ToolEnd` in call order, but UIs can avoid five identical headers.
    ToolBatchStart {
        label: String,
        calls: Vec<(String, Value)>,
    },
    ToolEnd {
        name: String,
        preview: String,
        /// Complete gated output for UI detail views. The regular transcript
        /// should keep showing only `preview`.
        content: String,
        is_error: bool,
    },
    /// Per-step usage (one model request).
    Usage(Usage),
    RateLimits(RateLimits),
    /// Usage spent inside a delegated `task` sub-agent. It contributes to
    /// cost/turn statistics, but not to the parent's context-window meter.
    DelegatedUsage(Usage),
    /// Context grew past the auto-compact threshold; a summary request
    /// is running before the actual turn.
    Compacting,
    /// A mutating call was declined without guidance. The turn is over so the
    /// user can provide the missing direction instead of the model guessing.
    AwaitingUserInput,
    Interrupted,
    TurnEnd,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("agent stopped after {MAX_STEPS} steps in one turn")]
    StepLimit,
    #[error("event channel closed")]
    ChannelClosed,
}

/// Mutable per-conversation state.
pub struct Session {
    pub ledger: Ledger,
    pub mode: PermissionMode,
    pub rules: PermissionRules,
    pub tool_ctx: ToolCtx,
    /// File snapshots for rewind; no-op unless persistence is set up.
    pub checkpoints: crate::checkpoint::CheckpointStore,
    /// Prompt size of the latest request (for the context status line).
    pub last_prompt_tokens: u64,
    pub turn_usage: Usage,
}

impl Session {
    pub fn new(tool_ctx: ToolCtx, mode: PermissionMode, rules: PermissionRules) -> Self {
        Self {
            ledger: Ledger::new(),
            mode,
            rules,
            tool_ctx,
            checkpoints: crate::checkpoint::CheckpointStore::default(),
            last_prompt_tokens: 0,
            turn_usage: Usage::default(),
        }
    }

    /// Tail self-awareness line: the model can only manage its context
    /// budget if it knows it. Appended inside the newest user entry, so
    /// the prompt prefix never changes retroactively (cache-safe).
    fn status_block(&self, context_window: u64) -> Option<ContentBlock> {
        if self.last_prompt_tokens == 0 {
            return None;
        }
        let pct = (self.last_prompt_tokens as f64 / context_window as f64 * 100.0).round();
        Some(ContentBlock::Text {
            text: format!(
                "<tcode-status>context ~{pct:.0}% of {}k tokens · permission-mode: {}</tcode-status>",
                context_window / 1000,
                self.mode.label()
            ),
        })
    }
}

pub struct Agent {
    /// Swappable model handle; each turn snapshots it once.
    pub model: crate::provider::ModelCell,
    pub tools: Vec<Arc<dyn Tool>>,
    pub system: String,
    pub watchdog: WatchdogConfig,
    pub hooks: crate::hooks::Hooks,
}

impl Agent {
    fn tool(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name)
    }

    fn tool_defs(&self) -> Vec<crate::ToolDef> {
        self.tools.iter().map(|t| t.as_ref().def()).collect()
    }

    /// Drive one user turn to completion: stream → run tools → repeat
    /// until the model stops calling tools or the user interrupts.
    pub async fn user_turn(
        &self,
        session: &mut Session,
        mut input: Vec<ContentBlock>,
        events: &mpsc::Sender<AgentEvent>,
        approver: &dyn Approver,
        cancel: CancellationToken,
    ) -> Result<(), AgentError> {
        let model = self.model.snapshot();
        // Auto-compact: pay the one-time cache invalidation before the
        // context overflows, not after.
        if session.last_prompt_tokens > model.context_window * 85 / 100 {
            self.emit(events, AgentEvent::Compacting).await?;
            self.compact(session, &cancel).await?;
        }
        if let Some(status) = session.status_block(model.context_window) {
            input.push(status);
        }
        session.ledger.append(Entry::User(input));
        session.turn_usage = Usage::default();

        for _step in 0..MAX_STEPS {
            let (blocks, usage, stop) =
                self.stream_step(&model, session, events, &cancel).await?;
            session.last_prompt_tokens = usage.total_input() + usage.output_tokens;
            session.turn_usage.input_tokens += usage.input_tokens;
            session.turn_usage.output_tokens += usage.output_tokens;
            session.turn_usage.cache_read_tokens += usage.cache_read_tokens;
            session.turn_usage.cache_write_tokens += usage.cache_write_tokens;
            self.emit(events, AgentEvent::Usage(usage)).await?;

            let (complete, dropped_malformed) = split_malformed(blocks);
            let tool_calls: Vec<(String, String, Value)> = complete
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect();
            if !complete.is_empty() {
                session.ledger.append(Entry::Assistant(complete));
            }

            if cancel.is_cancelled() {
                self.commit_interrupt(session, &tool_calls, &[], dropped_malformed);
                self.emit(events, AgentEvent::Interrupted).await?;
                return Ok(());
            }
            if tool_calls.is_empty() || stop != Some(StopReason::ToolUse) {
                self.emit(events, AgentEvent::TurnEnd).await?;
                return Ok(());
            }

            let outcome = self
                .run_tools(session, &tool_calls, events, approver, &cancel)
                .await?;
            if outcome.interrupted {
                self.emit(events, AgentEvent::Interrupted).await?;
                return Ok(());
            }
            if outcome.awaiting_user_input {
                self.emit(events, AgentEvent::AwaitingUserInput).await?;
                self.emit(events, AgentEvent::TurnEnd).await?;
                return Ok(());
            }
        }
        Err(AgentError::StepLimit)
    }

    /// Summarize the whole ledger into one entry — the single deliberate
    /// cache-invalidating operation. Also used by `/compact`.
    pub async fn compact(
        &self,
        session: &mut Session,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        const COMPACT_PROMPT: &str = "\
Context is being compacted. Write a summary that will REPLACE all \
earlier history; it is the only memory you will keep. Include: \
1) the task and its goal, 2) what was done so far (files read/changed, \
commands run, outcomes), 3) decisions made and constraints discovered, \
4) current state and what remains, 5) exact paths, names and other \
details needed to continue without re-discovering them. Output only \
the summary text.";
        if session.ledger.is_empty() {
            return Ok(());
        }
        let mut messages = session.ledger.as_messages();
        messages.push(crate::Message {
            role: crate::Role::User,
            content: vec![ContentBlock::Text {
                text: COMPACT_PROMPT.into(),
            }],
        });
        let model = self.model.snapshot();
        let req = Request {
            model: model.provider.model().to_string(),
            system: self.system.clone(),
            messages,
            tools: Vec::new(),
            max_tokens: model.max_tokens,
            effort: model.effort.clone(),
        };
        let mut stream = model.provider.stream(req, cancel.clone()).await?;
        let mut acc = ResponseAccumulator::new();
        while let Some(item) = stream.next().await {
            acc.feed(&item?);
        }
        let (blocks, usage, _) = acc.finish();
        let summary: String = blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        // A cancelled or empty summary must not wipe the history.
        if cancel.is_cancelled() || summary.trim().is_empty() {
            return Ok(());
        }
        let upto = session.ledger.len();
        session.ledger.compact(summary, upto);
        session.turn_usage.input_tokens += usage.input_tokens;
        session.turn_usage.output_tokens += usage.output_tokens;
        session.turn_usage.cache_read_tokens += usage.cache_read_tokens;
        session.turn_usage.cache_write_tokens += usage.cache_write_tokens;
        // Unknown until the next request reports it.
        session.last_prompt_tokens = 0;
        Ok(())
    }

    /// One model request with watchdog retries. Partial output from a
    /// failed attempt is discarded (UI told via `Retrying`).
    async fn stream_step(
        &self,
        model: &crate::provider::ActiveModel,
        session: &Session,
        events: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<(Vec<ContentBlock>, Usage, Option<StopReason>), AgentError> {
        let mut attempt = 0u32;
        'retry: loop {
            let req = Request {
                model: model.provider.model().to_string(),
                system: self.system.clone(),
                messages: session.ledger.as_messages(),
                tools: self.tool_defs(),
                max_tokens: model.max_tokens,
                effort: model.effort.clone(),
            };
            let mut stream = model.provider.stream(req, cancel.clone()).await?;
            let mut acc = ResponseAccumulator::new();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(ev) => {
                        match &ev {
                            StreamEvent::Started => {
                                self.emit(events, AgentEvent::Started).await?
                            }
                            StreamEvent::TextDelta(t) => {
                                self.emit(events, AgentEvent::TextDelta(t.clone())).await?
                            }
                            StreamEvent::ThinkingDelta(t) => {
                                self.emit(events, AgentEvent::ThinkingDelta(t.clone()))
                                    .await?
                            }
                            StreamEvent::RateLimits(limits) => {
                                self.emit(events, AgentEvent::RateLimits(*limits)).await?
                            }
                            _ => {}
                        }
                        acc.feed(&ev);
                    }
                    Err(e) if e.retryable() && attempt < self.watchdog.max_retries => {
                        attempt += 1;
                        self.emit(
                            events,
                            AgentEvent::Retrying {
                                attempt,
                                max: self.watchdog.max_retries,
                                error: e.to_string(),
                            },
                        )
                        .await?;
                        tokio::time::sleep(self.watchdog.initial_backoff() * attempt).await;
                        continue 'retry;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            let (blocks, usage, stop) = acc.finish();
            return Ok((blocks, usage, stop));
        }
    }

    async fn run_tools(
        &self,
        session: &mut Session,
        calls: &[(String, String, Value)],
        events: &mpsc::Sender<AgentEvent>,
        approver: &dyn Approver,
        cancel: &CancellationToken,
    ) -> Result<ToolsOutcome, AgentError> {
        if calls.len() > 1
            && calls
                .iter()
                .all(|(_, name, _)| is_parallel_read_only_tool(name))
        {
            return self
                .run_read_only_tools_parallel(session, calls, events, cancel)
                .await;
        }
        // Edits to distinct files are the other safe parallel case.  Do not
        // optimistically start any write: every permission prompt, hook and
        // collision check completes first, so a declined sibling can never
        // leave a partially-applied batch behind.
        if calls.len() > 1 && self.is_parallel_file_mutation_batch(session, calls) {
            return self
                .run_file_mutations_parallel(session, calls, events, approver, cancel)
                .await;
        }
        let mut results: Vec<ContentBlock> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let mut executed: Vec<String> = Vec::new();
        let mut declined = false;
        let mut awaiting_user_input = false;
        let mut interrupted_at: Option<usize> = None;

        for (i, (id, name, input)) in calls.iter().enumerate() {
            if cancel.is_cancelled() {
                interrupted_at = Some(i);
                results.push(tool_result(id, "Cancelled by user before execution.", true));
                continue;
            }
            if declined {
                results.push(tool_result(
                    id,
                    "Not executed: a previous tool call in this batch was declined.",
                    true,
                ));
                continue;
            }
            let Some(tool) = self.tool(name) else {
                results.push(tool_result(
                    id,
                    &format!(
                        "Unknown tool '{name}'. Available tools: {}",
                        self.tools
                            .iter()
                            .map(|t| t.name())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    true,
                ));
                continue;
            };

            let request = tool.permission(input);
            let mut approval_note: Option<String> = None;
            let is_user_question = matches!(request, PermissionRequest::UserInput { .. });
            match session.rules.decide(session.mode, &request) {
                Decision::Allow => {}
                Decision::Deny(reason) => {
                    results.push(tool_result(&id.clone(), &reason, true));
                    continue;
                }
                Decision::Ask => {
                    let (descriptor, summary) = match &request {
                        PermissionRequest::Ask {
                            descriptor, summary, ..
                        }
                        | PermissionRequest::UserInput {
                            descriptor, summary,
                        } => (descriptor, summary),
                        PermissionRequest::None => unreachable!("Ask decision implies a prompt"),
                    };
                    let approval = approver.ask(name, summary, descriptor, input).await;
                    match approval.decision {
                        ApprovalDecision::Yes => approval_note = approval.comment,
                        ApprovalDecision::YesAlways => {
                            if !is_user_question {
                                session.rules.allow.push(descriptor.clone());
                            }
                            approval_note = approval.comment;
                        }
                        ApprovalDecision::No => {
                            declined = true;
                            let Some(comment) = approval.comment else {
                                awaiting_user_input = true;
                                results.push(tool_result(
                                    id,
                                    "User declined this action without further guidance. Stop now and wait for the user's next instruction; do not guess an alternative.",
                                    true,
                                ));
                                continue;
                            };
                            results.push(tool_result(
                                id,
                                &format!("User declined this action. Reason: {comment}"),
                                true,
                            ));
                            continue;
                        }
                    }
                }
            }

            // Pre-tool hooks may veto the call (exit code 2).
            let pre = self
                .hooks
                .run(
                    crate::hooks::HookEvent::PreToolUse,
                    name,
                    input,
                    None,
                    &session.tool_ctx.cwd,
                )
                .await;
            if let Some(reason) = pre.block {
                results.push(tool_result(
                    id,
                    &format!("Blocked by pre-tool hook: {reason}"),
                    true,
                ));
                continue;
            }
            notes.extend(pre.notes);

            self.emit(
                events,
                AgentEvent::ToolStart {
                    name: name.clone(),
                    summary: summarize_call(name, input),
                    input: input.clone(),
                },
            )
            .await?;
            // Checkpoint the file this call is about to mutate.
            if let Some(raw) = tool.touches(input) {
                let path = session.tool_ctx.resolve(&raw);
                let len = session.ledger.len();
                if let Some(ev) = session.checkpoints.save(len, &path) {
                    session.ledger.record_aux(&ev);
                }
            }
            let mut output = {
                let (usage_tx, mut usage_rx) = mpsc::unbounded_channel();
                session.tool_ctx.set_usage_reporter(usage_tx);
                let run = tool.run(input.clone(), &session.tool_ctx, cancel);
                tokio::pin!(run);
                let output = loop {
                    tokio::select! {
                        Some(usage) = usage_rx.recv() => {
                            self.emit(events, AgentEvent::DelegatedUsage(usage)).await?;
                        }
                        output = &mut run => break output,
                    }
                };
                session.tool_ctx.clear_usage_reporter();
                output
            };
            // Post-tool hooks (e.g. a formatter after edit): their
            // failures are appended so the model sees them immediately.
            let post = self
                .hooks
                .run(
                    crate::hooks::HookEvent::PostToolUse,
                    name,
                    input,
                    Some(&output.content),
                    &session.tool_ctx.cwd,
                )
                .await;
            for note in post.notes {
                output.content.push_str(&format!("\n[hook] {note}"));
            }
            let output = self.gate(session, name, output);
            self.emit(
                events,
                AgentEvent::ToolEnd {
                    name: name.clone(),
                    preview: preview(&output.content),
                    content: output.content.clone(),
                    is_error: output.is_error,
                },
            )
            .await?;
            executed.push(name.clone());
            results.push(tool_result(id, &output.content, output.is_error));
            if let Some(note) = approval_note {
                if is_user_question {
                    notes.push(format!("User answered {name}: {note}"));
                } else {
                    notes.push(format!("Note from the user when approving {name}: {note}"));
                }
            }
        }

        session.ledger.append(Entry::ToolResults(results));
        for note in notes {
            session.ledger.append(Entry::Note(note));
        }

        if let Some(at) = interrupted_at {
            let cancelled: Vec<String> =
                calls[at..].iter().map(|(_, n, _)| n.clone()).collect();
            self.commit_interrupt(session, &[], &cancelled, false);
            // Executed calls before the cut already have real results.
            let _ = executed;
            return Ok(ToolsOutcome {
                interrupted: true,
                awaiting_user_input: false,
            });
        }
        Ok(ToolsOutcome {
            interrupted: false,
            awaiting_user_input,
        })
    }

    /// Models often request several independent reads/searches at once. Run
    /// only this read-only subset concurrently; all mutating, shell, question
    /// and sub-agent calls remain ordered so their approvals and side effects
    /// are unambiguous. Results are still appended in model-call order.
    async fn run_read_only_tools_parallel(
        &self,
        session: &mut Session,
        calls: &[(String, String, Value)],
        events: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<ToolsOutcome, AgentError> {
        let mut prepared = Vec::new();
        let mut results = Vec::new();
        let mut notes = Vec::new();
        for (id, name, input) in calls {
            let Some(tool) = self.tool(name).cloned() else {
                results.push(tool_result(id, &format!("Unknown tool '{name}'"), true));
                continue;
            };
            let pre = self
                .hooks
                .run(
                    crate::hooks::HookEvent::PreToolUse,
                    name,
                    input,
                    None,
                    &session.tool_ctx.cwd,
                )
                .await;
            notes.extend(pre.notes);
            if let Some(reason) = pre.block {
                results.push(tool_result(id, &format!("Blocked by pre-tool hook: {reason}"), true));
                continue;
            }
            prepared.push((id.clone(), name.clone(), input.clone(), tool));
        }

        self.emit(
            events,
            AgentEvent::ToolBatchStart {
                label: batch_label(&prepared),
                calls: prepared
                    .iter()
                    .map(|(_, name, input, _)| (name.clone(), input.clone()))
                    .collect(),
            },
        )
        .await?;

        let outputs = join_all(prepared.iter().map(|(_, _, input, tool)| {
            tool.run(input.clone(), &session.tool_ctx, cancel)
        }))
        .await;
        for ((id, name, input, _), mut output) in prepared.into_iter().zip(outputs) {
            let post = self
                .hooks
                .run(
                    crate::hooks::HookEvent::PostToolUse,
                    &name,
                    &input,
                    Some(&output.content),
                    &session.tool_ctx.cwd,
                )
                .await;
            for note in post.notes {
                output.content.push_str(&format!("\n[hook] {note}"));
            }
            let output = self.gate(session, &name, output);
            self.emit(
                events,
                AgentEvent::ToolEnd {
                    name,
                    preview: preview(&output.content),
                    content: output.content.clone(),
                    is_error: output.is_error,
                },
            )
            .await?;
            results.push(tool_result(&id, &output.content, output.is_error));
        }
        session.ledger.append(Entry::ToolResults(results));
        for note in notes {
            session.ledger.append(Entry::Note(note));
        }
        Ok(ToolsOutcome {
            interrupted: cancel.is_cancelled(),
            awaiting_user_input: false,
        })
    }

    /// `edit`/`write` calls targeting different normalized paths can safely
    /// share a turn.  This remains deliberately narrow: shell commands and
    /// arbitrary tools keep their normal, ordered semantics.
    fn is_parallel_file_mutation_batch(
        &self,
        session: &Session,
        calls: &[(String, String, Value)],
    ) -> bool {
        let mut paths = HashSet::new();
        calls.iter().all(|(_, name, input)| {
            if !matches!(name.as_str(), "edit" | "write") {
                return false;
            }
            let Some(tool) = self.tool(name) else {
                return false;
            };
            let Some(path) = tool.touches(input) else {
                return false;
            };
            paths.insert(normalize_path(session.tool_ctx.resolve(&path)))
        })
    }

    /// Execute a preflighted, independent edit/write batch.  The preflight is
    /// intentionally all-or-nothing: approval, pre-hook vetoes and snapshots
    /// happen before the first file is touched.
    async fn run_file_mutations_parallel(
        &self,
        session: &mut Session,
        calls: &[(String, String, Value)],
        events: &mpsc::Sender<AgentEvent>,
        approver: &dyn Approver,
        cancel: &CancellationToken,
    ) -> Result<ToolsOutcome, AgentError> {
        let mut prepared: Vec<(String, String, Value, Arc<dyn Tool>)> = Vec::new();
        let mut notes = Vec::new();

        for (id, name, input) in calls {
            if cancel.is_cancelled() {
                return self.cancel_unstarted_batch(session, calls, true);
            }
            // `is_parallel_file_mutation_batch` established this already.
            let tool = self.tool(name).expect("preflighted tool").clone();
            match session.rules.decide(session.mode, &tool.permission(input)) {
                Decision::Allow => {}
                Decision::Deny(reason) => {
                    return self.abort_file_batch(session, calls, id, &reason, false);
                }
                Decision::Ask => {
                    let PermissionRequest::Ask {
                        descriptor, summary, ..
                    } = tool.permission(input)
                    else {
                        unreachable!("file mutation needs an edit approval")
                    };
                    let approval = approver.ask(name, &summary, &descriptor, input).await;
                    match approval.decision {
                        ApprovalDecision::Yes => {
                            if let Some(note) = approval.comment {
                                notes.push(format!("Note from the user when approving {name}: {note}"));
                            }
                        }
                        ApprovalDecision::YesAlways => {
                            session.rules.allow.push(descriptor);
                            if let Some(note) = approval.comment {
                                notes.push(format!("Note from the user when approving {name}: {note}"));
                            }
                        }
                        ApprovalDecision::No => {
                            let (reason, awaiting_user_input) = match approval.comment {
                                Some(comment) => (
                                    format!("User declined this action. Reason: {comment}"),
                                    false,
                                ),
                                None => (
                                    "User declined this action without further guidance. Stop now and wait for the user's next instruction; do not guess an alternative.".to_string(),
                                    true,
                                ),
                            };
                            return self.abort_file_batch(
                                session,
                                calls,
                                id,
                                &reason,
                                awaiting_user_input,
                            );
                        }
                    }
                }
            }

            let pre = self
                .hooks
                .run(
                    crate::hooks::HookEvent::PreToolUse,
                    name,
                    input,
                    None,
                    &session.tool_ctx.cwd,
                )
                .await;
            if let Some(reason) = pre.block {
                return self.abort_file_batch(
                    session,
                    calls,
                    id,
                    &format!("Blocked by pre-tool hook: {reason}"),
                    false,
                );
            }
            notes.extend(pre.notes);
            prepared.push((id.clone(), name.clone(), input.clone(), tool));
        }

        // Save every original before a concurrent task gets a chance to
        // change it. `touches` is guaranteed by the batch predicate.
        for (_, _, input, tool) in &prepared {
            let path = session.tool_ctx.resolve(&tool.touches(input).expect("preflighted path"));
            let len = session.ledger.len();
            if let Some(ev) = session.checkpoints.save(len, &path) {
                session.ledger.record_aux(&ev);
            }
        }
        self.emit(
            events,
            AgentEvent::ToolBatchStart {
                label: batch_label(&prepared),
                calls: prepared
                    .iter()
                    .map(|(_, name, input, _)| (name.clone(), input.clone()))
                    .collect(),
            },
        )
        .await?;

        let outputs = join_all(prepared.iter().map(|(_, _, input, tool)| {
            tool.run(input.clone(), &session.tool_ctx, cancel)
        }))
        .await;
        let mut results = Vec::new();
        for ((id, name, input, _), mut output) in prepared.into_iter().zip(outputs) {
            let post = self
                .hooks
                .run(
                    crate::hooks::HookEvent::PostToolUse,
                    &name,
                    &input,
                    Some(&output.content),
                    &session.tool_ctx.cwd,
                )
                .await;
            for note in post.notes {
                output.content.push_str(&format!("\n[hook] {note}"));
            }
            let output = self.gate(session, &name, output);
            self.emit(
                events,
                AgentEvent::ToolEnd {
                    name,
                    preview: preview(&output.content),
                    content: output.content.clone(),
                    is_error: output.is_error,
                },
            )
            .await?;
            results.push(tool_result(&id, &output.content, output.is_error));
        }
        session.ledger.append(Entry::ToolResults(results));
        for note in notes {
            session.ledger.append(Entry::Note(note));
        }
        Ok(ToolsOutcome {
            interrupted: cancel.is_cancelled(),
            awaiting_user_input: false,
        })
    }

    fn abort_file_batch(
        &self,
        session: &mut Session,
        calls: &[(String, String, Value)],
        declined_id: &str,
        reason: &str,
        awaiting_user_input: bool,
    ) -> Result<ToolsOutcome, AgentError> {
        let results = calls
            .iter()
            .map(|(id, _, _)| {
                let message = if id == declined_id {
                    reason.to_string()
                } else {
                    "Not executed: the independent edit batch was not fully approved.".to_string()
                };
                tool_result(id, &message, true)
            })
            .collect();
        session.ledger.append(Entry::ToolResults(results));
        Ok(ToolsOutcome {
            interrupted: false,
            awaiting_user_input,
        })
    }

    fn cancel_unstarted_batch(
        &self,
        session: &mut Session,
        calls: &[(String, String, Value)],
        interrupted: bool,
    ) -> Result<ToolsOutcome, AgentError> {
        self.commit_interrupt(session, calls, &[], false);
        Ok(ToolsOutcome {
            interrupted,
            awaiting_user_input: false,
        })
    }

    /// Interrupt contract: tell the model exactly what happened so it
    /// never wastes tokens re-verifying state after an interrupt.
    fn commit_interrupt(
        &self,
        session: &mut Session,
        unstarted_calls: &[(String, String, Value)],
        cancelled_names: &[String],
        dropped_malformed: bool,
    ) {
        // Every committed tool_use must get a result (API invariant).
        if !unstarted_calls.is_empty() {
            let results: Vec<ContentBlock> = unstarted_calls
                .iter()
                .map(|(id, _, _)| tool_result(id, "Cancelled by user before execution.", true))
                .collect();
            session.ledger.append(Entry::ToolResults(results));
        }
        let mut msg = String::from("The user interrupted this turn.");
        let all_cancelled: Vec<&str> = unstarted_calls
            .iter()
            .map(|(_, n, _)| n.as_str())
            .chain(cancelled_names.iter().map(|s| s.as_str()))
            .collect();
        if all_cancelled.is_empty() {
            msg.push_str(" No tool calls were pending.");
        } else {
            msg.push_str(&format!(
                " Cancelled tool call(s) that did NOT run: {}. They made no changes to any file or system state.",
                all_cancelled.join(", ")
            ));
        }
        if dropped_malformed {
            msg.push_str(" An incomplete tool call was discarded mid-stream; it did not run.");
        }
        msg.push_str(
            " Results shown for earlier completed tool calls remain valid — do not re-verify them. Wait for the user's next instruction.",
        );
        session.ledger.append(Entry::Note(msg));
    }

    /// Central token-budget gate for tool outputs.
    fn gate(&self, session: &mut Session, tool: &str, output: ToolOutput) -> ToolOutput {
        let mut blobs = session.tool_ctx.blobs.lock().expect("blobs lock");
        let content = if output.is_error {
            output.content
        } else {
            compact_successful_test_output(output.content)
        };
        ToolOutput {
            content: blobs.gate(tool, content),
            is_error: output.is_error,
        }
    }

    async fn emit(
        &self,
        events: &mpsc::Sender<AgentEvent>,
        ev: AgentEvent,
    ) -> Result<(), AgentError> {
        events.send(ev).await.map_err(|_| AgentError::ChannelClosed)
    }
}

struct ToolsOutcome {
    interrupted: bool,
    awaiting_user_input: bool,
}

fn is_parallel_read_only_tool(name: &str) -> bool {
    matches!(name, "read" | "grep" | "glob" | "read_output")
}

fn batch_label(prepared: &[(String, String, Value, Arc<dyn Tool>)]) -> String {
    let count = prepared.len();
    let names: HashSet<&str> = prepared.iter().map(|(_, name, _, _)| name.as_str()).collect();
    if names.len() == 1 {
        let name = names.into_iter().next().unwrap_or("tool");
        let display = match name {
            "read" => "Read",
            "write" => "Write",
            "edit" => "Edit",
            "grep" => "Search",
            "glob" => "Find",
            other => other,
        };
        format!("{display} {count} {}", if count == 1 { "file" } else { "files" })
    } else {
        format!("Run {count} tools")
    }
}

/// Lexically normalize paths for the batch collision check.  We cannot rely
/// on `canonicalize`: a `write` target may not exist yet.  Resolving relative
/// paths against the session cwd first makes this conservative enough to spot
/// aliases such as `src/../src/lib.rs` before concurrent execution.
fn normalize_path(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Normal(segment) => normalized.push(segment),
        }
    }
    normalized
}

/// An interrupted stream can leave a tool_use whose input JSON never
/// finished; the accumulator falls back to a raw string for those.
/// They must not be replayed to the API.
fn split_malformed(blocks: Vec<ContentBlock>) -> (Vec<ContentBlock>, bool) {
    let mut dropped = false;
    let kept = blocks
        .into_iter()
        .filter(|b| match b {
            ContentBlock::ToolUse {
                input: Value::String(_),
                ..
            } => {
                dropped = true;
                false
            }
            _ => true,
        })
        .collect();
    (kept, dropped)
}

fn tool_result(id: &str, content: &str, is_error: bool) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content: content.to_string(),
        is_error,
    }
}

fn preview(s: &str) -> String {
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

/// Successful test runs often contain several nearly-identical target blocks
/// (especially doctests and crates with zero tests).  Keep the evidence that
/// matters to both the human and model while avoiding needless context use.
/// Any error-like marker leaves the original output untouched for diagnosis.
fn compact_successful_test_output(output: String) -> String {
    if !(output.contains("test result: ok.")
        && output.contains("running ")
        && !output.contains("test result: FAILED")
        && !output.contains("error:")
        && !output.contains("failures:"))
    {
        return output;
    }
    let running: Vec<&str> = output
        .lines()
        .filter(|line| line.trim_start().starts_with("running ") && line.contains(" tests"))
        .collect();
    let Some(first) = running
        .iter()
        .copied()
        .find(|line| !line.contains("running 0 tests"))
    else {
        return output;
    };
    let passed = output
        .lines()
        .filter(|line| line.trim_start().starts_with("test result: ok."))
        .find(|line| !line.contains("0 passed"))
        .unwrap_or("test result: ok.");
    format!("{first}\n… successful test output folded …\n{passed}")
}

/// One-line description of a call for the UI, e.g. `shell(cargo build)`.
pub fn summarize_call(name: &str, input: &Value) -> String {
    let arg = ["command", "path", "pattern", "id", "agent"]
        .iter()
        .find_map(|k| input.get(k).and_then(|v| v.as_str()))
        .unwrap_or("");
    if arg.is_empty() {
        name.to_string()
    } else {
        format!("{name}({arg})")
    }
}

#[cfg(test)]
mod tests {
    use super::compact_successful_test_output;

    #[test]
    fn folds_repeated_successful_test_blocks() {
        let output = "running 24 tests\n........................\ntest result: ok. 24 passed; 0 failed\n\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed";
        let folded = compact_successful_test_output(output.into());
        assert!(folded.contains("running 24 tests"));
        assert!(folded.contains("folded"));
        assert!(!folded.contains("running 0 tests"));
    }

    #[test]
    fn retains_failed_test_output() {
        let output = "running 2 tests\ntest result: FAILED. 1 passed; 1 failed";
        assert_eq!(compact_successful_test_output(output.into()), output);
    }
}
