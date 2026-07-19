//! Next-prompt suggestion: what the user most likely wants to say next.
//!
//! Cost is the whole design. A suggestion is a convenience worth a fraction of
//! a cent and a fraction of a second — never a share of the conversation.
//!
//! So it gets a conversation of its own: the prose spine of the session, one
//! `(what the user asked, what the agent answered)` pair per turn, and nothing
//! else. Tool calls, tool results, and mid-turn chatter never enter it — the
//! agent's closing markdown already *is* its account of what it did and what it
//! thinks should happen next, which is precisely what a prediction needs.
//!
//! That spine is append-only, exactly like the ledger: each turn adds one pair
//! and leaves every earlier byte untouched, so the provider's cache carries the
//! history and a turn only ever pays for its own newest pair. It runs under its
//! own cache scope, and on its own pinnable model role (`[agents.suggest]`,
//! `/agents`) so it can be something small and fast — a guess that arrives
//! after the user starts typing is the same as no guess at all.

use std::time::Duration;

use futures::StreamExt;

use crate::agent_roles::AgentRole;
use tokio_util::sync::CancellationToken;

use super::{Agent, Session};
use crate::ledger::{Entry, Ledger};
use crate::provider::{Request, StreamEvent};
use crate::types::{ContentBlock, Message, Role};

/// Room for a reasoning preamble, not for the answer: the suggestion itself is
/// one line. Models that always think would be truncated to nothing by a cap
/// sized for the answer alone — the same lesson as the Auto Mode classifier.
const SUGGEST_MAX_TOKENS: u32 = 512;
/// Longer than this is a paragraph, not a prompt the user would press → on.
const MAX_CHARS: usize = 120;
/// Its prefix is its own — never the agent's — so it must not borrow the
/// agent's cache id.
const SUGGEST_SCOPE: &str = "suggest";
/// Truncation keeps a runaway paste or a very long answer from dominating the
/// spine. Both cuts are deterministic, so a truncated pair stays byte-identical
/// on every later turn and the prefix keeps hitting cache.
const ASKED_HEAD: usize = 500;
const ANSWERED_HEAD: usize = 1_000;
/// A guess the user has already out-typed is worthless, so a slow model (or a
/// provider stuck in its retry backoff) must not keep a task alive behind the
/// session. Late is the same as never here.
const SUGGEST_TIMEOUT: Duration = Duration::from_secs(20);

const SUGGEST_SYSTEM: &str = include_str!("../../prompts/agent/suggest-system.md");

/// The closing turn of the suggestion's own conversation. Constant, so it never
/// disturbs the append-only spine in front of it.
const ASK: &str = "What do I type next?";

/// The suggestion's conversation, snapshotted while the caller still owns the
/// session. The request runs off the UI thread and is cancelled the moment the
/// user starts typing, so it must not borrow the session.
pub struct SuggestRequest {
    messages: Vec<Message>,
    expected_script: Option<WritingSystem>,
}

/// The small set of writing systems that need a guard against a model switching
/// languages mid-suggestion. Latin remains allowed alongside every one for code
/// identifiers and technical terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WritingSystem {
    Latin,
    Han,
    Japanese,
    Hangul,
    Cyrillic,
    Arabic,
    Devanagari,
    Greek,
    Hebrew,
    Thai,
}

impl WritingSystem {
    fn of(c: char) -> Option<Self> {
        match c {
            'A'..='Z' | 'a'..='z' | '\u{00c0}'..='\u{024f}' => Some(Self::Latin),
            '\u{4e00}'..='\u{9fff}' | '\u{3400}'..='\u{4dbf}' => Some(Self::Han),
            '\u{3040}'..='\u{30ff}' => Some(Self::Japanese),
            '\u{ac00}'..='\u{d7af}' | '\u{1100}'..='\u{11ff}' => Some(Self::Hangul),
            '\u{0400}'..='\u{052f}' => Some(Self::Cyrillic),
            '\u{0600}'..='\u{06ff}' | '\u{0750}'..='\u{077f}' => Some(Self::Arabic),
            '\u{0900}'..='\u{097f}' => Some(Self::Devanagari),
            '\u{0370}'..='\u{03ff}' => Some(Self::Greek),
            '\u{0590}'..='\u{05ff}' => Some(Self::Hebrew),
            '\u{0e00}'..='\u{0e7f}' => Some(Self::Thai),
            _ => None,
        }
    }
}

impl Agent {
    /// `None` unless the agent finished a turn *by talking*: an interrupted run,
    /// a turn that died mid-tool, or one still in flight has a half-told story,
    /// and predicting from it means predicting from work that never happened.
    /// This is what keeps a long Auto Mode run silent until it actually lands.
    pub fn suggest_request(&self, session: &Session) -> Option<SuggestRequest> {
        if !ends_in_prose(&session.ledger) {
            return None;
        }
        let exchanges = exchanges(&session.ledger);
        if exchanges.is_empty() {
            return None;
        }
        let expected_script = dominant_script(&exchanges.last()?.0);
        let mut messages = Vec::with_capacity(exchanges.len() * 2 + 1);
        for (asked, answered) in exchanges {
            messages.push(text_message(Role::User, head(&asked, ASKED_HEAD)));
            messages.push(text_message(
                Role::Assistant,
                head(&answered, ANSWERED_HEAD),
            ));
        }
        messages.push(text_message(Role::User, ASK.to_string()));
        Some(SuggestRequest {
            messages,
            expected_script,
        })
    }

    /// Every failure mode is the same failure mode: no suggestion. A dead
    /// endpoint, an unreachable pinned model, a refusal, a cancel, a model too
    /// slow to matter — none of them may surface an error, retry, or otherwise
    /// spend the user's attention on a feature whose entire value is that it
    /// costs them none.
    pub async fn suggest(&self, req: SuggestRequest, cancel: CancellationToken) -> Option<String> {
        let model = self
            .models
            .resolve(AgentRole::Suggest, &self.model)
            .expect("suggest always inherits the main model");
        let request = Request {
            model: model.provider.model().to_string(),
            system: SUGGEST_SYSTEM.to_string(),
            system_suffix: None,
            cache_scope: Some(SUGGEST_SCOPE.to_string()),
            messages: req.messages,
            tools: Vec::new(),
            max_tokens: SUGGEST_MAX_TOKENS,
            effort: Some("off".into()),
        };
        let expected_script = req.expected_script;
        let guess = async {
            let mut stream = model.provider.stream(request, cancel).await.ok()?;
            let mut text = String::new();
            while let Some(event) = stream.next().await {
                match event.ok()? {
                    StreamEvent::TextDelta(delta) => text.push_str(&delta),
                    StreamEvent::Done(_) => break,
                    _ => {}
                }
            }
            clean(&text, expected_script)
        };
        tokio::time::timeout(SUGGEST_TIMEOUT, guess).await.ok()?
    }
}

/// The prose spine: one `(asked, answered)` pair per completed turn.
///
/// `answered` is the *last* assistant prose of the turn — the closing summary,
/// not the "I'll go look at that" it opened with. An unanswered turn (the one
/// in flight, or one the user interrupted) contributes nothing.
fn exchanges(ledger: &Ledger) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut asked: Option<String> = None;
    let mut answered: Option<String> = None;
    for entry in ledger.entries() {
        match entry {
            Entry::User(blocks) => {
                let text = text_of(blocks);
                if text.is_empty() {
                    continue;
                }
                if let (Some(asked), Some(answered)) = (asked.take(), answered.take()) {
                    pairs.push((asked, answered));
                }
                asked = Some(text);
                answered = None;
            }
            Entry::Assistant(blocks) if asked.is_some() => {
                let text = text_of(blocks);
                if !text.is_empty() {
                    answered = Some(text);
                }
            }
            // Tool calls and their results, harness notes, compaction
            // summaries, imported logs: all absent by construction.
            _ => {}
        }
    }
    if let (Some(asked), Some(answered)) = (asked, answered) {
        pairs.push((asked, answered));
    }
    pairs
}

/// Did the agent stop by saying something? That is what a finished turn looks
/// like: the loop only returns to the user when the model answers instead of
/// calling another tool. A ledger ending in a tool call, a tool result, or the
/// user's own message means the turn is still running, was cancelled, or broke.
fn ends_in_prose(ledger: &Ledger) -> bool {
    match ledger.entries().last() {
        Some(Entry::Assistant(blocks)) => {
            // Prose *and* no tool call: text next to a `ToolUse` is a preface
            // ("I'll start with lib.rs"), not a conclusion — the turn goes on.
            let calls = blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
            !calls && !text_of(blocks).is_empty()
        }
        _ => false,
    }
}

/// Prose only. The status block is harness-generated, not something the user
/// typed; predicting from it would be predicting from our own noise.
fn text_of(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } if !text.starts_with("<tcode-status>") => Some(text.trim()),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn text_message(role: Role, text: String) -> Message {
    Message {
        role,
        content: vec![ContentBlock::Text { text }],
    }
}

fn head(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

/// One undecorated line, or nothing. A model that answers with prose, a
/// refusal, a paragraph, controls, or an unexpected writing system yields no
/// suggestion rather than a misleading ghost prompt.
fn clean(text: &str, expected_script: Option<WritingSystem>) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let line = line
        .trim_matches(|c| c == '"' || c == '`' || c == '\'')
        .trim();
    let long = line.chars().count() > MAX_CHARS;
    if line.is_empty()
        || long
        || line.eq_ignore_ascii_case("none")
        || line.chars().any(char::is_control)
        || !uses_expected_script(line, expected_script)
    {
        return None;
    }
    Some(line.to_string())
}

fn dominant_script(text: &str) -> Option<WritingSystem> {
    let mut counts = std::collections::BTreeMap::<u8, (WritingSystem, usize)>::new();
    for system in text.chars().filter_map(WritingSystem::of) {
        let key = system as u8;
        let entry = counts.entry(key).or_insert((system, 0));
        entry.1 += 1;
    }
    counts
        .into_values()
        .max_by_key(|(_, count)| *count)
        .map(|(system, _)| system)
}

fn uses_expected_script(text: &str, expected: Option<WritingSystem>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    text.chars().filter_map(WritingSystem::of).all(|actual| {
        actual == expected
            || actual == WritingSystem::Latin
            || (expected == WritingSystem::Japanese && actual == WritingSystem::Han)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(text: &str) -> Entry {
        Entry::User(vec![ContentBlock::Text { text: text.into() }])
    }

    fn assistant(text: &str) -> Entry {
        Entry::Assistant(vec![ContentBlock::Text { text: text.into() }])
    }

    #[test]
    fn only_a_bare_one_line_instruction_becomes_a_suggestion() {
        assert_eq!(
            clean("run the tests", Some(WritingSystem::Latin)),
            Some("run the tests".into())
        );
        assert_eq!(
            clean("\n\"fix the failing test\"\n", Some(WritingSystem::Latin)),
            Some("fix the failing test".into())
        );
        assert_eq!(clean("NONE", Some(WritingSystem::Latin)), None);
        assert_eq!(clean("", Some(WritingSystem::Latin)), None);
        assert_eq!(
            clean(&"x".repeat(MAX_CHARS + 1), Some(WritingSystem::Latin)),
            None
        );
    }

    #[test]
    fn suggestion_rejects_a_foreign_script_or_control_char() {
        assert_eq!(dominant_script("修复这个问题"), Some(WritingSystem::Han));
        assert_eq!(
            clean("修复这个问题", Some(WritingSystem::Han)),
            Some("修复这个问题".into())
        );
        assert_eq!(clean("修复这个问题 끝", Some(WritingSystem::Han)), None);
        assert_eq!(clean("fix it 修复", Some(WritingSystem::Latin)), None);
        assert_eq!(clean("fix\u{0007} it", Some(WritingSystem::Latin)), None);
    }

    /// The spine is prose only: no tool calls, no tool results, no mid-turn
    /// chatter, no harness status block — and it grows by exactly one pair per
    /// turn, which is what keeps its cached prefix intact.
    #[test]
    fn the_spine_is_one_prose_pair_per_turn_and_nothing_else() {
        let mut ledger = Ledger::new();
        ledger.append(user("fix the bug"));
        ledger.append(Entry::Assistant(vec![
            ContentBlock::Text {
                text: "I'll look at lib.rs first.".into(),
            },
            ContentBlock::ToolUse {
                id: "c1".into(),
                name: "read".into(),
                input: json!({"file_path": "lib.rs"}),
            },
        ]));
        ledger.append(Entry::ToolResults(vec![ContentBlock::ToolResult {
            tool_use_id: "c1".into(),
            content: "fn main() {}".into(),
            is_error: false,
            images: vec![],
        }]));
        ledger.append(assistant(
            "## Fixed\nThe off-by-one is gone. Tests not run yet.",
        ));

        let first = exchanges(&ledger);
        assert_eq!(
            first,
            vec![(
                "fix the bug".to_string(),
                "## Fixed\nThe off-by-one is gone. Tests not run yet.".to_string()
            )]
        );

        // A second turn appends; it must not disturb the first pair.
        ledger.append(Entry::User(vec![
            ContentBlock::Text {
                text: "now run them".into(),
            },
            ContentBlock::Text {
                text: "<tcode-status>context ~10%</tcode-status>".into(),
            },
        ]));
        ledger.append(assistant("All 42 tests pass."));

        let second = exchanges(&ledger);
        assert_eq!(second[0], first[0], "the cached prefix must not move");
        assert_eq!(
            second[1],
            (
                "now run them".to_string(),
                "All 42 tests pass".to_string() + "."
            )
        );
    }

    #[test]
    fn a_turn_still_in_flight_contributes_nothing_to_predict_from() {
        let mut ledger = Ledger::new();
        assert!(exchanges(&ledger).is_empty());
        ledger.append(user("fix the bug"));
        assert!(exchanges(&ledger).is_empty());
    }

    /// A long autonomous run is exactly where a premature guess would be most
    /// annoying: the agent says "I'll start with the ledger", then works for
    /// two minutes. Nothing is predictable until it stops and reports.
    #[test]
    fn a_turn_that_has_not_landed_yet_is_never_predicted_from() {
        let mut ledger = Ledger::new();
        ledger.append(user("refactor the ledger"));
        assert!(!ends_in_prose(&ledger), "the model has not spoken yet");

        ledger.append(Entry::Assistant(vec![
            ContentBlock::Text {
                text: "I'll start with lib.rs.".into(),
            },
            ContentBlock::ToolUse {
                id: "c1".into(),
                name: "read".into(),
                input: json!({"file_path": "lib.rs"}),
            },
        ]));
        assert!(!ends_in_prose(&ledger), "still mid-tool, only prefaced");

        ledger.append(Entry::ToolResults(vec![ContentBlock::ToolResult {
            tool_use_id: "c1".into(),
            content: "fn main() {}".into(),
            is_error: false,
            images: vec![],
        }]));
        assert!(
            !ends_in_prose(&ledger),
            "interrupted between tool and reply"
        );

        ledger.append(assistant("## Done\nThe ledger is append-only again."));
        assert!(ends_in_prose(&ledger), "the turn landed");
    }
}
