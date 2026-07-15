use std::fs;
use std::path::{Path, PathBuf};

use super::{CommandCtx, CommandOutcome, SlashCommand};
use crate::permission::PermissionMode;

pub struct PlanCommand;

impl SlashCommand for PlanCommand {
    fn name(&self) -> &'static str {
        "plan"
    }

    fn help(&self) -> &'static str {
        "enter plan mode · /plan last shows the latest saved plan"
    }

    fn run(&self, ctx: &mut CommandCtx<'_>, args: &str) -> CommandOutcome {
        match args {
            "" => {
                ctx.session.mode = PermissionMode::Plan;
                CommandOutcome::info("permission mode → plan")
            }
            "last" => match latest_plan(&ctx.session.tool_ctx.cwd) {
                Ok(path) => match fs::read_to_string(&path) {
                    Ok(plan) => CommandOutcome::info(format!(
                        "latest saved plan: {}\n\n{plan}",
                        path.display()
                    )),
                    Err(e) => CommandOutcome::error(format!(
                        "cannot read latest saved plan {}: {e}",
                        path.display()
                    )),
                },
                Err(message) => CommandOutcome::info(message),
            },
            _ => CommandOutcome::info("usage: /plan [last]"),
        }
    }
}

fn latest_plan(cwd: &Path) -> Result<PathBuf, String> {
    latest_plan_in(&crate::store::plans_dir(cwd))
}

fn latest_plan_in(dir: &Path) -> Result<PathBuf, String> {
    let entries =
        fs::read_dir(dir).map_err(|e| format!("no saved plans in {}: {e}", dir.display()))?;
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .max()
        .ok_or_else(|| format!("no saved plans in {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::super::{test_ctx_parts, CommandCtx, SlashCommand};
    use super::{latest_plan_in, PlanCommand};
    use crate::types::Usage;
    use crate::PermissionMode;

    #[test]
    fn bare_plan_enters_plan_mode_and_owes_the_enter_note() {
        let (mut session, opening) = test_ctx_parts();
        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            turn_usage: Usage::default(),
        };

        let outcome = PlanCommand.run(&mut ctx, "");

        assert_eq!(session.mode, PermissionMode::Plan);
        assert_eq!(outcome.messages[0].text, "permission mode → plan");
        // The command itself does not write history; the next turn delivery
        // point injects the guidance that makes the model plan-aware.
        assert!(session.ledger.is_empty());
        assert!(session.take_mode_note().is_some());
    }

    #[test]
    fn latest_plan_uses_the_timestamped_filename_order() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("plans");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("20260715-090000-first.md"), "first").unwrap();
        std::fs::write(dir.join("20260715-100000-latest.md"), "latest").unwrap();
        std::fs::write(dir.join("not-a-plan.txt"), "ignore").unwrap();

        let latest = latest_plan_in(&dir).unwrap();
        assert_eq!(latest.file_name().unwrap(), "20260715-100000-latest.md");
    }

    #[test]
    fn latest_plan_reports_an_empty_directory() {
        let root = tempfile::tempdir().unwrap();
        let error = latest_plan_in(root.path()).unwrap_err();
        assert!(error.contains("no saved plans"));
    }
}
