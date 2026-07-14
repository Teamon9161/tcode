mod compact;
mod session;
mod summarize;

pub use session::{CwdChange, Session};
pub use summarize::summarize_call;
use summarize::{compact_successful_test_output, preview, split_malformed};

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use futures::{future::join_all, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::accumulate::ResponseAccumulator;
use crate::config::WatchdogConfig;
use crate::ledger::Entry;
use crate::permission::{ApprovalDecision, Approver, Decision};
use crate::provider::{ProviderError, Request, StreamEvent};
use crate::tool::{BatchPolicy, PermissionRequest, Tool, ToolOutput};
use crate::types::{ContentBlock, RateLimits, StopReason, Usage};

/// Default ceiling on model round-trips per user turn; a runaway loop
/// should never bill unbounded. Configurable via `limits.max_steps_per_turn`.
pub const DEFAULT_MAX_STEPS: usize = 100;

/// Appended to the system prompt while `/dogfood` is on.
const DOGFOOD_SYSTEM: &str = include_str!("../../../../prompts/dogfood.md");

/// One-way events for the UI. Approval prompts go the other way through
/// the `Approver` trait.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Model accepted the request and is responding.
    Started,
    TextDelta(String),
    ThinkingDelta(String),
    /// Streaming fragment of a tool call's JSON arguments. Nothing here is
    /// rendered — it exists so token meters reflect that the model is actively
    /// producing output while assembling a tool call, instead of appearing
    /// frozen until the finished call arrives as `ToolStart`.
    ToolInputDelta(String),
    /// Streaming failed mid-turn; the request is being re-sent. Any partial
    /// assistant text was committed as transcript-only history, so the UI
    /// should bake rather than discard its live block.
    Retrying {
        attempt: u32,
        max: u32,
        error: String,
        /// Whether streamed assistant text was retained in transcript-only
        /// history before this retry.
        partial_output_retained: bool,
        /// How long the loop will wait before the next attempt, so the UI can
        /// show a live countdown.
        delay_ms: u64,
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
    /// The runaway guard ended the turn. Nothing is lost: the ledger is
    /// consistent and the user can simply ask to continue.
    StepLimitReached {
        max: usize,
    },
    Interrupted,
    TurnEnd,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("event channel closed")]
    ChannelClosed,
}

pub struct Agent {
    /// Swappable model handle; each turn snapshots it once.
    pub model: crate::provider::ModelCell,
    pub tools: Vec<Arc<dyn Tool>>,
    pub system: String,
    pub watchdog: WatchdogConfig,
    pub hooks: crate::hooks::Hooks,
    /// Runaway guard: model round-trips per user turn before the harness
    /// ends the turn gracefully.
    pub max_steps: usize,
}

impl Agent {
    fn tool(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name)
    }

    fn tool_defs(&self) -> Vec<crate::ToolDef> {
        self.tools.iter().map(|t| t.as_ref().def()).collect()
    }

    pub(super) fn system_prompt(&self, session: &Session) -> String {
        let mut prompt = self.system.clone();
        if !session.opening_context().is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(session.opening_context());
        }
        if session.dogfood() {
            prompt.push_str("\n\n");
            prompt.push_str(DOGFOOD_SYSTEM);
        }
        prompt
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
        // Background tasks that finished between turns: tell the model
        // before its new instruction (pure append, cache-safe).
        self.note_finished_background_tasks(session);
        if let Some(reminder) = session
            .tool_ctx
            .memory
            .lock()
            .expect("memory lock")
            .maintenance_reminder()
        {
            input.push(ContentBlock::Text { text: reminder });
        }
        if let Some(status) = session.status_block(model.context_window) {
            input.push(status);
        }
        session.ledger.append(Entry::User(input));
        session.turn_usage = Usage::default();

        let max_steps = self.max_steps.max(1);
        // One-shot heads-up at ~80% so the model wraps up instead of being
        // cut off mid-exploration.
        let warn_at = max_steps * 4 / 5;
        for step in 0..max_steps {
            let (blocks, usage, stop) = self.stream_step(&model, session, events, &cancel).await?;
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
            // End of a tool batch is a safe append boundary for background
            // task completion notes.
            self.note_finished_background_tasks(session);
            if step + 1 == warn_at {
                session.ledger.append(Entry::Note(format!(
                    "step {} of {max_steps} for this turn; the harness ends \
                     the turn at the limit. Finish up or report progress \
                     instead of starting new exploration.",
                    step + 1
                )));
            }
        }
        // Runaway guard tripped. The ledger is consistent (the last tool
        // batch committed its results), so end the turn instead of erroring:
        // the user can review and simply ask to continue.
        session.ledger.append(Entry::Note(format!(
            "Turn ended by the harness after {max_steps} model steps \
             (runaway guard). Nothing was lost; continue where you left off \
             when the user asks."
        )));
        self.emit(events, AgentEvent::StepLimitReached { max: max_steps })
            .await?;
        self.emit(events, AgentEvent::TurnEnd).await?;
        Ok(())
    }

    /// One model request with watchdog retries. Text emitted by a failed
    /// attempt is preserved as transcript-only history before the retry.
    async fn stream_step(
        &self,
        model: &crate::provider::ActiveModel,
        session: &mut Session,
        events: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<(Vec<ContentBlock>, Usage, Option<StopReason>), AgentError> {
        let mut attempt = 0u32;
        'retry: loop {
            let req = Request {
                model: model.provider.model().to_string(),
                system: self.system_prompt(session),
                messages: session.ledger.as_messages(),
                tools: self.tool_defs(),
                max_tokens: model.max_tokens,
                effort: model.effort.clone(),
            };
            // The provider does a single connection attempt; this loop owns all
            // retries (connect failures and mid-stream stalls alike) so every
            // one is visible and backs off exponentially.
            let mut stream = match model.provider.stream(req, cancel.clone()).await {
                Ok(stream) => stream,
                Err(e) if e.retryable() && attempt < self.watchdog.max_retries => {
                    attempt += 1;
                    self.emit_retry(events, attempt, &e.to_string(), false)
                        .await?;
                    continue 'retry;
                }
                Err(e) => return Err(e.into()),
            };
            let mut acc = ResponseAccumulator::new();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(ev) => {
                        match &ev {
                            StreamEvent::Started => self.emit(events, AgentEvent::Started).await?,
                            StreamEvent::TextDelta(t) => {
                                self.emit(events, AgentEvent::TextDelta(t.clone())).await?
                            }
                            StreamEvent::ThinkingDelta(t) => {
                                self.emit(events, AgentEvent::ThinkingDelta(t.clone()))
                                    .await?
                            }
                            StreamEvent::ToolUseInputDelta { fragment, .. } => {
                                self.emit(events, AgentEvent::ToolInputDelta(fragment.clone()))
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
                        let partial_output_retained =
                            self.preserve_incomplete_assistant(session, acc, &e);
                        attempt += 1;
                        self.emit_retry(events, attempt, &e.to_string(), partial_output_retained)
                            .await?;
                        continue 'retry;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            let (blocks, usage, stop) = acc.finish();
            return Ok((blocks, usage, stop));
        }
    }

    fn preserve_incomplete_assistant(
        &self,
        session: &mut Session,
        acc: ResponseAccumulator,
        error: &ProviderError,
    ) -> bool {
        let (blocks, _, _) = acc.finish();
        let text = blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        if text.trim().is_empty() {
            return false;
        }
        session.ledger.append(Entry::IncompleteAssistant {
            text,
            error: error.to_string(),
        });
        true
    }

    /// Announce a retry and wait out the exponential backoff. The event carries
    /// the delay so the UI can render a live countdown.
    async fn emit_retry(
        &self,
        events: &mpsc::Sender<AgentEvent>,
        attempt: u32,
        error: &str,
        partial_output_retained: bool,
    ) -> Result<(), AgentError> {
        let delay = self.watchdog.backoff(attempt);
        self.emit(
            events,
            AgentEvent::Retrying {
                attempt,
                max: self.watchdog.max_retries,
                error: error.to_string(),
                partial_output_retained,
                delay_ms: delay.as_millis() as u64,
            },
        )
        .await?;
        tokio::time::sleep(delay).await;
        Ok(())
    }

    async fn run_tools(
        &self,
        session: &mut Session,
        calls: &[(String, String, Value)],
        events: &mpsc::Sender<AgentEvent>,
        approver: &dyn Approver,
        cancel: &CancellationToken,
    ) -> Result<ToolsOutcome, AgentError> {
        let memory_note = self.preflight_memory(session, calls);
        if let Some(note) = &memory_note {
            let has_mutation = calls.iter().any(|(_, name, input)| {
                self.tool(name)
                    .is_some_and(|tool| tool.is_mutating() && !tool.context_paths(input).is_empty())
            });
            if has_mutation {
                let message = "Not executed: new directory-scoped instructions were loaded for this tool batch. Review them and retry only the actions that still comply; no side effects occurred.";
                let results = calls
                    .iter()
                    .map(|(id, _, _)| tool_result(id, message, true))
                    .collect();
                session.ledger.append(Entry::ToolResults(results));
                session.ledger.append(Entry::Note(note.clone()));
                return Ok(ToolsOutcome {
                    interrupted: false,
                    awaiting_user_input: false,
                });
            }
        }
        if calls.len() > 1 && self.all_calls_have_policy(calls, BatchPolicy::ParallelReadOnly) {
            return self
                .run_read_only_tools_parallel(session, calls, events, cancel, memory_note)
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
        // Shell / bash: batch the approval (show all commands at once),
        // then run sequentially so side effects from earlier commands
        // are visible to later ones.
        if calls.len() > 1 && self.all_calls_have_policy(calls, BatchPolicy::SequentialBatch) {
            return self
                .run_sequential_batch_combined_approval(
                    session,
                    calls,
                    events,
                    approver,
                    cancel,
                    memory_note,
                )
                .await;
        }
        let mut results: Vec<ContentBlock> = Vec::new();
        let mut notes: Vec<String> = memory_note.into_iter().collect();
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
                            descriptor,
                            summary,
                            ..
                        }
                        | PermissionRequest::UserInput {
                            descriptor,
                            summary,
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
            if !output.is_error {
                if let Some(raw) = tool.touches(input) {
                    let path = session.tool_ctx.resolve(&raw);
                    session
                        .tool_ctx
                        .memory
                        .lock()
                        .expect("memory lock")
                        .mark_written(&path);
                }
            }
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
            results.push(tool_result_with_images(
                id,
                &output.content,
                output.is_error,
                output.images.clone(),
            ));
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
            let cancelled: Vec<String> = calls[at..].iter().map(|(_, n, _)| n.clone()).collect();
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
        memory_note: Option<String>,
    ) -> Result<ToolsOutcome, AgentError> {
        let mut prepared = Vec::new();
        let mut results = Vec::new();
        let mut notes: Vec<String> = memory_note.into_iter().collect();
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
                results.push(tool_result(
                    id,
                    &format!("Blocked by pre-tool hook: {reason}"),
                    true,
                ));
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

        let outputs = join_all(
            prepared
                .iter()
                .map(|(_, _, input, tool)| tool.run(input.clone(), &session.tool_ctx, cancel)),
        )
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
            results.push(tool_result_with_images(
                &id,
                &output.content,
                output.is_error,
                output.images.clone(),
            ));
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
            let Some(tool) = self.tool(name) else {
                return false;
            };
            if tool.batch_policy() != BatchPolicy::ParallelPerFile {
                return false;
            }
            let Some(path) = tool.touches(input) else {
                return false;
            };
            paths.insert(normalize_path(session.tool_ctx.resolve(&path)))
        })
    }

    /// The batch header these calls (all from one assistant message) ran
    /// under, or `None` if they ran as ordinary sequential calls. The live
    /// loop decides batching right below; replay asks *this* instead of
    /// re-deriving the rule, so a resumed transcript groups calls exactly as
    /// they were executed.
    pub fn batch_display_label(
        &self,
        session: &Session,
        calls: &[(String, String, Value)],
    ) -> Option<String> {
        if calls.len() < 2 {
            return None;
        }
        if self.all_calls_have_policy(calls, BatchPolicy::SequentialBatch) {
            return Some(sequential_batch_label(calls.len()));
        }
        if !self.all_calls_have_policy(calls, BatchPolicy::ParallelReadOnly)
            && !self.is_parallel_file_mutation_batch(session, calls)
        {
            return None;
        }
        let prepared: Vec<(String, String, Value, Arc<dyn Tool>)> = calls
            .iter()
            .map(|(id, name, input)| {
                self.tool(name)
                    .map(|tool| (id.clone(), name.clone(), input.clone(), tool.clone()))
            })
            .collect::<Option<_>>()?;
        Some(batch_label(&prepared))
    }

    /// Whether every call in the batch belongs to a tool declaring `policy`.
    fn all_calls_have_policy(
        &self,
        calls: &[(String, String, Value)],
        policy: BatchPolicy,
    ) -> bool {
        calls
            .iter()
            .all(|(_, name, _)| self.tool(name).is_some_and(|t| t.batch_policy() == policy))
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
                        descriptor,
                        summary,
                        ..
                    } = tool.permission(input)
                    else {
                        unreachable!("file mutation needs an edit approval")
                    };
                    let approval = approver.ask(name, &summary, &descriptor, input).await;
                    match approval.decision {
                        ApprovalDecision::Yes => {
                            if let Some(note) = approval.comment {
                                notes.push(format!(
                                    "Note from the user when approving {name}: {note}"
                                ));
                            }
                        }
                        ApprovalDecision::YesAlways => {
                            session.rules.allow.push(descriptor);
                            if let Some(note) = approval.comment {
                                notes.push(format!(
                                    "Note from the user when approving {name}: {note}"
                                ));
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
            let path = session
                .tool_ctx
                .resolve(&tool.touches(input).expect("preflighted path"));
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

        let outputs = join_all(
            prepared
                .iter()
                .map(|(_, _, input, tool)| tool.run(input.clone(), &session.tool_ctx, cancel)),
        )
        .await;
        let mut results = Vec::new();
        for ((id, name, input, _), mut output) in prepared.into_iter().zip(outputs) {
            if !output.is_error {
                if let Some(tool) = self.tool(&name) {
                    if let Some(raw) = tool.touches(&input) {
                        let path = session.tool_ctx.resolve(&raw);
                        session
                            .tool_ctx
                            .memory
                            .lock()
                            .expect("memory lock")
                            .mark_written(&path);
                    }
                }
            }
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
            results.push(tool_result_with_images(
                &id,
                &output.content,
                output.is_error,
                output.images.clone(),
            ));
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

    /// Run multiple shell/bash calls with a single combined approval step.
    /// All commands are shown in the batch display, the user approves once,
    /// then they run sequentially so side effects are visible to later calls.
    async fn run_sequential_batch_combined_approval(
        &self,
        session: &mut Session,
        calls: &[(String, String, Value)],
        events: &mpsc::Sender<AgentEvent>,
        approver: &dyn Approver,
        cancel: &CancellationToken,
        memory_note: Option<String>,
    ) -> Result<ToolsOutcome, AgentError> {
        // Pre-flight: gather tools, hooks, and build the batch label.
        let mut prepared: Vec<(String, String, Value, Arc<dyn Tool>)> = Vec::new();
        let mut notes: Vec<String> = memory_note.into_iter().collect();
        for (id, name, input) in calls {
            let Some(tool) = self.tool(name).cloned() else {
                return Ok(ToolsOutcome {
                    interrupted: false,
                    awaiting_user_input: false,
                });
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
                let results: Vec<ContentBlock> = calls
                    .iter()
                    .map(|(cid, _, _)| {
                        let msg = if cid == id {
                            format!("Blocked by pre-tool hook: {reason}")
                        } else {
                            "Not executed: a previous tool call in this batch was blocked.".into()
                        };
                        tool_result(cid, &msg, true)
                    })
                    .collect();
                session.ledger.append(Entry::ToolResults(results));
                return Ok(ToolsOutcome {
                    interrupted: false,
                    awaiting_user_input: false,
                });
            }
            prepared.push((id.clone(), name.clone(), input.clone(), tool));
        }

        let batch_label = sequential_batch_label(prepared.len());

        // Combined approval: ask once for the whole batch.
        for (id, name, input, tool) in &prepared {
            let request = tool.permission(input);
            match session.rules.decide(session.mode, &request) {
                Decision::Allow => {}
                Decision::Deny(reason) => {
                    let results: Vec<ContentBlock> = calls
                        .iter()
                        .map(|(cid, _, _)| {
                            tool_result(
                                cid,
                                if cid == id {
                                    &reason
                                } else {
                                    "Not executed: a previous tool call in this batch was declined."
                                },
                                true,
                            )
                        })
                        .collect();
                    session.ledger.append(Entry::ToolResults(results));
                    return Ok(ToolsOutcome {
                        interrupted: false,
                        awaiting_user_input: false,
                    });
                }
                Decision::Ask => {
                    let (PermissionRequest::Ask {
                        descriptor,
                        summary,
                        ..
                    }
                    | PermissionRequest::UserInput {
                        descriptor,
                        summary,
                    }) = &request
                    else {
                        unreachable!()
                    };
                    let approval = approver.ask(name, summary, descriptor, input).await;
                    match approval.decision {
                        ApprovalDecision::Yes => {
                            if let Some(note) = approval.comment {
                                notes.push(format!(
                                    "Note from the user when approving {name}: {note}"
                                ));
                            }
                        }
                        ApprovalDecision::YesAlways => {
                            if !matches!(request, PermissionRequest::UserInput { .. }) {
                                session.rules.allow.push(descriptor.clone());
                            }
                            if let Some(note) = approval.comment {
                                notes.push(format!(
                                    "Note from the user when approving {name}: {note}"
                                ));
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
                            let results: Vec<ContentBlock> = calls
                                .iter()
                                .map(|(cid, _, _)| {
                                    tool_result(
                                        cid,
                                        if cid == id {
                                            &reason
                                        } else {
                                            "Not executed: a previous tool call in this batch was declined."
                                        },
                                        true,
                                    )
                                })
                                .collect();
                            session.ledger.append(Entry::ToolResults(results));
                            return Ok(ToolsOutcome {
                                interrupted: false,
                                awaiting_user_input,
                            });
                        }
                    }
                    // Only ask once for the whole batch
                    break;
                }
            }
        }

        // Emit batch start for display.
        let batch_calls: Vec<(String, Value)> = prepared
            .iter()
            .map(|(_, name, input, _)| (name.clone(), input.clone()))
            .collect();
        self.emit(
            events,
            AgentEvent::ToolBatchStart {
                label: batch_label,
                calls: batch_calls,
            },
        )
        .await?;

        // Run sequentially.
        let mut results: Vec<ContentBlock> = Vec::new();
        for (id, name, input, tool) in &prepared {
            if cancel.is_cancelled() {
                results.push(tool_result(id, "Cancelled by user before execution.", true));
                let cancelled: Vec<ContentBlock> = prepared
                    .iter()
                    .skip(results.len())
                    .map(|(cid, _, _, _)| {
                        tool_result(cid, "Cancelled by user before execution.", true)
                    })
                    .collect();
                results.extend(cancelled);
                break;
            }
            self.emit(
                events,
                AgentEvent::ToolStart {
                    name: name.clone(),
                    summary: summarize_call(name, input),
                    input: input.clone(),
                },
            )
            .await?;

            let mut output = tool.run(input.clone(), &session.tool_ctx, cancel).await;

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
            results.push(tool_result_with_images(
                id,
                &output.content,
                output.is_error,
                output.images.clone(),
            ));
        }

        for note in notes {
            session.ledger.append(Entry::Note(note));
        }
        session.ledger.append(Entry::ToolResults(results));
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

    /// Append harness notes for background tasks that finished since the
    /// last check. Only called at safe boundaries (turn start, after a
    /// completed tool batch) so history stays append-only.
    fn note_finished_background_tasks(&self, session: &mut Session) {
        let notes = session
            .tool_ctx
            .background
            .lock()
            .expect("background lock")
            .take_completion_notes();
        for note in notes {
            session.ledger.append(Entry::Note(note));
        }
    }

    fn preflight_memory(
        &self,
        session: &mut Session,
        calls: &[(String, String, Value)],
    ) -> Option<String> {
        let paths: Vec<PathBuf> = calls
            .iter()
            .filter_map(|(_, name, input)| self.tool(name).map(|tool| (tool, input)))
            .flat_map(|(tool, input)| tool.context_paths(input))
            .map(|path| session.tool_ctx.resolve(&path))
            .collect();
        if paths.is_empty() {
            return None;
        }
        let mut memory = session.tool_ctx.memory.lock().expect("memory lock");
        memory.restore_from_entries(session.ledger.entries());
        memory.discover_for_paths(&paths).map(|update| update.note)
    }

    /// Central token-budget gate for tool outputs. Locating/content tools
    /// opt out (`gates_output` = false): their output is precise and
    /// self-paginating, so spilling it to a file would only force a re-`read`
    /// of a result the model needed whole.
    fn gate(&self, session: &mut Session, tool: &str, output: ToolOutput) -> ToolOutput {
        let is_error = output.is_error;
        let images = output.images;
        let content = if is_error {
            output.content
        } else {
            compact_successful_test_output(output.content)
        };
        let gates = self.tool(tool).is_none_or(|t| t.gates_output());
        if !gates {
            return ToolOutput {
                content,
                is_error,
                images,
            };
        }
        let mut blobs = session.tool_ctx.blobs.lock().expect("blobs lock");
        // Only text is budget-gated; images never go to the blob store.
        ToolOutput {
            content: blobs.gate(tool, content),
            is_error,
            images,
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

/// Header for a parallel tool batch. Each tool describes its own fragment
/// (`Tool::batch_label`); a homogeneous batch shows one fragment, a mixed
/// batch joins them so the header names every tool instead of hiding them.
fn batch_label(prepared: &[(String, String, Value, Arc<dyn Tool>)]) -> String {
    // Group calls by tool in first-seen order, carrying each tool's inputs so
    // it can shape its own fragment (e.g. read's "ranges" vs "files").
    let mut groups: Vec<(&Arc<dyn Tool>, Vec<&Value>)> = Vec::new();
    for (_, name, input, tool) in prepared {
        match groups.iter_mut().find(|(t, _)| t.name() == name.as_str()) {
            Some((_, inputs)) => inputs.push(input),
            None => groups.push((tool, vec![input])),
        }
    }
    groups
        .iter()
        .map(|(tool, inputs)| tool.batch_label(inputs))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Header for a shell batch, which is approved as a group and then run in
/// order (the commands themselves are listed as batch items).
fn sequential_batch_label(count: usize) -> String {
    format!(
        "Run {count} {}",
        if count == 1 { "command" } else { "commands" }
    )
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

fn tool_result(id: &str, content: &str, is_error: bool) -> ContentBlock {
    tool_result_with_images(id, content, is_error, Vec::new())
}

fn tool_result_with_images(
    id: &str,
    content: &str,
    is_error: bool,
    images: Vec<ContentBlock>,
) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content: content.to_string(),
        is_error,
        images,
    }
}
