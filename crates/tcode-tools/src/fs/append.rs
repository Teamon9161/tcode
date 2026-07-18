use std::collections::HashSet;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::freshness::{content_hash, Visibility};
use tcode_core::{AutoSafety, BatchPolicy, PermissionRequest, Tool, ToolCtx, ToolOutput};

use super::{numbered, rel, write_error, write_with_windows_retry};

pub struct AppendTool;

#[async_trait]
impl Tool for AppendTool {
    fn name(&self) -> &str {
        "append"
    }

    fn batch_policy(&self) -> BatchPolicy {
        BatchPolicy::ParallelPerFile
    }

    fn batch_label(&self, inputs: &[&Value]) -> String {
        let changes = inputs.len();
        let files: HashSet<&str> = inputs
            .iter()
            .filter_map(|input| input["path"].as_str())
            .collect();
        if changes == files.len() {
            format!(
                "Append {changes} {}",
                if changes == 1 { "file" } else { "files" }
            )
        } else {
            format!(
                "Append {changes} changes across {} {}",
                files.len(),
                if files.len() == 1 { "file" } else { "files" }
            )
        }
    }

    fn description(&self) -> &str {
        "Append text to the end of a UTF-8 file, written exactly as given — \
         no newline is added for you, so start with '\\n' if the file's last \
         line must remain intact. Appending to an existing file requires \
         having read its current version (a partial read counts); a missing \
         file is created. For insertion in the middle of a file use `edit`."
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
            descriptor: format!("append({path})"),
            aliases: Vec::new(),
            summary: format!(
                "append {} bytes to {path}",
                input["content"].as_str().map_or(0, |c| c.len())
            ),
            is_edit: true,
        }
    }

    fn auto_safety(&self, _input: &Value) -> AutoSafety {
        AutoSafety::AllowInProjectOrScratchEdit
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
        if content.is_empty() {
            return ToolOutput::err("content must not be empty");
        }
        let path = ctx.resolve(path_str);
        let old = match tokio::fs::read(&path).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    return ToolOutput::err(format!(
                        "{} is not valid UTF-8; append only supports text files \
                         and will not extend bytes lossily",
                        rel(&path, &ctx.cwd).display()
                    ));
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Missing file: create it. Everything in it is model-authored,
                // so the new version counts as fully seen.
                if let Some(parent) = path.parent() {
                    if let Err(e) = tokio::fs::create_dir_all(parent).await {
                        return ToolOutput::err(format!("cannot create {}: {e}", parent.display()));
                    }
                }
                if let Err(error) = write_with_windows_retry(&path, content.as_bytes()).await {
                    return ToolOutput::err(write_error(&path, &error));
                }
                ctx.freshness
                    .lock()
                    .expect("freshness lock")
                    .record_write(&path, content_hash(content.as_bytes()));
                let lines: Vec<&str> = content.lines().collect();
                let count = lines.len();
                let snippet = numbered(&lines, 1);
                return ToolOutput::ok(format!(
                    "created new file {} ({count} line{}). Result:\n{snippet}",
                    rel(&path, &ctx.cwd).display(),
                    if count == 1 { "" } else { "s" },
                ));
            }
            Err(e) => return ToolOutput::err(format!("cannot read {}: {e}", path.display())),
        };
        // Gate: the model must have seen the current version (a partial read
        // counts — append destroys nothing, it only needs to know what it is
        // extending). Lock scope: see the note in `write`.
        let visibility = {
            let freshness = ctx.freshness.lock().expect("freshness lock");
            freshness.visibility(&path, content_hash(old.as_bytes()))
        };
        match visibility {
            Visibility::Full | Visibility::Partial(_) => {}
            Visibility::Stale => {
                return ToolOutput::err(format!(
                    "{} changed on disk since you last read it; re-read it \
                     before appending.",
                    rel(&path, &ctx.cwd).display()
                ));
            }
            Visibility::Unseen => {
                return ToolOutput::err(format!(
                    "{} already exists and you have not read its current version; \
                     read it (even partially) before appending so you know what \
                     you are extending.",
                    rel(&path, &ctx.cwd).display()
                ));
            }
        }
        // Read-modify-write rather than OpenOptions::append: the old bytes are
        // already in hand for the gate, and this reuses the Windows retry
        // path. The gate-to-write race is the same accepted exposure as
        // write/edit; same-file batch calls are lane-serialized.
        let new_text = format!("{old}{content}");
        if let Err(error) = write_with_windows_retry(&path, new_text.as_bytes()).await {
            return ToolOutput::err(write_error(&path, &error));
        }
        let old_lines = old.lines().count();
        let merged = !(old.is_empty() || old.ends_with('\n'));
        let appended_start = if merged { old_lines } else { old_lines + 1 };
        let new_total = new_text.lines().count().max(appended_start);
        // Echo the tail so the model sees where its text landed: the appended
        // lines plus up to 3 lines of prior context.
        let start = appended_start.saturating_sub(3).max(1);
        // Record what reaches the model: the appendix plus the echoed context
        // lines, under the new hash. Prior visibility carries forward inside
        // `record_append`; a partial view never silently becomes full.
        ctx.freshness.lock().expect("freshness lock").record_append(
            &path,
            content_hash(new_text.as_bytes()),
            (start, new_total),
        );
        let shown: Vec<&str> = new_text.lines().skip(start - 1).collect();
        let snippet = numbered(&shown, start);
        let count = content.lines().count();
        let merge_note = if merged {
            "\nnote: the file did not end with a newline; the appended text \
             continues its last line."
        } else {
            ""
        };
        ToolOutput::ok(format!(
            "appended {count} line{} to {} (now {new_total} lines).{merge_note} Result:\n{snippet}",
            if count == 1 { "" } else { "s" },
            rel(&path, &ctx.cwd).display(),
        ))
    }
}
