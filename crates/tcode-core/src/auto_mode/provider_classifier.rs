use async_trait::async_trait;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use super::{ClassifierDecision, ClassifierRequest, SafetyClassifier};
use crate::provider::{AgentModels, ModelCell, Request, StreamEvent};
use crate::types::{ContentBlock, Message, Role};

/// A truncation guard, not a budget: a well-behaved verdict costs one token
/// whatever the cap is. It must clear a reasoning preamble, because some
/// models think unconditionally — they ignore "off" (Hy3) or count reasoning
/// against the cap (OpenAI's reasoning models). A cap sized for the verdict
/// alone truncates those to an empty reply, which reads as a classifier
/// outage and takes Auto Mode offline. Both stages share it; stage one is
/// short because its prompt says so, not because it is capped.
const VERDICT_MAX_TOKENS: u32 = 1_024;
/// The classifier runs on the agent's provider but never on the agent's
/// prefix. Each `ClassifierRequest` carries a session-specific cache scope so
/// dynamically-expanded policy never shares a provider cache ID with another
/// session.
///
/// Provider-backed two-stage classifier. It deliberately uses the ordinary
/// provider interface: classifier requests have no tools and never enter the
/// main ledger, so the safety model stays isolated from the agent's context.
#[derive(Clone)]
pub struct ProviderSafetyClassifier {
    parent_model: ModelCell,
    pinned: AgentModels,
}

impl ProviderSafetyClassifier {
    /// `auto` is a model role, not a task kind: an absent pin deliberately
    /// follows `/model`, while a pin takes effect on the next classification.
    pub fn new(parent_model: ModelCell, pinned: AgentModels) -> Self {
        Self {
            parent_model,
            pinned,
        }
    }

    fn model(&self) -> crate::provider::ActiveModel {
        self.pinned
            .get("auto")
            .unwrap_or_else(|| self.parent_model.snapshot())
    }

    async fn run_stage(
        &self,
        request: &ClassifierRequest,
        suffix: &str,
        max_tokens: u32,
        effort: Option<String>,
        cancel: CancellationToken,
    ) -> Result<String, String> {
        let model = self.model();
        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: request
                    .transcript
                    .clone()
                    .with_pending_call(&request.tool_name, &request.input)
                    .text,
            }],
        }];
        let req = Request {
            model: model.provider.model().to_string(),
            // `request.policy` is byte-identical for both stages. The suffix
            // deliberately changes alone so stage two can reuse the stage-one
            // prefix at a provider cache boundary.
            system: request.policy.clone(),
            system_suffix: Some(suffix.to_string()),
            cache_scope: Some(request.cache_scope.clone()),
            messages,
            tools: vec![],
            // Providers without an output cap (Codex 400s on `max_output_tokens`)
            // get their brevity from the verdict prompt alone.
            max_tokens,
            effort,
        };
        let mut stream = model
            .provider
            .stream(req, cancel)
            .await
            .map_err(|error| error.to_string())?;
        let mut output = String::new();
        while let Some(event) = stream.next().await {
            match event.map_err(|error| error.to_string())? {
                StreamEvent::TextDelta(text) => output.push_str(&text),
                StreamEvent::Done(_) => break,
                _ => {}
            }
        }
        Ok(output)
    }
}

#[async_trait]
impl SafetyClassifier for ProviderSafetyClassifier {
    async fn classify(
        &self,
        request: ClassifierRequest,
        cancel: CancellationToken,
    ) -> ClassifierDecision {
        let fast = self
            .run_stage(
                &request,
                FAST_STAGE,
                VERDICT_MAX_TOKENS,
                Some("off".into()),
                cancel.clone(),
            )
            .await;
        let fast = match fast {
            Ok(output) => output,
            Err(reason) => return ClassifierDecision::Unavailable { reason },
        };
        match fast_verdict(&fast) {
            Some(true) => ClassifierDecision::Allow,
            Some(false) => {
                let effort = self.model().effort;
                let reasoned = self
                    .run_stage(&request, REASONED_STAGE, VERDICT_MAX_TOKENS, effort, cancel)
                    .await;
                match reasoned {
                    Ok(output) => reasoned_verdict(&output),
                    Err(reason) => ClassifierDecision::Unavailable { reason },
                }
            }
            None => ClassifierDecision::Unavailable {
                reason: format!("fast classifier returned an invalid verdict: {fast:?}"),
            },
        }
    }
}

/// The two stages share the policy as their cached prefix and differ only in
/// this suffix (see `Request::system_suffix`).
const FAST_STAGE: &str = include_str!("../../../../prompts/auto-classifier-fast.md");
const REASONED_STAGE: &str = include_str!("../../../../prompts/auto-classifier-reasoned.md");

/// The verdict word on its own line, tolerating decoration the model adds
/// despite the prompt (`**BLOCK**`, `ALLOW.`, `` `ALLOW` ``). Deliberately not
/// a search: a line that carries any other word is not a verdict, and an
/// unreadable verdict is an outage — which fails closed into a user prompt —
/// never a guessed allow.
fn verdict_word(line: &str) -> Option<bool> {
    let letters: String = line.chars().filter(char::is_ascii_alphabetic).collect();
    match letters.to_ascii_uppercase().as_str() {
        "ALLOW" => Some(true),
        "BLOCK" => Some(false),
        _ => None,
    }
}

fn lines(output: &str) -> impl Iterator<Item = &str> {
    output.lines().map(str::trim).filter(|l| !l.is_empty())
}

fn fast_verdict(output: &str) -> Option<bool> {
    verdict_word(lines(output).next()?)
}

fn reasoned_verdict(output: &str) -> ClassifierDecision {
    let mut lines = lines(output);
    match lines.next().and_then(verdict_word) {
        Some(true) => ClassifierDecision::Allow,
        Some(false) => ClassifierDecision::Block {
            reason: lines
                .next()
                .unwrap_or("The action is not safe or directly authorized.")
                .to_string(),
        },
        None => ClassifierDecision::Unavailable {
            reason: format!("reasoned classifier returned an invalid verdict: {output:?}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_parsers_are_strict() {
        assert_eq!(fast_verdict(" ALLOW\n"), Some(true));
        assert_eq!(fast_verdict("BLOCK"), Some(false));
        assert_eq!(fast_verdict("allow because"), None);
        assert_eq!(
            reasoned_verdict("ALLOW\nignored"),
            ClassifierDecision::Allow
        );
        assert_eq!(
            reasoned_verdict("BLOCK\nwould force-push shared history"),
            ClassifierDecision::Block {
                reason: "would force-push shared history".into()
            }
        );
    }

    /// Decoration must not take Auto Mode offline, but a sentence that merely
    /// mentions a verdict must never be read as one.
    #[test]
    fn a_decorated_verdict_still_parses_but_prose_never_does() {
        assert_eq!(fast_verdict("**BLOCK**"), Some(false));
        assert_eq!(fast_verdict("`ALLOW`"), Some(true));
        assert_eq!(fast_verdict("ALLOW."), Some(true));
        assert_eq!(fast_verdict("I would ALLOW this"), None);
        assert_eq!(fast_verdict("ALLOW BLOCK"), None);
        assert_eq!(fast_verdict(""), None);
        assert_eq!(
            reasoned_verdict("**BLOCK**\n- deletes the remote branch"),
            ClassifierDecision::Block {
                reason: "- deletes the remote branch".into()
            }
        );
        assert!(matches!(
            reasoned_verdict("The command looks fine, so ALLOW."),
            ClassifierDecision::Unavailable { .. }
        ));
    }
}
