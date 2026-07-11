use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::{PermissionRequest, Tool, ToolCtx, ToolOutput};

/// Pages through tool outputs that were too large for the context and
/// were parked in the blob store by the budget gate.
pub struct ReadOutputTool;

#[async_trait]
impl Tool for ReadOutputTool {
    fn name(&self) -> &str {
        "read_output"
    }

    fn description(&self) -> &str {
        "Read a stored tool output by id: o1… for truncated results, b1… for \
         live/finished background task output. offset is 1-based line number; \
         limit defaults to 200."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "offset": { "type": "integer" },
                "limit": { "type": "integer" }
            },
            "required": ["id"]
        })
    }

    fn permission(&self, _input: &Value) -> PermissionRequest {
        PermissionRequest::None
    }

    async fn run(&self, input: Value, ctx: &ToolCtx, _cancel: &CancellationToken) -> ToolOutput {
        let Some(id) = input["id"].as_str() else {
            return ToolOutput::err("missing required parameter: id");
        };
        let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = input["limit"].as_u64().unwrap_or(200).clamp(1, 500) as usize;
        let result = if id.starts_with('b') {
            ctx.background
                .lock()
                .expect("background lock")
                .read(id, offset, limit)
        } else {
            ctx.blobs
                .lock()
                .expect("blobs lock")
                .read(id, offset, limit)
        };
        match result {
            Ok(page) => ToolOutput::ok(page),
            Err(e) => ToolOutput::err(e),
        }
    }
}
