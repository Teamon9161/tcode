use super::{CommandCtx, CommandOutcome, SlashCommand};

pub struct ModeCommand;

impl SlashCommand for ModeCommand {
    fn name(&self) -> &'static str {
        "mode"
    }

    fn help(&self) -> &'static str {
        "cycle permission mode"
    }

    fn run(&self, ctx: &mut CommandCtx<'_>, _args: &str) -> CommandOutcome {
        ctx.session.mode = ctx.session.mode.cycle();
        CommandOutcome::info(format!("permission mode → {}", ctx.session.mode.label()))
    }
}
