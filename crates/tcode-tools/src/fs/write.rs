use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::freshness::{content_hash, Visibility};
use tcode_core::{AutoSafety, BatchPolicy, PermissionRequest, Tool, ToolCtx, ToolOutput};

use crate::redact::{marker_error, read_marker};

use super::{rel, write_error, write_with_windows_retry};

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
         current version in full."
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
            aliases: Vec::new(),
            summary: format!(
                "write {path} ({} bytes)",
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
        // `write` replaces the whole file from context, and the read-in-full
        // gate only tracks *line ranges* — it cannot tell that a line the
        // model saw was clipped or redacted inside. Without this check the
        // markers `read` adds are a data-corruption seed.
        if let Some(kind) = read_marker(content) {
            return ToolOutput::err(marker_error(kind, "content"));
        }
        let path = ctx.resolve(path_str);
        // The freshness lock is taken in short scopes rather than held across
        // the IO: a `std::sync::MutexGuard` alive across an `.await` would make
        // this future non-Send, and holding it would serialize a whole
        // parallel write batch behind one file's disk latency. Batched writes
        // are guaranteed to target distinct paths, so nothing needs the lock
        // held across the read-modify-write.
        if let Ok(existing) = tokio::fs::read(&path).await {
            let visibility = {
                let freshness = ctx.freshness.lock().expect("freshness lock");
                freshness.visibility(&path, content_hash(&existing))
            };
            match visibility {
                Visibility::Full => {}
                Visibility::Partial(ranges) => {
                    let seen: Vec<String> =
                        ranges.iter().map(|(s, e)| format!("{s}-{e}")).collect();
                    return ToolOutput::err(format!(
                        "{} already exists and you have only seen lines {} of its \
                         current version; `write` replaces the whole file. Read the \
                         remaining lines first, or use `edit`/`append` for a \
                         targeted change.",
                        rel(&path, &ctx.cwd).display(),
                        seen.join(", ")
                    ));
                }
                Visibility::Stale => {
                    return ToolOutput::err(format!(
                        "{} changed on disk since you last read it; re-read it \
                         before overwriting so the external changes are not \
                         destroyed unknowingly.",
                        rel(&path, &ctx.cwd).display()
                    ));
                }
                Visibility::Unseen => {
                    return ToolOutput::err(format!(
                        "{} already exists and you have not read its current version; \
                         read it first so no content is destroyed unknowingly.",
                        rel(&path, &ctx.cwd).display()
                    ));
                }
            }
        }
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
        ToolOutput::ok(format!(
            "wrote {} ({} lines)",
            rel(&path, &ctx.cwd).display(),
            content.lines().count()
        ))
    }
}
