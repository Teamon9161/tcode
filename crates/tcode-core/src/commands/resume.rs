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
                let session_id = resumed.store.id.clone();
                let ckpt_dir = data_dir.join("checkpoints").join(&session_id);
                ctx.session.checkpoints =
                    crate::checkpoint::CheckpointStore::load(ckpt_dir, resumed.checkpoints);
                ctx.session.ledger = resumed.ledger;
                ctx.session.ledger.attach_sink(Box::new(resumed.store));
                ctx.session.bind_scratch_session(&session_id);
                let opening = (ctx.opening_context)(
                    &ctx.session.tool_ctx.cwd,
                    &ctx.session.tool_ctx.scratch_dir,
                );
                ctx.session.replace_opening_context_for_resume(opening);
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
        let (mut session, opening) = test_ctx_parts();
        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
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
            turn_usage: Usage::default(),
        };
        let outcome = ResumeCommand.run(&mut ctx, "no-such-session-id");
        assert!(outcome.effects.is_empty());
        assert!(!outcome.messages.is_empty());
    }
}
