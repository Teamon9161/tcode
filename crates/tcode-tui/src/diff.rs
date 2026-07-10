//! Red/green diff blocks for edit and write tool calls.

use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::theme;

const MAX_DIFF_LINES: usize = 40;

/// Render the file-change preview for an edit/write call, if applicable.
pub fn render_change(tool: &str, input: &Value) -> Vec<Line<'static>> {
    match tool {
        "edit" => {
            let old = input["old_string"].as_str().unwrap_or("");
            let new = input["new_string"].as_str().unwrap_or("");
            diff_lines(old, new)
        }
        "write" => {
            let content = input["content"].as_str().unwrap_or("");
            content
                .lines()
                .take(20)
                .map(|l| {
                    Line::from(vec![
                        Span::styled("  + ", theme::diff_add()),
                        Span::styled(l.to_string(), theme::diff_add()),
                    ])
                })
                .chain(if content.lines().count() > 20 {
                    Some(Line::styled(
                        format!("    … +{} more lines", content.lines().count() - 20),
                        theme::dim(),
                    ))
                } else {
                    None
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

fn diff_lines(old: &str, new: &str) -> Vec<Line<'static>> {
    let diff = similar::TextDiff::from_lines(old, new);
    let mut out = Vec::new();
    for change in diff.iter_all_changes() {
        if out.len() >= MAX_DIFF_LINES {
            out.push(Line::styled("    … (diff truncated)", theme::dim()));
            break;
        }
        let text = change.value().trim_end_matches('\n').to_string();
        let line = match change.tag() {
            similar::ChangeTag::Delete => Line::from(vec![
                Span::styled("  - ", theme::diff_del()),
                Span::styled(text, theme::diff_del()),
            ]),
            similar::ChangeTag::Insert => Line::from(vec![
                Span::styled("  + ", theme::diff_add()),
                Span::styled(text, theme::diff_add()),
            ]),
            similar::ChangeTag::Equal => Line::from(vec![
                Span::styled("    ", theme::dim()),
                Span::styled(text, theme::dim()),
            ]),
        };
        out.push(line);
    }
    out
}
