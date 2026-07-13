use super::{CommandCtx, CommandOutcome, SlashCommand};

pub struct CostCommand;

impl SlashCommand for CostCommand {
    fn name(&self) -> &'static str {
        "cost"
    }

    fn help(&self) -> &'static str {
        "show last turn token usage"
    }

    fn run(&self, ctx: &mut CommandCtx<'_>, _args: &str) -> CommandOutcome {
        let u = ctx.turn_usage;
        CommandOutcome::info(format!(
            "last turn: in {} | out {} | cache r {} w {}",
            u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cache_write_tokens
        ))
    }
}
