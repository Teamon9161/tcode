use super::{CommandCtx, CommandOutcome, SlashCommand};

pub struct ExportCommand;

impl SlashCommand for ExportCommand {
    fn name(&self) -> &'static str {
        "export"
    }

    fn help(&self) -> &'static str {
        "export transcript: /export [path.md]"
    }

    fn run(&self, ctx: &mut CommandCtx<'_>, args: &str) -> CommandOutcome {
        if ctx.session.ledger.is_empty() {
            return CommandOutcome::info("nothing to export yet");
        }
        let path = if args.is_empty() {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            std::path::PathBuf::from(format!("tcode-transcript-{secs}.md"))
        } else {
            std::path::PathBuf::from(args)
        };
        let markdown =
            crate::export_markdown(ctx.session.ledger.entries(), "tcode conversation");
        match std::fs::write(&path, markdown) {
            Ok(()) => CommandOutcome::info(format!("transcript exported → {}", path.display())),
            Err(e) => CommandOutcome::error(format!("export failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{test_ctx_parts, CommandCtx, SlashCommand};
    use super::ExportCommand;
    use crate::types::Usage;
    use crate::Entry;

    #[test]
    fn export_writes_a_markdown_file() {
        let (mut session, opening) = test_ctx_parts();
        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            turn_usage: Usage::default(),
        };
        assert_eq!(
            ExportCommand.run(&mut ctx, "").messages[0].text,
            "nothing to export yet"
        );

        session.ledger.append(Entry::Note("hello".into()));
        let target = std::env::temp_dir().join(format!("tcode-cmd-export-{}.md", std::process::id()));
        let _ = std::fs::remove_file(&target);
        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            turn_usage: Usage::default(),
        };
        let outcome = ExportCommand.run(&mut ctx, target.to_str().unwrap());
        assert!(outcome.messages[0].text.starts_with("transcript exported"));
        assert!(target.exists());
        let _ = std::fs::remove_file(&target);
    }
}
