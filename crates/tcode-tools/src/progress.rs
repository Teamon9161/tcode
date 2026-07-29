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
//!   keeps exactly one phase's prose in context at a time — and a resend may
//!   leave `detail` out to keep what the file holds, so the model pays for the
//!   reasoning once. Both halves have to be in the model-facing text: a tool
//!   that silently charged for detail on every phase flip would teach the model
//!   to stop writing any.

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
        "Maintain the durable work breakdown for genuinely multi-phase work; skip it entirely for simple or localized tasks, which need no file. This is a progress tracker, not a proposal or a generic inspect/edit/test checklist: use a short ordered `phases` list only where it reflects the work's real dependencies, risks, or user-visible milestones, at most two levels deep. A plan needing a third level should be two separate progress files.\n\nSend `title` on the first call for a task; it names the file everyone later looks for. Resend the full `phases` list every time — it is applied as-is, not diffed. Keep exactly one phase in_progress, mark a phase completed the moment it lands, and move the next real phase to in_progress in the same call. Never leave everything pending and flip it all to completed at the end.\n\nThe phase titles are an index; each phase's `detail` is where the work is described. Write it for someone who was not in this conversation — the session that opens this file tomorrow, or you after a compact — and describe the work to be done, not how you explored. How much to write follows who acts on it: a file you opened to track your own work wants a line or two per phase, enough to pick it up cold; a plan a person will review or another session will execute wants what the phase changes, which files, why it comes here, and what could break. Detail is cheap to keep — you write a phase's detail once, a later call that omits `detail` keeps the stored text instead of erasing it, and a phase's detail is handed back to you when that phase starts, so it stays out of your context until it is the phase you are on. To read the whole file — every phase and every detail — call `progress` with nothing but the title of the open file, or no arguments at all. Do that when you need the plan rather than the checklist; you cannot rewrite a phase's `detail` you have not been shown, and it is how you catch up on a plan written before this conversation.\n\n`state` is the lifecycle and is usually omitted. Omit it to track your own work — a file you open needs nobody's approval. Send `state: \"draft\"` when the user asked you to plan first: a draft means you have NOT been cleared to execute it, so keep refining and do not start changing things. Send `state: \"active\"` only to submit that draft for the user's approval; they may rewrite it before accepting, and the version that comes back is authoritative. Send `state: \"done\"` once every phase is completed.\n\nNever `edit`, `write` or `read` the progress file yourself — this tool owns it, and the no-argument call is how you read it. If the user edited it, you will be handed their version; continue from that."
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
                "detail": { "type": "string", "description": "What this sub-phase changes, which files, what could break — for a reader who was not in this conversation. Omit on a later call to keep what you already wrote. Handed back to you when this sub-phase starts." }
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
                            "detail": { "type": "string", "description": "What this phase changes, which files, why here, what could break — for a reader who was not in this conversation. Omit on a later call to keep what you already wrote. Handed back to you when this phase starts." },
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
