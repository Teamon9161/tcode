//! Read-only import of conversations produced by other terminal agents.
//!
//! An import deliberately copies text messages into a new tcode session. Tool
//! calls/results are historical context, not actions tcode may safely replay.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::ledger::Entry;
use crate::store::{Resumed, SessionStore, StoreError};
use crate::types::ContentBlock;

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
    if entries.is_empty() {
        return Err(StoreError::External("no importable text messages".into()));
    }
    let mut store = SessionStore::create(data_dir, cwd)?;
    let mut ledger = crate::ledger::Ledger::new();
    for entry in entries {
        store.record(&crate::store::LogEvent::Append {
            entry: entry.clone(),
        });
        ledger.append(entry);
    }
    Ok(Resumed {
        store,
        ledger,
        checkpoints: Vec::new(),
    })
}

fn external_info(cwd: &Path, source: ExternalSource, path: PathBuf) -> Option<ExternalSessionInfo> {
    let first_cwd = recorded_cwd(&path).ok().flatten();
    if source == ExternalSource::Codex && first_cwd.as_deref() != Some(cwd.to_string_lossy().as_ref()) {
        return None;
    }
    // Claude's project directory is already scoped, but retain the cwd check
    // when it is present so copied/moved JSONL files cannot leak in.
    if source == ExternalSource::Claude
        && first_cwd.as_deref().is_some_and(|recorded| recorded != cwd.to_string_lossy())
    {
        return None;
    }
    // Listing must stay responsive even for a Codex JSONL with megabytes of
    // tool output.  Do not deserialize every historical result merely to
    // obtain the picker preview; only decode candidate user-message lines.
    let last_user_preview = last_user_preview(source, &path)?;
    let id = path.file_stem()?.to_string_lossy().into_owned();
    Some(ExternalSessionInfo {
        source,
        id,
        path,
        last_user_preview,
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
    for line in BufReader::new(File::open(path).ok()?).lines().flatten() {
        if let Some(preview) = preview_from_line(source, &line) {
            latest = Some(preview);
        }
    }
    latest
}

fn tail_preview(source: ExternalSource, tail: &str) -> Option<String> {
    tail.lines().rev().find_map(|line| preview_from_line(source, line))
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
            ExternalSource::Claude => {
                if let Some((is_user, text)) = claude_message(&value).filter(|(_, text)| !text.trim().is_empty()) {
                    entries.push(text_entry(is_user, text));
                }
            }
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
            (!text.is_empty()).then(|| vec![text_entry(false, text)]).unwrap_or_default()
        }
        Some("function_call") => {
            let name = payload.get("name").and_then(Value::as_str).unwrap_or("tool");
            if name == "wait" || name.ends_with(".wait") {
                if let Some(call_id) = payload.get("call_id").and_then(Value::as_str) {
                    suppressed_calls.insert(call_id.to_owned());
                }
                return Vec::new();
            }
            let arguments = payload.get("arguments").and_then(Value::as_str).unwrap_or("");
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
        if let Some(patch) = js_value_after(source, "const patch = ").and_then(|value| value.as_str().map(str::to_owned)) {
            return vec![Entry::ImportedTool {
                name: "apply_patch".into(),
                input: Value::Null,
                content: cap_text(&patch, 12_000),
            }];
        }
    }
    if let Some(arguments) = js_value_after(source, "tools.exec_command(") {
        let (name, input, content) = normalize_codex_call("exec", &arguments.to_string());
        return vec![Entry::ImportedTool { name, input, content }];
    }
    // Keep an uncommon custom call visible under its own label, but do not
    // leak its implementation wrapper into the resumed model prompt.
    let name = payload.get("name").and_then(Value::as_str).unwrap_or("tool");
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

/// Convert common Codex internals to the tcode vocabulary used in the live
/// transcript. The mapping is visual-only; imported calls remain non-runnable.
fn normalize_codex_call(name: &str, arguments: &str) -> (String, Value, String) {
    let parsed = serde_json::from_str::<Value>(arguments).ok();
    if name.contains("apply_patch") {
        if let Some(patch) = parsed
            .as_ref()
            .and_then(|value| value.get("patch"))
            .and_then(Value::as_str)
        {
            return (
                "apply_patch".into(),
                Value::Null,
                cap_text(patch, 12_000),
            );
        }
    }
    if name.ends_with("exec") || name.ends_with("exec_command") {
        if let Some(command) = parsed
            .as_ref()
            .and_then(|value| value.get("cmd").or_else(|| value.get("command")))
            .and_then(Value::as_str)
        {
            let tool = if command.trim_start().starts_with("rg ") {
                if command.contains("--files") { "glob" } else { "grep" }
            } else if matches!(command.trim_start().split_whitespace().next(), Some("sed" | "head" | "tail" | "cat")) {
                "read"
            } else {
                "shell"
            };
            let key = if tool == "shell" { "command" } else if tool == "read" { "path" } else { "pattern" };
            return (
                tool.into(),
                serde_json::json!({ key: command }),
                String::new(),
            );
        }
    }
    let body = parsed
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| arguments.to_owned());
    (name.to_owned(), Value::Null, format!("```json\n{}\n```", cap_text(&body, 2_000)))
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
    let meaningful = running.iter().copied().find(|line| !line.contains("running 0 tests"))?;
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

fn claude_message(value: &Value) -> Option<(bool, String)> {
    if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let kind = value.get("type").and_then(Value::as_str)?;
    let is_user = match kind {
        "user" => true,
        "assistant" => false,
        _ => return None,
    };
    let message = value.get("message")?;
    let text = content_text(message.get("content")?);
    (!text.is_empty()).then_some((is_user, text))
}

fn content_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part {
                Value::String(text) => Some(text.clone()),
                Value::Object(_) => part
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn claude_project_name(cwd: &Path) -> String {
    cwd.to_string_lossy().replace('/', "-")
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
