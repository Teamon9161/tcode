//! `/export`: render ledger entries as a human-readable markdown
//! transcript. A pure view over the same entries the renderer and resume
//! replay consume — no extra state to keep in sync.

use crate::agent::summarize_call;
use crate::ledger::Entry;
use crate::types::ContentBlock;

pub fn export_markdown(entries: &[Entry], title: &str) -> String {
    let mut out = format!("# {title}\n");
    for entry in entries {
        match entry {
            Entry::User(blocks) => {
                let text = visible_text(blocks);
                if !text.is_empty() {
                    out.push_str("\n## User\n\n");
                    out.push_str(&text);
                    out.push('\n');
                }
            }
            Entry::Assistant(blocks) => {
                let text = visible_text(blocks);
                if !text.is_empty() {
                    out.push_str("\n## Assistant\n\n");
                    out.push_str(&text);
                    out.push('\n');
                }
                for block in blocks {
                    if let ContentBlock::ToolUse { name, input, .. } = block {
                        out.push_str(&format!("\n> 🔧 `{}`\n", summarize_call(name, input)));
                    }
                }
            }
            Entry::ToolResults(blocks) => {
                for block in blocks {
                    let ContentBlock::ToolResult {
                        content, is_error, ..
                    } = block
                    else {
                        continue;
                    };
                    if content.trim().is_empty() {
                        continue;
                    }
                    let marker = if *is_error { " (error)" } else { "" };
                    out.push_str(&format!(
                        "\n<details><summary>tool result{marker}</summary>\n\n{}\n\n</details>\n",
                        fence(content)
                    ));
                }
            }
            Entry::Note(text) => out.push_str(&format!("\n> ⚑ {}\n", text.replace('\n', "\n> "))),
            Entry::Summary(text) => {
                out.push_str("\n---\n\n## Compacted summary of earlier conversation\n\n");
                out.push_str(text);
                out.push_str("\n\n---\n");
            }
            Entry::ImportedTool {
                name,
                input,
                content,
            } => {
                let label = if input.is_null() {
                    name.clone()
                } else {
                    summarize_call(name, input)
                };
                out.push_str(&format!("\n> 🕘 imported: `{label}`\n"));
                if !content.trim().is_empty() {
                    out.push_str(&format!("\n{}\n", fence(content)));
                }
            }
        }
    }
    out
}

/// User/assistant text without harness plumbing (status line, images).
fn visible_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } if !text.starts_with("<tcode-status>") => {
                Some(text.as_str())
            }
            ContentBlock::Image { .. } => Some("*(image attachment)*"),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
        .trim()
        .to_string()
}

/// Fence with enough backticks that embedded ``` cannot break out.
fn fence(content: &str) -> String {
    let longest_run = content
        .lines()
        .map(|l| l.trim_start().chars().take_while(|&c| c == '`').count())
        .max()
        .unwrap_or(0);
    let fence = "`".repeat((longest_run + 1).max(3));
    format!("{fence}\n{}\n{fence}", content.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exports_roles_tools_and_notes() {
        let entries = vec![
            Entry::User(vec![
                ContentBlock::Text {
                    text: "fix the bug".into(),
                },
                ContentBlock::Text {
                    text: "<tcode-status>context 10%</tcode-status>".into(),
                },
            ]),
            Entry::Assistant(vec![
                ContentBlock::Text {
                    text: "Looking now.".into(),
                },
                ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "read".into(),
                    input: json!({"path": "src/main.rs"}),
                },
            ]),
            Entry::ToolResults(vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "fn main() {}".into(),
                is_error: false,
                images: vec![],
            }]),
            Entry::Note("background task b1 finished".into()),
        ];
        let md = export_markdown(&entries, "session x");
        assert!(md.starts_with("# session x"));
        assert!(md.contains("## User\n\nfix the bug"));
        assert!(!md.contains("tcode-status"));
        assert!(md.contains("`read(src/main.rs)`"));
        assert!(md.contains("fn main() {}"));
        assert!(md.contains("> ⚑ background task b1 finished"));
    }

    #[test]
    fn embedded_fences_cannot_escape() {
        let fenced = fence("code\n```\nnested\n```");
        assert!(fenced.starts_with("````\n"));
        assert!(fenced.ends_with("\n````"));
    }
}
