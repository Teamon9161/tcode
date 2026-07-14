use std::path::{Path, PathBuf};

use crate::ledger::{Entry, Ledger};
use crate::permission::{PermissionMode, PermissionRules};
use crate::tool::ToolCtx;
use crate::types::{ContentBlock, Usage};

#[derive(Debug, Clone)]
pub struct CwdChange {
    pub old: PathBuf,
    pub new: PathBuf,
    pub changed: bool,
    /// The session has no model-visible history, so its opening system context
    /// must be regenerated for the new cwd instead of appending a note.
    pub refresh_opening_context: bool,
}

/// Mutable per-conversation state.
pub struct Session {
    pub ledger: Ledger,
    /// Cwd-specific opening context (project map, instructions, memory).
    /// It is replaceable only before the first model-visible entry exists.
    opening_context: String,
    pub mode: PermissionMode,
    pub rules: PermissionRules,
    pub tool_ctx: ToolCtx,
    /// File snapshots for rewind; no-op unless persistence is set up.
    pub checkpoints: crate::checkpoint::CheckpointStore,
    /// Prompt size of the latest request (for the context status line).
    pub last_prompt_tokens: u64,
    pub turn_usage: Usage,
    /// `/dogfood`: also report harness tool defects while working.
    dogfood: bool,
}

impl Session {
    pub fn new(tool_ctx: ToolCtx, mode: PermissionMode, rules: PermissionRules) -> Self {
        Self {
            ledger: Ledger::new(),
            opening_context: String::new(),
            mode,
            rules,
            tool_ctx,
            checkpoints: crate::checkpoint::CheckpointStore::default(),
            last_prompt_tokens: 0,
            turn_usage: Usage::default(),
            dogfood: false,
        }
    }

    /// Cwd-specific portion of the system prompt used for request construction.
    pub fn opening_context(&self) -> &str {
        &self.opening_context
    }

    pub fn dogfood(&self) -> bool {
        self.dogfood
    }

    /// Toggling changes the system prompt, i.e. the cached prefix. That is a
    /// one-time re-prime, the same class of cost as `/compact` — deliberately
    /// preferred over re-sending the directive in every turn's tail, which
    /// would pay for it forever and bloat the history it lands in.
    pub fn set_dogfood(&mut self, on: bool) {
        self.dogfood = on;
    }

    /// Set the cwd-specific part of the system prompt before a first turn.
    pub fn set_opening_context(&mut self, context: String) {
        debug_assert!(self.ledger.is_empty());
        self.opening_context = context;
    }

    /// Change the conversation working directory. Before any model-visible
    /// history exists, the caller refreshes the opening context for the new
    /// directory. Later changes are append-only notes so cached history stays
    /// valid.
    pub fn change_cwd(&mut self, arg: &str) -> Result<CwdChange, String> {
        let old = self.tool_ctx.cwd.clone();
        let Some(new) = resolve_cd_target(&old, arg)? else {
            return Ok(CwdChange {
                old: old.clone(),
                new: old,
                changed: false,
                refresh_opening_context: false,
            });
        };
        if same_path(&old, &new) {
            return Ok(CwdChange {
                old,
                new,
                changed: false,
                refresh_opening_context: false,
            });
        }

        let refresh_opening_context = self.ledger.is_empty();
        self.tool_ctx.cwd = new.clone();
        if refresh_opening_context {
            self.tool_ctx.memory = std::sync::Mutex::new(crate::MemoryManager::new(&new));
            return Ok(CwdChange {
                old,
                new,
                changed: true,
                refresh_opening_context: true,
            });
        }

        let memory_note = {
            let mut memory = self.tool_ctx.memory.lock().expect("memory lock");
            memory.restore_from_entries(self.ledger.entries());
            memory
                .discover_for_paths(std::slice::from_ref(&new))
                .map(|update| update.note)
        };
        let mut note = format!(
            "Working directory changed by the user from {} to {}. Future relative tool paths, default shell cwd, grep/glob defaults, and cwd-relative operations now resolve against {}. Do not rely on the startup project map or earlier cwd-specific assumptions for the new directory; inspect as needed.",
            old.display(),
            new.display(),
            new.display()
        );
        if let Some(memory_note) = memory_note {
            note.push_str("\n\n");
            note.push_str(&memory_note);
        }
        self.ledger.append(Entry::Note(note));
        Ok(CwdChange {
            old,
            new,
            changed: true,
            refresh_opening_context: false,
        })
    }

    /// Tail self-awareness line: the model can only manage its context
    /// budget if it knows it. Appended inside the newest user entry, so
    /// the prompt prefix never changes retroactively (cache-safe).
    pub(super) fn status_block(&self, context_window: u64) -> Option<ContentBlock> {
        if self.last_prompt_tokens == 0 {
            return None;
        }
        let pct = (self.last_prompt_tokens as f64 / context_window as f64 * 100.0).round();
        let background = {
            let tasks = self.tool_ctx.background.lock().expect("background lock");
            let running = tasks.running();
            if running.is_empty() {
                String::new()
            } else {
                format!(" · background tasks running: {}", running.join(", "))
            }
        };
        Some(ContentBlock::Text {
            text: format!(
                "<tcode-status>context ~{pct:.0}% of {}k tokens · permission-mode: {}{background}</tcode-status>",
                context_window / 1000,
                self.mode.label()
            ),
        })
    }
}

fn resolve_cd_target(current: &Path, arg: &str) -> Result<Option<PathBuf>, String> {
    let arg = strip_matching_quotes(arg.trim());
    if arg.is_empty() {
        return Ok(None);
    }
    let raw = if arg == "~" {
        dirs::home_dir().ok_or_else(|| "cannot expand ~: no home directory found".to_string())?
    } else if let Some(rest) = arg.strip_prefix("~/").or_else(|| arg.strip_prefix("~\\")) {
        dirs::home_dir()
            .ok_or_else(|| "cannot expand ~: no home directory found".to_string())?
            .join(rest)
    } else {
        PathBuf::from(arg)
    };
    let candidate = if raw.is_absolute() {
        raw
    } else {
        current.join(raw)
    };
    let resolved = candidate
        .canonicalize()
        .map_err(|e| format!("cannot cd to {}: {e}", candidate.display()))?;
    if !resolved.is_dir() {
        return Err(format!("not a directory: {}", resolved.display()));
    }
    Ok(Some(resolved))
}

fn strip_matching_quotes(s: &str) -> &str {
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        if (bytes[0] == b'\"' && bytes[s.len() - 1] == b'\"')
            || (bytes[0] == b'\'' && bytes[s.len() - 1] == b'\'')
        {
            return &s[1..s.len() - 1];
        }
    }
    s
}

fn same_path(a: &Path, b: &Path) -> bool {
    path_key(a) == path_key(b)
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::Session;
    use crate::{Entry, PermissionMode, PermissionRules, ToolCtx};

    #[test]
    fn change_cwd_refreshes_fresh_context_or_appends_a_history_note() {
        let root = std::env::temp_dir().join(format!(
            "tcode-cd-test-{}-{}",
            std::process::id(),
            "change-cwd"
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("child")).unwrap();
        let root = root.canonicalize().unwrap();
        let child = root.join("child").canonicalize().unwrap();

        let mut session = Session::new(
            ToolCtx::new(root.clone(), 1_000),
            PermissionMode::Default,
            PermissionRules::default(),
        );
        let change = session.change_cwd("child").unwrap();

        assert!(change.changed);
        assert!(change.refresh_opening_context);
        assert_eq!(change.old, root);
        assert_eq!(change.new, child);
        assert_eq!(session.tool_ctx.cwd, child);
        assert!(session.ledger.is_empty());

        session.ledger.append(Entry::Note("history".into()));
        let change = session.change_cwd("..").unwrap();
        assert!(change.changed);
        assert!(!change.refresh_opening_context);
        assert_eq!(session.tool_ctx.cwd, root);
        assert!(matches!(
            session.ledger.entries().last(),
            Some(Entry::Note(text))
                if text.contains("Working directory changed by the user")
                    && text.contains("Future relative tool paths")
        ));

        let entries = session.ledger.len();
        let change = session.change_cwd(".").unwrap();
        assert!(!change.changed);
        assert!(!change.refresh_opening_context);
        assert_eq!(session.ledger.len(), entries);

        let _ = std::fs::remove_dir_all(&root);
    }
}
