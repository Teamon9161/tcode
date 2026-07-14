//! Slash commands as pluggable units, mirroring the `Tool` registry model.
//!
//! Ownership rule: a command whose substance acts on the `Session`, the
//! `Ledger` or the filesystem lives here; a command whose substance drives a
//! frontend-owned object (model picker, provider wizard, key table) stays in
//! the frontend. Frontends are effect interpreters: they dispatch a line into
//! the registry and apply the returned [`CommandEffect`]s.

mod cd;
mod clear;
mod compact;
mod cost;
mod dogfood;
mod exit;
mod export;
mod memory;
mod mode;
mod note;
mod resume;

use std::path::Path;
use std::sync::Arc;

use crate::agent::Session;
use crate::types::Usage;

/// Rebuilds the cwd-specific opening context (project map, instructions)
/// when `/cd` runs before any model-visible history exists.
pub type OpeningContextFn = Arc<dyn Fn(&Path) -> String + Send + Sync>;

/// How a frontend should style a command's feedback. `Note` marks text the
/// user addressed to the model (e.g. `/note`), not harness status output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Info,
    Error,
    Note,
}

#[derive(Debug, Clone)]
pub struct CommandMessage {
    pub kind: MessageKind,
    pub text: String,
}

/// Follow-up work a command completed on the `Session` still needs from the
/// frontend. Admission rule: a variant must either have a non-trivial
/// interpretation in every frontend (`Exit`, `Compact`, the conversation
/// resets) or a defined degradation (`OpenResumePicker` → plain frontends
/// point at `/resume <id>`). Anything else belongs in the command itself.
#[derive(Debug, Clone)]
pub enum CommandEffect {
    Exit,
    /// The ledger was emptied: reset conversation-scoped UI state.
    ConversationCleared,
    /// The ledger was replaced wholesale (resume): reset conversation-scoped
    /// UI state and replay the transcript.
    ConversationReplaced,
    /// Run compaction like a turn (spinner, cancellation and usage reporting
    /// are frontend concerns).
    Compact {
        focus: Option<String>,
    },
    OpenResumePicker,
    /// The dogfood switch flipped: write it to state.toml so it survives a
    /// restart. Persisting program state is the frontend's job here — it
    /// already owns it for the `/model` choice, and core's own tests must not
    /// write the developer's home directory to exercise a command.
    PersistDogfood(bool),
}

#[derive(Debug, Default)]
pub struct CommandOutcome {
    pub messages: Vec<CommandMessage>,
    pub effects: Vec<CommandEffect>,
}

impl CommandOutcome {
    pub fn info(text: impl Into<String>) -> Self {
        Self::message(MessageKind::Info, text)
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self::message(MessageKind::Error, text)
    }

    pub fn message(kind: MessageKind, text: impl Into<String>) -> Self {
        Self {
            messages: vec![CommandMessage {
                kind,
                text: text.into(),
            }],
            effects: Vec::new(),
        }
    }

    pub fn effect(effect: CommandEffect) -> Self {
        Self {
            messages: Vec::new(),
            effects: vec![effect],
        }
    }

    pub fn with_effect(mut self, effect: CommandEffect) -> Self {
        self.effects.push(effect);
        self
    }
}

/// Everything a command may touch. Commands run synchronously; the only
/// long-running operation (`/compact`) is returned as an effect so the
/// frontend can wrap it in its own turn machinery.
pub struct CommandCtx<'a> {
    pub session: &'a mut Session,
    pub opening_context: &'a OpeningContextFn,
    /// The frontend's display tally for `/cost` (the TUI includes delegated
    /// sub-agent usage; the plain REPL passes `session.turn_usage`).
    pub turn_usage: Usage,
}

pub trait SlashCommand: Send + Sync {
    /// Command name without the leading slash, e.g. `"cd"`.
    fn name(&self) -> &'static str;
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }
    fn help(&self) -> &'static str;
    /// Keep the command out of /help and completion while leaving it
    /// dispatchable. For developer instruments whose surface would only
    /// confuse a user who has no reason to run them.
    fn hidden(&self) -> bool {
        false
    }
    fn run(&self, ctx: &mut CommandCtx<'_>, args: &str) -> CommandOutcome;
}

pub struct CommandRegistry {
    commands: Vec<Box<dyn SlashCommand>>,
    /// `("/name", help)` for the advertised commands, in registration order:
    /// what /help lists and what completion offers. Hidden commands are
    /// absent here but still resolve through `find` / `dispatch`.
    entries: Vec<(String, &'static str)>,
}

impl CommandRegistry {
    pub fn new(commands: Vec<Box<dyn SlashCommand>>) -> Self {
        let entries = commands
            .iter()
            .filter(|c| !c.hidden())
            .map(|c| (format!("/{}", c.name()), c.help()))
            .collect();
        Self { commands, entries }
    }

    pub fn builtin() -> Self {
        Self::new(vec![
            Box::new(cd::CdCommand),
            Box::new(mode::ModeCommand),
            Box::new(cost::CostCommand),
            Box::new(compact::CompactCommand),
            Box::new(clear::ClearCommand),
            Box::new(resume::ResumeCommand),
            Box::new(note::NoteCommand),
            Box::new(memory::MemoryCommand),
            Box::new(export::ExportCommand),
            Box::new(exit::ExitCommand),
            Box::new(dogfood::DogfoodCommand),
        ])
    }

    /// `("/name", help)` pairs in registration order.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &'static str)> + '_ {
        self.entries
            .iter()
            .map(|(name, help)| (name.as_str(), *help))
    }

    /// The command a line like `/cd ../foo` addresses, if registered.
    pub fn find(&self, line: &str) -> Option<&dyn SlashCommand> {
        let (name, _) = split_line(line)?;
        self.commands
            .iter()
            .find(|c| c.name() == name || c.aliases().contains(&name))
            .map(|c| c.as_ref())
    }

    /// Parse and run a slash line. `None` means the command is unknown and
    /// the frontend should fall back to its own dispatch or report it.
    pub fn dispatch(&self, ctx: &mut CommandCtx<'_>, line: &str) -> Option<CommandOutcome> {
        let (name, args) = split_line(line)?;
        let cmd = self
            .commands
            .iter()
            .find(|c| c.name() == name || c.aliases().contains(&name))?;
        Some(cmd.run(ctx, args))
    }
}

fn split_line(line: &str) -> Option<(&str, &str)> {
    let rest = line.trim().strip_prefix('/')?;
    Some(match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim()),
        None => (rest, ""),
    })
}

#[cfg(test)]
pub(crate) fn test_ctx_parts() -> (Session, OpeningContextFn) {
    use crate::{PermissionMode, PermissionRules, ToolCtx};
    let session = Session::new(
        ToolCtx::new(std::env::temp_dir(), 1_000),
        PermissionMode::Default,
        PermissionRules::default(),
    );
    let opening: OpeningContextFn = Arc::new(|_| String::new());
    (session, opening)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_splits_name_and_args() {
        assert_eq!(split_line("/cd ../foo"), Some(("cd", "../foo")));
        assert_eq!(split_line("/cd"), Some(("cd", "")));
        assert_eq!(
            split_line("  /note  hello world "),
            Some(("note", "hello world"))
        );
        assert_eq!(split_line("plain text"), None);
    }

    #[test]
    fn unknown_commands_return_none() {
        let registry = CommandRegistry::builtin();
        let (mut session, opening) = test_ctx_parts();
        let mut ctx = CommandCtx {
            session: &mut session,
            opening_context: &opening,
            turn_usage: Usage::default(),
        };
        assert!(registry
            .dispatch(&mut ctx, "/definitely-not-a-command")
            .is_none());
        assert!(registry.find("/cdfoo").is_none());
    }

    #[test]
    fn aliases_resolve_to_the_same_command() {
        let registry = CommandRegistry::builtin();
        assert_eq!(registry.find("/exit").unwrap().name(), "exit");
        assert_eq!(registry.find("/quit").unwrap().name(), "exit");
    }

    #[test]
    fn entries_keep_registration_order_with_slashes() {
        let registry = CommandRegistry::builtin();
        let names: Vec<&str> = registry.entries().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            [
                "/cd", "/mode", "/cost", "/compact", "/clear", "/resume", "/note", "/memory",
                "/export", "/exit"
            ]
        );
    }
}
