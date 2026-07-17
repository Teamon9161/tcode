//! Read-only import of conversations produced by other terminal agents.
//!
//! An import deliberately copies text messages into a new tcode session. Tool
//! calls/results are historical context, not actions tcode may safely replay.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use tcode_core::store::StoreError;
use tcode_core::{import_entries, ContentBlock, Entry, Resumed};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalSource {
    Codex,
    Claude,
}

impl ExternalSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExternalSessionInfo {
    pub source: ExternalSource,
    pub id: String,
    pub path: PathBuf,
    pub last_user_preview: String,
    pub modified: Option<std::time::SystemTime>,
}

/// Find conversations whose recorded working directory is the supplied
/// project. Missing external installations simply produce an empty list.
pub fn list_external_sessions(cwd: &Path, source: ExternalSource) -> Vec<ExternalSessionInfo> {
    let mut files = match source {
        ExternalSource::Codex => dirs::home_dir()
            .map(|home| home.join(".codex/sessions"))
            .map(|root| jsonl_files_recursive(&root))
            .unwrap_or_default(),
        ExternalSource::Claude => dirs::home_dir()
            .map(|home| home.join(".claude/projects").join(claude_project_name(cwd)))
            .map(|root| jsonl_files(&root))
            .unwrap_or_default(),
    };
    files.sort_by_key(|path| fs::metadata(path).and_then(|m| m.modified()).ok());
    files.reverse();
    files
        .into_iter()
        .filter_map(|path| external_info(cwd, source, path))
        // The picker renders eight rows. Keep a small reserve for scrolling
        // without doing unnecessary IO across an entire Codex archive.
        .take(24)
        .collect()
}

/// Copy an external transcript into a fresh tcode log. The original JSONL is
/// opened read-only and is never linked to the new session.
pub fn import_external_session(
    data_dir: &Path,
    cwd: &Path,
    external: &ExternalSessionInfo,
) -> Result<Resumed, StoreError> {
    let entries = parse_entries(external)?;
    import_entries(data_dir, cwd, external.source.label(), entries)
}

fn external_info(cwd: &Path, source: ExternalSource, path: PathBuf) -> Option<ExternalSessionInfo> {
    let first_cwd = recorded_cwd(&path).ok().flatten();
    if source == ExternalSource::Codex
        && first_cwd.as_deref() != Some(cwd.to_string_lossy().as_ref())
    {
        return None;
    }
    // Claude's project directory is already scoped, but retain the cwd check
    // when it is present so copied/moved JSONL files cannot leak in.
    if source == ExternalSource::Claude
        && first_cwd
            .as_deref()
            .is_some_and(|recorded| recorded != cwd.to_string_lossy())
    {
        return None;
    }
    // Listing must stay responsive even for a Codex JSONL with megabytes of
    // tool output.  Do not deserialize every historical result merely to
    // obtain the picker preview; only decode candidate user-message lines.
    let last_user_preview = last_user_preview(source, &path)?;
    let id = path.file_stem()?.to_string_lossy().into_owned();
    let modified = fs::metadata(&path).and_then(|m| m.modified()).ok();
    Some(ExternalSessionInfo {
        source,
        id,
        path,
        last_user_preview,
        modified,
    })
}

fn last_user_preview(source: ExternalSource, path: &Path) -> Option<String> {
    const TAIL_BYTES: u64 = 1024 * 1024;
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    if let Some(preview) = tail_preview(source, &String::from_utf8_lossy(&tail)) {
        return Some(preview);
    }

    // A very long final tool output can theoretically push the user's last
    // message beyond the tail. Fall back only in that uncommon case.
    let mut latest = None;
    for line in BufReader::new(File::open(path).ok()?)
        .lines()
        .map_while(Result::ok)
    {
        if let Some(preview) = preview_from_line(source, &line) {
            latest = Some(preview);
        }
    }
    latest
}

fn tail_preview(source: ExternalSource, tail: &str) -> Option<String> {
    tail.lines()
        .rev()
        .find_map(|line| preview_from_line(source, line))
}

fn preview_from_line(source: ExternalSource, line: &str) -> Option<String> {
    let is_candidate = match source {
        ExternalSource::Codex => line.contains("\"type\":\"user_message\""),
        ExternalSource::Claude => line.contains("\"type\":\"user\""),
    };
    if !is_candidate {
        return None;
    }
    let value: Value = serde_json::from_str(line).ok()?;
    let text = match source {
        ExternalSource::Codex => value.pointer("/payload/message").and_then(Value::as_str),
        ExternalSource::Claude => value.pointer("/message/content").and_then(Value::as_str),
    }?;
    if is_transcript_noise(text) {
        return None;
    }
    text.lines()
        .next()
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
}

fn parse_entries(external: &ExternalSessionInfo) -> Result<Vec<Entry>, StoreError> {
    parse_entries_at(external.source, &external.path)
}

fn parse_entries_at(source: ExternalSource, path: &Path) -> Result<Vec<Entry>, StoreError> {
    let mut entries = Vec::new();
    let mut suppressed_calls = HashSet::new();
    for line in BufReader::new(File::open(path)?).lines() {
        let value: Value = serde_json::from_str(&line?)?;
        match source {
            ExternalSource::Codex => entries.extend(codex_entries(&value, &mut suppressed_calls)),
            ExternalSource::Claude => entries.extend(claude_entries(&value)),
        }
    }
    Ok(entries)
}

fn recorded_cwd(path: &Path) -> Result<Option<String>, StoreError> {
    for line in BufReader::new(File::open(path)?).lines() {
        let value: Value = serde_json::from_str(&line?)?;
        if let Some(cwd) = value.get("cwd").and_then(Value::as_str) {
            return Ok(Some(cwd.to_owned()));
        }
        if let Some(cwd) = value.pointer("/payload/cwd").and_then(Value::as_str) {
            return Ok(Some(cwd.to_owned()));
        }
    }
    Ok(None)
}

fn codex_entries(value: &Value, suppressed_calls: &mut HashSet<String>) -> Vec<Entry> {
    if value.get("type") == Some(&Value::String("event_msg".into()))
        && value.pointer("/payload/type").and_then(Value::as_str) == Some("user_message")
    {
        return value
            .pointer("/payload/message")
            .and_then(Value::as_str)
            .filter(|text| !is_transcript_noise(text))
            .map(|text| vec![text_entry(true, text.to_owned())])
            .unwrap_or_default();
    }
    if value.get("type").and_then(Value::as_str) != Some("response_item") {
        return Vec::new();
    }
    let Some(payload) = value.get("payload") else {
        return Vec::new();
    };
    match payload.get("type").and_then(Value::as_str) {
        Some("message") if payload.get("role").and_then(Value::as_str) == Some("assistant") => {
            let text = payload.get("content").map(content_text).unwrap_or_default();
            if !text.is_empty() {
                vec![text_entry(false, text)]
            } else {
                Default::default()
            }
        }
        Some("function_call") => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            if name == "wait" || name.ends_with(".wait") {
                if let Some(call_id) = payload.get("call_id").and_then(Value::as_str) {
                    suppressed_calls.insert(call_id.to_owned());
                }
                return Vec::new();
            }
            let arguments = payload
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("");
            let (name, input, content) = normalize_codex_call(name, arguments);
            vec![Entry::ImportedTool {
                name,
                input,
                content,
            }]
        }
        Some("custom_tool_call") => custom_codex_call(payload),
        Some("function_call_output") | Some("custom_tool_call_output") => {
            if payload
                .get("call_id")
                .and_then(Value::as_str)
                .is_some_and(|call_id| suppressed_calls.remove(call_id))
            {
                return Vec::new();
            }
            let output = payload.get("output").map(content_text).unwrap_or_default();
            (!output.trim().is_empty())
                .then(|| Entry::ImportedTool {
                    name: "output".into(),
                    input: Value::Null,
                    content: compact_output(&output),
                })
                .into_iter()
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Current Codex records local tools as `custom_tool_call`: the outer name is
/// often just `exec`, while the actual tcode-like action is embedded in its
/// JavaScript source. Decode the two common forms rather than dropping them.
fn custom_codex_call(payload: &Value) -> Vec<Entry> {
    let source = payload.get("input").and_then(Value::as_str).unwrap_or("");
    if source.contains("tools.apply_patch(") {
        if let Some(patch) = js_value_after(source, "const patch = ")
            .and_then(|value| value.as_str().map(str::to_owned))
        {
            return vec![Entry::ImportedTool {
                name: "apply_patch".into(),
                input: Value::Null,
                content: cap_text(&patch, 12_000),
            }];
        }
    }
    if let Some(arguments) = js_value_after(source, "tools.exec_command(") {
        let (name, input, content) = normalize_codex_call("exec", &arguments.to_string());
        return vec![Entry::ImportedTool {
            name,
            input,
            content,
        }];
    }
    // Keep an uncommon custom call visible under its own label, but do not
    // leak its implementation wrapper into the resumed model prompt.
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    vec![Entry::ImportedTool {
        name: name.to_owned(),
        input: Value::Null,
        content: cap_text(source, 2_000),
    }]
}

/// Parse one JSON value embedded after a known JavaScript call prefix. Serde's
/// streaming deserializer consumes only the value and tolerates `);` after it.
fn js_value_after(source: &str, marker: &str) -> Option<Value> {
    let rest = source.split_once(marker)?.1.trim_start();
    let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
    stream.next()?.ok()
}

fn text_entry(is_user: bool, text: String) -> Entry {
    let block = vec![ContentBlock::Text { text }];
    if is_user {
        Entry::User(block)
    } else {
        Entry::Assistant(block)
    }
}

/// Reduce a Codex call to what actually happened: a patch is a patch, an
/// exec is a shell command. No guessing which tcode tool it "would have
/// been" — imported history must not fabricate inputs that never existed.
fn normalize_codex_call(name: &str, arguments: &str) -> (String, Value, String) {
    let parsed = serde_json::from_str::<Value>(arguments).ok();
    if name.contains("apply_patch") {
        if let Some(patch) = parsed
            .as_ref()
            .and_then(|value| value.get("patch"))
            .and_then(Value::as_str)
        {
            return ("apply_patch".into(), Value::Null, cap_text(patch, 12_000));
        }
    }
    if name.ends_with("exec") || name.ends_with("exec_command") {
        if let Some(command) = parsed
            .as_ref()
            .and_then(|value| value.get("cmd").or_else(|| value.get("command")))
            .and_then(Value::as_str)
        {
            return (
                "shell".into(),
                serde_json::json!({ "command": command }),
                String::new(),
            );
        }
    }
    let body = parsed
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| arguments.to_owned());
    (
        name.to_owned(),
        Value::Null,
        format!("```json\n{}\n```", cap_text(&body, 2_000)),
    )
}

fn compact_output(output: &str) -> String {
    let output = strip_codex_envelope(output);
    if let Some(summary) = test_summary(output) {
        return summary;
    }
    cap_text(output, 1_200)
}

/// `custom_tool_call_output` contains a tiny execution receipt followed by
/// the real stdout. The receipt is transport noise, not imported history.
fn strip_codex_envelope(output: &str) -> &str {
    output
        .split_once("\nOutput:\n")
        .map(|(_, body)| body.trim_start_matches('\n'))
        .unwrap_or(output)
}

/// Cargo prints one block per test target, including many unhelpful `0 tests`
/// blocks. The live UI already shows only a preview; imported history applies
/// the same signal-to-noise rule while retaining the original JSONL on disk.
fn test_summary(output: &str) -> Option<String> {
    let running: Vec<&str> = output
        .lines()
        .filter(|line| line.trim_start().starts_with("running ") && line.contains(" tests"))
        .collect();
    let passed: Vec<&str> = output
        .lines()
        .filter(|line| line.trim_start().starts_with("test result: ok."))
        .collect();
    let meaningful = running
        .iter()
        .copied()
        .find(|line| !line.contains("running 0 tests"))?;
    let passed = passed
        .iter()
        .copied()
        .find(|line| !line.contains("0 passed"))
        .unwrap_or_else(|| passed.last().copied().unwrap_or("test result: ok."));
    Some(format!("{meaningful}\n… test output folded …\n{passed}"))
}

fn cap_text(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{prefix}\n… historical output truncated …")
    } else {
        prefix
    }
}

/// One Claude Code JSONL record → ledger entries. Text becomes normal
/// user/assistant entries; tool_use calls and tool_result outputs become
/// `Entry::ImportedTool` (transcript-only), keeping Claude's real tool
/// names and inputs — no mapping guesses.
fn claude_entries(value: &Value) -> Vec<Entry> {
    if value.get("isSidechain").and_then(Value::as_bool) == Some(true)
        || value.get("isMeta").and_then(Value::as_bool) == Some(true)
    {
        return Vec::new();
    }
    let is_user = match value.get("type").and_then(Value::as_str) {
        Some("user") => true,
        Some("assistant") => false,
        _ => return Vec::new(),
    };
    let Some(content) = value.pointer("/message/content") else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    let mut text = String::new();
    let parts = match content {
        Value::String(t) => {
            text.push_str(t);
            &[][..]
        }
        Value::Array(parts) => parts.as_slice(),
        _ => &[][..],
    };
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
            }
            Some("tool_use") => {
                flush_text(is_user, &mut text, &mut entries);
                entries.push(Entry::ImportedTool {
                    name: part
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_owned(),
                    input: part.get("input").cloned().unwrap_or(Value::Null),
                    content: String::new(),
                });
            }
            Some("tool_result") => {
                flush_text(is_user, &mut text, &mut entries);
                let output = part.get("content").map(content_text).unwrap_or_default();
                if !output.trim().is_empty() {
                    entries.push(Entry::ImportedTool {
                        name: "output".into(),
                        input: Value::Null,
                        content: compact_output(&output),
                    });
                }
            }
            _ => {}
        }
    }
    flush_text(is_user, &mut text, &mut entries);
    entries
}

fn flush_text(is_user: bool, text: &mut String, entries: &mut Vec<Entry>) {
    let taken = std::mem::take(text);
    let trimmed = taken.trim();
    if !trimmed.is_empty() && !is_transcript_noise(trimmed) {
        entries.push(text_entry(is_user, trimmed.to_owned()));
    }
}

/// Harness-injected records that read as noise outside their original UI:
/// slash-command envelopes, caveat banners, and environment blobs.
fn is_transcript_noise(text: &str) -> bool {
    let head = text.trim_start();
    head.starts_with("<command-")
        || head.starts_with("<local-command")
        || head.starts_with("<system-reminder>")
        || head.starts_with("<user_instructions>")
        || head.starts_with("<environment_context>")
        || head.starts_with("Caveat: the messages below")
}

fn content_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part {
                Value::String(text) => Some(text.clone()),
                Value::Object(_) => part.get("text").and_then(Value::as_str).map(str::to_owned),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Claude Code derives its project directory by replacing every
/// non-alphanumeric character with '-' (`C:\code\rust\tcode` →
/// `C--code-rust-tcode`), so the same rule works on every platform.
fn claude_project_name(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn jsonl_files(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect()
}

fn jsonl_files_recursive(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                files.push(path);
            }
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_project_name_matches_claude_code_on_windows_paths() {
        assert_eq!(
            claude_project_name(Path::new(r"C:\code\rust\tcode")),
            "C--code-rust-tcode"
        );
        assert_eq!(
            claude_project_name(Path::new("/home/user/proj")),
            "-home-user-proj"
        );
    }

    #[test]
    fn codex_exec_maps_to_shell_without_guessing_other_tools() {
        for cmd in ["rg -n foo src/", "sed -n 1,40p main.rs", "cargo test"] {
            let (name, input, content) = normalize_codex_call(
                "exec_command",
                &format!(r#"{{"cmd":{}}}"#, Value::from(cmd)),
            );
            assert_eq!(name, "shell");
            assert_eq!(input["command"].as_str(), Some(cmd));
            assert!(content.is_empty());
        }
    }

    #[test]
    fn claude_assistant_tool_use_becomes_imported_tool() {
        let record: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[
                {"type":"text","text":"Let me check."},
                {"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"src/main.rs"}}
            ]}}"#,
        )
        .unwrap();
        let entries = claude_entries(&record);
        assert_eq!(entries.len(), 2);
        assert!(matches!(&entries[0], Entry::Assistant(_)));
        match &entries[1] {
            Entry::ImportedTool { name, input, .. } => {
                assert_eq!(name, "Read");
                assert_eq!(input["file_path"].as_str(), Some("src/main.rs"));
            }
            other => panic!("expected ImportedTool, got {other:?}"),
        }
    }

    #[test]
    fn claude_tool_result_becomes_output_entry() {
        let record: Value = serde_json::from_str(
            r#"{"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"fn main() {}"}]}
            ]}}"#,
        )
        .unwrap();
        let entries = claude_entries(&record);
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            Entry::ImportedTool { name, content, .. } => {
                assert_eq!(name, "output");
                assert!(content.contains("fn main"));
            }
            other => panic!("expected ImportedTool, got {other:?}"),
        }
    }

    #[test]
    fn claude_sidechain_and_meta_records_are_skipped() {
        let record: Value = serde_json::from_str(
            r#"{"type":"user","isSidechain":true,"message":{"content":"hi"}}"#,
        )
        .unwrap();
        assert!(claude_entries(&record).is_empty());
    }

    #[test]
    fn harness_envelopes_are_noise() {
        assert!(is_transcript_noise("<command-name>/clear</command-name>"));
        assert!(is_transcript_noise(
            "Caveat: the messages below were generated…"
        ));
        assert!(!is_transcript_noise("please fix <T> handling in parse"));
    }
}
