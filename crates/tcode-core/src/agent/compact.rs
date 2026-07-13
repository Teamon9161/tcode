use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::accumulate::ResponseAccumulator;
use crate::ledger::Entry;
use crate::provider::Request;
use crate::types::ContentBlock;

use super::{Agent, AgentError, Session};

const COMPACT_PROMPT: &str = include_str!("../../../../prompts/compact.md");

impl Agent {
    /// Summarize the whole ledger into one entry — the single deliberate
    /// cache-invalidating operation. Also used by `/compact`.
    pub async fn compact(
        &self,
        session: &mut Session,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        self.compact_with_focus(session, None, cancel).await
    }

    /// Compact with an optional user-requested emphasis. The focus guides the
    /// summary but never replaces the baseline continuation requirements.
    pub async fn compact_with_focus(
        &self,
        session: &mut Session,
        focus: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        if session.ledger.is_empty() {
            return Ok(());
        }
        let mut messages = session.ledger.as_messages();
        messages.push(crate::Message {
            role: crate::Role::User,
            content: vec![ContentBlock::Text {
                text: compact_prompt(focus),
            }],
        });
        let model = self.model.snapshot();
        let req = Request {
            model: model.provider.model().to_string(),
            system: self.system_prompt(session),
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
        let memory_note = session
            .tool_ctx
            .memory
            .lock()
            .expect("memory lock")
            .post_compact_note();
        if let Some(note) = memory_note {
            session.ledger.append(Entry::Note(note));
        }
        session.turn_usage.input_tokens += usage.input_tokens;
        session.turn_usage.output_tokens += usage.output_tokens;
        session.turn_usage.cache_read_tokens += usage.cache_read_tokens;
        session.turn_usage.cache_write_tokens += usage.cache_write_tokens;
        // Unknown until the next request reports it.
        session.last_prompt_tokens = 0;
        Ok(())
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
        assert!(prompt.contains("prioritize API decisions and migration risks"));
        assert!(prompt.contains("supplements, not replaces"));
        assert!(!prompt.contains("{{USER_FOCUS}}"));
        assert!(prompt.ends_with("Output only the summary text.\n"));
    }
}
