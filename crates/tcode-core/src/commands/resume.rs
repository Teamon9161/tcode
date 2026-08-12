use super::{CommandCtx, CommandEffect, CommandOutcome, SlashCommand};

pub struct ResumeCommand;

impl SlashCommand for ResumeCommand {
    fn name(&self) -> &'static str {
        "resume"
    }

    fn help(&self) -> &'static str {
        "resume a session: /resume <id>"
    }

    fn run(&self, ctx: &mut CommandCtx<'_>, args: &str) -> CommandOutcome {
        if args.is_empty() {
            return CommandOutcome::effect(CommandEffect::OpenResumePicker);
        }
        let Some(data_dir) = crate::store::project_data_dir(&ctx.session.tool_ctx.cwd) else {
            return CommandOutcome::info("cannot locate tcode session storage");
        };
        match crate::store::SessionStore::resume(&data_dir, Some(args)) {
            Ok(resumed) => {
                let crate::store::Resumed {
                    store,
                    ledger,
                    checkpoints,
                    startup,
                    environment: previous_environment,
                    delivered_environment,
                    progress,
                    capabilities,
                    delivered_capabilities,
                } = resumed;
                let session_id = store.id.clone();
                let ckpt_dir = data_dir.join("checkpoints").join(&session_id);
                ctx.session.checkpoints =
                    crate::checkpoint::CheckpointStore::load(ckpt_dir, checkpoints);
                ctx.session.ledger = ledger;
                ctx.session.ledger.attach_sink(Box::new(store));
                ctx.session.bind_scratch_session(&session_id);

                let recovered_startup = startup.unwrap_or_else(|| {
                    (ctx.opening_context)(
                        &ctx.session.tool_ctx.cwd,
                        &ctx.session.tool_ctx.scratch_dir,
                        &ctx.session.tool_ctx.instruction_discovery,
                    )
                });
                ctx.session.restore_startup_context(
                    recovered_startup,
                    previous_environment,
                    delivered_environment,
                    capabilities,
                    delivered_capabilities,
                );
                let current = (ctx.environment)(&ctx.session.tool_ctx.cwd);
                ctx.session.sync_environment(current, None);
                ctx.session.sync_capabilities(ctx.capabilities.clone());
                // The resumed conversation's plan is re-read from disk, not
                // trusted from the log: the user may have edited it since.
                ctx.session.restore_progress(progress.as_deref());
                // Unknown until the next usage event; the TUI re-estimates in
                // its ConversationReplaced handler.
                ctx.session.last_prompt_tokens = 0;
                ctx.session
                    .tool_ctx
                    .freshness
                    .lock()
                    .expect("freshness lock")
                    .clear();
                CommandOutcome::effect(CommandEffect::ConversationReplaced)
            }
            Err(e) => CommandOutcome::error(format!("cannot resume session {args}: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{test_ctx_parts, CommandCtx, CommandEffect, SlashCommand};
    use super::ResumeCommand;
    use crate::types::Usage;
    use crate::{ContentBlock, Entry, Ledger, LogEvent, RuntimeCapabilities, SessionStore};

    #[test]
    fn bare_resume_opens_the_picker_and_bad_ids_report_an_error() {
        let (mut session, opening, environment) = test_ctx_parts();
        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            environment: &environment,
            capabilities: crate::RuntimeCapabilities::new("test", std::iter::empty::<&str>()),
            turn_usage: Usage::default(),
        };
        let outcome = ResumeCommand.run(&mut ctx, "");
        assert!(matches!(
            &outcome.effects[..],
            [CommandEffect::OpenResumePicker]
        ));

        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            environment: &environment,
            capabilities: crate::RuntimeCapabilities::new("test", std::iter::empty::<&str>()),
            turn_usage: Usage::default(),
        };
        let outcome = ResumeCommand.run(&mut ctx, "no-such-session-id");
        assert!(outcome.effects.is_empty());
        assert!(!outcome.messages.is_empty());
    }

    #[test]
    fn explicit_resume_stages_capability_note_for_the_next_delivery_point() {
        crate::home::testing::temp_home();
        let cwd = tempfile::tempdir().unwrap();
        let data_dir = crate::store::project_data_dir(cwd.path()).unwrap();
        let store = SessionStore::create(&data_dir, cwd.path()).unwrap();
        let session_id = store.id.clone();
        let mut ledger = Ledger::new();
        ledger.attach_sink(Box::new(store));
        ledger.record_aux(&LogEvent::RuntimeCapabilitiesDelivered {
            capabilities: RuntimeCapabilities::new("tui", ["bash", "read"]),
        });
        ledger.append(Entry::User(vec![ContentBlock::Text {
            text: "resume me".into(),
        }]));
        drop(ledger);

        let (mut session, opening, environment) = test_ctx_parts();
        session.tool_ctx.cwd = cwd.path().to_path_buf();
        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            environment: &environment,
            capabilities: RuntimeCapabilities::new("app", ["bash", "browser", "read", "show"]),
            turn_usage: Usage::default(),
        };

        let outcome = ResumeCommand.run(&mut ctx, &session_id);
        assert!(matches!(
            &outcome.effects[..],
            [CommandEffect::ConversationReplaced]
        ));
        let entries = ctx.session.take_deferred_context_entries();
        assert!(matches!(
            entries.as_slice(),
            [Entry::Note(text), Entry::Note(environment)]
                if text.contains("Runtime frontend/tool capabilities changed")
                    && text.contains("Frontend: tui")
                    && text.contains("app")
                    && text.contains("Tools now available: browser, show")
                    && environment.contains("environment")
        ));
    }
}
