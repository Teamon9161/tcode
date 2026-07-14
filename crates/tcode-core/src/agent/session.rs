use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::ledger::{Entry, Ledger};
use crate::permission::{PermissionMode, PermissionRules};
use crate::tool::ToolCtx;
use crate::types::{ContentBlock, Usage};

/// A message the user has sent but the model has not seen yet.
///
/// It is a whole message, not a string: an image pasted while the agent works
/// belongs to the prompt it was pasted into. `blocks` is what the ledger gets;
/// `text` and `attachments` are what the frontend shows while it waits and
/// again once it is delivered — the same two renderings a normal prompt has.
#[derive(Clone, Debug)]
pub struct PendingMessage {
    pub text: String,
    /// Attachment labels, in the order they hang under the prompt.
    pub attachments: Vec<String>,
    pub blocks: Vec<ContentBlock>,
}

/// Messages the user submitted while a turn was still running.
///
/// They cannot be appended the moment they are typed: between an assistant's
/// `tool_use` and its `ToolResults` nothing may come, or the request is
/// malformed. The agent loop therefore drains this queue at the first legal
/// point — right after a tool batch commits its results — where the ledger
/// merges the message into the same user turn as those results. The model sees
/// it on its very next step, and the prefix stays append-only.
///
/// A shared handle rather than a plain field: the frontend keeps taking input
/// while the running turn owns the `Session`.
#[derive(Clone, Default)]
pub struct PendingInput(Arc<Mutex<VecDeque<PendingMessage>>>);

impl PendingInput {
    pub fn push(&self, message: PendingMessage) {
        self.0
            .lock()
            .expect("pending input lock")
            .push_back(message);
    }

    /// Hands over everything queued so far. Atomic, so the loop's boundary
    /// drain and the frontend's end-of-turn flush can race harmlessly: exactly
    /// one of them gets each message.
    pub fn take(&self) -> Vec<PendingMessage> {
        self.0
            .lock()
            .expect("pending input lock")
            .drain(..)
            .collect()
    }

    /// What the frontend still owes the model, for display.
    pub fn queued(&self) -> Vec<PendingMessage> {
        self.0
            .lock()
            .expect("pending input lock")
            .iter()
            .cloned()
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.0.lock().expect("pending input lock").is_empty()
    }
}

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
    /// Messages typed while the turn was running, delivered at the next safe
    /// append boundary. The frontend holds a clone of this handle.
    pub pending: PendingInput,
    /// File snapshots for rewind; no-op unless persistence is set up.
    pub checkpoints: crate::checkpoint::CheckpointStore,
    /// Prompt size of the latest request (for the context status line).
    pub last_prompt_tokens: u64,
    pub turn_usage: Usage,
    /// `/dogfood`: also report harness tool defects while working.
    dogfood: bool,
    /// `/suggestions`: guess the next prompt when the turn ends. Session state
    /// rather than a frontend flag, so the toggle and its persistence work the
    /// same way from the TUI and the REPL.
    suggestions: bool,
    /// Auto Mode's classifier denial backstop. These counters are session
    /// state rather than ledger entries: they guard runaway retries but must
    /// not alter the model-visible append-only history.
    auto_consecutive_denials: u8,
    auto_total_denials: u8,
    /// Consecutive classifier outages pause Auto Mode rather than repeatedly
    /// dropping the user into unexplained manual approvals.
    auto_consecutive_unavailable: u8,
    /// Provider cache scope for this conversation; see `Request::cache_scope`.
    /// A sub-agent shares its parent's provider but not its prefix, so it must
    /// not share its cache id.
    cache_scope: Option<String>,
}

impl Session {
    pub fn new(tool_ctx: ToolCtx, mode: PermissionMode, rules: PermissionRules) -> Self {
        Self {
            ledger: Ledger::new(),
            opening_context: String::new(),
            mode,
            rules,
            tool_ctx,
            pending: PendingInput::default(),
            checkpoints: crate::checkpoint::CheckpointStore::default(),
            last_prompt_tokens: 0,
            turn_usage: Usage::default(),
            dogfood: false,
            suggestions: true,
            auto_consecutive_denials: 0,
            auto_total_denials: 0,
            auto_consecutive_unavailable: 0,
            cache_scope: None,
        }
    }

    /// Names this session's provider cache scope. Every conversation that is
    /// not the main one needs a distinct name, or its prefix and the main
    /// agent's take turns evicting each other's cache affinity.
    pub fn with_cache_scope(mut self, scope: impl Into<String>) -> Self {
        self.cache_scope = Some(scope.into());
        self
    }

    pub fn cache_scope(&self) -> Option<String> {
        self.cache_scope.clone()
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

    pub fn suggestions(&self) -> bool {
        self.suggestions
    }

    /// Unlike `/dogfood` this leaves the cached prefix alone: the guess is a
    /// separate conversation, so turning it off costs the session nothing.
    pub fn set_suggestions(&mut self, on: bool) {
        self.suggestions = on;
    }

    /// Records a classifier result. Any allowed classified action breaks a
    /// consecutive-denial streak; repeated blocks pause Auto Mode before the
    /// model can keep probing for a way around the boundary.
    pub fn record_auto_classification(&mut self, allowed: bool) -> Option<String> {
        self.auto_consecutive_unavailable = 0;
        if allowed {
            self.auto_consecutive_denials = 0;
            return None;
        }
        self.auto_consecutive_denials = self.auto_consecutive_denials.saturating_add(1);
        self.auto_total_denials = self.auto_total_denials.saturating_add(1);
        if self.auto_consecutive_denials < 3 && self.auto_total_denials < 20 {
            return None;
        }
        self.mode = PermissionMode::Default;
        let reason = if self.auto_consecutive_denials >= 3 {
            format!(
                "Auto Mode paused after {} consecutive safety-classifier denials; permission mode is now default.",
                self.auto_consecutive_denials
            )
        } else {
            "Auto Mode paused after 20 safety-classifier denials in this session; permission mode is now default.".into()
        };
        self.auto_consecutive_denials = 0;
        self.auto_total_denials = 0;
        Some(reason)
    }

    /// Pauses Auto Mode after repeated classifier outages. The caller renders
    /// the returned notice as a frontend-only system message.
    pub fn record_auto_classifier_unavailable(&mut self) -> Option<String> {
        self.auto_consecutive_unavailable = self.auto_consecutive_unavailable.saturating_add(1);
        if self.auto_consecutive_unavailable < 3 {
            return None;
        }
        self.mode = PermissionMode::Default;
        self.auto_consecutive_unavailable = 0;
        Some(
            "Auto Mode paused after 3 consecutive classifier failures; permission mode is now default. Check the auto model in /agents or its connection, then re-enable Auto Mode when it is working.".into(),
        )
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
    fn three_auto_mode_classifier_failures_pause_to_default() {
        let root = std::env::temp_dir();
        let mut session = Session::new(
            ToolCtx::new(root, 1_000),
            PermissionMode::Auto,
            PermissionRules::default(),
        );
        assert!(session.record_auto_classifier_unavailable().is_none());
        assert!(session.record_auto_classifier_unavailable().is_none());
        let notice = session
            .record_auto_classifier_unavailable()
            .expect("pause notice");
        assert!(notice.contains("classifier failures"));
        assert!(notice.contains("/agents"));
        assert_eq!(session.mode, PermissionMode::Default);
    }

    #[test]
    fn three_auto_mode_denials_pause_to_default() {
        let root = std::env::temp_dir();
        let mut session = Session::new(
            ToolCtx::new(root, 1_000),
            PermissionMode::Auto,
            PermissionRules::default(),
        );
        assert!(session.record_auto_classification(false).is_none());
        assert!(session.record_auto_classification(false).is_none());
        let notice = session
            .record_auto_classification(false)
            .expect("pause notice");
        assert!(notice.contains("3 consecutive"));
        assert_eq!(session.mode, PermissionMode::Default);
    }

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
