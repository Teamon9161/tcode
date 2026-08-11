use super::{CommandCtx, CommandOutcome, SlashCommand};

pub struct KongCommand;

impl SlashCommand for KongCommand {
    fn name(&self) -> &'static str {
        "kong"
    }

    fn help(&self) -> &'static str {
        "toggle kong mode (developer)"
    }

    fn hidden(&self) -> bool {
        true
    }

    fn run(&self, ctx: &mut CommandCtx<'_>, args: &str) -> CommandOutcome {
        let on = match args.trim() {
            "" => !ctx.session.kong(),
            "on" => true,
            "off" => false,
            other => {
                return CommandOutcome::error(format!("usage: /kong [on|off] (got '{other}')"))
            }
        };
        if on == ctx.session.kong() {
            return CommandOutcome::info(format!("kong already {}", if on { "on" } else { "off" }));
        }
        ctx.session.set_kong(on);
        CommandOutcome::info(if on {
            "kong on (persists across sessions)"
        } else {
            "kong off"
        })
        .with_effect(super::CommandEffect::PersistKong(on))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{test_ctx_parts, CommandCtx, CommandRegistry, SlashCommand};
    use super::KongCommand;
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
        assert!(!ctx.session.kong());
        KongCommand.run(&mut ctx, "");
        assert!(ctx.session.kong());
        KongCommand.run(&mut ctx, "");
        assert!(!ctx.session.kong());
        KongCommand.run(&mut ctx, "on");
        assert!(ctx.session.kong());
        KongCommand.run(&mut ctx, "off");
        assert!(!ctx.session.kong());
        assert!(KongCommand.run(&mut ctx, "sideways").messages[0]
            .text
            .contains("usage"));
    }

    #[test]
    fn dispatchable_but_absent_from_help_and_completion() {
        let registry = CommandRegistry::builtin();
        assert!(registry.find("/kong").is_some());
        assert!(!registry.entries().any(|(name, _)| name == "/kong"));
    }
}
