//! Hidden command: ask the model to also report the harness's own tool
//! defects while it works. Grounded in-situ critique is worth far more than
//! asking "how could these tools be better?" out of context — but it is a
//! developer's instrument, not a feature, so it stays out of /help.

use super::{CommandCtx, CommandOutcome, SlashCommand};

pub struct DogfoodCommand;

impl SlashCommand for DogfoodCommand {
    fn name(&self) -> &'static str {
        "dogfood"
    }

    fn help(&self) -> &'static str {
        "toggle in-session tool-defect reporting (developer)"
    }

    fn hidden(&self) -> bool {
        true
    }

    fn run(&self, ctx: &mut CommandCtx<'_>, args: &str) -> CommandOutcome {
        let on = match args.trim() {
            "" => !ctx.session.dogfood(),
            "on" => true,
            "off" => false,
            other => {
                return CommandOutcome::error(format!("usage: /dogfood [on|off] (got '{other}')"))
            }
        };
        if on == ctx.session.dogfood() {
            return CommandOutcome::info(format!(
                "dogfood already {}",
                if on { "on" } else { "off" }
            ));
        }
        ctx.session.set_dogfood(on);
        // A mode you must remember to re-enable every session is a mode you
        // will forget to enable, so it persists — but writing state.toml is
        // the frontend's job (as it already is for the `/model` choice), not
        // something a core unit test should do to the developer's home.
        CommandOutcome::info(if on {
            "dogfood on (persists across sessions) — the model will report tool friction it \
             hits. The system prompt changed, so the next request re-primes the cache once."
        } else {
            "dogfood off — the next request re-primes the cache once."
        })
        .with_effect(super::CommandEffect::PersistDogfood(on))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{test_ctx_parts, CommandCtx, CommandRegistry, SlashCommand};
    use super::DogfoodCommand;
    use crate::types::Usage;

    #[test]
    fn toggles_and_accepts_explicit_states() {
        let (mut session, opening, environment) = test_ctx_parts();
        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            environment: &environment,
            turn_usage: Usage::default(),
        };
        assert!(!ctx.session.dogfood());
        DogfoodCommand.run(&mut ctx, "");
        assert!(ctx.session.dogfood());
        DogfoodCommand.run(&mut ctx, "");
        assert!(!ctx.session.dogfood());
        DogfoodCommand.run(&mut ctx, "on");
        assert!(ctx.session.dogfood());
        DogfoodCommand.run(&mut ctx, "off");
        assert!(!ctx.session.dogfood());
        assert!(DogfoodCommand.run(&mut ctx, "sideways").messages[0]
            .text
            .contains("usage"));
    }

    #[test]
    fn dispatchable_but_absent_from_help_and_completion() {
        let registry = CommandRegistry::builtin();
        assert!(registry.find("/dogfood").is_some());
        assert!(!registry.entries().any(|(name, _)| name == "/dogfood"));
    }
}
