use super::{CommandCtx, CommandEffect, CommandOutcome, SlashCommand};

pub struct ExitCommand;

impl SlashCommand for ExitCommand {
    fn name(&self) -> &'static str {
        "exit"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["quit"]
    }

    fn help(&self) -> &'static str {
        "quit tcode"
    }

    fn run(&self, _ctx: &mut CommandCtx<'_>, _args: &str) -> CommandOutcome {
        CommandOutcome::effect(CommandEffect::Exit)
    }
}
