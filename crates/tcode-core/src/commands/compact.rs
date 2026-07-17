use super::{CommandCtx, CommandEffect, CommandOutcome, SlashCommand};

pub struct CompactCommand;

impl SlashCommand for CompactCommand {
    fn name(&self) -> &'static str {
        "compact"
    }

    fn help(&self) -> &'static str {
        "summarize history · /compact <focus>"
    }

    fn run(&self, ctx: &mut CommandCtx<'_>, args: &str) -> CommandOutcome {
        if ctx.session.ledger.is_empty() {
            return CommandOutcome::info("nothing to compact");
        }
        let focus = (!args.is_empty()).then(|| args.to_string());
        CommandOutcome::effect(CommandEffect::Compact { focus })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{test_ctx_parts, CommandCtx, CommandEffect, SlashCommand};
    use super::CompactCommand;
    use crate::types::Usage;
    use crate::Entry;

    #[test]
    fn compact_needs_history_and_carries_the_focus() {
        let (mut session, opening, environment) = test_ctx_parts();
        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            environment: &environment,
            turn_usage: Usage::default(),
        };
        let outcome = CompactCommand.run(&mut ctx, "");
        assert!(outcome.effects.is_empty());
        assert_eq!(outcome.messages[0].text, "nothing to compact");

        session.ledger.append(Entry::Note("history".into()));
        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            environment: &environment,
            turn_usage: Usage::default(),
        };
        let outcome = CompactCommand.run(&mut ctx, "keep the api decisions");
        assert!(outcome.messages.is_empty());
        assert!(matches!(
            &outcome.effects[..],
            [CommandEffect::Compact { focus: Some(f) }] if f == "keep the api decisions"
        ));
    }
}
