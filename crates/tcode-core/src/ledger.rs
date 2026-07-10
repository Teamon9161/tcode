use serde::{Deserialize, Serialize};

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
    /// freshness notices, user annotations on approvals...).
    Note(String),
    /// Product of a compaction; replaces everything before it.
    Summary(String),
}

impl Entry {
    fn role(&self) -> Role {
        match self {
            Entry::Assistant(_) => Role::Assistant,
            _ => Role::User,
        }
    }

    fn blocks(&self) -> Vec<ContentBlock> {
        match self {
            Entry::User(b) | Entry::Assistant(b) | Entry::ToolResults(b) => b.clone(),
            Entry::Note(text) => vec![ContentBlock::Text {
                text: format!("<harness-note>\n{text}\n</harness-note>"),
            }],
            Entry::Summary(text) => vec![ContentBlock::Text {
                text: format!(
                    "<conversation-summary>\nEarlier conversation was compacted. Summary:\n{text}\n</conversation-summary>"
                ),
            }],
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
