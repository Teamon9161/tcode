use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use super::{ClassifierDecision, ClassifierRequest, SafetyClassifier};
use crate::agent_roles::AgentRole;
use crate::config::AutoClassifierConfig;
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

struct ClassifierStage {
    name: &'static str,
    suffix: &'static str,
    max_tokens: u32,
    effort: Option<String>,
    timeout: Duration,
    accepts: fn(&str) -> bool,
}

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
    config: AutoClassifierConfig,
}

impl ProviderSafetyClassifier {
    /// `auto` is a model role, not a task kind: an absent pin deliberately
    /// follows `/model`, while a pin takes effect on the next classification.
    pub fn new(parent_model: ModelCell, pinned: AgentModels) -> Self {
        Self {
            parent_model,
            pinned,
            config: AutoClassifierConfig::default(),
        }
    }

    /// Supply the user-global stage deadlines and retry policy. This remains
    /// separate from the ordinary provider watchdog: a live SSE stream can
    /// still fail to produce a usable safety verdict.
    pub fn with_config(mut self, config: AutoClassifierConfig) -> Self {
        self.config = config;
        self
    }

    fn model(&self) -> crate::provider::ActiveModel {
        self.pinned
            .resolve(AgentRole::Auto, &self.parent_model)
            .expect("auto always inherits the main model")
    }

    async fn run_stage_once(
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

    /// Run one stage to a *valid* verdict, cancelling a timed-out stream before
    /// immediately retrying it at most once. The deadline is intentionally
    /// end-to-end: a response-created frame or SSE heartbeat cannot keep an
    /// unresponsive classifier alive forever.
    async fn run_stage(
        &self,
        request: &ClassifierRequest,
        stage: ClassifierStage,
        cancel: CancellationToken,
    ) -> Result<String, String> {
        let attempts = self.config.retry_count + 1;
        let mut last_failure = String::new();
        for attempt in 1..=attempts {
            let attempt_cancel = cancel.child_token();
            let result = tokio::select! {
                _ = cancel.cancelled() => return Err(format!("{} classifier cancelled", stage.name)),
                result = tokio::time::timeout(
                    stage.timeout,
                    self.run_stage_once(
                        request,
                        stage.suffix,
                        stage.max_tokens,
                        stage.effort.clone(),
                        attempt_cancel.clone(),
                    ),
                ) => result,
            };
            // Drop/cancel the old stream before opening the next request. A
            // provider may otherwise keep its prior HTTP body alive after this
            // task stops awaiting it.
            attempt_cancel.cancel();
            match result {
                Ok(Ok(output)) if (stage.accepts)(&output) => return Ok(output),
                Ok(Ok(output)) => {
                    last_failure = format!(
                        "{} classifier returned an invalid verdict: {output:?}",
                        stage.name
                    )
                }
                Ok(Err(reason)) => last_failure = reason,
                Err(_) => {
                    last_failure = format!(
                        "{} classifier timed out after {}s",
                        stage.name,
                        stage.timeout.as_secs()
                    )
                }
            }
            if attempt < attempts {
                continue;
            }
        }
        if attempts > 1 {
            Err(format!(
                "{} classifier failed after {attempts} attempts: {last_failure}",
                stage.name
            ))
        } else {
            Err(last_failure)
        }
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
                ClassifierStage {
                    name: "fast",
                    suffix: FAST_STAGE,
                    max_tokens: VERDICT_MAX_TOKENS,
                    effort: Some("off".into()),
                    timeout: self.config.fast_timeout,
                    accepts: |output| fast_verdict(output).is_some(),
                },
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
                    .run_stage(
                        &request,
                        ClassifierStage {
                            name: "reasoned",
                            suffix: REASONED_STAGE,
                            max_tokens: VERDICT_MAX_TOKENS,
                            effort,
                            timeout: self.config.reasoned_timeout,
                            accepts: |output| {
                                !matches!(
                                    reasoned_verdict(output),
                                    ClassifierDecision::Unavailable { .. }
                                )
                            },
                        },
                        cancel,
                    )
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
const FAST_STAGE: &str = include_str!("../../prompts/auto_mode/fast.md");
const REASONED_STAGE: &str = include_str!("../../prompts/auto_mode/reasoned.md");

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

    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::auto_mode::ClassifierTranscript;
    use crate::provider::{ActiveModel, CacheStrategy, EventStream, Provider, ProviderError};
    use crate::types::StopReason;

    enum Script {
        Pending,
        Output(&'static str),
        StreamError(&'static str),
    }

    struct ScriptedProvider {
        scripts: Mutex<VecDeque<Script>>,
        cancellations: Mutex<Vec<CancellationToken>>,
        requests: AtomicUsize,
    }

    impl ScriptedProvider {
        fn new(scripts: Vec<Script>) -> Self {
            Self {
                scripts: Mutex::new(scripts.into()),
                cancellations: Mutex::new(Vec::new()),
                requests: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }

        fn model(&self) -> &str {
            "scripted-model"
        }

        fn cache_strategy(&self) -> CacheStrategy {
            CacheStrategy::ImplicitPrefix
        }

        async fn stream(
            &self,
            _req: Request,
            cancel: CancellationToken,
        ) -> Result<EventStream, ProviderError> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.cancellations.lock().unwrap().push(cancel);
            match self.scripts.lock().unwrap().pop_front().unwrap() {
                Script::Pending => Ok(futures::stream::pending().boxed()),
                Script::Output(output) => Ok(futures::stream::iter([
                    Ok(StreamEvent::TextDelta(output.to_string())),
                    Ok(StreamEvent::Done(StopReason::EndTurn)),
                ])
                .boxed()),
                Script::StreamError(message) => Ok(futures::stream::iter([Err(
                    ProviderError::Network(message.into()),
                )])
                .boxed()),
            }
        }
    }

    fn classifier(scripts: Vec<Script>) -> (ProviderSafetyClassifier, Arc<ScriptedProvider>) {
        let provider = Arc::new(ScriptedProvider::new(scripts));
        let model = ActiveModel {
            provider: provider.clone(),
            max_tokens: 1_024,
            context_window: 32_768,
            effort: None,
        };
        let config = AutoClassifierConfig {
            fast_timeout: Duration::from_millis(1),
            reasoned_timeout: Duration::from_millis(1),
            retry_count: 1,
        };
        (
            ProviderSafetyClassifier::new(ModelCell::new(model), AgentModels::default())
                .with_config(config),
            provider,
        )
    }

    fn request() -> ClassifierRequest {
        ClassifierRequest {
            policy: "classifier policy".into(),
            cache_scope: "auto-classifier:test".into(),
            transcript: ClassifierTranscript::default(),
            tool_name: "shell".into(),
            input: serde_json::json!({"command": "echo ok"}),
        }
    }

    #[tokio::test]
    async fn fast_timeout_cancels_the_attempt_and_retries_to_a_verdict() {
        let (classifier, provider) = classifier(vec![Script::Pending, Script::Output("ALLOW")]);

        assert_eq!(
            classifier
                .classify(request(), CancellationToken::new())
                .await,
            ClassifierDecision::Allow
        );
        assert_eq!(provider.requests.load(Ordering::Relaxed), 2);
        assert!(
            provider.cancellations.lock().unwrap()[0].is_cancelled(),
            "the timed-out stream must be cancelled before retrying"
        );
    }

    #[tokio::test]
    async fn reasoned_timeout_retries_without_repeating_the_fast_stage() {
        let (classifier, provider) = classifier(vec![
            Script::Output("BLOCK"),
            Script::Pending,
            Script::Output("BLOCK\nThe command changes tracked files."),
        ]);

        assert_eq!(
            classifier
                .classify(request(), CancellationToken::new())
                .await,
            ClassifierDecision::Block {
                reason: "The command changes tracked files.".into(),
            }
        );
        assert_eq!(provider.requests.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn stream_errors_and_invalid_verdicts_each_retry_once() {
        let (retry_classifier, provider) = classifier(vec![
            Script::StreamError("temporary provider outage"),
            Script::Output("ALLOW"),
        ]);
        assert_eq!(
            retry_classifier
                .classify(request(), CancellationToken::new())
                .await,
            ClassifierDecision::Allow
        );
        assert_eq!(provider.requests.load(Ordering::Relaxed), 2);

        let (classifier, provider) = classifier(vec![
            Script::Output("not a verdict"),
            Script::Output("still not a verdict"),
        ]);
        assert!(matches!(
            classifier
                .classify(request(), CancellationToken::new())
                .await,
            ClassifierDecision::Unavailable { reason }
                if reason.contains("fast classifier failed after 2 attempts")
                    && reason.contains("invalid verdict")
        ));
        assert_eq!(provider.requests.load(Ordering::Relaxed), 2);
    }
}
