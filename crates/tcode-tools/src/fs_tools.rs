use std::collections::HashSet;
use std::path::Path;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::freshness::{content_hash, ReadStatus};
use tcode_core::{BatchPolicy, PermissionRequest, Tool, ToolCtx, ToolOutput};

const DEFAULT_READ_LIMIT: usize = 2000;
/// Requests below this are widened: extra lines are cheap, but a model
/// walking a file in 10-line slices costs a round-trip per slice.
const MIN_READ_WINDOW: usize = 120;
const MAX_LINE_CHARS: usize = 500;
/// Files above this are never slurped into memory. A range read of a giant
/// log/dataset belongs to grep or `sed -n`, not a full load.
const MAX_READ_FILE_BYTES: u64 = 10 * 1024 * 1024;
/// Cap the bytes a single read emits into context, independent of the line
/// count — 2000 lines of long lines would otherwise be ~1 MB.
const MAX_READ_OUTPUT_BYTES: usize = 128 * 1024;
/// Largest image inlined into a tool result. Anthropic caps images near 5 MB;
/// bigger ones are rejected with a resize hint rather than silently failing.
const MAX_IMAGE_INLINE_BYTES: u64 = 5 * 1024 * 1024;

/// Sniff a supported raster image by magic bytes; the extension is not trusted.
fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn rel<'a>(path: &'a Path, cwd: &Path) -> &'a Path {
    path.strip_prefix(cwd).unwrap_or(path)
}

/// Render numbered lines until the line count runs out or the byte budget is
/// hit. Returns the text and how many lines were actually emitted, so the
/// caller can tell the model where to resume.
fn numbered_capped(lines: &[&str], start: usize, budget: usize) -> (String, usize) {
    let mut out = String::new();
    let mut emitted = 0;
    for (i, line) in lines.iter().enumerate() {
        let clipped: String = if line.chars().count() > MAX_LINE_CHARS {
            line.chars().take(MAX_LINE_CHARS).collect::<String>() + "…"
        } else {
            (*line).to_string()
        };
        let row = format!("{:>6}\t{clipped}\n", start + i);
        // Always emit at least one line so a single huge line still makes progress.
        if emitted > 0 && out.len() + row.len() > budget {
            break;
        }
        out.push_str(&row);
        emitted += 1;
    }
    (out, emitted)
}

fn numbered(lines: &[&str], start: usize) -> String {
    numbered_capped(lines, start, usize::MAX).0
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

    fn batch_policy(&self) -> BatchPolicy {
        BatchPolicy::ParallelReadOnly
    }

    fn batch_label(&self, inputs: &[&Value]) -> String {
        // Multiple reads of one file are ranges within it, not distinct files.
        let unique_paths: HashSet<&str> =
            inputs.iter().filter_map(|i| i["path"].as_str()).collect();
        let count = inputs.len();
        if unique_paths.len() < count {
            format!("Read {count} ranges")
        } else {
            format!("Read {count} {}", if count == 1 { "file" } else { "files" })
        }
    }

    // Self-paginating via offset/limit — never blob-gate.
    fn gates_output(&self) -> bool {
        false
    }

    fn description(&self) -> &str {
        "Read a file with line numbers. Use offset/limit for large files; \
         limits under 120 lines are widened to 120, so read generous windows \
         instead of many small slices. Images (png/jpeg/gif/webp) are read \
         straight into context so you can see them; other binaries are \
         refused. Need several files or regions? Issue all reads in one \
         message — they run in parallel. If the harness reports the file \
         unchanged since your last read, the content is already in your \
         context — do not re-read; pass force=true only if you have a \
         specific reason."
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
        // Stat first so a huge file is rejected before it is loaded into memory.
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolOutput::err(not_found_help(&path));
            }
            Err(e) => return ToolOutput::err(format!("cannot read {}: {e}", path.display())),
        };
        if meta.is_dir() {
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
        if meta.len() > MAX_READ_FILE_BYTES {
            return ToolOutput::err(format!(
                "{} is {:.1} MB — too large to load into context. Search it with \
                 grep, or read a specific range via shell, e.g. `sed -n '2000,2100p'`.",
                rel(&path, &ctx.cwd).display(),
                meta.len() as f64 / (1024.0 * 1024.0),
            ));
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolOutput::err(not_found_help(&path));
            }
            Err(e) => return ToolOutput::err(format!("cannot read {}: {e}", path.display())),
        };
        // Supported images are read into context as image blocks, deduped by
        // content hash like a text read. This must come before the binary
        // rejection below, since images are binary.
        if let Some(mime) = detect_image_mime(&bytes) {
            let hash = content_hash(&bytes);
            let force = input["force"].as_bool().unwrap_or(false);
            let mut freshness = ctx.freshness.lock().expect("freshness lock");
            let status = freshness.check_read(&path, hash, None);
            if status == ReadStatus::Unchanged && !force {
                return ToolOutput::ok(format!(
                    "unchanged: {} has not changed since you last read it; the image \
                     is already in your context above. (force=true overrides.)",
                    rel(&path, &ctx.cwd).display()
                ));
            }
            freshness.record_read(&path, hash, None);
            drop(freshness);
            if bytes.len() as u64 > MAX_IMAGE_INLINE_BYTES {
                return ToolOutput::err(format!(
                    "{} is a {mime} image but {:.1} MB exceeds the {} MB inline limit; \
                     resize it smaller before reading.",
                    rel(&path, &ctx.cwd).display(),
                    bytes.len() as f64 / (1024.0 * 1024.0),
                    MAX_IMAGE_INLINE_BYTES / (1024 * 1024),
                ));
            }
            use base64::Engine as _;
            let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let text = format!(
                "Read image {} ({mime}, {:.0} KB).",
                rel(&path, &ctx.cwd).display(),
                bytes.len() as f64 / 1024.0,
            );
            return ToolOutput::ok(text).with_images(vec![tcode_core::ContentBlock::Image {
                media_type: mime.to_string(),
                data,
            }]);
        }
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
        // Overlapping re-read (e.g. same offset, wider window): return only
        // the unseen slice so already-seen lines aren't re-appended to the
        // ledger. Full reads and fragmented gaps fall through to the request.
        let (mut view_start, mut view_end) = (start, end);
        let mut overlap_note: Option<String> = None;
        if status == ReadStatus::NewRange && !force {
            if let Some((gs, ge)) = range.and_then(|r| freshness.uncovered_gap(&path, hash, r)) {
                view_start = gs - 1;
                view_end = ge;
                overlap_note = Some(format!(
                    "note: showing only the new lines {gs}-{ge}; the rest of the \
                     requested range {}-{end} is already in your context from an \
                     earlier read.\n",
                    start + 1
                ));
            }
        }
        freshness.record_read(&path, hash, range);
        drop(freshness);

        let mut out = String::new();
        if status == ReadStatus::ChangedOnDisk {
            out.push_str("note: this file changed on disk since you last read it.\n");
        }
        if let Some(note) = overlap_note {
            out.push_str(&note);
        }
        let (body, emitted) = numbered_capped(
            &lines[view_start..view_end],
            view_start + 1,
            MAX_READ_OUTPUT_BYTES,
        );
        out.push_str(&body);
        let shown_end = view_start + emitted;
        if shown_end < total {
            out.push_str(&format!(
                "[showing lines {}-{shown_end} of {total}; continue with offset={}]",
                view_start + 1,
                shown_end + 1
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

    fn batch_policy(&self) -> BatchPolicy {
        BatchPolicy::ParallelPerFile
    }

    fn batch_label(&self, inputs: &[&Value]) -> String {
        let count = inputs.len();
        format!(
            "Write {count} {}",
            if count == 1 { "file" } else { "files" }
        )
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

    fn batch_policy(&self) -> BatchPolicy {
        BatchPolicy::ParallelPerFile
    }

    fn batch_label(&self, inputs: &[&Value]) -> String {
        let count = inputs.len();
        format!("Edit {count} {}", if count == 1 { "file" } else { "files" })
    }

    fn description(&self) -> &str {
        "Exact string replacement in a file. `old_string` must match the \
         current content exactly (including whitespace; line endings may be \
         LF/CRLF and are normalized to the file's style) and be unique unless \
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

        let Some(plan) = replacement_plan(&text, old, new) else {
            let mut msg = near_miss_help(&text, old);
            if !seen {
                msg.push_str(
                    "\nnote: you have not read the current version of this \
                         file; read it to get the exact text.",
                );
            }
            return ToolOutput::err(msg);
        };
        let replace_all = input["replace_all"].as_bool().unwrap_or(false);
        match plan.count {
            0 => unreachable!("replacement_plan only returns matching needles"),
            1 => {}
            n if !replace_all => {
                let occurrences = occurrence_help(&text, &plan.old, 8);
                return ToolOutput::err(format!(
                    "old_string appears {n} times; add surrounding context to make it \
                     unique, or set replace_all=true.\nOccurrences:\n{}",
                    occurrences.join("\n")
                ));
            }
            _ => {}
        }

        let new_text = if replace_all {
            text.replace(&plan.old, &plan.new)
        } else {
            text.replacen(&plan.old, &plan.new, 1)
        };
        if let Err(e) = std::fs::write(&path, &new_text) {
            return ToolOutput::err(format!("cannot write {}: {e}", path.display()));
        }
        freshness.record_write(&path, content_hash(new_text.as_bytes()));

        // Show the edited region so the model sees the result without
        // re-reading the file.
        let pos = new_text.find(&plan.new).unwrap_or(0);
        let line_no = new_text[..pos].lines().count().max(1);
        let lines: Vec<&str> = new_text.lines().collect();
        let start = line_no.saturating_sub(3).max(1);
        let end = (line_no + plan.new.lines().count() + 2).min(lines.len());
        let snippet = numbered(&lines[start - 1..end], start);
        ToolOutput::ok(format!(
            "edited {} ({} replacement{}). Result:\n{snippet}",
            rel(&path, &ctx.cwd).display(),
            if replace_all { plan.count } else { 1 },
            if replace_all && plan.count > 1 {
                "s"
            } else {
                ""
            },
        ))
    }
}

struct ReplacementPlan {
    old: String,
    new: String,
    count: usize,
}

fn replacement_plan(text: &str, old: &str, new: &str) -> Option<ReplacementPlan> {
    let eol = dominant_line_ending(text);
    let mut candidates = Vec::new();
    candidates.push((old.to_string(), normalize_newlines(new, eol)));
    if old.contains('\n') || old.contains('\r') {
        candidates.push((normalize_newlines(old, eol), normalize_newlines(new, eol)));
        candidates.push((normalize_newlines(old, "\n"), normalize_newlines(new, "\n")));
        candidates.push((
            normalize_newlines(old, "\r\n"),
            normalize_newlines(new, "\r\n"),
        ));
    }

    let mut seen = std::collections::HashSet::new();
    let exact = candidates.into_iter().find_map(|(old, new)| {
        if !seen.insert(old.clone()) {
            return None;
        }
        let count = text.match_indices(&old).count();
        (count > 0).then_some(ReplacementPlan { old, new, count })
    });
    if exact.is_some() {
        return exact;
    }
    // Last resort: models often emit typographic punctuation (– " " …) where
    // the file has plain ASCII. Match with punctuation normalized, but splice
    // the *actual* file bytes back in so nothing else is disturbed.
    let orig = find_punct_normalized(text, old)?;
    let count = text.match_indices(&orig).count();
    (count > 0).then_some(ReplacementPlan {
        old: orig,
        new: normalize_newlines(new, eol),
        count,
    })
}

/// Map common typographic punctuation to its ASCII equivalent. Only 1-char →
/// 1-char maps, so char positions stay aligned between original and normalized.
fn normalize_punct(c: char) -> char {
    match c {
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-', // hyphens, dashes, minus
        '\u{2018}' | '\u{2019}' | '\u{201B}' => '\'', // single quotes
        '\u{201C}' | '\u{201D}' | '\u{201F}' => '"', // double quotes
        _ => c,
    }
}

/// Find `old` in `text` comparing with punctuation normalized, and return the
/// exact original substring at that location (so the real bytes are replaced).
/// Returns None when `old` has no typographic punctuation to normalize.
fn find_punct_normalized(text: &str, old: &str) -> Option<String> {
    let pat: Vec<char> = old.chars().map(normalize_punct).collect();
    if pat.iter().copied().eq(old.chars()) {
        return None; // nothing was normalized — the exact pass already tried this
    }
    let tchars: Vec<char> = text.chars().collect();
    if pat.is_empty() || pat.len() > tchars.len() {
        return None;
    }
    (0..=tchars.len() - pat.len()).find_map(|i| {
        let window = &tchars[i..i + pat.len()];
        window
            .iter()
            .copied()
            .map(normalize_punct)
            .eq(pat.iter().copied())
            .then(|| window.iter().collect())
    })
}

fn normalize_newlines(s: &str, eol: &str) -> String {
    let lf = s.replace("\r\n", "\n").replace('\r', "\n");
    if eol == "\n" {
        lf
    } else {
        lf.replace('\n', eol)
    }
}

fn dominant_line_ending(text: &str) -> &'static str {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count().saturating_sub(crlf);
    if crlf > lf {
        "\r\n"
    } else {
        "\n"
    }
}

fn occurrence_help(text: &str, needle: &str, limit: usize) -> Vec<String> {
    text.match_indices(needle)
        .take(limit)
        .map(|(byte, _)| {
            let line = text[..byte].bytes().filter(|b| *b == b'\n').count() + 1;
            let body = text[byte..]
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .chars()
                .take(160)
                .collect::<String>();
            format!("  line {line}: {body}")
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_match_accepts_lf_old_string_in_crlf_file() {
        let text = "one\r\ntwo\r\nthree\r\n";
        let plan = replacement_plan(text, "two\nthree\n", "deux\ntrois\n").unwrap();

        assert_eq!(plan.old, "two\r\nthree\r\n");
        assert_eq!(plan.new, "deux\r\ntrois\r\n");
        assert_eq!(
            text.replacen(&plan.old, &plan.new, 1),
            "one\r\ndeux\r\ntrois\r\n"
        );
    }

    #[test]
    fn edit_matches_through_typographic_punctuation() {
        // File has ASCII; model's old_string uses an en-dash and curly quotes.
        let text = "let x = a - b; // \"note\"\n";
        let old = "a \u{2013} b; // \u{201C}note\u{201D}";
        let plan = replacement_plan(text, old, "a + b; // ok").unwrap();
        assert_eq!(plan.count, 1);
        assert_eq!(plan.old, "a - b; // \"note\"");
        assert_eq!(
            text.replacen(&plan.old, &plan.new, 1),
            "let x = a + b; // ok\n"
        );
    }

    #[test]
    fn edit_occurrences_use_actual_match_line_not_first_probe_line() {
        let text = "header\nalpha\nbeta\nalpha\nbeta\n";
        let needle = "alpha\nbeta\n";
        assert_eq!(
            occurrence_help(text, needle, 8),
            vec!["  line 2: alpha", "  line 4: alpha"]
        );
    }

    #[test]
    fn detect_image_mime_by_magic_bytes() {
        assert_eq!(
            detect_image_mime(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            Some("image/png")
        );
        assert_eq!(
            detect_image_mime(&[0xFF, 0xD8, 0xFF, 0x00]),
            Some("image/jpeg")
        );
        assert_eq!(detect_image_mime(b"GIF89a....."), Some("image/gif"));
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBP");
        assert_eq!(detect_image_mime(&webp), Some("image/webp"));
        // Plain text is not an image even though it starts with printable bytes.
        assert_eq!(detect_image_mime(b"#!/bin/sh\n"), None);
    }

    #[tokio::test]
    async fn read_inlines_a_png_as_an_image_block() {
        let dir = std::env::temp_dir().join(format!("tcode-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("shot.png");
        // Valid PNG magic + arbitrary payload (incl. null bytes) is enough:
        // detection is by magic bytes and the body is base64-encoded verbatim.
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&[0u8; 32]);
        std::fs::write(&png, &bytes).unwrap();

        let ctx = ToolCtx::new(dir.clone(), 10_000);
        let out = ReadTool
            .run(
                json!({ "path": png.to_str().unwrap() }),
                &ctx,
                &CancellationToken::new(),
            )
            .await;

        assert!(!out.is_error);
        assert!(out.content.contains("Read image"));
        assert_eq!(out.images.len(), 1);
        assert!(matches!(
            &out.images[0],
            tcode_core::ContentBlock::Image { media_type, .. } if media_type == "image/png"
        ));

        // A second read of the unchanged image dedupes: no image re-sent.
        let again = ReadTool
            .run(
                json!({ "path": png.to_str().unwrap() }),
                &ctx,
                &CancellationToken::new(),
            )
            .await;
        assert!(again.images.is_empty());
        assert!(again.content.contains("unchanged"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
