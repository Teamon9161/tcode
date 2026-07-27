//! `exit_plan`: the model's way out of plan mode. The agent loop saves its
//! draft before opening the review; this tool writes the approved or
//! editor-revised version to that same file.

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::{plan_draft, PermissionRequest, Tool, ToolCtx, ToolOutput};

pub struct ExitPlanTool;

#[async_trait]
impl Tool for ExitPlanTool {
    fn name(&self) -> &str {
        "exit_plan"
    }
    fn description(&self) -> &str {
        "Submit a finished plan for the user to review and leave plan mode. Call this only in plan mode, once you have a concrete, executable implementation plan — the phases, the files each phase touches, and the risks — not exploration notes. `plan` is the full plan as markdown; `title` is a short optional name. The user either approves it (which switches the permission mode so you can start executing) or returns feedback for you to revise, in which case you stay in plan mode. Do not begin implementing until the plan is approved."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "plan": { "type": "string", "description": "The full plan, as markdown." },
                "title": { "type": "string", "description": "Short name for the plan." }
            },
            "required": ["plan"]
        })
    }
    fn permission(&self, input: &Value) -> PermissionRequest {
        PermissionRequest::PlanReview {
            title: plan_title(input),
        }
    }
    async fn run(&self, input: Value, ctx: &ToolCtx, _: &CancellationToken) -> ToolOutput {
        let plan = input["plan"].as_str().unwrap_or("").trim();
        if plan.is_empty() {
            return ToolOutput::err("exit_plan needs a non-empty `plan` (markdown).");
        }
        let Some(file) = plan_draft::plan_path_in(&input, &ctx.cwd) else {
            return ToolOutput::err(
                "exit_plan could not find its saved plan draft. Submit the plan again so tcode can save it before review.",
            );
        };
        match tokio::fs::write(&file, plan).await {
            Ok(()) => ToolOutput::ok(format!("Plan approved and saved to {}.", file.display())),
            Err(e) => ToolOutput::err(format!(
                "Plan approval succeeded, but updating {} failed: {e}.",
                file.display()
            )),
        }
    }
}

/// The plan's title: the explicit `title`, else the first markdown heading,
/// else the plan's opening line. It names the file the human later goes
/// looking for, so falling back to a bare "Plan" — which is what a directory
/// of `…-plan.md` files comes from — is the last resort, not the second.
fn plan_title(input: &Value) -> String {
    if let Some(title) = input["title"].as_str() {
        let title = title.trim();
        if !title.is_empty() {
            return title.to_string();
        }
    }
    let plan = input["plan"].as_str();
    plan.and_then(first_heading)
        .or_else(|| plan.and_then(first_line))
        .unwrap_or_else(|| "Plan".to_string())
}

fn first_heading(plan: &str) -> Option<String> {
    plan.lines().find_map(|line| {
        let heading = line.trim_start().trim_start_matches('#').trim();
        (line.trim_start().starts_with('#') && !heading.is_empty()).then(|| heading.to_string())
    })
}

/// The first line with words in it, stripped of the list/quote markers a plan
/// often opens with.
fn first_line(plan: &str) -> Option<String> {
    plan.lines()
        .map(|line| line.trim().trim_start_matches(['-', '*', '>', '#', ' ']))
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// Only exercised through the agent loop end-to-end; the pure helpers are unit
/// tested here.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_prefers_explicit_then_heading_then_default() {
        assert_eq!(plan_title(&json!({"plan": "x", "title": "Do it"})), "Do it");
        assert_eq!(
            plan_title(&json!({"plan": "# Refactor ledger\n\nbody"})),
            "Refactor ledger"
        );
        assert_eq!(
            plan_title(&json!({"plan": "- Rewrite the resume path\n- then test"})),
            "Rewrite the resume path"
        );
        assert_eq!(plan_title(&json!({"plan": "   "})), "Plan");
    }
}
