use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::ledger::{Entry, Ledger};
use crate::permission::{PermissionMode, PermissionRules};
use crate::template::PromptVariables;
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

/// A permission-mode switch the user requested while a turn was running.
///
/// Like [`PendingInput`], it is a shared handle the frontend keeps while the
/// running turn owns the `Session`. A key press never mutates the ledger and
/// never flips the live mode mid-batch; it only writes the staged target here.
/// The agent loop commits it at the next batch boundary (and at turn start and
/// end), where permission for a whole batch is judged under one mode.
///
/// Pressing shift+tab repeatedly while running leaves only the final target:
/// the cycle reads staged-else-committed, so intermediate stops collapse.
#[derive(Clone, Default)]
pub struct PendingMode(Arc<Mutex<Option<PermissionMode>>>);

impl PendingMode {
    /// Stage a target mode, replacing any earlier unstaged target.
    pub fn set(&self, mode: PermissionMode) {
        *self.0.lock().expect("pending mode lock") = Some(mode);
    }

    /// The staged target, if any (for the frontend's cycle base and status).
    pub fn get(&self) -> Option<PermissionMode> {
        *self.0.lock().expect("pending mode lock")
    }

    /// Take the staged target, clearing it.
    pub fn take(&self) -> Option<PermissionMode> {
        self.0.lock().expect("pending mode lock").take()
    }

    /// Drop any staged target, e.g. when an approval dialog set the mode
    /// itself and a stale earlier staging must not override it.
    pub fn clear(&self) {
        *self.0.lock().expect("pending mode lock") = None;
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
    /// A permission-mode switch staged while the turn was running, committed
    /// at the next batch boundary. The frontend holds a clone of this handle.
    pub pending_mode: PendingMode,
    /// The mode the model was last told about. Comparing it to `mode` at a
    /// delivery point yields the plan-enter note without a per-keypress event
    /// stream — flipping through modes and landing back where you started
    /// produces no note at all.
    last_notified_mode: PermissionMode,
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
    /// Runtime values are captured from trusted harness state before prompt
    /// construction. They only change as part of a full conversation
    /// replacement, which also changes the provider cache scope.
    prompt_variables: PromptVariables,
}

impl Session {
    pub fn new(tool_ctx: ToolCtx, mode: PermissionMode, rules: PermissionRules) -> Self {
        let prompt_variables = PromptVariables::new(&tool_ctx.cwd, &tool_ctx.scratch_dir);
        Self {
            ledger: Ledger::new(),
            opening_context: String::new(),
            mode,
            rules,
            tool_ctx,
            pending: PendingInput::default(),
            pending_mode: PendingMode::default(),
            // Seed as "not yet in plan" so a session started in plan mode still
            // injects the plan-enter note with its first prompt. Only entering
            // plan ever injects, so any non-plan seed is equivalent for the
            // other modes.
            last_notified_mode: if mode == PermissionMode::Plan {
                PermissionMode::Default
            } else {
                mode
            },
            checkpoints: crate::checkpoint::CheckpointStore::default(),
            last_prompt_tokens: 0,
            turn_usage: Usage::default(),
            dogfood: false,
            suggestions: true,
            auto_consecutive_denials: 0,
            auto_total_denials: 0,
            auto_consecutive_unavailable: 0,
            cache_scope: None,
            prompt_variables,
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

    /// Runtime values for one-pass expansion in harness-owned prompt templates.
    pub fn prompt_variables(&self) -> &PromptVariables {
        &self.prompt_variables
    }

    pub fn classifier_cache_scope(&self) -> String {
        let session_scope = self
            .cache_scope
            .as_deref()
            .unwrap_or_else(|| self.prompt_variables.session_id());
        format!("auto-classifier:{session_scope}")
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

    /// Guidance injected into the ledger the first time the model is in plan
    /// mode after a delivery point. Raw text; the ledger wraps it as a note.
    const PLAN_ENTER_NOTE: &'static str = include_str!("../../../../prompts/plan-mode-enter.md");

    /// Commit a staged permission-mode switch, if one is pending. Returns the
    /// new mode only when it differs from the current one, so a net-zero cycle
    /// (staging that flipped back to the live mode) reports no change. Called
    /// at every safe boundary: turn start, batch boundary, turn end.
    pub fn commit_pending_mode(&mut self) -> Option<PermissionMode> {
        let staged = self.pending_mode.take()?;
        if staged == self.mode {
            return None;
        }
        self.mode = staged;
        Some(staged)
    }

    /// Apply a mode transition an approval dialog chose (e.g. `exit_plan`
    /// approved with "auto-accept edits"). Syncs `last_notified_mode` — the
    /// tool result already states the new mode, so no note is owed — and drops
    /// any staged switch so a stale earlier keypress cannot override it.
    pub fn apply_approved_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
        self.last_notified_mode = mode;
        self.pending_mode.clear();
    }

    /// The note owed to the model at a delivery point, evaluated by comparing
    /// the live mode with the one it was last told about. Only *entering* plan
    /// mode injects guidance; every other transition is absorbed by the
    /// permission gate and needs no model-visible note. Always resyncs the
    /// last-notified mode, so back-and-forth switching never restacks notes.
    pub fn take_mode_note(&mut self) -> Option<String> {
        let note = (self.mode == PermissionMode::Plan
            && self.last_notified_mode != PermissionMode::Plan)
            .then(|| Self::PLAN_ENTER_NOTE.trim().to_string());
        self.last_notified_mode = self.mode;
        note
    }

    /// Set the cwd-specific part of the system prompt before a first turn.
    pub fn set_opening_context(&mut self, context: String) {
        debug_assert!(self.ledger.is_empty());
        self.opening_context = context;
    }

    /// Replace the startup context when a whole conversation is restored or
    /// imported before its next request. Ordinary cwd changes must use
    /// `set_opening_context`'s empty-ledger guard instead, preserving the
    /// append-only cached prefix invariant.
    pub fn replace_opening_context_for_resume(&mut self, context: String) {
        self.opening_context = context;
    }

    /// Bind ephemeral tool state to a persistent conversation. This is a full
    /// conversation replacement (`/resume` or startup), so it first changes the
    /// provider cache scope and only then captures the replacement variables.
    pub fn bind_scratch_session(&mut self, session_id: &str) {
        let scratch = crate::store::session_scratchpad_dir(&self.tool_ctx.cwd, session_id);
        self.tool_ctx.rebind_scratch_dir(scratch);
        self.cache_scope = Some(format!("main:{session_id}"));
        self.prompt_variables =
            PromptVariables::new(&self.tool_ctx.cwd, &self.tool_ctx.scratch_dir);
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
            self.prompt_variables = PromptVariables::new(&new, &self.tool_ctx.scratch_dir);
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

    fn plan_session() -> Session {
        Session::new(
            ToolCtx::new(std::env::temp_dir(), 1_000),
            PermissionMode::Default,
            PermissionRules::default(),
        )
    }

    #[test]
    fn staging_keeps_only_the_final_target_and_commits_the_net_change() {
        let mut session = plan_session();
        // Repeated presses while running leave only the last staged target.
        session.pending_mode.set(PermissionMode::AcceptEdits);
        session.pending_mode.set(PermissionMode::Plan);
        assert_eq!(session.pending_mode.get(), Some(PermissionMode::Plan));
        assert_eq!(session.commit_pending_mode(), Some(PermissionMode::Plan));
        assert_eq!(session.mode, PermissionMode::Plan);
        // The staging is consumed.
        assert_eq!(session.commit_pending_mode(), None);
    }

    #[test]
    fn a_net_zero_cycle_commits_nothing() {
        let mut session = plan_session();
        // Staged back to the live mode: no change reported, mode untouched.
        session.pending_mode.set(PermissionMode::Default);
        assert_eq!(session.commit_pending_mode(), None);
        assert_eq!(session.mode, PermissionMode::Default);
    }

    #[test]
    fn only_entering_plan_injects_a_note_and_only_once() {
        let mut session = plan_session();
        // default → no note.
        assert!(session.take_mode_note().is_none());
        // enter plan → one note.
        session.mode = PermissionMode::Plan;
        assert!(session.take_mode_note().is_some());
        // still plan, already notified → no restacking.
        assert!(session.take_mode_note().is_none());
        // leaving plan → no note.
        session.mode = PermissionMode::AcceptEdits;
        assert!(session.take_mode_note().is_none());
    }

    #[test]
    fn a_session_started_in_plan_mode_injects_the_opening_note() {
        let mut session = Session::new(
            ToolCtx::new(std::env::temp_dir(), 1_000),
            PermissionMode::Plan,
            PermissionRules::default(),
        );
        assert!(session.take_mode_note().is_some());
        assert!(session.take_mode_note().is_none());
    }

    #[test]
    fn back_and_forth_between_notes_yields_no_note() {
        let mut session = plan_session();
        session.mode = PermissionMode::AcceptEdits;
        session.mode = PermissionMode::Default;
        // Never landed on plan between delivery points → nothing owed.
        assert!(session.take_mode_note().is_none());
    }

    #[test]
    fn an_approved_transition_owes_no_note() {
        let mut session = plan_session();
        session.mode = PermissionMode::Plan;
        // Simulate the plan-enter note already delivered.
        assert!(session.take_mode_note().is_some());
        // exit_plan approved into accept-edits: result already states the mode.
        session.apply_approved_mode(PermissionMode::AcceptEdits);
        assert!(session.take_mode_note().is_none());
        // A stale staged switch cannot override the approved mode.
        assert_eq!(session.pending_mode.get(), None);
    }

    #[test]
    fn session_replacement_changes_cache_scope_before_refreshing_variables() {
        let root = std::env::temp_dir();
        let mut session = Session::new(
            ToolCtx::new(root, 1_000),
            PermissionMode::Auto,
            PermissionRules::default(),
        );
        let initial_scratch = session.prompt_variables().expand("${TCODE_SCRATCH_DIR}");
        let initial_scope = session.classifier_cache_scope();

        session.bind_scratch_session("resumed-session");

        assert_ne!(
            session.prompt_variables().expand("${TCODE_SCRATCH_DIR}"),
            initial_scratch
        );
        assert_ne!(session.classifier_cache_scope(), initial_scope);
        assert_eq!(
            session.classifier_cache_scope(),
            "auto-classifier:main:resumed-session"
        );
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
        let initial_project = session.prompt_variables().expand("${TCODE_PROJECT_DIR}");
        let change = session.change_cwd("child").unwrap();

        assert!(change.changed);
        assert!(change.refresh_opening_context);
        assert_eq!(change.old, root);
        assert_eq!(change.new, child);
        assert_eq!(session.tool_ctx.cwd, child);
        assert_eq!(
            session.prompt_variables().expand("${TCODE_PROJECT_DIR}"),
            child.display().to_string()
        );
        assert_ne!(
            session.prompt_variables().expand("${TCODE_PROJECT_DIR}"),
            initial_project
        );
        assert!(session.ledger.is_empty());

        session.ledger.append(Entry::Note("history".into()));
        let frozen_project = session.prompt_variables().expand("${TCODE_PROJECT_DIR}");
        let change = session.change_cwd("..").unwrap();
        assert!(change.changed);
        assert!(!change.refresh_opening_context);
        assert_eq!(session.tool_ctx.cwd, root);
        assert_eq!(
            session.prompt_variables().expand("${TCODE_PROJECT_DIR}"),
            frozen_project
        );
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
