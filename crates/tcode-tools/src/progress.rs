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
//! - **A plan is not a phase list.** `background` is where everything that
//!   belongs to no single phase goes, and `description` is the one line the
//!   inventory shows. Both follow the same omit-to-keep rule as `detail`. Say
//!   so in the description too: a model that finds no field for its reasoning
//!   does not stop having any — it writes a markdown file somewhere else and
//!   this tool stops describing the work.

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::progress;
use tcode_core::{CallRoute, PermissionRequest, Tool, ToolCtx, ToolOutput};

pub struct ProgressTool;

#[async_trait]
impl Tool for ProgressTool {
    fn name(&self) -> &str {
        "progress"
    }
    fn description(&self) -> &str {
        "Maintain the durable plan for genuinely multi-phase work; skip it entirely for simple or localized tasks, which need no file. This is a progress tracker, not a proposal or a generic inspect/edit/test checklist: use a short ordered `phases` list only where it reflects the work's real dependencies, risks, or user-visible milestones, at most two levels deep. A plan needing a third level should be two separate progress files.\n\nThe file has three parts and they are not interchangeable. `description` is one line naming what this plan is for — it is all the plan listing shows, so it is what decides whether anyone opens the file. `background` is the plan's prose: everything that belongs to no single phase. `phases` is the index into the work. Send `title` on the first call for a task; it names the file everyone later looks for.\n\nWrite `background` as free markdown with your own `##` sections — there is no fixed set, because what a plan has to establish differs per task. What goes there: what you decided and why, the alternatives you ruled out and what ruled them out, the facts your investigation established (measurements, versions, what a file actually does), the shape of the data everything else follows from, and the constraints that hold across every phase. What does not go there: narration of how you explored. If you find yourself repeating the same paragraph in two phases' `detail`, it was background. Do not write a separate markdown plan file with `write` — this is where a plan lives, and a plan split across two files is one nobody will keep current.\n\nResend the full `phases` list every time — it is applied as-is, not diffed. Keep exactly one phase in_progress, mark a phase completed the moment it lands, and move the next real phase to in_progress in the same call. Never leave everything pending and flip it all to completed at the end.\n\nName a phase after the work, not after the stage of work it belongs to. `phase` is an imperative verb plus the concrete thing it acts on — the file, module, artifact or behaviour — so that reading the list tells someone which task this is. A breakdown whose titles are the names of workflow steps (decide the approach, implement it, verify it — in any wording or language) is true of every task ever undertaken, so as an index it says nothing; if that is the shape yours is taking, the phases you actually have are hiding inside the middle one.\n\nThe phase titles are an index; each phase's `detail` is where that phase's own work is described. Write it for someone who was not in this conversation — the session that opens this file tomorrow, or you after a compact — and describe the work to be done: what it changes, which files, why it comes here, what could break. Not the narration of how you explored, and not how it went — but what the exploring turned up belongs here. Do not repeat `background` in it; a fact that holds for the whole plan is written once, up there.\n\nHow much to write, in `background` and in every `detail`, is decided by `state` and not by how large the task feels. With no `state` you are tracking your own work inside a conversation that still holds everything you know, so a line or two per phase is enough to pick it up cold and `background` is optional. A `draft` has other readers by definition — the person reviewing it, and possibly a fresh session handed nothing but this file — so there it all gets written out in full: `description`, `background`, and every phase's reasoning with the files and symbols by name. Write it even when that runs long; a draft submitted with any of the three missing is rejected before the user is asked anything, and a fact you leave out is one whoever executes this pays to discover again.\n\nProse is written once, when you plan it, and every later call leaves it out — omitting `detail`, `background` or `description` keeps the stored text, so a phase flip costs you nothing and the reasoning stays out of your context until that phase starts, when it is handed back to you. Never rewrite a finished phase's `detail` into a report of what happened: results and evidence belong in your reply to the user, and overwriting the plan with them destroys the one thing a session resuming this file needed. What a completed phase taught you belongs in the phases still ahead, or in `background` if it now holds for the whole plan — rewrite those instead. To read the whole file — the prose, every phase and every detail — call `progress` with nothing but the title of the open file, or no arguments at all. Do that when you need the plan rather than the checklist; you cannot rewrite prose you have not been shown, and it is how you catch up on a plan written before this conversation.\n\n`state` is the lifecycle and is usually omitted. Omit it to track your own work — a file you open needs nobody's approval. Send `state: \"draft\"` when the user asked you to plan first: a draft means you have NOT been cleared to execute it, so keep refining and do not start changing things. Send `state: \"active\"` only to submit that draft for the user's approval; they may rewrite it before accepting, and the version that comes back is authoritative. Send `state: \"done\"` once every phase is completed.\n\nNever `edit`, `write` or `read` the progress file yourself — this tool owns it, and the no-argument call is how you read it. If the user edited it, you will be handed their version; continue from that."
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
                "description": { "type": "string", "description": "One line: what this plan is for. All the plan listing shows, so it is what decides whether the file is opened at all. Omit on a later call to keep what you already wrote." },
                "background": { "type": "string", "description": "The plan's prose, as free markdown with your own `##` sections: what you decided and why, what you ruled out, the facts your investigation established, the shape of the data, the constraints that hold across every phase. Everything that belongs to no single phase. Omit on a later call to keep what you already wrote; it is handed back to you whenever a session picks this file up." },
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
    /// One tool, two surfaces: a phase flip is the plan display's business, but
    /// a plan submitted for review is a document the user reads in the
    /// conversation and must still be able to find there afterwards.
    fn route(&self, input: &Value) -> CallRoute {
        match progress::is_plan_document(input) {
            true => CallRoute::Transcript,
            false => CallRoute::Progress,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every frontend reads this instead of keeping its own list of tool names,
    /// so the two answers are pinned here rather than in each of them.
    #[test]
    fn a_phase_flip_feeds_the_plan_surface_and_a_submission_is_a_document() {
        let flip = json!({ "phases": [{ "phase": "one", "status": "in_progress" }] });
        assert_eq!(ProgressTool.route(&flip), CallRoute::Progress);
        assert_eq!(ProgressTool.route(&Value::Null), CallRoute::Progress);

        let submission = json!({
            "title": "Rewrite the resume path",
            "state": "active",
            "phases": [{ "phase": "one", "status": "pending" }]
        });
        assert_eq!(ProgressTool.route(&submission), CallRoute::Transcript);
        // Only a submission asks the human anything; a flip must never prompt.
        assert!(matches!(
            ProgressTool.permission(&flip),
            PermissionRequest::None
        ));
        assert!(matches!(
            ProgressTool.permission(&submission),
            PermissionRequest::PlanReview { .. }
        ));
    }
}
