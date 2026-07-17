use super::{CommandCtx, CommandEffect, CommandOutcome, SlashCommand};

pub struct ResumeCommand;

impl SlashCommand for ResumeCommand {
    fn name(&self) -> &'static str {
        "resume"
    }

    fn help(&self) -> &'static str {
        "resume a session: /resume <id>"
    }

    fn run(&self, ctx: &mut CommandCtx<'_>, args: &str) -> CommandOutcome {
        if args.is_empty() {
            return CommandOutcome::effect(CommandEffect::OpenResumePicker);
        }
        let Some(data_dir) = crate::store::project_data_dir(&ctx.session.tool_ctx.cwd) else {
            return CommandOutcome::info("cannot locate tcode session storage");
        };
        match crate::store::SessionStore::resume(&data_dir, Some(args)) {
            Ok(resumed) => {
                let crate::store::Resumed {
                    store,
                    ledger,
                    checkpoints,
                    startup,
                    environment: previous_environment,
                    delivered_environment,
                } = resumed;
                let session_id = store.id.clone();
                let ckpt_dir = data_dir.join("checkpoints").join(&session_id);
                ctx.session.checkpoints =
                    crate::checkpoint::CheckpointStore::load(ckpt_dir, checkpoints);
                ctx.session.ledger = ledger;
                ctx.session.ledger.attach_sink(Box::new(store));
                ctx.session.bind_scratch_session(&session_id);

                let recovered_startup = startup.unwrap_or_else(|| {
                    (ctx.opening_context)(
                        &ctx.session.tool_ctx.cwd,
                        &ctx.session.tool_ctx.scratch_dir,
                    )
                });
                ctx.session.restore_startup_context(
                    recovered_startup,
                    previous_environment,
                    delivered_environment,
                );
                let current = (ctx.environment)(&ctx.session.tool_ctx.cwd);
                ctx.session.sync_environment(current, None);
                // Unknown until the next usage event; the TUI re-estimates in
                // its ConversationReplaced handler.
                ctx.session.last_prompt_tokens = 0;
                ctx.session
                    .tool_ctx
                    .freshness
                    .lock()
                    .expect("freshness lock")
                    .clear();
                CommandOutcome::effect(CommandEffect::ConversationReplaced)
            }
            Err(e) => CommandOutcome::error(format!("cannot resume session {args}: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{test_ctx_parts, CommandCtx, CommandEffect, SlashCommand};
    use super::ResumeCommand;
    use crate::types::Usage;

    #[test]
    fn bare_resume_opens_the_picker_and_bad_ids_report_an_error() {
        let (mut session, opening, environment) = test_ctx_parts();
        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            environment: &environment,
            turn_usage: Usage::default(),
        };
        let outcome = ResumeCommand.run(&mut ctx, "");
        assert!(matches!(
            &outcome.effects[..],
            [CommandEffect::OpenResumePicker]
        ));

        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            environment: &environment,
            turn_usage: Usage::default(),
        };
        let outcome = ResumeCommand.run(&mut ctx, "no-such-session-id");
        assert!(outcome.effects.is_empty());
        assert!(!outcome.messages.is_empty());
    }
}
