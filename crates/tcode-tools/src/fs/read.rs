use std::collections::HashSet;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::freshness::{content_hash, ReadStatus};
use tcode_core::images::detect_image_mime;
use tcode_core::{AutoSafety, BatchPolicy, PermissionRequest, Tool, ToolCtx, ToolOutput};

use super::{
    not_found_help, numbered_capped, rel, DEFAULT_READ_LIMIT, MAX_IMAGE_SOURCE_BYTES,
    MAX_READ_FILE_BYTES, MAX_READ_OUTPUT_BYTES, MIN_READ_WINDOW,
};

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
         instead of many small slices. Images (png/jpeg/gif/webp) come \
         straight into context so you can see them. If the harness reports the \
         file unchanged since your last read, the content is already in your \
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

    fn auto_safety(&self, _input: &Value) -> AutoSafety {
        AutoSafety::Allow
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
        let meta = match tokio::fs::metadata(&path).await {
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
        if meta.len() > MAX_IMAGE_SOURCE_BYTES {
            return ToolOutput::err(format!(
                "{} is {:.1} MB — too large to load safely. Images must be below {} MB before normalization; use a smaller source file.",
                rel(&path, &ctx.cwd).display(),
                meta.len() as f64 / (1024.0 * 1024.0),
                MAX_IMAGE_SOURCE_BYTES / (1024 * 1024),
            ));
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolOutput::err(not_found_help(&path));
            }
            Err(e) => return ToolOutput::err(format!("cannot read {}: {e}", path.display())),
        };
        if let Some(mime) = detect_image_mime(&bytes) {
            // A text-only model must not record this read: after `/model` swaps
            // to a vision model, freshness must not hide the image as unchanged.
            if ctx
                .model
                .as_ref()
                .is_some_and(|model| !model.snapshot().provider.supports_vision())
            {
                return ToolOutput::err(format!(
                    "{} is an image, but the current model cannot view images. Delegate it: view_image(paths=[\"{}\"], prompt=\"<specific question>\")",
                    rel(&path, &ctx.cwd).display(),
                    path_str,
                ));
            }
            let hash = content_hash(&bytes);
            let force = input["force"].as_bool().unwrap_or(false);
            let status = {
                let freshness = ctx.freshness.lock().expect("freshness lock");
                freshness.check_read(&path, hash, None)
            };
            if status == ReadStatus::Unchanged && !force {
                return ToolOutput::ok(format!(
                    "unchanged: {} has not changed since you last read it; the image \
                     is already in your context above. (force=true overrides.)",
                    rel(&path, &ctx.cwd).display()
                ));
            }
            let normalized = match tokio::task::spawn_blocking(move || {
                tcode_core::images::normalize_image(&bytes)
            })
            .await
            {
                Ok(Ok(image)) => image,
                Ok(Err(error)) => {
                    return ToolOutput::err(format!(
                        "{} is a {mime} image but could not be normalized: {error}",
                        rel(&path, &ctx.cwd).display()
                    ));
                }
                Err(error) => {
                    return ToolOutput::err(format!("image normalization failed: {error}"))
                }
            };
            {
                let mut freshness = ctx.freshness.lock().expect("freshness lock");
                freshness.record_read(&path, hash, None);
            }
            let dimensions = if normalized.resized {
                format!(" → {}x{}", normalized.width, normalized.height)
            } else {
                format!(" {}x{}", normalized.width, normalized.height)
            };
            let text = format!(
                "Read image {} ({mime},{dimensions}, {:.0} KB).",
                rel(&path, &ctx.cwd).display(),
                normalized.bytes.len() as f64 / 1024.0,
            );
            return ToolOutput::ok(text).with_images(vec![normalized.into_block()]);
        }
        if bytes.len() as u64 > MAX_READ_FILE_BYTES {
            return ToolOutput::err(format!(
                "{} is {:.1} MB — too large to load into context. Search it with \
                 grep, or read a specific range via shell, e.g. `sed -n '2000,2100p'`.",
                rel(&path, &ctx.cwd).display(),
                bytes.len() as f64 / (1024.0 * 1024.0),
            ));
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
        if start == end && total != 0 {
            return ToolOutput::ok(format!(
                "{} has {total} lines; offset {offset} is past the end of the file.",
                rel(&path, &ctx.cwd).display()
            ));
        }
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
        let recorded_range = if view_start == 0 && shown_end == total {
            None
        } else {
            Some((view_start + 1, shown_end))
        };
        // Freshness represents what reached the model, not what was requested:
        // `numbered_capped` can stop early on the output-byte budget.
        freshness.record_read(&path, hash, recorded_range);
        drop(freshness);
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
