use super::{CommandCtx, CommandOutcome, SlashCommand};

pub struct MemoryCommand;

impl SlashCommand for MemoryCommand {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn help(&self) -> &'static str {
        "show memory sources · /memory on|off"
    }

    fn run(&self, ctx: &mut CommandCtx<'_>, args: &str) -> CommandOutcome {
        let (status, toggle_note) = {
            let mut memory = ctx.session.tool_ctx.memory.lock().expect("memory lock");
            memory.restore_from_entries(ctx.session.ledger.entries());
            let note = match args {
                "" => None,
                "on" => Some(memory.set_enabled(true)),
                "off" => Some(memory.set_enabled(false)),
                _ => return CommandOutcome::info("usage: /memory [on|off]"),
            };
            (memory.status(), note)
        };
        if let Some(note) = toggle_note {
            ctx.session.stage_memory_note(note);
        }
        let indented = status
            .lines()
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        CommandOutcome::info(indented)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{test_ctx_parts, CommandCtx, SlashCommand};
    use super::MemoryCommand;
    use crate::types::Usage;

    #[test]
    fn toggling_memory_defers_and_coalesces_its_model_note() {
        let (mut session, opening, environment) = test_ctx_parts();
        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            environment: &environment,
            turn_usage: Usage::default(),
        };
        let outcome = MemoryCommand.run(&mut ctx, "off");
        assert!(!outcome.messages.is_empty());
        assert!(
            session.ledger.is_empty(),
            "a control toggle is not model context"
        );

        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            environment: &environment,
            turn_usage: Usage::default(),
        };
        let outcome = MemoryCommand.run(&mut ctx, "on");
        assert!(!outcome.messages.is_empty());
        let notes = session.take_deferred_context_notes();
        assert_eq!(notes.len(), 1, "the final toggle replaces the first");
        assert!(notes[0].contains("enabled"));
        assert!(!notes[0].contains("disabled"));

        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            environment: &environment,
            turn_usage: Usage::default(),
        };
        let outcome = MemoryCommand.run(&mut ctx, "bogus");
        assert!(outcome.messages[0].text.starts_with("usage:"));
    }
}
