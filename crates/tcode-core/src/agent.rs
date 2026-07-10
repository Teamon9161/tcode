use std::sync::Arc;

use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::accumulate::ResponseAccumulator;
use crate::config::WatchdogConfig;
use crate::ledger::{Entry, Ledger};
use crate::permission::{
    ApprovalDecision, Approver, Decision, PermissionMode, PermissionRules,
};
use crate::provider::{Provider, ProviderError, Request, StreamEvent};
use crate::tool::{PermissionRequest, Tool, ToolCtx, ToolOutput};
use crate::types::{ContentBlock, StopReason, Usage};

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
    ToolEnd {
        name: String,
        preview: String,
        is_error: bool,
    },
    /// Per-step usage (one model request).
    Usage(Usage),
    /// Context grew past the auto-compact threshold; a summary request
    /// is running before the actual turn.
    Compacting,
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
    pub provider: Arc<dyn Provider>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub system: String,
    pub max_tokens: u32,
    pub context_window: u64,
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
        // Auto-compact: pay the one-time cache invalidation before the
        // context overflows, not after.
        if session.last_prompt_tokens > self.context_window * 85 / 100 {
            self.emit(events, AgentEvent::Compacting).await?;
            self.compact(session, &cancel).await?;
        }
        if let Some(status) = session.status_block(self.context_window) {
            input.push(status);
        }
        session.ledger.append(Entry::User(input));
        session.turn_usage = Usage::default();

        for _step in 0..MAX_STEPS {
            let (blocks, usage, stop) = self.stream_step(session, events, &cancel).await?;
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
        let req = Request {
            model: self.provider.model().to_string(),
            system: self.system.clone(),
            messages,
            tools: Vec::new(),
            max_tokens: self.max_tokens,
        };
        let mut stream = self.provider.stream(req, cancel.clone()).await?;
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
        session: &Session,
        events: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<(Vec<ContentBlock>, Usage, Option<StopReason>), AgentError> {
        let mut attempt = 0u32;
        'retry: loop {
            let req = Request {
                model: self.provider.model().to_string(),
                system: self.system.clone(),
                messages: session.ledger.as_messages(),
                tools: self.tool_defs(),
                max_tokens: self.max_tokens,
            };
            let mut stream = self.provider.stream(req, cancel.clone()).await?;
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
        let mut results: Vec<ContentBlock> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let mut executed: Vec<String> = Vec::new();
        let mut declined = false;
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
            match session.rules.decide(session.mode, &request) {
                Decision::Allow => {}
                Decision::Deny(reason) => {
                    results.push(tool_result(&id.clone(), &reason, true));
                    continue;
                }
                Decision::Ask => {
                    let PermissionRequest::Ask {
                        descriptor,
                        summary,
                        ..
                    } = &request
                    else {
                        unreachable!("Ask decision implies Ask request");
                    };
                    let approval = approver.ask(name, summary, descriptor).await;
                    match approval.decision {
                        ApprovalDecision::Yes => approval_note = approval.comment,
                        ApprovalDecision::YesAlways => {
                            session.rules.allow.push(descriptor.clone());
                            approval_note = approval.comment;
                        }
                        ApprovalDecision::No => {
                            declined = true;
                            let reason = approval
                                .comment
                                .map(|c| format!(" Reason: {c}"))
                                .unwrap_or_default();
                            results.push(tool_result(
                                id,
                                &format!("User declined this action.{reason}"),
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
            let mut output = tool.run(input.clone(), &session.tool_ctx, cancel).await;
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
                    is_error: output.is_error,
                },
            )
            .await?;
            executed.push(name.clone());
            results.push(tool_result(id, &output.content, output.is_error));
            if let Some(note) = approval_note {
                notes.push(format!("Note from the user when approving {name}: {note}"));
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
            return Ok(ToolsOutcome { interrupted: true });
        }
        Ok(ToolsOutcome { interrupted: false })
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
        ToolOutput {
            content: blobs.gate(tool, output.content),
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
