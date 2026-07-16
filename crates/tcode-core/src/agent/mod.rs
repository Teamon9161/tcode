mod compact;
mod session;
mod suggest;
mod summarize;

pub use session::{CwdChange, PendingInput, PendingMessage, PendingMode, Session};
pub use suggest::SuggestRequest;
pub use summarize::summarize_call;
use summarize::{compact_successful_test_output, preview, split_malformed};

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use futures::{future::join_all, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::accumulate::ResponseAccumulator;
use crate::auto_mode::{
    is_protected_path, AutoModePolicy, AutoRoute, ClassifierDecision, ClassifierRequest,
    ClassifierTranscript, SafetyClassifier,
};
use crate::config::WatchdogConfig;
use crate::ledger::Entry;
use crate::memory::MemoryUpdate;
use crate::permission::{ApprovalDecision, Approver, Decision, PermissionMode};
use crate::provider::{ProviderError, Request, StreamEvent};
use crate::tool::{BatchPolicy, PermissionRequest, Tool, ToolOutput};
use crate::types::{ContentBlock, RateLimits, StopReason, Usage};

/// Default ceiling on model round-trips per user turn; a runaway loop should
/// never bill unbounded. It is a backstop, not a budget: set high enough that
/// honest long tasks never see it, because a ceiling the model can feel is a
/// ceiling that distorts its work. Configurable via `limits.max_steps_per_turn`.
pub const DEFAULT_MAX_STEPS: usize = 500;

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
    /// Reference context expanded from explicit `@path` markers immediately
    /// before a user entry is appended. The transcript keeps the concise marker;
    /// this event lets UI context meters count the hidden snapshot accurately.
    ReferencesExpanded {
        labels: Vec<String>,
        added_tokens: usize,
    },
    /// A message the user typed while the turn was running, now delivered into
    /// the ledger at a safe boundary (see `PendingInput`). It is a real user
    /// entry — the model reads it on its next step, and Auto Mode treats it as
    /// authorization, exactly like any other thing the user says.
    QueuedInput {
        text: String,
        /// Attachment labels, so the delivered prompt renders with the images
        /// and pasted files that were part of it.
        attachments: Vec<String>,
        /// Ledger index of the entry, so the transcript can tag it for rewind
        /// like a normal prompt.
        entry_index: usize,
    },
    /// The original text of a user's approval annotation. Sent only after its
    /// tool result has committed, matching the ledger order used by resume.
    UserNote {
        text: String,
        /// `ask_user` answers have their own question-and-answer transcript
        /// record; approval annotations render as `Note:`.
        answer: bool,
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
    /// History was replaced by this summary. It carries the text because the
    /// summary is now the only record of everything before it — the user must
    /// be able to read what the model is standing on.
    Compacted(String),
    /// A staged permission-mode switch was committed at a safe boundary (turn
    /// start, batch boundary, turn end). The frontend promotes its pending
    /// status marker and bakes a record — the transcript is the source of
    /// truth for which boundary a mode took effect at.
    ModeChanged(crate::permission::PermissionMode),
    /// The classifier was unavailable repeatedly, so the session falls back
    /// to ordinary human approvals until Auto Mode is explicitly re-enabled.
    AutoModePaused(String),
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
    /// Pinned auxiliary model roles (`[agents.<role>]`, `/agents`). Only the
    /// roles the agent itself runs are read here — `suggest` today; sub-agent
    /// kinds are resolved by the `task` tool, which shares the same handle.
    /// An unpinned role follows `model`.
    pub models: crate::provider::AgentModels,
    pub tools: Vec<Arc<dyn Tool>>,
    pub system: String,
    pub watchdog: WatchdogConfig,
    pub hooks: crate::hooks::Hooks,
    /// Independent model used only to gate Auto Mode actions. `None` keeps Auto
    /// Mode fail-closed by falling back to the normal human approval prompt.
    pub safety_classifier: Option<Arc<dyn SafetyClassifier>>,
    /// Fixed classifier policy assembled from global configuration. It is
    /// intentionally not taken from project-local configuration.
    pub auto_policy: String,
    /// Runaway guard: model round-trips per user turn before the harness
    /// ends the turn gracefully.
    pub max_steps: usize,
}

struct PermissionCheck<'a> {
    name: &'a str,
    input: &'a Value,
    request: &'a PermissionRequest,
    cancel: &'a CancellationToken,
    events: &'a mpsc::Sender<AgentEvent>,
}

impl Agent {
    fn tool(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name)
    }

    fn tool_defs(&self) -> Vec<crate::ToolDef> {
        self.tools.iter().map(|t| t.as_ref().def()).collect()
    }

    /// Estimate the current request's context occupancy before a provider has
    /// returned authoritative usage. Request construction belongs here, so
    /// every frontend and resume path counts the same system prompt, tool
    /// definitions and model-visible ledger entries.
    pub fn estimate_context_tokens(&self, session: &Session) -> u64 {
        use crate::blobs::approx_tokens;

        let system = approx_tokens(&self.system_prompt(session)) as u64;
        let tool_defs: u64 = self
            .tool_defs()
            .iter()
            .map(|tool| {
                let schema = serde_json::to_string(&tool.input_schema).unwrap_or_default();
                (approx_tokens(&tool.name)
                    + approx_tokens(&tool.description)
                    + approx_tokens(&schema)) as u64
            })
            .sum();
        let conversation: u64 = session
            .ledger
            .entries()
            .iter()
            .map(|entry| match entry {
                Entry::User(blocks) | Entry::Assistant(blocks) | Entry::ToolResults(blocks) => {
                    blocks
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
                            // Provider-specific image accounting is unavailable
                            // until usage arrives; reserve a conservative budget.
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
                        .sum()
                }
                // These variants grow XML-like wrappers in Ledger::as_messages.
                Entry::Note(text) => approx_tokens(text) as u64 + 12,
                Entry::UserNote { about, text, .. } => {
                    (approx_tokens(about) + approx_tokens(text)) as u64 + 24
                }
                Entry::Summary(text) => approx_tokens(text) as u64 + 24,
                Entry::ImportedTool { .. } | Entry::IncompleteAssistant { .. } => 0,
            })
            .sum();
        system
            .saturating_add(tool_defs)
            .saturating_add(conversation)
    }

    /// Apply Auto Mode's tool-declared fast paths and, only when necessary,
    /// ask the independent classifier. The caller still owns interactive
    /// approval: unavailable classification deliberately becomes `Ask`.
    async fn permission_decision(
        &self,
        session: &mut Session,
        tool: &dyn Tool,
        check: PermissionCheck<'_>,
    ) -> Decision {
        let initial = session.rules.decide(session.mode, check.request);
        let mut decision = initial;
        if matches!(decision, Decision::Allow)
            && matches!(session.mode, crate::permission::PermissionMode::Auto)
            && tool
                .touches(check.input)
                .is_some_and(|path| is_protected_path(&session.tool_ctx.resolve(&path)))
        {
            // A broad allow rule may pre-approve ordinary work, but must not
            // bypass agent instruction/configuration protection.
            decision = Decision::Auto;
        }
        if !matches!(decision, Decision::Auto) {
            return decision;
        }

        let target = tool.safety_target(check.input);
        match AutoModePolicy::new(&session.tool_ctx.cwd, &session.tool_ctx.scratch_dir)
            .route(tool.auto_safety(check.input), target.as_deref())
        {
            AutoRoute::Allow => Decision::Allow,
            AutoRoute::Prompt => Decision::Ask,
            AutoRoute::Classify => {
                let Some(classifier) = &self.safety_classifier else {
                    return Decision::Ask;
                };
                let instructions = session
                    .tool_ctx
                    .memory
                    .lock()
                    .expect("memory lock")
                    .classifier_instructions();
                let policy = if instructions.is_empty() {
                    self.auto_policy.clone()
                } else {
                    format!(
                        "{}\n\n# Active project instructions\n{}",
                        self.auto_policy, instructions
                    )
                };
                let request = ClassifierRequest {
                    policy,
                    transcript: ClassifierTranscript::from_ledger(&session.ledger),
                    tool_name: check.name.to_string(),
                    input: check.input.clone(),
                };
                match classifier.classify(request, check.cancel.clone()).await {
                    ClassifierDecision::Allow => {
                        session.record_auto_classification(true);
                        Decision::Allow
                    }
                    ClassifierDecision::Block { reason } => {
                        let paused = session.record_auto_classification(false);
                        let paused = paused
                            .map(|notice| format!("\n{notice}"))
                            .unwrap_or_default();
                        Decision::Deny(format!(
                            "Blocked by Auto Mode safety classifier: {reason}\nFind a safer alternative. Do not try to route around this boundary.{paused}"
                        ))
                    }
                    ClassifierDecision::Unavailable { .. } => {
                        if let Some(notice) = session.record_auto_classifier_unavailable() {
                            let _ = check.events.send(AgentEvent::AutoModePaused(notice)).await;
                        }
                        Decision::Ask
                    }
                }
            }
        }
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
            self.compact(session, events, &cancel).await?;
        }
        // Background tasks that finished between turns: tell the model
        // before its new instruction (pure append, cache-safe).
        self.note_finished_background_tasks(session);
        // Commit a mode staged before this turn and, if it just entered plan
        // mode, inject the plan-enter note ahead of the user's instruction.
        self.commit_mode(session, events).await?;
        let expanded = crate::references::expand_references(
            session.tool_ctx.cwd.clone(),
            std::mem::take(&mut input),
        )
        .await;
        input = expanded.blocks;
        if !expanded.labels.is_empty() {
            self.emit(
                events,
                AgentEvent::ReferencesExpanded {
                    labels: expanded.labels,
                    added_tokens: expanded.added_tokens,
                },
            )
            .await?;
        }
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
        for _ in 0..max_steps {
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
            // task completion notes, a staged mode switch, and anything the
            // user said meanwhile. The mode commits before pending input so a
            // switch to plan takes effect for the next batch and its note lands
            // ahead of whatever the user queued.
            self.note_finished_background_tasks(session);
            self.commit_mode(session, events).await?;
            self.deliver_pending_input(session, events).await?;
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
                system_suffix: None,
                cache_scope: session.cache_scope(),
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
        let memory_update = self.preflight_memory(session, calls);
        let blocked_by_new_instructions = memory_update
            .as_ref()
            .map(|update| self.affected_mutations(session, calls, update))
            .unwrap_or_default();
        let memory_note = memory_update.map(|update| update.note);
        // A newly discovered instruction must stop mutations it governs, but
        // it must not discard unrelated work or read-only reconnaissance from
        // the same model batch. Blocked batches use the ordinary per-call path
        // below, preserving approval, hook and ledger ordering semantics.
        if blocked_by_new_instructions.is_empty()
            && calls.len() > 1
            && self.all_calls_have_policy(calls, BatchPolicy::ParallelReadOnly)
        {
            return self
                .run_read_only_tools_parallel(session, calls, events, cancel, memory_note)
                .await;
        }
        // File mutations are scheduled per normalized path: calls within one
        // file retain model order, while independent file lanes run together.
        if blocked_by_new_instructions.is_empty()
            && calls.len() > 1
            && self.is_file_mutation_batch(calls)
        {
            return self
                .run_file_mutation_lanes(session, calls, events, approver, cancel)
                .await;
        }
        // Shell / bash: batch the approval (show all commands at once),
        // then run sequentially so side effects from earlier commands
        // are visible to later ones.
        if blocked_by_new_instructions.is_empty()
            && calls.len() > 1
            && self.all_calls_have_policy(calls, BatchPolicy::SequentialBatch)
        {
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
        let mut notes: Vec<Entry> = memory_note.into_iter().map(Entry::Note).collect();
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
            if blocked_by_new_instructions.contains(&i) {
                results.push(tool_result(
                    id,
                    "Not executed: newly discovered directory-scoped instructions apply to this mutation. Review the instruction note and retry this action only if it complies.",
                    true,
                ));
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
            // An approval may replace the artifact that actually runs (the
            // original tool use stays immutable in the ledger).
            let mut approved_input: Option<Value> = None;
            // A mode transition an approval carried (only the plan-review
            // dialog sets one). Applied generically before the tool runs.
            let mut applied_mode: Option<PermissionMode> = None;
            let is_user_question = matches!(request, PermissionRequest::UserInput { .. });
            match self
                .permission_decision(
                    session,
                    tool.as_ref(),
                    PermissionCheck {
                        name,
                        input,
                        request: &request,
                        cancel,
                        events,
                    },
                )
                .await
            {
                Decision::Allow => {}
                Decision::Deny(reason) => {
                    results.push(tool_result(&id.clone(), &reason, true));
                    continue;
                }
                Decision::Ask | Decision::Auto => {
                    let approval = approver
                        .ask(
                            name,
                            request.summary(),
                            &request.approval_label(),
                            request.allows_rule(),
                            input,
                        )
                        .await;
                    match approval.decision {
                        ApprovalDecision::Yes => {
                            approval_note = approval.comment;
                            applied_mode = approval.set_mode;
                            approved_input = approval.approved_input;
                        }
                        ApprovalDecision::YesSession | ApprovalDecision::YesProject => {
                            self.persist_approval_rule(
                                session,
                                &request,
                                approval.decision,
                                &mut notes,
                            )
                            .await;
                            approval_note = approval.comment;
                            applied_mode = approval.set_mode;
                            approved_input = approval.approved_input;
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

            // Keep the model-issued call for event/replay identity. The
            // replacement below is only the reviewed artifact executed by the
            // tool and seen by hooks.
            let model_input = input;
            let input = approved_input.as_ref().unwrap_or(model_input);

            // A plan approval switches the permission mode before the tool
            // runs, so the mode the tool executes under (and reports) is the
            // approved one, and its note is already synced.
            if let Some(mode) = applied_mode {
                session.apply_approved_mode(mode);
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
            notes.extend(pre.notes.into_iter().map(Entry::Note));

            self.emit(
                events,
                AgentEvent::ToolStart {
                    name: name.clone(),
                    summary: summarize_call(name, model_input),
                    input: model_input.clone(),
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
            let mut output = self.gate(session, name, output);
            // Tell the model, in the same result, which mode execution now runs
            // under. Only a plan approval sets `applied_mode`.
            if let Some(mode) = applied_mode.filter(|_| !output.is_error) {
                output.content.push_str(&format!(
                    "\n\nPermission mode is now {}. Proceed with the plan.",
                    mode.label()
                ));
            }
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
                // The human's words go in whole. Which call they were about is
                // a fact the entry carries, not a sentence baked into it: the
                // model gets that framing from `Entry::blocks`, a transcript
                // renders the note under the call and needs no reminder.
                notes.push(Entry::UserNote {
                    about: name.clone(),
                    answer: is_user_question,
                    text: note,
                });
            }
        }

        session.ledger.append(Entry::ToolResults(results));
        self.append_notes(session, events, notes).await?;

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
    /// only this read-only subset concurrently; all mutating, shell, question,
    /// and capability-changing sub-agent calls remain ordered so their approvals
    /// and side effects are unambiguous. Results are still appended in
    /// model-call order.
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

    /// `edit`/`write` calls form a file-mutation batch. The executor uses
    /// each normalized path as an ordered lane, so only calls within a file
    /// serialize while independent files remain concurrent.
    fn is_file_mutation_batch(&self, calls: &[(String, String, Value)]) -> bool {
        calls.iter().all(|(_, name, input)| {
            self.tool(name).is_some_and(|tool| {
                tool.batch_policy_for(input) == BatchPolicy::ParallelPerFile
                    && tool.touches(input).is_some()
            })
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
        if self.calls_were_blocked_by_new_instructions(session, calls) {
            return None;
        }
        if self.all_calls_have_policy(calls, BatchPolicy::SequentialBatch) {
            return Some(sequential_batch_label(calls.len()));
        }
        if !self.all_calls_have_policy(calls, BatchPolicy::ParallelReadOnly)
            && !self.is_file_mutation_batch(calls)
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
        calls.iter().all(|(_, name, input)| {
            self.tool(name)
                .is_some_and(|tool| tool.batch_policy_for(input) == policy)
        })
    }

    fn calls_were_blocked_by_new_instructions(
        &self,
        session: &Session,
        calls: &[(String, String, Value)],
    ) -> bool {
        let call_ids: HashSet<&str> = calls.iter().map(|(id, _, _)| id.as_str()).collect();
        session.ledger.entries().windows(2).any(|pair| {
            let Entry::Assistant(blocks) = &pair[0] else {
                return false;
            };
            let assistant_ids: HashSet<&str> = blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                    _ => None,
                })
                .collect();
            if assistant_ids != call_ids {
                return false;
            }
            let Entry::ToolResults(results) = &pair[1] else {
                return false;
            };
            results.iter().any(|block| {
                matches!(block, ContentBlock::ToolResult { content, is_error: true, .. }
                    if content.starts_with("Not executed: newly discovered directory-scoped instructions"))
            })
        })
    }

    /// Execute an edit/write batch in per-file lanes. Approval, pre-hooks and
    /// checkpoints are deliberately all complete before the first mutation;
    /// after that, calls to one path preserve the model's order, while separate
    /// paths run concurrently.
    async fn run_file_mutation_lanes(
        &self,
        session: &mut Session,
        calls: &[(String, String, Value)],
        events: &mpsc::Sender<AgentEvent>,
        approver: &dyn Approver,
        cancel: &CancellationToken,
    ) -> Result<ToolsOutcome, AgentError> {
        let mut prepared: Vec<(String, String, Value, Arc<dyn Tool>)> = Vec::new();
        let mut notes: Vec<Entry> = Vec::new();

        for (id, name, input) in calls {
            if cancel.is_cancelled() {
                return self.cancel_unstarted_batch(session, calls, true);
            }
            let tool = self.tool(name).expect("preflighted tool").clone();
            let request = tool.permission(input);
            match self
                .permission_decision(
                    session,
                    tool.as_ref(),
                    PermissionCheck {
                        name,
                        input,
                        request: &request,
                        cancel,
                        events,
                    },
                )
                .await
            {
                Decision::Allow => {}
                Decision::Deny(reason) => {
                    return self.abort_file_batch(session, calls, id, &reason, false);
                }
                Decision::Ask | Decision::Auto => {
                    let PermissionRequest::Ask {
                        descriptor: _,
                        summary,
                        ..
                    } = tool.permission(input)
                    else {
                        unreachable!("file mutation needs an edit approval")
                    };
                    let approval = approver
                        .ask(
                            name,
                            &summary,
                            &request.approval_label(),
                            request.allows_rule(),
                            input,
                        )
                        .await;
                    match approval.decision {
                        ApprovalDecision::Yes => {
                            if let Some(note) = approval.comment {
                                notes.push(Entry::UserNote {
                                    about: name.clone(),
                                    answer: false,
                                    text: note,
                                });
                            }
                        }
                        ApprovalDecision::YesSession | ApprovalDecision::YesProject => {
                            self.persist_approval_rule(
                                session,
                                &request,
                                approval.decision,
                                &mut notes,
                            )
                            .await;
                            if let Some(note) = approval.comment {
                                notes.push(Entry::UserNote {
                                    about: name.clone(),
                                    answer: false,
                                    text: note,
                                });
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
            notes.extend(pre.notes.into_iter().map(Entry::Note));
            prepared.push((id.clone(), name.clone(), input.clone(), tool));
        }

        // Save each original once before any lane starts. A checkpoint at this
        // ledger position restores the state before the entire batch, not an
        // intermediate state after an earlier same-file call.
        let mut checkpointed = HashSet::new();
        for (_, _, input, tool) in &prepared {
            let path = normalize_path(
                session
                    .tool_ctx
                    .resolve(&tool.touches(input).expect("preflighted path")),
            );
            if checkpointed.insert(path.clone()) {
                let len = session.ledger.len();
                if let Some(ev) = session.checkpoints.save(len, &path) {
                    session.ledger.record_aux(&ev);
                }
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

        let mut lane_by_path: HashMap<PathBuf, usize> = HashMap::new();
        let mut lanes: Vec<Vec<usize>> = Vec::new();
        for (index, (_, _, input, tool)) in prepared.iter().enumerate() {
            let path = normalize_path(
                session
                    .tool_ctx
                    .resolve(&tool.touches(input).expect("preflighted path")),
            );
            let lane = match lane_by_path.get(&path) {
                Some(&lane) => lane,
                None => {
                    let lane = lanes.len();
                    lane_by_path.insert(path, lane);
                    lanes.push(Vec::new());
                    lane
                }
            };
            lanes[lane].push(index);
        }

        // Every call runs and gets its own result. A lane does not stop at a
        // failure: `edit` and `write` are atomic, so a failed one leaves the
        // file byte-for-byte as the calls after it expect — skipping them
        // would discard work that was never in danger (one no-op edit used to
        // cost the seven independent edits queued behind it). Calls on one
        // file still run in the model's order; only cancellation halts a lane.
        let lane_outputs = join_all(lanes.iter().map(|lane| {
            let prepared = &prepared;
            let tool_ctx = &session.tool_ctx;
            async move {
                let mut outputs = Vec::with_capacity(lane.len());
                for &index in lane {
                    let (_, _, input, tool) = &prepared[index];
                    let (output, ran) = if cancel.is_cancelled() {
                        (
                            ToolOutput::err("Cancelled by user before execution."),
                            false,
                        )
                    } else {
                        (tool.run(input.clone(), tool_ctx, cancel).await, true)
                    };
                    outputs.push((index, output, ran));
                }
                outputs
            }
        }))
        .await;
        let mut outputs: Vec<(usize, ToolOutput, bool)> =
            lane_outputs.into_iter().flatten().collect();
        outputs.sort_by_key(|(index, _, _)| *index);

        let mut results = Vec::new();
        let mut status = Vec::new();
        let mut had_failure = false;
        for ((index, (id, name, input, tool)), (_, mut output, ran)) in
            prepared.into_iter().enumerate().zip(outputs)
        {
            if ran && !output.is_error {
                let path = session
                    .tool_ctx
                    .resolve(&tool.touches(&input).expect("preflighted path"));
                session
                    .tool_ctx
                    .memory
                    .lock()
                    .expect("memory lock")
                    .mark_written(&path);
            }
            if ran {
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
            }
            let output = self.gate(session, &name, output);
            let state = if !ran {
                "skipped"
            } else if output.is_error {
                "failed"
            } else {
                "succeeded"
            };
            had_failure |= output.is_error;
            status.push(format!("step {} ({id}, {name}): {state}", index + 1));
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
        if had_failure {
            notes.push(Entry::Note(format!(
                "File mutation batch status (model call order): {}. See each tool result for exact failure details.",
                status.join("; "),
            )));
        }
        self.append_notes(session, events, notes).await?;
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
        let mut notes: Vec<Entry> = memory_note.into_iter().map(Entry::Note).collect();
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
            notes.extend(pre.notes.into_iter().map(Entry::Note));
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
            match self
                .permission_decision(
                    session,
                    tool.as_ref(),
                    PermissionCheck {
                        name,
                        input,
                        request: &request,
                        cancel,
                        events,
                    },
                )
                .await
            {
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
                Decision::Ask | Decision::Auto => {
                    let (PermissionRequest::Ask {
                        descriptor: _,
                        summary,
                        ..
                    }
                    | PermissionRequest::UserInput {
                        descriptor: _,
                        summary,
                    }) = &request
                    else {
                        unreachable!()
                    };
                    let approval = approver
                        .ask(
                            name,
                            summary,
                            &request.approval_label(),
                            request.allows_rule(),
                            input,
                        )
                        .await;
                    match approval.decision {
                        ApprovalDecision::Yes => {
                            if let Some(note) = approval.comment {
                                notes.push(Entry::UserNote {
                                    about: name.clone(),
                                    answer: false,
                                    text: note,
                                });
                            }
                        }
                        ApprovalDecision::YesSession | ApprovalDecision::YesProject => {
                            self.persist_approval_rule(
                                session,
                                &request,
                                approval.decision,
                                &mut notes,
                            )
                            .await;
                            if let Some(note) = approval.comment {
                                notes.push(Entry::UserNote {
                                    about: name.clone(),
                                    answer: false,
                                    text: note,
                                });
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

        session.ledger.append(Entry::ToolResults(results));
        self.append_notes(session, events, notes).await?;
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

    /// Commit a staged permission-mode switch at a safe boundary and, if the
    /// commit entered plan mode, append the plan-enter note. A net-zero cycle
    /// commits nothing and injects nothing. This is the single place a staged
    /// switch becomes live inside a running turn.
    async fn commit_mode(
        &self,
        session: &mut Session,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<(), AgentError> {
        if let Some(mode) = session.commit_pending_mode() {
            self.emit(events, AgentEvent::ModeChanged(mode)).await?;
        }
        if let Some(note) = session.take_mode_note() {
            session.ledger.append(Entry::Note(note));
        }
        Ok(())
    }

    /// Hand the model whatever the user said while it was working.
    ///
    /// Only callable where a user entry is legal — after a tool batch's results
    /// are committed, never between a `tool_use` and its result. The ledger
    /// merges it into that same user message, so this stays a pure append and
    /// the model reads it on its next step rather than after the whole turn.
    async fn deliver_pending_input(
        &self,
        session: &mut Session,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<(), AgentError> {
        for message in session.pending.take() {
            let expanded =
                crate::references::expand_references(session.tool_ctx.cwd.clone(), message.blocks)
                    .await;
            let entry_index = session.ledger.len();
            session.ledger.append(Entry::User(expanded.blocks));
            if !expanded.labels.is_empty() {
                self.emit(
                    events,
                    AgentEvent::ReferencesExpanded {
                        labels: expanded.labels,
                        added_tokens: expanded.added_tokens,
                    },
                )
                .await?;
            }
            self.emit(
                events,
                AgentEvent::QueuedInput {
                    text: message.text,
                    attachments: message.attachments,
                    entry_index,
                },
            )
            .await?;
        }
        Ok(())
    }

    fn preflight_memory(
        &self,
        session: &mut Session,
        calls: &[(String, String, Value)],
    ) -> Option<MemoryUpdate> {
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
        memory.discover_for_paths(&paths)
    }

    fn affected_mutations(
        &self,
        session: &Session,
        calls: &[(String, String, Value)],
        update: &MemoryUpdate,
    ) -> HashSet<usize> {
        // Use the same canonical form as MemoryManager. A mutation may name a
        // file that does not exist yet, so canonicalize its deepest existing
        // ancestor and retain the missing tail rather than comparing a regular
        // path to a Windows `\\?\\` instruction root.
        let roots: Vec<PathBuf> = update
            .affected_roots
            .iter()
            .map(|root| crate::memory::canonical_target(root))
            .collect();
        calls
            .iter()
            .enumerate()
            .filter_map(|(index, (_, name, input))| {
                let tool = self.tool(name)?;
                if !tool.is_mutating() {
                    return None;
                }
                tool.context_paths(input)
                    .into_iter()
                    .map(|path| crate::memory::canonical_target(&session.tool_ctx.resolve(&path)))
                    .any(|path| roots.iter().any(|root| path.starts_with(root)))
                    .then_some(index)
            })
            .collect()
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
            content: blobs.gate(tool, content, is_error),
            is_error,
            images,
        }
    }

    async fn persist_approval_rule(
        &self,
        session: &mut Session,
        request: &PermissionRequest,
        decision: ApprovalDecision,
        notes: &mut Vec<Entry>,
    ) {
        if !request.allows_rule()
            || !matches!(
                decision,
                ApprovalDecision::YesSession | ApprovalDecision::YesProject
            )
        {
            return;
        }
        let descriptor = request.descriptor().to_string();
        if !session.rules.allow.iter().any(|rule| rule == &descriptor) {
            session.rules.allow.push(descriptor.clone());
        }
        if decision != ApprovalDecision::YesProject {
            return;
        }
        match crate::config::Config::add_project_allow(session.tool_ctx.cwd.clone(), descriptor).await {
            Ok(true) => notes.push(Entry::Note(
                "Permission saved to .tcode/config.toml and allowed for this session.".into(),
            )),
            Ok(false) => notes.push(Entry::Note(
                "Permission was already present in .tcode/config.toml and is allowed for this session.".into(),
            )),
            Err(error) => notes.push(Entry::Note(format!(
                "Could not save the project permission ({error}); this approval remains effective for this session only."
            ))),
        }
    }

    async fn append_notes(
        &self,
        session: &mut Session,
        events: &mpsc::Sender<AgentEvent>,
        notes: Vec<Entry>,
    ) -> Result<(), AgentError> {
        for note in notes {
            let user_note = match &note {
                Entry::UserNote { text, answer, .. } => Some((text.clone(), *answer)),
                _ => None,
            };
            session.ledger.append(note);
            if let Some((text, answer)) = user_note {
                self.emit(events, AgentEvent::UserNote { text, answer })
                    .await?;
            }
        }
        Ok(())
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
