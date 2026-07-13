use super::{CommandCtx, CommandOutcome, MessageKind, SlashCommand};
use crate::ledger::Entry;

pub struct NoteCommand;

impl SlashCommand for NoteCommand {
    fn name(&self) -> &'static str {
        "note"
    }

    fn help(&self) -> &'static str {
        "add a durable conversation note"
    }

    fn run(&self, ctx: &mut CommandCtx<'_>, args: &str) -> CommandOutcome {
        if args.is_empty() {
            return CommandOutcome::info("usage: /note <text>");
        }
        ctx.session.ledger.append(Entry::Note(args.to_string()));
        CommandOutcome::message(MessageKind::Note, args)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{test_ctx_parts, CommandCtx, MessageKind, SlashCommand};
    use super::NoteCommand;
    use crate::types::Usage;
    use crate::Entry;

    #[test]
    fn note_appends_a_ledger_note() {
        let (mut session, opening) = test_ctx_parts();
        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            turn_usage: Usage::default(),
        };
        let outcome = NoteCommand.run(&mut ctx, "remember the tests");
        assert_eq!(outcome.messages[0].kind, MessageKind::Note);
        assert!(matches!(
            session.ledger.entries().last(),
            Some(Entry::Note(text)) if text == "remember the tests"
        ));
    }
}
