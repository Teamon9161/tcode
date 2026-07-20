//! `/suggest` (alias `/suggestions`): the greyed-out guess at your next prompt (→ accepts it).
//!
//! It costs one small request per turn, so it has to be refusable without
//! editing a config file — and the choice has to outlive the session, because
//! a setting you must re-toggle every morning is one you will stop using.

use super::{CommandCtx, CommandOutcome, SlashCommand};

pub struct SuggestionsCommand;

impl SlashCommand for SuggestionsCommand {
    fn name(&self) -> &'static str {
        "suggest"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["suggestions"]
    }

    fn help(&self) -> &'static str {
        "toggle the next-prompt guess (→ accepts it)"
    }

    fn run(&self, ctx: &mut CommandCtx<'_>, args: &str) -> CommandOutcome {
        let on = match args.trim() {
            "" => !ctx.session.suggestions(),
            "on" => true,
            "off" => false,
            other => {
                return CommandOutcome::error(format!("usage: /suggest [on|off] (got '{other}')"))
            }
        };
        if on == ctx.session.suggestions() {
            return CommandOutcome::info(format!(
                "suggestions already {}",
                if on { "on" } else { "off" }
            ));
        }
        ctx.session.set_suggestions(on);
        CommandOutcome::info(if on {
            "suggestions on (persists across sessions) — when a turn ends, the next prompt is \
             guessed in grey; press → to accept it. Pin a small model for it with /agents."
        } else {
            "suggestions off (persists across sessions) — no more per-turn guess requests."
        })
        .with_effect(super::CommandEffect::PersistSuggestions(on))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{test_ctx_parts, CommandCtx, CommandEffect, CommandRegistry, SlashCommand};
    use super::SuggestionsCommand;
    use crate::types::Usage;

    #[test]
    fn toggles_persistently_and_appears_in_help() {
        let (mut session, opening, environment) = test_ctx_parts();
        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            environment: &environment,
            turn_usage: Usage::default(),
        };
        assert!(!ctx.session.suggestions(), "off by default");

        let outcome = SuggestionsCommand.run(&mut ctx, "");
        assert!(ctx.session.suggestions());
        assert!(matches!(
            outcome.effects[0],
            CommandEffect::PersistSuggestions(true)
        ));

        SuggestionsCommand.run(&mut ctx, "off");
        assert!(!ctx.session.suggestions());
        assert!(SuggestionsCommand.run(&mut ctx, "sideways").messages[0]
            .text
            .contains("usage"));

        let registry = CommandRegistry::builtin();
        assert!(registry.entries().any(|(name, _)| name == "/suggest"));
        assert_eq!(registry.find("/suggestions").unwrap().name(), "suggest");
    }
}
