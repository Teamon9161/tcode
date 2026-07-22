//! `exit_plan`: the model's way out of plan mode. It carries a finished plan
//! for the user to review; approving it switches the permission mode (the
//! agent loop applies the transition the approval carries) and mirrors the
//! plan to disk. The plan the model relies on lives in this call's input in the
//! ledger — the file is a convenience copy for the human.

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::{store, PermissionRequest, Tool, ToolCtx, ToolOutput};

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
        // Reached only after approval: plan mode denies the call otherwise, and
        // a decline never runs the tool. Mirror the plan to disk for the human.
        let plan = input["plan"].as_str().unwrap_or("").trim();
        if plan.is_empty() {
            return ToolOutput::err("exit_plan needs a non-empty `plan` (markdown).");
        }
        let dir = store::plans_dir(&ctx.cwd);
        let file = dir.join(format!("{}-{}.md", timestamp(), slug(&plan_title(&input))));
        match tokio::fs::create_dir_all(&dir).await {
            Ok(()) => match tokio::fs::write(&file, plan).await {
                Ok(()) => ToolOutput::ok(format!("Plan approved and saved to {}.", file.display())),
                // The plan is safely in the ledger regardless; the mirror is a
                // convenience, so a write failure is not fatal to the approval.
                Err(e) => ToolOutput::ok(format!(
                    "Plan approved. (Could not save a copy to {}: {e}.)",
                    dir.display()
                )),
            },
            Err(e) => ToolOutput::ok(format!(
                "Plan approved. (Could not create {}: {e}.)",
                dir.display()
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
/// often opens with. `slug` does the length limiting.
fn first_line(plan: &str) -> Option<String> {
    plan.lines()
        .map(|line| line.trim().trim_start_matches(['-', '*', '>', '#', ' ']))
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// Filesystem-safe short slug from a title.
fn slug(title: &str) -> String {
    let mut slug: String = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug = slug.trim_matches('-');
    let slug: String = slug.chars().take(40).collect();
    if slug.is_empty() {
        "plan".to_string()
    } else {
        slug
    }
}

/// `yyyymmdd-HHMMSS` in UTC, without pulling in a date crate (civil-from-days).
fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86400;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}{mo:02}{d:02}-{h:02}{m:02}{s:02}")
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

    #[test]
    fn slug_is_filesystem_safe() {
        assert_eq!(slug("Refactor the Ledger!"), "refactor-the-ledger");
        assert_eq!(slug("  "), "plan");
    }
}
