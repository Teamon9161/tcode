//! `/plan`: the one entry point to the planning posture.
//!
//! Planning used to be a permission mode, which conflated two orthogonal
//! questions — "am I still deciding what to do?" and "how much risk will I
//! take once I start?" — and cost the user the ability to run exploration in
//! auto or unsafe mode, which is exactly the phase that most wants it. Here
//! planning is a state of a *file*, and the permission mode stays whatever the
//! user chose.

use std::fs;
use std::path::{Path, PathBuf};

use super::{CommandCtx, CommandEffect, CommandOutcome, SlashCommand};
use crate::progress::{inventory, Progress, INVENTORY_LIMIT};

pub struct PlanCommand;

impl SlashCommand for PlanCommand {
    fn name(&self) -> &'static str {
        "plan"
    }

    fn help(&self) -> &'static str {
        "plan before executing · list | resume <n> | last | export <path>"
    }

    fn run(&self, ctx: &mut CommandCtx<'_>, args: &str) -> CommandOutcome {
        let cwd = ctx.session.tool_ctx.cwd.clone();
        let (verb, rest) = split_verb(args);
        match verb {
            "" => plan_turn(""),
            "list" => list(&cwd),
            "resume" => resume(ctx, &cwd, rest),
            "export" => export(ctx, rest),
            "last" => match latest_plan(&cwd) {
                Ok(path) => match fs::read_to_string(&path) {
                    Ok(plan) => {
                        CommandOutcome::info(format!("latest plan: {}\n\n{plan}", path.display()))
                    }
                    Err(e) => CommandOutcome::error(format!("cannot read {}: {e}", path.display())),
                },
                Err(message) => CommandOutcome::info(message),
            },
            // A bare description is the common case: `/plan rewrite the resume
            // path` should plan that, not be rejected for an unknown subcommand.
            _ => plan_turn(args.trim()),
        }
    }
}

/// The planning turn itself. `/plan`'s guidance is harness-authored model
/// context, not text the user wrote, so it must not appear in the transcript.
/// Nothing here touches the permission mode.
fn plan_turn(task: &str) -> CommandOutcome {
    let instruction = match task {
        "" => PLAN_REQUEST.to_string(),
        task => format!("{PLAN_REQUEST}\n\nTask: {task}"),
    };
    CommandOutcome::effect(CommandEffect::SubmitInstruction(instruction))
}

const PLAN_REQUEST: &str = include_str!("../../prompts/commands/plan.md");

fn split_verb(args: &str) -> (&str, &str) {
    let args = args.trim();
    match args.split_once(char::is_whitespace) {
        Some((verb, rest)) => (verb, rest.trim()),
        None => (args, ""),
    }
}

fn list(cwd: &Path) -> CommandOutcome {
    let entries = inventory(cwd, INVENTORY_LIMIT.max(20));
    if entries.is_empty() {
        return CommandOutcome::info("no unfinished plans");
    }
    let mut out = String::from("unfinished plans (/plan resume <n> to take one over):");
    for (i, entry) in entries.iter().enumerate() {
        out.push_str(&format!(
            "\n{:>2}. {} [{}] {}/{} phases · {}",
            i + 1,
            entry.title,
            entry.state.label(),
            entry.done,
            entry.total,
            entry.file_name()
        ));
    }
    CommandOutcome::info(out)
}

/// Taking over a plan is deliberately explicit. A draft sitting on disk is not
/// a request — it may be a different task from three days ago — so nothing but
/// this command (or the user saying so) makes the model continue one.
fn resume(ctx: &mut CommandCtx<'_>, cwd: &Path, rest: &str) -> CommandOutcome {
    let entries = inventory(cwd, INVENTORY_LIMIT.max(20));
    let Ok(index) = rest.trim().parse::<usize>() else {
        return CommandOutcome::error("usage: /plan resume <n> (see /plan list)");
    };
    let Some(entry) = index.checked_sub(1).and_then(|i| entries.get(i)) else {
        return CommandOutcome::error(format!("no plan {index}; see /plan list"));
    };
    match ctx.session.adopt_progress(&entry.path) {
        Ok(title) => CommandOutcome::info(format!("now working through: {title}")),
        Err(error) => CommandOutcome::error(error),
    }
}

/// Progress files are runtime state under `~/.tcode`, never written into the
/// user's repository: an agent that drops files in a checkout produces git
/// noise and accidental commits. Putting one in the repo is a decision, and
/// this is where the user makes it.
fn export(ctx: &mut CommandCtx<'_>, rest: &str) -> CommandOutcome {
    if rest.is_empty() {
        return CommandOutcome::error("usage: /plan export <path>");
    }
    let Some(text) = ctx.session.progress().as_ref().map(Progress::render) else {
        return CommandOutcome::error("no plan is open; see /plan list");
    };
    let target = ctx.session.tool_ctx.resolve(rest);
    if let Some(parent) = target.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return CommandOutcome::error(format!("cannot create {}: {e}", parent.display()));
        }
    }
    match fs::write(&target, text) {
        Ok(()) => CommandOutcome::info(format!("plan exported to {}", target.display())),
        Err(e) => CommandOutcome::error(format!("cannot write {}: {e}", target.display())),
    }
}

fn latest_plan(cwd: &Path) -> Result<PathBuf, String> {
    latest_plan_in(&crate::store::progress_dir(cwd))
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
    use super::super::{test_ctx_parts, CommandCtx, CommandEffect, MessageKind, SlashCommand};
    use super::{latest_plan_in, PlanCommand};
    use crate::types::Usage;

    fn outcome(args: &str) -> super::CommandOutcome {
        let (mut session, opening, environment) = test_ctx_parts();
        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            environment: &environment,
            turn_usage: Usage::default(),
        };
        PlanCommand.run(&mut ctx, args)
    }

    #[test]
    fn bare_plan_asks_the_model_to_plan_without_touching_the_permission_mode() {
        let (mut session, opening, environment) = test_ctx_parts();
        let mode = session.mode;
        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            environment: &environment,
            turn_usage: Usage::default(),
        };

        let outcome = PlanCommand.run(&mut ctx, "");

        assert!(matches!(
            &outcome.effects[..],
            [CommandEffect::SubmitInstruction(instruction)] if instruction.contains("state: \"draft\"")
        ));
        assert_eq!(session.mode, mode, "planning is not a permission mode");
    }

    #[test]
    fn a_task_description_is_carried_into_the_planning_request() {
        let outcome = outcome("rewrite the resume path");
        assert!(matches!(
            &outcome.effects[..],
            [CommandEffect::SubmitInstruction(instruction)]
                if instruction.contains("Task: rewrite the resume path")
        ));
    }

    #[test]
    fn list_reports_an_empty_directory_instead_of_failing() {
        crate::home::testing::temp_home();
        let outcome = outcome("list");
        assert!(outcome.messages[0].text.contains("no unfinished plans"));
    }

    #[test]
    fn resume_needs_an_index_from_the_listing() {
        crate::home::testing::temp_home();
        let outcome = outcome("resume");
        assert_eq!(outcome.messages[0].kind, MessageKind::Error);
        assert!(outcome.messages[0].text.contains("/plan list"));
    }

    #[test]
    fn latest_plan_uses_the_timestamped_filename_order() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("progress");
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
