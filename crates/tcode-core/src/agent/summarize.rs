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

/// Successful test runs often contain several nearly-identical target blocks
/// (especially doctests and crates with zero tests).  Keep the evidence that
/// matters to both the human and model while avoiding needless context use.
/// Any error-like marker leaves the original output untouched for diagnosis.
pub(super) fn compact_successful_test_output(output: String) -> String {
    if !(output.contains("test result: ok.")
        && output.contains("running ")
        && !output.contains("test result: FAILED")
        && !output.contains("error:")
        && !output.contains("failures:"))
    {
        return output;
    }
    let running: Vec<&str> = output
        .lines()
        .filter(|line| line.trim_start().starts_with("running ") && line.contains(" tests"))
        .collect();
    let Some(first) = running
        .iter()
        .copied()
        .find(|line| !line.contains("running 0 tests"))
    else {
        return output;
    };
    let passed = output
        .lines()
        .filter(|line| line.trim_start().starts_with("test result: ok."))
        .find(|line| !line.contains("0 passed"))
        .unwrap_or("test result: ok.");
    format!("{first}\n… successful test output folded …\n{passed}")
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

#[cfg(test)]
mod tests {
    use super::compact_successful_test_output;

    #[test]
    fn folds_repeated_successful_test_blocks() {
        let output = "running 24 tests\n........................\ntest result: ok. 24 passed; 0 failed\n\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed";
        let folded = compact_successful_test_output(output.into());
        assert!(folded.contains("running 24 tests"));
        assert!(folded.contains("folded"));
        assert!(!folded.contains("running 0 tests"));
    }

    #[test]
    fn retains_failed_test_output() {
        let output = "running 2 tests\ntest result: FAILED. 1 passed; 1 failed";
        assert_eq!(compact_successful_test_output(output.into()), output);
    }
}
