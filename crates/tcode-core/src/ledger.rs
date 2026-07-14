use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::store::LogEvent;
use crate::types::{ContentBlock, Message, Role};

/// One unit of conversation history. Entries are the source of truth;
/// API messages are a derived view (`as_messages`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Entry {
    User(Vec<ContentBlock>),
    Assistant(Vec<ContentBlock>),
    /// Tool results produced by the harness (sent with user role).
    ToolResults(Vec<ContentBlock>),
    /// Harness-injected note the model should see (interrupt contract,
    /// freshness notices, background-task completions...). Machine-authored:
    /// the text is the whole fact.
    Note(String),
    /// The human's own words attached to a tool decision. The ledger keeps
    /// the fact — whose words, about which call — rather than a pre-baked
    /// sentence, because the two consumers need different things: the model
    /// reads the note as a standalone block detached from the call, so it
    /// must be told what the note is about, while a transcript already shows
    /// the note under the call it belongs to and would only stutter. Baking
    /// the sentence into the ledger forced replay to reverse-engineer the
    /// human's words back out of it.
    UserNote {
        /// The tool the decision was about.
        about: String,
        /// The words answer an `ask_user` question form, rather than annotate
        /// an approval.
        answer: bool,
        text: String,
    },
    /// Product of a compaction; replaces everything before it.
    Summary(String),
    /// Assistant text from a streaming attempt that failed before the
    /// provider completed it. Persisted for transcript/export/rewind, but
    /// deliberately never returned to a provider: a retry resends the same
    /// prompt and must not treat speculative output as model history.
    IncompleteAssistant {
        text: String,
        error: String,
    },
    /// Read-only visual history imported from another agent.  It is persisted
    /// and replayed in the terminal, but intentionally never becomes prompt
    /// content or a runnable tool call for the current model.
    ImportedTool {
        name: String,
        /// A normalized tcode-shaped input used only for transcript display.
        /// Old imported logs did not have it, so keep them resumable.
        #[serde(default)]
        input: Value,
        content: String,
    },
}

impl Entry {
    fn role(&self) -> Role {
        match self {
            Entry::Assistant(_) => Role::Assistant,
            Entry::IncompleteAssistant { .. } | Entry::ImportedTool { .. } => Role::Assistant,
            _ => Role::User,
        }
    }

    fn blocks(&self) -> Vec<ContentBlock> {
        match self {
            Entry::User(b) | Entry::Assistant(b) | Entry::ToolResults(b) => b.clone(),
            Entry::Note(text) => vec![ContentBlock::Text {
                text: format!("<harness-note>\n{text}\n</harness-note>"),
            }],
            Entry::UserNote {
                about,
                answer,
                text,
            } => vec![ContentBlock::Text {
                text: format!(
                    "<harness-note>\n{}\n</harness-note>",
                    if *answer {
                        format!("User answered {about}: {text}")
                    } else {
                        format!("From the user, approving {about}: {text}")
                    }
                ),
            }],
            Entry::Summary(text) => vec![ContentBlock::Text {
                text: format!(
                    "<conversation-summary>\nEarlier conversation was compacted. Summary:\n{text}\n</conversation-summary>"
                ),
            }],
            Entry::ImportedTool { .. } | Entry::IncompleteAssistant { .. } => Vec::new(),
        }
    }
}

/// Receives every legal ledger mutation, e.g. to persist it. Living
/// inside the Ledger, it cannot be bypassed by a forgetful call site.
pub trait LedgerSink: Send + Sync {
    fn record(&mut self, ev: &LogEvent);
}

/// Append-only conversation ledger. The only legal mutations are
/// `append`, `truncate_tail` (rewind) and `compact` — all of which keep
/// the prompt prefix stable, which is what makes caching work.
#[derive(Default)]
pub struct Ledger {
    entries: Vec<Entry>,
    sink: Option<Box<dyn LedgerSink>>,
}

impl std::fmt::Debug for Ledger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ledger")
            .field("entries", &self.entries)
            .finish_non_exhaustive()
    }
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach persistence. Mutations made from now on are recorded.
    pub fn attach_sink(&mut self, sink: Box<dyn LedgerSink>) {
        self.sink = Some(sink);
    }

    pub fn append(&mut self, e: Entry) {
        if let Some(sink) = &mut self.sink {
            sink.record(&LogEvent::Append { entry: e.clone() });
        }
        self.entries.push(e);
    }

    /// Persist a non-conversation event (e.g. a file checkpoint) into
    /// the same session log. Never touches the entries.
    pub fn record_aux(&mut self, ev: &LogEvent) {
        if let Some(sink) = &mut self.sink {
            sink.record(ev);
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Rewind: drop everything after `len` entries. The retained prefix
    /// is untouched, so provider caches still hit.
    pub fn truncate_tail(&mut self, len: usize) {
        if len >= self.entries.len() {
            return;
        }
        if let Some(sink) = &mut self.sink {
            sink.record(&LogEvent::TruncateTail { len });
        }
        self.entries.truncate(len);
    }

    /// Atomically replace entries [0, upto) with a summary. The one
    /// deliberate cache-invalidating operation.
    pub fn compact(&mut self, summary: String, upto: usize) {
        let upto = upto.min(self.entries.len());
        if let Some(sink) = &mut self.sink {
            sink.record(&LogEvent::Compact {
                summary: summary.clone(),
                upto,
            });
        }
        let tail = self.entries.split_off(upto);
        self.entries = Vec::with_capacity(tail.len() + 1);
        self.entries.push(Entry::Summary(summary));
        self.entries.extend(tail);
    }

    /// Derived API view: consecutive same-role entries merge into one
    /// message (providers require alternating roles or tolerate merging).
    pub fn as_messages(&self) -> Vec<Message> {
        let mut out: Vec<Message> = Vec::new();
        for e in &self.entries {
            // Imported history and incomplete retry attempts are for the human
            // transcript only. They are neither evidence the current model
            // needs nor content it may replay.
            if matches!(
                e,
                Entry::ImportedTool { .. } | Entry::IncompleteAssistant { .. }
            ) {
                continue;
            }
            let role = e.role();
            let blocks = e.blocks();
            match out.last_mut() {
                Some(last) if last.role == role => last.content.extend(blocks),
                _ => out.push(Message {
                    role,
                    content: blocks,
                }),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text {
            text: s.to_string(),
        }]
    }

    #[test]
    fn merges_consecutive_same_role() {
        let mut l = Ledger::new();
        l.append(Entry::User(text("hi")));
        l.append(Entry::Assistant(text("hello")));
        l.append(Entry::ToolResults(vec![ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        }]));
        l.append(Entry::Note("user approved with comment".into()));
        let msgs = l.as_messages();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[1].role, Role::Assistant);
        assert_eq!(msgs[2].role, Role::User);
        assert_eq!(msgs[2].content.len(), 2); // tool result + note merged
    }

    #[test]
    fn user_note_keeps_original_words_and_frames_prompt_context() {
        let mut l = Ledger::new();
        l.append(Entry::UserNote {
            about: "bash".into(),
            answer: false,
            text: "use 4 spaces".into(),
        });

        assert!(matches!(
            &l.entries()[0],
            Entry::UserNote { text, .. } if text == "use 4 spaces"
        ));
        let messages = l.as_messages();
        assert!(matches!(
            &messages[0].content[..],
            [ContentBlock::Text { text }]
                if text == "<harness-note>\nFrom the user, approving bash: use 4 spaces\n</harness-note>"
        ));
    }

    #[test]
    fn incomplete_assistant_is_persistable_but_not_prompt_content() {
        let mut l = Ledger::new();
        l.append(Entry::User(text("hi")));
        l.append(Entry::IncompleteAssistant {
            text: "partial answer".into(),
            error: "network error".into(),
        });
        l.append(Entry::Assistant(text("recovered answer")));

        let messages = l.as_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].role, Role::Assistant);
        assert!(matches!(
            &messages[1].content[..],
            [ContentBlock::Text { text }] if text == "recovered answer"
        ));
    }

    #[test]
    fn compact_replaces_prefix() {
        let mut l = Ledger::new();
        l.append(Entry::User(text("a")));
        l.append(Entry::Assistant(text("b")));
        l.append(Entry::User(text("c")));
        l.compact("summary".into(), 2);
        assert_eq!(l.len(), 2);
        assert!(matches!(&l.entries()[0], Entry::Summary(s) if s == "summary"));
        assert!(matches!(&l.entries()[1], Entry::User(_)));
    }

    #[test]
    fn truncate_tail_keeps_prefix() {
        let mut l = Ledger::new();
        l.append(Entry::User(text("a")));
        l.append(Entry::Assistant(text("b")));
        l.append(Entry::User(text("c")));
        l.truncate_tail(1);
        assert_eq!(l.len(), 1);
        assert!(matches!(&l.entries()[0], Entry::User(_)));
    }
}
