use super::{CommandCtx, CommandEffect, CommandOutcome, SlashCommand};

pub struct ClearCommand;

impl SlashCommand for ClearCommand {
    fn name(&self) -> &'static str {
        "clear"
    }

    fn help(&self) -> &'static str {
        "start a fresh conversation"
    }

    fn run(&self, ctx: &mut CommandCtx<'_>, _args: &str) -> CommandOutcome {
        let cwd = ctx.session.tool_ctx.cwd.clone();

        // Try to create a new session file, preserving the old conversation
        // as a separate resumable session. Fall back to in-place truncation
        // only when persistence is unavailable.
        let created = crate::store::project_data_dir(&cwd).and_then(|data_dir| {
            let store = crate::store::SessionStore::create(&data_dir, &cwd).ok()?;
            Some((data_dir, store))
        });

        match created {
            Some((data_dir, store)) => {
                let session_id = store.id.clone();
                ctx.session.ledger = crate::Ledger::new();
                ctx.session.ledger.attach_sink(Box::new(store));
                ctx.session.checkpoints = crate::CheckpointStore::new(
                    data_dir.join("checkpoints").join(&session_id),
                );
                ctx.session.bind_scratch_session(&session_id);
                ctx.session.set_startup_context((ctx.opening_context)(
                    &ctx.session.tool_ctx.cwd,
                    &ctx.session.tool_ctx.scratch_dir,
                    &ctx.session.tool_ctx.instruction_discovery,
                ));
                let current = (ctx.environment)(&ctx.session.tool_ctx.cwd);
                ctx.session.sync_environment(current, None);
                ctx.session.restore_progress(None);
            }
            None => {
                ctx.session.ledger.truncate_tail(0);
            }
        }

        ctx.session.last_prompt_tokens = 0;
        ctx.session
            .tool_ctx
            .freshness
            .lock()
            .expect("freshness lock")
            .clear();
        CommandOutcome::info("conversation cleared").with_effect(CommandEffect::ConversationCleared)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{test_ctx_parts, CommandCtx, CommandEffect, SlashCommand};
    use super::ClearCommand;
    use crate::types::Usage;
    use crate::Entry;

    #[test]
    fn clear_empties_the_ledger_and_signals_the_frontend() {
        let (mut session, opening, environment) = test_ctx_parts();
        session.ledger.append(Entry::Note("history".into()));
        session.last_prompt_tokens = 1234;
        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            environment: &environment,
            turn_usage: Usage::default(),
        };
        let outcome = ClearCommand.run(&mut ctx, "");
        assert!(session.ledger.is_empty());
        assert_eq!(session.last_prompt_tokens, 0);
        assert!(matches!(
            &outcome.effects[..],
            [CommandEffect::ConversationCleared]
        ));
    }
}
