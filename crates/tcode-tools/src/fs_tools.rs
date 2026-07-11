use std::path::Path;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::freshness::{content_hash, ReadStatus};
use tcode_core::{PermissionRequest, Tool, ToolCtx, ToolOutput};

const DEFAULT_READ_LIMIT: usize = 2000;
/// Requests below this are widened: extra lines are cheap, but a model
/// walking a file in 10-line slices costs a round-trip per slice.
const MIN_READ_WINDOW: usize = 120;
const MAX_LINE_CHARS: usize = 500;

fn rel<'a>(path: &'a Path, cwd: &Path) -> &'a Path {
    path.strip_prefix(cwd).unwrap_or(path)
}

fn numbered(lines: &[&str], start: usize) -> String {
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        let clipped: String = if line.chars().count() > MAX_LINE_CHARS {
            line.chars().take(MAX_LINE_CHARS).collect::<String>() + "…"
        } else {
            (*line).to_string()
        };
        out.push_str(&format!("{:>6}\t{clipped}\n", start + i));
    }
    out
}

/// Self-healing ENOENT: show what IS there so the model can correct the
/// path without another exploratory turn.
fn not_found_help(path: &Path) -> String {
    let mut msg = format!("File not found: {}", path.display());
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    match parent {
        Some(dir) if dir.is_dir() => {
            let mut entries: Vec<String> = std::fs::read_dir(dir)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| {
                            let name = e.file_name().to_string_lossy().into_owned();
                            if e.path().is_dir() {
                                format!("{name}/")
                            } else {
                                name
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            entries.sort();
            entries.truncate(20);
            msg.push_str(&format!(
                "\nThe directory {} exists and contains: {}",
                dir.display(),
                entries.join(", ")
            ));
        }
        Some(dir) => {
            msg.push_str(&format!(
                "\nThe directory {} does not exist either.",
                dir.display()
            ));
        }
        None => {}
    }
    msg
}

// ---------------------------------------------------------------- read

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a file with line numbers. Use offset/limit for large files; \
         limits under 120 lines are widened to 120, so read generous windows \
         instead of many small slices. Need several files or regions? Issue \
         all reads in one message — they run in parallel. If the harness \
         reports the file unchanged since your last read, the content is \
         already in your context — do not re-read; pass force=true only if \
         you have a specific reason."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path (absolute or relative to cwd)" },
                "offset": { "type": "integer", "description": "1-based first line to read" },
                "limit": { "type": "integer", "description": "Max lines to read (default 2000)" },
                "force": { "type": "boolean", "description": "Bypass the unchanged-file check" }
            },
            "required": ["path"]
        })
    }

    fn permission(&self, _input: &Value) -> PermissionRequest {
        PermissionRequest::None
    }

    fn context_paths(&self, input: &Value) -> Vec<String> {
        input["path"]
            .as_str()
            .map(String::from)
            .into_iter()
            .collect()
    }

    async fn run(&self, input: Value, ctx: &ToolCtx, _cancel: &CancellationToken) -> ToolOutput {
        let Some(path_str) = input["path"].as_str() else {
            return ToolOutput::err("missing required parameter: path");
        };
        let path = ctx.resolve(path_str);
        if path.is_dir() {
            let listing = std::fs::read_dir(&path)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .take(50)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            return ToolOutput::err(format!(
                "{} is a directory, not a file. It contains: {listing}",
                path.display()
            ));
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolOutput::err(not_found_help(&path));
            }
            Err(e) => return ToolOutput::err(format!("cannot read {}: {e}", path.display())),
        };
        if bytes[..bytes.len().min(8192)].contains(&0) {
            return ToolOutput::err(format!(
                "{} is a binary file ({} bytes); refusing to dump it into context.",
                path.display(),
                bytes.len()
            ));
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();

        let offset = (input["offset"].as_u64().unwrap_or(1) as usize).max(1);
        let limit = (input["limit"].as_u64().unwrap_or(DEFAULT_READ_LIMIT as u64) as usize)
            .max(MIN_READ_WINDOW);
        let start = (offset - 1).min(total);
        let end = start.saturating_add(limit).min(total);
        let whole_file = start == 0 && end == total;
        let range = if whole_file {
            None
        } else {
            Some((start + 1, end))
        };

        let hash = content_hash(&bytes);
        let force = input["force"].as_bool().unwrap_or(false);
        let mut freshness = ctx.freshness.lock().expect("freshness lock");
        let status = freshness.check_read(&path, hash, range);
        if status == ReadStatus::Unchanged && !force {
            return ToolOutput::ok(format!(
                "unchanged: {} has not changed since you last read it; the content \
                 is already in your context above. (force=true overrides.)",
                rel(&path, &ctx.cwd).display()
            ));
        }
        freshness.record_read(&path, hash, range);
        drop(freshness);

        let mut out = String::new();
        if status == ReadStatus::ChangedOnDisk {
            out.push_str("note: this file changed on disk since you last read it.\n");
        }
        out.push_str(&numbered(&lines[start..end], start + 1));
        if end < total {
            out.push_str(&format!(
                "[showing lines {}-{end} of {total}; continue with offset={}]",
                start + 1,
                end + 1
            ));
        }
        if out.is_empty() {
            out = "(empty file)".into();
        }
        ToolOutput::ok(out)
    }
}

// --------------------------------------------------------------- write

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Create or overwrite a file. Prefer `edit` for modifying existing \
         files. Overwriting an existing file requires having read its \
         current version."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        })
    }

    fn permission(&self, input: &Value) -> PermissionRequest {
        let path = input["path"].as_str().unwrap_or("?");
        PermissionRequest::Ask {
            descriptor: format!("write({path})"),
            summary: format!(
                "write {path} ({} bytes)",
                input["content"].as_str().map_or(0, |c| c.len())
            ),
            is_edit: true,
        }
    }

    fn touches(&self, input: &Value) -> Option<String> {
        input["path"].as_str().map(String::from)
    }

    fn context_paths(&self, input: &Value) -> Vec<String> {
        input["path"]
            .as_str()
            .map(String::from)
            .into_iter()
            .collect()
    }

    fn is_mutating(&self) -> bool {
        true
    }

    async fn run(&self, input: Value, ctx: &ToolCtx, _cancel: &CancellationToken) -> ToolOutput {
        let (Some(path_str), Some(content)) = (input["path"].as_str(), input["content"].as_str())
        else {
            return ToolOutput::err("missing required parameters: path, content");
        };
        let path = ctx.resolve(path_str);
        let mut freshness = ctx.freshness.lock().expect("freshness lock");
        if let Ok(existing) = std::fs::read(&path) {
            if !freshness.seen_current(&path, content_hash(&existing)) {
                return ToolOutput::err(format!(
                    "{} already exists and you have not read its current version; \
                     read it first so no content is destroyed unknowingly.",
                    rel(&path, &ctx.cwd).display()
                ));
            }
        }
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolOutput::err(format!("cannot create {}: {e}", parent.display()));
            }
        }
        if let Err(e) = std::fs::write(&path, content) {
            return ToolOutput::err(format!("cannot write {}: {e}", path.display()));
        }
        freshness.record_write(&path, content_hash(content.as_bytes()));
        ToolOutput::ok(format!(
            "wrote {} ({} lines)",
            rel(&path, &ctx.cwd).display(),
            content.lines().count()
        ))
    }
}

// ---------------------------------------------------------------- edit

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Exact string replacement in a file. `old_string` must match the \
         current content exactly (including whitespace) and be unique unless \
         replace_all is set. Only edit text you have actually seen in this \
         session (read or grep output both count) and whose surroundings you \
         understand; if you are unsure of the exact content or the impact of \
         the change, read the file first instead of guessing."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" },
                "replace_all": { "type": "boolean", "default": false }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn permission(&self, input: &Value) -> PermissionRequest {
        let path = input["path"].as_str().unwrap_or("?");
        PermissionRequest::Ask {
            descriptor: format!("edit({path})"),
            summary: format!("edit {path}"),
            is_edit: true,
        }
    }

    fn touches(&self, input: &Value) -> Option<String> {
        input["path"].as_str().map(String::from)
    }

    fn context_paths(&self, input: &Value) -> Vec<String> {
        input["path"]
            .as_str()
            .map(String::from)
            .into_iter()
            .collect()
    }

    fn is_mutating(&self) -> bool {
        true
    }

    async fn run(&self, input: Value, ctx: &ToolCtx, _cancel: &CancellationToken) -> ToolOutput {
        let (Some(path_str), Some(old), Some(new)) = (
            input["path"].as_str(),
            input["old_string"].as_str(),
            input["new_string"].as_str(),
        ) else {
            return ToolOutput::err("missing required parameters: path, old_string, new_string");
        };
        if old == new {
            return ToolOutput::err("old_string and new_string are identical");
        }
        let path = ctx.resolve(path_str);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolOutput::err(not_found_help(&path));
            }
            Err(e) => return ToolOutput::err(format!("cannot read {}: {e}", path.display())),
        };
        // No read-before-edit gate: the exact, unique match against current
        // disk content is the verification. A stale or guessed old_string
        // fails safely below.
        let mut freshness = ctx.freshness.lock().expect("freshness lock");
        let seen = freshness.seen_current(&path, content_hash(&bytes));
        let text = String::from_utf8_lossy(&bytes).into_owned();

        let count = text.matches(old).count();
        let replace_all = input["replace_all"].as_bool().unwrap_or(false);
        match count {
            0 => {
                let mut msg = near_miss_help(&text, old);
                if !seen {
                    msg.push_str(
                        "\nnote: you have not read the current version of this \
                         file; read it to get the exact text.",
                    );
                }
                return ToolOutput::err(msg);
            }
            1 => {}
            n if !replace_all => {
                let occurrences: Vec<String> = text
                    .lines()
                    .enumerate()
                    .filter(|(_, l)| l.contains(old.lines().next().unwrap_or(old)))
                    .take(8)
                    .map(|(i, l)| format!("  line {}: {}", i + 1, l.trim()))
                    .collect();
                return ToolOutput::err(format!(
                    "old_string appears {n} times; add surrounding context to make it \
                     unique, or set replace_all=true.\nOccurrences:\n{}",
                    occurrences.join("\n")
                ));
            }
            _ => {}
        }

        let new_text = if replace_all {
            text.replace(old, new)
        } else {
            text.replacen(old, new, 1)
        };
        if let Err(e) = std::fs::write(&path, &new_text) {
            return ToolOutput::err(format!("cannot write {}: {e}", path.display()));
        }
        freshness.record_write(&path, content_hash(new_text.as_bytes()));

        // Show the edited region so the model sees the result without
        // re-reading the file.
        let pos = new_text.find(new).unwrap_or(0);
        let line_no = new_text[..pos].lines().count().max(1);
        let lines: Vec<&str> = new_text.lines().collect();
        let start = line_no.saturating_sub(3).max(1);
        let end = (line_no + new.lines().count() + 2).min(lines.len());
        let snippet = numbered(&lines[start - 1..end], start);
        ToolOutput::ok(format!(
            "edited {} ({} replacement{}). Result:\n{snippet}",
            rel(&path, &ctx.cwd).display(),
            if replace_all { count } else { 1 },
            if replace_all && count > 1 { "s" } else { "" },
        ))
    }
}

/// Self-healing "old_string not found": locate the closest region so the
/// model can fix the mismatch without a re-read turn.
fn near_miss_help(text: &str, old: &str) -> String {
    let mut msg = String::from("old_string not found in file.");
    let probe = old
        .lines()
        .map(str::trim)
        .filter(|l| l.len() >= 8)
        .max_by_key(|l| l.len());
    if let Some(probe) = probe {
        if let Some((idx, _)) = text
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains(probe) || l.trim() == probe)
        {
            let lines: Vec<&str> = text.lines().collect();
            let start = idx.saturating_sub(3);
            let end = (idx + 4).min(lines.len());
            msg.push_str(&format!(
                " The closest matching region (whitespace/indentation likely \
                 differs from your old_string):\n{}",
                numbered(&lines[start..end], start + 1)
            ));
            return msg;
        }
    }
    msg.push_str(" No similar line found — the content may differ more than expected; re-read the relevant range.");
    msg
}
