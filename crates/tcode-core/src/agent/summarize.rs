use serde_json::Value;

use crate::types::ContentBlock;

/// One-line description of a call for the UI, e.g. `shell(cargo build)`.
pub fn summarize_call(name: &str, input: &Value) -> String {
    // "file_path" covers imported Claude Code calls (Read/Edit/Write).
    let arg = [
        "command",
        "path",
        "file_path",
        "pattern",
        "id",
        "agent",
        "url",
        "query",
    ]
    .iter()
    .find_map(|k| input.get(k).and_then(|v| v.as_str()))
    .unwrap_or("");
    if arg.is_empty() {
        name.to_string()
    } else {
        format!("{name}({arg})")
    }
}

pub(super) fn preview(s: &str) -> String {
    let mut line = s.lines().next().unwrap_or("").to_string();
    if line.chars().count() > 120 {
        line = line.chars().take(120).collect::<String>() + "…";
    }
    let extra = s.lines().count().saturating_sub(1);
    if extra > 0 {
        line.push_str(&format!(" (+{extra} lines)"));
    }
    line
}

/// An interrupted stream can leave a tool_use whose input JSON never
/// finished; the accumulator falls back to a raw string for those.
/// They must not be replayed to the API.
pub(super) fn split_malformed(blocks: Vec<ContentBlock>) -> (Vec<ContentBlock>, bool) {
    let mut dropped = false;
    let kept = blocks
        .into_iter()
        .filter(|b| match b {
            ContentBlock::ToolUse {
                input: Value::String(_),
                ..
            } => {
                dropped = true;
                false
            }
            _ => true,
        })
        .collect();
    (kept, dropped)
}
