use super::{CommandCtx, CommandOutcome, SlashCommand};

pub struct CdCommand;

impl SlashCommand for CdCommand {
    fn name(&self) -> &'static str {
        "cd"
    }

    fn help(&self) -> &'static str {
        "change working directory: /cd <path>"
    }

    fn run(&self, ctx: &mut CommandCtx<'_>, args: &str) -> CommandOutcome {
        match ctx.session.change_cwd(args) {
            Ok(change) => {
                if change.refresh_opening_context {
                    let startup =
                        (ctx.opening_context)(&change.new, &ctx.session.tool_ctx.scratch_dir);
                    ctx.session.set_startup_context(startup);
                } else if change.changed {
                    let environment = (ctx.environment)(&change.new);
                    ctx.session
                        .sync_environment(environment, change.memory_note);
                }
                if change.changed {
                    let _ = std::env::set_current_dir(&change.new);
                    CommandOutcome::info(format!("cwd → {}", change.new.display()))
                } else {
                    CommandOutcome::info(format!("cwd: {}", change.new.display()))
                }
            }
            Err(e) => CommandOutcome::error(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        test_ctx_parts, CommandCtx, EnvironmentFn, MessageKind, OpeningContextFn, SlashCommand,
    };
    use super::CdCommand;
    use crate::types::Usage;
    use crate::{EnvironmentSnapshot, StartupContext};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn cd_refreshes_opening_context_before_history_exists() {
        let root = std::env::temp_dir().join(format!("tcode-cmd-cd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("child")).unwrap();

        let (mut session, _, _) = test_ctx_parts();
        session.tool_ctx.cwd = root.canonicalize().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = calls.clone();
        let opening: OpeningContextFn = Arc::new(move |cwd, _| {
            calls2.fetch_add(1, Ordering::SeqCst);
            StartupContext {
                text: "fresh map".into(),
                environment: EnvironmentSnapshot {
                    cwd: cwd.display().to_string(),
                    platform: "test".into(),
                    os_version: None,
                    command_shells: vec!["test shell".into()],
                    git: Default::default(),
                    date: "1970-01-01".into(),
                },
            }
        });
        let environment: EnvironmentFn = Arc::new(|cwd| EnvironmentSnapshot {
            cwd: cwd.display().to_string(),
            platform: "test".into(),
            os_version: None,
            command_shells: vec!["test shell".into()],
            git: Default::default(),
            date: "1970-01-01".into(),
        });
        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            environment: &environment,
            turn_usage: Usage::default(),
        };

        let outcome = CdCommand.run(&mut ctx, "child");
        assert_eq!(outcome.messages[0].kind, MessageKind::Info);
        assert!(outcome.messages[0].text.starts_with("cwd →"));
        assert!(outcome.effects.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(session.opening_context(), "fresh map");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cd_to_a_bad_path_is_an_error_message() {
        let (mut session, opening, environment) = test_ctx_parts();
        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            environment: &environment,
            turn_usage: Usage::default(),
        };
        let outcome = CdCommand.run(&mut ctx, "definitely-missing-dir-xyz");
        assert_eq!(outcome.messages[0].kind, MessageKind::Error);
        assert!(outcome.effects.is_empty());
    }
}
