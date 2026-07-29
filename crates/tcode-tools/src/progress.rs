//! `progress`: the model's one handle on the session's durable work breakdown.
//!
//! Everything that makes this cheap lives in `tcode_core::progress`; this file
//! is the schema the model sees plus the two lines that dispatch. Two rules are
//! worth restating where the model-facing text is:
//!
//! - **This tool is the file's only writer.** Never `edit` a progress file: an
//!   edit costs a read plus an exact-match old_string, so flipping one phase
//!   would pull the whole plan through the context twice — and would break the
//!   moment the user rewrote the file by hand.
//! - **The result carries the entered phase's detail**, so a twelve-phase plan
//!   keeps exactly one phase's prose in context at a time.

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::progress;
use tcode_core::{PermissionRequest, Tool, ToolCtx, ToolOutput};

pub struct ProgressTool;

#[async_trait]
impl Tool for ProgressTool {
    fn name(&self) -> &str {
        "progress"
    }
    fn description(&self) -> &str {
        "Maintain the durable work breakdown for genuinely multi-phase work; skip it entirely for simple or localized tasks, which need no file. This is a progress tracker, not a proposal or a generic inspect/edit/test checklist: use a short ordered `phases` list only where it reflects the work's real dependencies, risks, or user-visible milestones, at most two levels deep. A plan needing a third level should be two separate progress files.\n\nSend `title` on the first call for a task; it names the file everyone later looks for. Resend the full `phases` list every time — it is applied as-is, not diffed. Keep exactly one phase in_progress, mark a phase completed the moment it lands, and move the next real phase to in_progress in the same call. Never leave everything pending and flip it all to completed at the end. Put the reasoning in each phase's `detail` (why, which files, what risk): detail is not carried in your context, it is handed back to you when that phase starts, so write it for your future self.\n\n`state` is the lifecycle and is usually omitted. Omit it to track your own work — a file you open needs nobody's approval. Send `state: \"draft\"` when the user asked you to plan first: a draft means you have NOT been cleared to execute it, so keep refining and do not start changing things. Send `state: \"active\"` only to submit that draft for the user's approval; they may rewrite it before accepting, and the version that comes back is authoritative. Send `state: \"done\"` once every phase is completed.\n\nNever `edit` or `write` the progress file yourself — this tool owns it. If the user edited it, you will be handed their version; continue from that."
    }
    fn input_schema(&self) -> Value {
        // Written out at both levels rather than generated from one closure:
        // the nesting cap is two, so "the level that has sub-phases" and "the
        // level that cannot" are genuinely different schemas, and a recursive
        // builder only tempts someone into emitting a null `phases` for the
        // leaf — which is not a legal JSON Schema value and is rejected by the
        // provider before the request is ever sent.
        let sub_phase = json!({
            "type": "object",
            "properties": {
                "phase": { "type": "string", "description": "The sub-phase, as a concrete piece of work." },
                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] },
                "detail": { "type": "string", "description": "Why, which files, what risk. Handed back to you when this sub-phase starts." }
            },
            "required": ["phase", "status"]
        });
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Short name for this task. Required on the first call." },
                "state": { "type": "string", "enum": ["draft", "active", "done"], "description": "Lifecycle. Omit unless changing it." },
                "phases": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "phase": { "type": "string", "description": "The phase itself, as a concrete piece of work." },
                            "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] },
                            "detail": { "type": "string", "description": "Why, which files, what risk. Handed back to you when this phase starts." },
                            "phases": {
                                "type": "array",
                                "description": "Sub-phases. This is the last level; nesting deeper is rejected.",
                                "items": sub_phase
                            }
                        },
                        "required": ["phase", "status"]
                    }
                }
            }
        })
    }
    fn permission(&self, input: &Value) -> PermissionRequest {
        if !progress::is_submission(input) {
            return PermissionRequest::None;
        }
        PermissionRequest::PlanReview {
            title: input["title"]
                .as_str()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .unwrap_or("Proposed plan")
                .to_string(),
        }
    }
    async fn run(&self, input: Value, ctx: &ToolCtx, _: &CancellationToken) -> ToolOutput {
        match progress::apply_call(ctx, &input) {
            Ok(result) => ToolOutput::ok(result),
            Err(error) => ToolOutput::err(error),
        }
    }
}
