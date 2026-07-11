//! Small, provider-neutral interaction tools. They turn plans and user
//! questions into structured calls rather than relying on brittle prose.

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::{PermissionRequest, Tool, ToolCtx, ToolOutput};

pub struct UpdatePlanTool;

#[async_trait]
impl Tool for UpdatePlanTool {
    fn name(&self) -> &str {
        "update_plan"
    }
    fn description(&self) -> &str {
        "Record the current execution plan. Use a short ordered list; each item must have a step and status (pending, in_progress, or completed). Keep it current as work advances."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "plan": { "type": "array", "items": { "type": "object", "properties": {
                "step": { "type": "string" },
                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] }
            }, "required": ["step", "status"] } } },
            "required": ["plan"]
        })
    }
    fn permission(&self, _: &Value) -> PermissionRequest {
        PermissionRequest::None
    }
    async fn run(&self, _: Value, _: &ToolCtx, _: &CancellationToken) -> ToolOutput {
        ToolOutput::ok("plan updated")
    }
}

pub struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }
    fn description(&self) -> &str {
        "Ask the user a blocking question when a choice is required to continue. Provide 2–4 concise options. The selected option and any note are returned as a harness note."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": { "type": "string" },
                "options": { "type": "array", "items": { "type": "string" }, "minItems": 2, "maxItems": 4 }
            },
            "required": ["question", "options"]
        })
    }
    fn permission(&self, input: &Value) -> PermissionRequest {
        PermissionRequest::UserInput {
            descriptor: "ask_user".into(),
            summary: input["question"]
                .as_str()
                .unwrap_or("Choose how to continue")
                .into(),
        }
    }
    async fn run(&self, _: Value, _: &ToolCtx, _: &CancellationToken) -> ToolOutput {
        ToolOutput::ok("user answered; read the following harness note before continuing")
    }
}

pub struct AddNoteTool;

#[async_trait]
impl Tool for AddNoteTool {
    fn name(&self) -> &str {
        "add_note"
    }
    fn description(&self) -> &str {
        "Record a concise durable note for the current conversation before continuing. Use it for decisions, constraints, or handoff context."
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] })
    }
    fn permission(&self, _: &Value) -> PermissionRequest {
        PermissionRequest::None
    }
    async fn run(&self, input: Value, _: &ToolCtx, _: &CancellationToken) -> ToolOutput {
        ToolOutput::ok(format!(
            "note recorded: {}",
            input["text"].as_str().unwrap_or("")
        ))
    }
}
