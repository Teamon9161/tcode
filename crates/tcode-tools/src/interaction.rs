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
        "Record and maintain the visible execution plan for genuinely multi-step work; skip it for simple or localized tasks. Do not create a generic three-step checklist like inspect/edit/test, locate/change/verify, or read/implement/run tests — those add no information. Use a short ordered list only when the steps reflect the task's real dependencies, risks, or user-visible milestones. Update incrementally as work advances: keep exactly one step in_progress, mark a step completed the moment it lands, and immediately move the next real step to in_progress when continuing. Never leave every step pending and then flip them all to completed at the end — a plan that is only accurate once the work is over told the user nothing. To complete a specific step, resend the full current list with that step marked completed (and the next step in_progress if work continues). If the plan is done or no longer applies to the user's current request, send an empty plan array to clear the plan display."
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
        "Ask the user one or more blocking questions when a choice is required to continue. Provide a `questions` array; each question has 2–4 `options` and an optional `multiSelect` to let the user pick several. Usually one question is enough — use multiple only for independent choices. All answers come back as a single harness note.\n\nAn option is `{label, description?, preview?}`: `label` is the choice in 1–5 words, `description` says what picking it means. `preview` is shown in a panel beside the options and re-rendered as the user moves between them — give it only when the choice is between concrete artifacts the user must SEE to decide: layout mockups, code snippets, diffs, config samples. Write the preview as the artifact itself, not prose about it. Omit it for plain preference questions, where label and description already say everything; it is also ignored on a multiSelect question, since several selections have no single preview."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": { "type": "string" },
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "maxItems": 4,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": { "type": "string", "description": "The choice itself, 1–5 words." },
                                        "description": { "type": "string", "description": "What picking this option means." },
                                        "preview": { "type": "string", "description": "The artifact this option produces, shown beside the options. Multi-line text. Single-select questions only." }
                                    },
                                    "required": ["label"]
                                }
                            },
                            "multiSelect": { "type": "boolean" }
                        },
                        "required": ["question", "options"]
                    }
                }
            },
            "required": ["questions"]
        })
    }
    fn permission(&self, input: &Value) -> PermissionRequest {
        PermissionRequest::UserInput {
            descriptor: "ask_user".into(),
            summary: summarize_questions(input),
        }
    }
    async fn run(&self, _: Value, _: &ToolCtx, _: &CancellationToken) -> ToolOutput {
        ToolOutput::ok("user answered; read the following harness note before continuing")
    }
}

/// A one-line-per-question summary for the approval prompt. The paged TUI
/// dialog reads the raw `questions` itself; this text is what the plain
/// line-approver shows and what the transcript records, so it must carry
/// every question. Tolerates a legacy single `question` + `options` shape.
fn summarize_questions(input: &Value) -> String {
    let questions = input["questions"].as_array().cloned().unwrap_or_else(|| {
        input
            .get("question")
            .map(|_| vec![input.clone()])
            .unwrap_or_default()
    });
    if questions.len() == 1 {
        return questions[0]["question"]
            .as_str()
            .unwrap_or("Choose how to continue")
            .to_string();
    }
    if questions.is_empty() {
        return "Choose how to continue".into();
    }
    let body = questions
        .iter()
        .enumerate()
        .map(|(i, q)| format!("{}. {}", i + 1, q["question"].as_str().unwrap_or("")))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{} questions:\n{body}", questions.len())
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
