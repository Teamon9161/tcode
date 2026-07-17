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
        // truncate_tail is one of the ledger's three legal operations; a
        // fresh conversation must not invent another mutation path.
        ctx.session.ledger.truncate_tail(0);
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
