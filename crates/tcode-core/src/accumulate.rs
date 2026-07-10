use std::collections::HashMap;

use serde_json::Value;

use crate::provider::StreamEvent;
use crate::types::{ContentBlock, StopReason, Usage};

/// Builds the assistant message out of a stream of events.
/// Provider-agnostic: both backends emit the same event vocabulary.
#[derive(Debug, Default)]
pub struct ResponseAccumulator {
    blocks: Vec<ContentBlock>,
    /// provider block index -> (position in `blocks`, accumulated JSON)
    tools: HashMap<usize, (usize, String)>,
    pub usage: Usage,
    pub stop_reason: Option<StopReason>,
}

impl ResponseAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, ev: &StreamEvent) {
        match ev {
            StreamEvent::Started => {}
            StreamEvent::TextDelta(t) => match self.blocks.last_mut() {
                Some(ContentBlock::Text { text }) => text.push_str(t),
                _ => self.blocks.push(ContentBlock::Text { text: t.clone() }),
            },
            StreamEvent::ThinkingDelta(t) => match self.blocks.last_mut() {
                Some(ContentBlock::Thinking { thinking, .. }) => thinking.push_str(t),
                _ => self.blocks.push(ContentBlock::Thinking {
                    thinking: t.clone(),
                    signature: None,
                }),
            },
            StreamEvent::ToolUseStart { index, id, name } => {
                self.blocks.push(ContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: Value::Null,
                });
                self.tools
                    .insert(*index, (self.blocks.len() - 1, String::new()));
            }
            StreamEvent::ToolUseInputDelta { index, fragment } => {
                if let Some((_, json)) = self.tools.get_mut(index) {
                    json.push_str(fragment);
                }
            }
            StreamEvent::Usage(u) => self.usage.merge_max(u),
            StreamEvent::Done(reason) => self.stop_reason = Some(reason.clone()),
        }
    }

    /// Finalize: parse accumulated tool-call JSON into inputs.
    pub fn finish(mut self) -> (Vec<ContentBlock>, Usage, Option<StopReason>) {
        for (pos, json) in self.tools.values() {
            let input = if json.trim().is_empty() {
                Value::Object(Default::default())
            } else {
                serde_json::from_str(json).unwrap_or(Value::String(json.clone()))
            };
            if let Some(ContentBlock::ToolUse { input: slot, .. }) = self.blocks.get_mut(*pos) {
                *slot = input;
            }
        }
        (self.blocks, self.usage, self.stop_reason)
    }

    /// Tool calls collected so far (name only; input not yet parsed).
    pub fn has_tool_use(&self) -> bool {
        !self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_text_and_tool_call() {
        let mut acc = ResponseAccumulator::new();
        acc.feed(&StreamEvent::TextDelta("Hel".into()));
        acc.feed(&StreamEvent::TextDelta("lo".into()));
        acc.feed(&StreamEvent::ToolUseStart {
            index: 1,
            id: "t1".into(),
            name: "read".into(),
        });
        acc.feed(&StreamEvent::ToolUseInputDelta {
            index: 1,
            fragment: "{\"path\":".into(),
        });
        acc.feed(&StreamEvent::ToolUseInputDelta {
            index: 1,
            fragment: "\"a.rs\"}".into(),
        });
        acc.feed(&StreamEvent::Done(StopReason::ToolUse));
        let (blocks, _, stop) = acc.finish();
        assert_eq!(blocks.len(), 2);
        assert!(matches!(&blocks[0], ContentBlock::Text { text } if text == "Hello"));
        match &blocks[1] {
            ContentBlock::ToolUse { name, input, .. } => {
                assert_eq!(name, "read");
                assert_eq!(input["path"], "a.rs");
            }
            _ => panic!("expected tool use"),
        }
        assert_eq!(stop, Some(StopReason::ToolUse));
    }
}
