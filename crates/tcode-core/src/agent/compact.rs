use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::accumulate::ResponseAccumulator;
use crate::agent_roles::AgentRole;
use crate::ledger::Entry;
use crate::provider::Request;
use crate::types::ContentBlock;

use super::{Agent, AgentError, AgentEvent, Session};

const COMPACT_PROMPT: &str = include_str!("../../prompts/agent/compact.md");

impl Agent {
    /// Summarize the whole ledger into one entry — the single deliberate
    /// cache-invalidating operation. Also used by `/compact`.
    ///
    /// Returns whether the history was actually replaced. It can legitimately
    /// come back `false` — an empty ledger, an interrupted attempt, a model
    /// that answered with no text — and the caller has to know, because the
    /// window is then exactly as full as it was.
    pub async fn compact(
        &self,
        session: &mut Session,
        events: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<bool, AgentError> {
        self.compact_with_focus(session, None, events, cancel).await
    }

    /// Compact with an optional user-requested emphasis. The focus guides the
    /// summary but never replaces the baseline continuation requirements.
    ///
    /// On success it emits `AgentEvent::Compacted` carrying the summary: that
    /// text is now the only record of everything before it, so both callers —
    /// the auto-compact in `user_turn` and `/compact` — must be able to show
    /// the user what the model is left standing on.
    pub async fn compact_with_focus(
        &self,
        session: &mut Session,
        focus: Option<&str>,
        events: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<bool, AgentError> {
        if session.ledger.is_empty() {
            return Ok(false);
        }
        let mut messages = session.ledger.as_messages();
        messages.push(crate::Message {
            role: crate::Role::User,
            content: vec![ContentBlock::Text {
                text: compact_prompt(focus),
            }],
        });
        // A pin selects the summarizer without changing when compaction starts:
        // that threshold remains tied to the main conversation model's window.
        let model = self
            .models
            .resolve(AgentRole::Compact, self.model_cell(session))
            .expect("compact always inherits the main model");
        let req = Request {
            model: model.provider.model().to_string(),
            system: self.system_prompt(session),
            system_suffix: None,
            // Compaction shares the session's prefix, so it stays in its scope.
            cache_scope: session.cache_scope(),
            messages,
            // Byte-identical to the turn requests, tools included: the tool
            // definitions sit inside the cached prefix, so dropping them would
            // miss the entire prefix — and compaction fires exactly when that
            // prefix is at its largest. The model is told to summarize and any
            // tool_use it returns anyway is ignored below.
            tools: self.tool_defs(),
            max_tokens: model.max_tokens,
            effort: model.effort.clone(),
        };
        // Use the same retry owner, retry budget, exponential backoff, and
        // cancellation semantics as a normal model step. A failed attempt has
        // no ledger side effect, so its partial summary is simply discarded.
        let mut attempt = 0u32;
        let (blocks, usage, _) = 'retry: loop {
            // The provider makes one connection attempt. Retrying here keeps
            // compact failures visible and consistent with normal turns.
            let mut stream = match model.provider.stream(req.clone(), cancel.clone()).await {
                Ok(stream) => stream,
                Err(error) if error.retryable() && attempt < self.watchdog.max_retries => {
                    attempt += 1;
                    if self
                        .emit_retry(events, attempt, &error.to_string(), false, cancel)
                        .await?
                    {
                        continue 'retry;
                    }
                    return Ok(false);
                }
                Err(error) => return Err(error.into()),
            };
            let mut acc = ResponseAccumulator::new();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(event) => {
                        if let crate::StreamEvent::RateLimits(limits) = &event {
                            self.emit(events, AgentEvent::RateLimits(*limits)).await?;
                        }
                        acc.feed(&event);
                    }
                    Err(error) if error.retryable() && attempt < self.watchdog.max_retries => {
                        attempt += 1;
                        if self
                            .emit_retry(events, attempt, &error.to_string(), false, cancel)
                            .await?
                        {
                            continue 'retry;
                        }
                        return Ok(false);
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            break acc.finish();
        };
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
            return Ok(false);
        }
        let upto = session.ledger.len();
        session.ledger.compact(summary.clone(), upto);
        // Every read in the replaced prefix is gone from the model's context,
        // so the freshness cache is now answering about content the model
        // cannot see: the same reset a rewind does, for the same reason.
        // Without it a post-compaction `read` returns "unchanged, already in
        // context" about a file whose bytes the summary did not carry, and the
        // model has to spend a turn discovering that and a `force` read to
        // undo it — the exact waste the zero-guessing principle exists to stop.
        session.forget_seen_files();
        // The `progress` calls that carried the plan just left the history, so
        // the model no longer has it. This is one of the three moments the
        // harness re-describes the file rather than assuming the model kept it.
        session.mark_progress_injection();
        let memory_note = session
            .tool_ctx
            .memory
            .lock()
            .expect("memory lock")
            .post_compact_note();
        if let Some(note) = memory_note {
            session.ledger.append(Entry::Instruction(note));
        }
        session.turn_usage.input_tokens += usage.input_tokens;
        session.turn_usage.output_tokens += usage.output_tokens;
        session.turn_usage.cache_read_tokens += usage.cache_read_tokens;
        session.turn_usage.cache_write_tokens += usage.cache_write_tokens;
        // Unknown until the next request reports it.
        session.last_prompt_tokens = 0;
        // A compaction that landed re-arms the automatic one, whatever an
        // earlier attempt did.
        session.auto_compact_declined = false;
        self.emit(events, AgentEvent::Compacted(summary)).await?;
        Ok(true)
    }
}

fn compact_prompt(focus: Option<&str>) -> String {
    let focus = focus
        .map(str::trim)
        .filter(|focus| !focus.is_empty())
        .map(|focus| {
            format!(
                "Additional user-requested summary focus (this supplements, not replaces, the required continuation details):\n{focus}\n\n"
            )
        })
        .unwrap_or_default();
    COMPACT_PROMPT.replace("{{USER_FOCUS}}", &focus)
}

#[cfg(test)]
mod tests {
    use super::compact_prompt;

    #[test]
    fn compact_prompt_omits_focus_section_when_none_is_given() {
        let prompt = compact_prompt(None);
        assert!(!prompt.contains("Additional user-requested summary focus"));
        assert!(!prompt.contains("{{USER_FOCUS}}"));
    }

    #[test]
    fn compact_focus_supplements_required_summary_details() {
        let prompt = compact_prompt(Some("prioritize API decisions and migration risks"));
        assert!(prompt.contains("**Current state**"));
        assert!(prompt.contains("**Next steps**"));
        assert!(prompt.contains("**Active progress**"));
        assert!(prompt.contains("prioritize API decisions and migration risks"));
        assert!(prompt.contains("supplements, not replaces"));
        assert!(!prompt.contains("{{USER_FOCUS}}"));
        assert!(prompt.trim_end().ends_with("Output only the summary text."));
    }
}
