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

use super::{split_line, CommandCtx, CommandEffect, CommandOutcome, SlashCommand};
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

    /// `/plan task` is a user prompt in disguise: the args become the task the
    /// frontend submits as the user's own message, so the line is echoed as
    /// that prompt rather than twice (raw command + task). Subcommands (`list`,
    /// `resume`, `export`, `last`) answer inline and keep the normal command
    /// echo.
    fn as_prompt(&self, line: &str) -> Option<String> {
        let (_, args) = split_line(line)?;
        let task = args.trim();
        match split_verb(task).0 {
            "" | "list" | "resume" | "export" | "last" => None,
            _ => Some(task.to_string()),
        }
    }
}

/// The planning turn itself. `/plan`'s guidance is harness-authored model
/// context, not text the user wrote, so it must not appear in the transcript.
/// The task description is the user's own words: it travels as the effect's
/// `task` and the frontend submits it as a user prompt, so `@path` references
/// in it expand exactly as they would in a plain prompt. Nothing here touches
/// the permission mode.
fn plan_turn(task: &str) -> CommandOutcome {
    CommandOutcome::effect(CommandEffect::PlanTurn {
        instruction: planning_instruction(""),
        task: task.trim().to_string(),
    })
}

/// The instruction a planning turn carries, with an optional task description.
///
/// Public because a frontend without a slash-command surface still has to be
/// able to start one — the desktop app offers planning as a control on its
/// composer — and two copies of this text would be two definitions of what
/// planning means.
///
/// The command path (`/plan task`) does not pass the task here: it keeps the
/// task as the user's own message (see [`CommandEffect::PlanTurn`]) so the
/// task stays in the transcript and `@path` references expand. The task form
/// remains for programmatic callers that bake the whole prompt into a single
/// instruction.
pub fn planning_instruction(task: &str) -> String {
    match task.trim() {
        "" => PLAN_REQUEST.to_string(),
        task => format!("{PLAN_REQUEST}\n\nTask: {task}"),
    }
}

/// The instruction that hands an approved plan to a fresh conversation, for the
/// review option that executes somewhere other than where it was planned. The
/// plan travels in the text because that session has none of the planning
/// conversation — only the progress file it just adopted.
pub fn execution_instruction(plan: &str) -> String {
    format!("{PLAN_EXECUTION_REQUEST}\n{}", plan.trim())
}

const PLAN_REQUEST: &str = include_str!("../../prompts/commands/plan.md");
const PLAN_EXECUTION_REQUEST: &str = include_str!("../../prompts/commands/plan-execution.md");

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
        // The line that makes this a listing rather than a set of filenames:
        // which plan you want is decided here, not by opening each one.
        if !entry.description.is_empty() {
            out.push_str(&format!("\n    {}", entry.description));
        }
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
            capabilities: crate::RuntimeCapabilities::new("test", std::iter::empty::<&str>()),
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
            capabilities: crate::RuntimeCapabilities::new("test", std::iter::empty::<&str>()),
            turn_usage: Usage::default(),
        };

        let outcome = PlanCommand.run(&mut ctx, "");

        assert!(matches!(
            &outcome.effects[..],
            [CommandEffect::PlanTurn { instruction, task }]
                if instruction.contains("state: \"draft\"") && task.is_empty()
        ));
        assert_eq!(session.mode, mode, "planning is not a permission mode");
    }

    #[test]
    fn a_task_description_is_carried_as_the_users_own_prompt_text() {
        let outcome = outcome("rewrite the resume path");
        assert!(matches!(
            &outcome.effects[..],
            [CommandEffect::PlanTurn { instruction, task }]
                if !instruction.contains("Task: rewrite the resume path")
                    && task == "rewrite the resume path"
        ));
    }

    #[test]
    fn only_a_task_description_is_a_prompt_not_a_subcommand() {
        let command = PlanCommand;
        assert_eq!(command.as_prompt("/plan"), None);
        assert_eq!(command.as_prompt("/plan list"), None);
        assert_eq!(command.as_prompt("/plan resume 2"), None);
        assert_eq!(command.as_prompt("/plan last"), None);
        assert_eq!(command.as_prompt("/plan export out.md"), None);
        assert_eq!(
            command.as_prompt("/plan rewrite @notes.md").as_deref(),
            Some("rewrite @notes.md")
        );
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
