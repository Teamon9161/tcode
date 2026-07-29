use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::config::FolderTrust;
use crate::cwd_scope::{CwdScoped, CwdScopes};
use crate::environment::{EnvironmentSnapshot, StartupContext};
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
#[derive(Default)]
struct PendingState {
    messages: VecDeque<PendingMessage>,
    /// Ctrl+C ends the current turn, so anything already queued belongs to the
    /// fresh turn the frontend starts after this worker exits.
    defer_to_next_turn: bool,
}

#[derive(Clone, Default)]
pub struct PendingInput(Arc<Mutex<PendingState>>);

impl PendingInput {
    pub fn push(&self, message: PendingMessage) {
        self.0
            .lock()
            .expect("pending input lock")
            .messages
            .push_back(message);
    }

    /// Make queued input unavailable to the current turn's safe-boundary drain.
    /// The frontend consumes it after the worker exits, which gives the new turn
    /// a fresh cancellation token.
    pub fn defer_to_next_turn(&self) {
        self.0
            .lock()
            .expect("pending input lock")
            .defer_to_next_turn = true;
    }

    /// Hands over messages that may legally join the current turn. A Ctrl+C
    /// handoff is decided under the same lock as the drain, so a cancelled turn
    /// cannot consume a message intended for its successor.
    pub fn take_at_safe_boundary(&self) -> Vec<PendingMessage> {
        let mut pending = self.0.lock().expect("pending input lock");
        if pending.defer_to_next_turn {
            return Vec::new();
        }
        pending.messages.drain(..).collect()
    }

    /// Hands over everything once the current worker has exited. Atomic, so the
    /// loop's boundary drain and the frontend's end-of-turn flush can race
    /// harmlessly: exactly one of them gets each message.
    pub fn take_for_next_turn(&self) -> Vec<PendingMessage> {
        let mut pending = self.0.lock().expect("pending input lock");
        pending.defer_to_next_turn = false;
        pending.messages.drain(..).collect()
    }

    /// What the frontend still owes the model, for display.
    pub fn queued(&self) -> Vec<PendingMessage> {
        self.0
            .lock()
            .expect("pending input lock")
            .messages
            .iter()
            .cloned()
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.0
            .lock()
            .expect("pending input lock")
            .messages
            .is_empty()
    }
}

/// A permission-mode switch the user requested while a turn was running.
///
/// Like [`PendingInput`], it is a shared handle the frontend keeps while the
/// running turn owns the `Session`. A key press never mutates the ledger; it
/// only writes the staged target here.
///
/// The agent loop commits it (the gate only) at every permission-decision
/// point — before a batch is judged and before each remaining call in a serial
/// batch — as well as at turn start and end. Switch to unsafe/auto and the very
/// next call it faces is judged under it, rather than waiting a whole round-trip
/// for the next batch boundary. A key press is UI input, not model context: the
/// mode the model reads is re-derived into every turn's tail by `status_block`,
/// so nothing has to be appended — and nothing has to be retracted.
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
    /// Memory discovered for the target directory. It is delivered as a
    /// model-visible instruction entry rather than transcript history.
    pub memory_note: Option<String>,
    /// What the re-derived `CwdScoped` state has to say about the new
    /// directory. Shown to the user; never sent to the model, since it
    /// describes harness configuration rather than the conversation.
    pub scope_notes: Vec<String>,
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
    /// File snapshots for rewind; no-op unless persistence is set up.
    pub checkpoints: crate::checkpoint::CheckpointStore,
    /// The current progress file needs re-describing to the model at the next
    /// user delivery point. Set when the model cannot already have it: a fresh
    /// session or `/resume`, and after a `compact` dropped the tool calls that
    /// carried it. Never set by the model's own updates — those are in the
    /// ledger, and re-sending them is pure waste.
    progress_injection_pending: bool,
    /// The progress path the session log already names, so `record_progress_file`
    /// writes only on a real change.
    recorded_progress: Option<String>,
    /// Prompt size of the latest request (for the context status line).
    pub last_prompt_tokens: u64,
    pub turn_usage: Usage,
    /// `/dogfood`: also report harness tool defects while working.
    dogfood: bool,
    /// `/suggest`: guess the next prompt when the turn ends. Session state
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
    /// The user-selected local trust level of the current canonical cwd.
    /// It is runtime state, not a ledger entry: the classifier receives it as
    /// trusted harness context and a `/cd` can change it without rewriting the
    /// conversation prefix.
    folder_trust: FolderTrust,
    /// Whether the current folder has been decided for this process. An
    /// untrusted persisted choice is known and must not prompt again.
    folder_trust_known: bool,
    /// The latest actual harness environment, persisted even if the user has
    /// not yet sent it to the model.
    environment: Option<EnvironmentSnapshot>,
    /// The last environment explicitly represented in the system prefix or a
    /// model-facing environment Note.
    delivered_environment: Option<EnvironmentSnapshot>,
    /// One coalesced directory instruction awaiting a genuine user delivery
    /// point. Repeated `/cd` replaces it rather than growing append-only
    /// model history with unobserved intermediate directories.
    pending_environment_instruction: Option<String>,
    pending_environment_delivery: bool,
    /// One coalesced `/memory on|off` explanation awaiting delivery. The local
    /// setting changes immediately; only its final state reaches the model.
    pending_memory_note: Option<String>,
    /// Runtime values are captured from trusted harness state before prompt
    /// construction. They only change as part of a full conversation
    /// replacement, which also changes the provider cache scope.
    prompt_variables: PromptVariables,
    /// State the frontend derived from the working directory, re-derived by
    /// `change_cwd`. See `CwdScoped` for what belongs here and what must not.
    cwd_scopes: CwdScopes,
}

impl Session {
    /// Runtime placeholder values captured from this context's live state,
    /// including the auto-memory root if one could be resolved (so prompts
    /// like the Auto Mode classifier policy can reference `${TCODE_MEMORY_DIR}`).
    fn capture_prompt_variables(tool_ctx: &ToolCtx) -> PromptVariables {
        let memory_dir = tool_ctx
            .memory
            .lock()
            .expect("memory lock")
            .auto_dir()
            .map(Path::to_path_buf);
        PromptVariables::new(&tool_ctx.cwd, &tool_ctx.scratch_dir)
            .with_memory_dir(memory_dir.as_deref())
    }

    pub fn new(tool_ctx: ToolCtx, mode: PermissionMode, rules: PermissionRules) -> Self {
        let prompt_variables = Self::capture_prompt_variables(&tool_ctx);
        Self {
            ledger: Ledger::new(),
            opening_context: String::new(),
            mode,
            rules,
            tool_ctx,
            pending: PendingInput::default(),
            pending_mode: PendingMode::default(),
            checkpoints: crate::checkpoint::CheckpointStore::default(),
            progress_injection_pending: false,
            recorded_progress: None,
            last_prompt_tokens: 0,
            turn_usage: Usage::default(),
            dogfood: false,
            suggestions: false,
            auto_consecutive_denials: 0,
            auto_total_denials: 0,
            auto_consecutive_unavailable: 0,
            cache_scope: None,
            folder_trust: FolderTrust::Untrusted,
            folder_trust_known: false,
            environment: None,
            delivered_environment: None,
            pending_environment_instruction: None,
            pending_environment_delivery: false,
            pending_memory_note: None,
            prompt_variables,
            cwd_scopes: CwdScopes::default(),
        }
    }

    /// Register state that has to follow the working directory. Anything
    /// registered is re-derived by every `change_cwd`; read `CwdScoped` before
    /// adding one, since some cwd-derived state cannot be swapped mid-session.
    pub fn register_cwd_scope(&mut self, scoped: Arc<dyn CwdScoped>) {
        self.cwd_scopes.push(scoped);
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
        // The classifier's prefix includes trusted cwd and folder trust. An
        // opaque suffix prevents a /cd or trust choice from reusing a cache
        // entry generated under a different safety boundary.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.tool_ctx.cwd.hash(&mut hasher);
        self.folder_trust.hash(&mut hasher);
        format!("auto-classifier:{session_scope}:{:016x}", hasher.finish())
    }

    pub fn folder_trust(&self) -> FolderTrust {
        self.folder_trust
    }

    pub fn folder_trust_known(&self) -> bool {
        self.folder_trust_known
    }

    pub fn set_folder_trust(&mut self, trust: FolderTrust) {
        self.folder_trust = trust;
        self.folder_trust_known = true;
    }

    pub fn clear_folder_trust(&mut self) {
        self.folder_trust = FolderTrust::Untrusted;
        self.folder_trust_known = false;
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

    /// Apply a mode transition an approval dialog chose (e.g. a plan approved
    /// with "auto-accept edits"). The tool result itself states the new mode,
    /// so no additional explanation is owed; drop any staged switch so a stale
    /// earlier keypress cannot override it.
    pub fn apply_approved_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
        self.pending_mode.clear();
    }

    /// This conversation's current progress file, if it has one.
    pub fn progress(&self) -> std::sync::MutexGuard<'_, Option<crate::progress::Progress>> {
        self.tool_ctx.progress.lock().expect("progress lock")
    }

    /// Take over a progress file found on disk (`/plan resume`, or a resumed
    /// session's own file). The file is re-read rather than trusted from the
    /// session record: the record is a snapshot of what was true then, and the
    /// user may have edited the plan since — including finishing it by hand,
    /// which is why a `done` file is refused rather than revived.
    pub fn adopt_progress(&mut self, path: &Path) -> Result<String, String> {
        let progress = crate::progress::Progress::load(path)?;
        if progress.state() == crate::progress::ProgressState::Done {
            return Err(format!("'{}' is already finished", progress.title));
        }
        let title = progress.title.clone();
        *self.progress() = Some(progress);
        self.progress_injection_pending = true;
        self.record_progress_file();
        Ok(title)
    }

    /// Adopt whatever a resumed session log named, or nothing. Also resets the
    /// baseline, so the first `record_progress_file` after a resume compares
    /// against that log rather than against the conversation just replaced. A
    /// plan that has since been deleted or finished simply leaves none.
    pub fn restore_progress(&mut self, recorded: Option<&Path>) -> Option<String> {
        *self.progress() = None;
        self.recorded_progress = recorded.map(|path| path.display().to_string());
        let title = recorded.and_then(|path| self.adopt_progress(path).ok());
        if title.is_none() {
            self.recorded_progress = None;
        }
        title
    }

    /// Note in the session log which progress file this conversation is working
    /// through, so a resume finds its way back to it. Only the path is
    /// recorded — the plan itself is re-read, since the file outlives the
    /// session and the user may have changed it meanwhile.
    ///
    /// Cheap and idempotent: it writes only when the answer changed, which is
    /// why callers can put it on a per-batch path without thinking about it.
    pub fn record_progress_file(&mut self) {
        let current = self
            .progress()
            .as_ref()
            .map(|progress| progress.path().display().to_string());
        if current == self.recorded_progress {
            return;
        }
        self.recorded_progress = current.clone();
        // A finished (or dropped) plan records the empty path: resume must know
        // the conversation moved on, not silently reopen the last plan it saw.
        self.ledger
            .record_aux(&crate::store::LogEvent::ProgressAdopted {
                path: current.unwrap_or_default(),
            });
    }

    /// Owe the model a fresh description of the current progress file. Called
    /// where the model provably cannot still have one: a resumed session, and
    /// a `compact` that dropped the calls carrying it. The model's own updates
    /// are in the ledger and are never re-sent.
    pub fn mark_progress_injection(&mut self) {
        self.progress_injection_pending = true;
    }

    /// The progress summary owed at the next user delivery point, if any.
    fn take_progress_note(&mut self) -> Option<String> {
        if !std::mem::take(&mut self.progress_injection_pending) {
            return None;
        }
        crate::progress::current_summary(&self.tool_ctx)
    }

    /// Set the complete persisted startup context before the first request.
    /// A pre-turn `/cd` may replace it; the session log keeps the last version.
    pub fn set_startup_context(&mut self, startup: StartupContext) {
        debug_assert!(
            self.opening_context.is_empty(),
            "startup context may only be recorded before tcode sends a request"
        );
        self.opening_context = startup.text;
        self.environment = Some(startup.environment.clone());
        self.delivered_environment = Some(startup.environment);
        self.ledger
            .record_aux(&crate::store::LogEvent::StartupContext {
                startup: StartupContext {
                    text: self.opening_context.clone(),
                    environment: self.environment.clone().expect("startup environment"),
                },
            });
    }

    /// Restore the already-sent startup prefix without changing its bytes.
    pub fn restore_startup_context(
        &mut self,
        startup: StartupContext,
        environment: Option<EnvironmentSnapshot>,
        delivered_environment: Option<EnvironmentSnapshot>,
    ) {
        let StartupContext {
            text,
            environment: startup_environment,
        } = startup;
        self.opening_context = text;
        self.environment = environment;
        self.delivered_environment = delivered_environment.or_else(|| {
            // Old JSONL recorded environment changes only when it also appended
            // the Note, so its final stored snapshot is model-known.
            self.environment.clone().or(Some(startup_environment))
        });
        self.pending_environment_instruction = None;
        self.pending_environment_delivery = false;
        self.pending_memory_note = None;
    }

    /// Set the cwd-specific part of the system prompt before a first turn.
    /// Kept for focused tests and callers that do not persist a snapshot.
    pub fn set_opening_context(&mut self, context: String) {
        debug_assert!(self.ledger.is_empty());
        self.opening_context = context;
    }

    /// Bind ephemeral tool state to a persistent conversation. This is a full
    /// conversation replacement (`/resume` or startup), so it first changes the
    /// provider cache scope and only then captures the replacement variables.
    pub fn bind_scratch_session(&mut self, session_id: &str) {
        let scratch = crate::store::session_scratchpad_dir(&self.tool_ctx.cwd, session_id);
        self.tool_ctx.rebind_scratch_dir(scratch);
        // Task traces live and die with the session (like checkpoints), so
        // they bind here too — every persistent binding point flows through
        // this method.
        self.tool_ctx.bind_task_trace_root(
            crate::store::project_data_dir(&self.tool_ctx.cwd)
                .map(|dir| dir.join("tasks").join(session_id)),
        );
        self.cache_scope = Some(format!("main:{session_id}"));
        self.prompt_variables = Self::capture_prompt_variables(&self.tool_ctx);
    }

    /// Record a fresh runtime environment immediately, but defer its
    /// model-facing explanation until a user interaction actually reaches a
    /// legal append boundary. The auxiliary snapshot survives a crash/resume;
    /// repeated changes replace the explanation for unobserved intermediate
    /// directories.
    pub fn sync_environment(&mut self, current: EnvironmentSnapshot, instruction: Option<String>) {
        self.ledger
            .record_aux(&crate::store::LogEvent::EnvironmentObserved {
                environment: current.clone(),
            });
        self.environment = Some(current);
        self.pending_environment_instruction = instruction;
        self.pending_environment_delivery = true;
    }

    /// Defer the model-facing explanation of an immediately applied memory
    /// setting. A second toggle replaces the first explanation before either
    /// reaches append-only history.
    pub fn stage_memory_note(&mut self, note: String) {
        self.pending_memory_note = Some(note);
    }

    /// Consume all coalescible context whose final state has become meaningful
    /// to the model at a user delivery point.
    pub fn take_deferred_context_entries(&mut self) -> Vec<Entry> {
        let mut entries = self
            .pending_memory_note
            .take()
            .map(Entry::Note)
            .into_iter()
            .collect::<Vec<_>>();
        entries.extend(self.take_progress_note().map(Entry::Note));
        if !std::mem::take(&mut self.pending_environment_delivery) {
            return entries;
        }
        let Some(current) = self.environment.clone() else {
            return entries;
        };
        let diff = self
            .delivered_environment
            .as_ref()
            .map(|previous| previous.diff_lines(&current));
        let note = match diff {
            Some(lines) if !lines.is_empty() => format!(
                "Runtime environment changed since the model last received an environment update:\n{}\n\nFuture relative tool paths, default shell cwd, grep/glob defaults, and cwd-relative operations resolve against {}. The startup project map is historical; inspect files or Git status when current detail matters.",
                lines
                    .iter()
                    .map(|line| format!("- {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                current.cwd
            ),
            None if self.delivered_environment.is_none() => format!(
                "This session predates a delivered environment snapshot. Current working directory is {}. The startup project map may be historical; inspect files or Git status when current detail matters.",
                current.cwd
            ),
            _ => String::new(),
        };
        if !note.is_empty() {
            self.delivered_environment = Some(current.clone());
            self.ledger
                .record_aux(&crate::store::LogEvent::EnvironmentDelivered {
                    environment: current,
                });
            entries.push(Entry::Note(note));
        }
        if let Some(instruction) = self.pending_environment_instruction.take() {
            entries.push(Entry::Instruction(instruction));
        }
        entries
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
                memory_note: None,
                scope_notes: Vec::new(),
            });
        };
        if same_path(&old, &new) {
            return Ok(CwdChange {
                old,
                new,
                changed: false,
                refresh_opening_context: false,
                memory_note: None,
                scope_notes: Vec::new(),
            });
        }

        let refresh_opening_context = self.ledger.is_empty();
        self.tool_ctx.cwd = new.clone();
        // One place, for every registered scope, on every real change — so a
        // capability discovered from the project directory cannot go on
        // answering from the directory the process happened to start in.
        let scope_notes = self.cwd_scopes.rescope_all(&new);
        if refresh_opening_context {
            self.tool_ctx.memory = std::sync::Mutex::new(crate::MemoryManager::new_with_discovery(
                &new,
                self.tool_ctx.instruction_discovery.clone(),
            ));
            self.prompt_variables = Self::capture_prompt_variables(&self.tool_ctx);
            return Ok(CwdChange {
                old,
                new,
                changed: true,
                refresh_opening_context: true,
                memory_note: None,
                scope_notes,
            });
        }

        let memory_note = {
            let mut memory = self.tool_ctx.memory.lock().expect("memory lock");
            memory.restore_from_entries(self.ledger.entries());
            memory
                .discover_for_paths(std::slice::from_ref(&new))
                .map(|update| update.note)
        };
        Ok(CwdChange {
            old,
            new,
            changed: true,
            refresh_opening_context: false,
            memory_note,
            scope_notes,
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
            let mut running = tasks.running().join(", ");
            let agents = tasks.background_agent_ids();
            if !agents.is_empty() {
                if !running.is_empty() {
                    running.push_str(", ");
                }
                running.push_str(&agents.join(", "));
            }
            if running.is_empty() {
                String::new()
            } else {
                format!(" · background running: {running}")
            }
        };
        // A draft is "do not start yet" as a fact about the file rather than a
        // one-off instruction the model has to remember: it is re-derived every
        // turn, and it stops appearing the moment the user approves the plan.
        // The status block is the turn's tail, never the cached prefix.
        let draft = match self
            .tool_ctx
            .progress
            .lock()
            .expect("progress lock")
            .as_ref()
        {
            Some(progress) if progress.state() == crate::progress::ProgressState::Draft => {
                format!(
                    " · plan \"{}\" is an unapproved draft: keep refining it, do not start executing",
                    progress.title
                )
            }
            _ => String::new(),
        };
        Some(ContentBlock::Text {
            text: format!(
                "<tcode-status>context ~{pct:.0}% of {}k tokens · permission-mode: {}{background}{draft}</tcode-status>",
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
    // On Windows, canonicalize returns the `\\?\` extended-path form.
    // Strip it so the cwd we store looks normal to the model.
    #[cfg(windows)]
    let resolved = {
        let s = resolved.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            PathBuf::from(rest)
        } else {
            resolved
        }
    };
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
    use super::{PendingInput, PendingMessage, Session};
    use crate::cwd_scope::CwdScoped;
    use crate::{Entry, EnvironmentSnapshot, PermissionMode, PermissionRules, ToolCtx};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    /// Canonicalize and strip the `\\?\` extended-path prefix that Windows
    /// `std::fs::canonicalize` emits. The stripped form matches what
    /// `resolve_cd_target` now produces.
    fn canonicalized(path: impl AsRef<Path>) -> PathBuf {
        let path = std::fs::canonicalize(path.as_ref()).unwrap();
        #[cfg(windows)]
        {
            let s = path.to_string_lossy();
            if let Some(rest) = s.strip_prefix(r"\\?\") {
                return PathBuf::from(rest);
            }
        }
        path
    }

    #[test]
    fn ctrl_c_handoff_keeps_queued_messages_out_of_the_cancelled_turn() {
        let pending = PendingInput::default();
        pending.push(PendingMessage {
            text: "start the new turn".into(),
            attachments: vec![],
            blocks: vec![],
        });

        pending.defer_to_next_turn();
        assert!(pending.take_at_safe_boundary().is_empty());

        let next_turn = pending.take_for_next_turn();
        assert_eq!(next_turn.len(), 1);
        assert_eq!(next_turn[0].text, "start the new turn");
        assert!(pending.take_at_safe_boundary().is_empty());
    }

    #[test]
    fn three_auto_mode_classifier_failures_pause_to_default() {
        let root = std::env::temp_dir();
        let mut session = Session::new(
            ToolCtx::for_test(root, 1_000),
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
            ToolCtx::for_test(root, 1_000),
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
            ToolCtx::for_test(std::env::temp_dir(), 1_000),
            PermissionMode::Default,
            PermissionRules::default(),
        )
    }

    #[test]
    fn staging_keeps_only_the_final_target_and_commits_the_net_change() {
        let mut session = plan_session();
        // Repeated presses while running leave only the last staged target.
        session.pending_mode.set(PermissionMode::AcceptEdits);
        session.pending_mode.set(PermissionMode::Auto);
        assert_eq!(session.pending_mode.get(), Some(PermissionMode::Auto));
        assert_eq!(session.commit_pending_mode(), Some(PermissionMode::Auto));
        assert_eq!(session.mode, PermissionMode::Auto);
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

    /// "Do not start yet" is re-derived from the file every turn instead of
    /// being injected once and trusted to memory — and it rides in the turn's
    /// tail, never the cached prefix, because a phase flip must not cost the
    /// whole prefix at the moment the context is largest.
    #[test]
    fn an_unapproved_draft_is_re_stated_in_every_turn_tail() {
        crate::home::testing::temp_home();
        let cwd = tempfile::tempdir().unwrap();
        let mut session = Session::new(
            ToolCtx::for_test(cwd.path().to_path_buf(), 1_000),
            PermissionMode::Default,
            PermissionRules::default(),
        );
        session.last_prompt_tokens = 100;
        let text = |session: &Session| match session.status_block(100_000) {
            Some(crate::ContentBlock::Text { text }) => text,
            other => panic!("expected a status block, got {other:?}"),
        };
        assert!(!text(&session).contains("draft"));

        crate::progress::apply_call(
            &session.tool_ctx,
            &serde_json::json!({ "title": "Rewrite the resume path", "state": "draft",
                                 "phases": [{ "phase": "one", "status": "pending" }] }),
        )
        .unwrap();
        let drafting = text(&session);
        assert!(drafting.contains("Rewrite the resume path"), "{drafting}");
        assert!(drafting.contains("do not start executing"), "{drafting}");

        // Approval is what makes it stop appearing; nothing has to retract it.
        session
            .progress()
            .as_mut()
            .unwrap()
            .set_state(crate::progress::ProgressState::Active)
            .unwrap();
        assert!(!text(&session).contains("do not start executing"));
    }

    /// Cross-session continuation is one of the three things this design buys,
    /// and it rests entirely on the log naming the file. A finished plan must
    /// record the *absence* just as explicitly: resume reopening a completed
    /// plan is worse than resume opening nothing.
    #[test]
    fn the_log_names_the_open_plan_and_records_when_there_is_none() {
        crate::home::testing::temp_home();
        let cwd = tempfile::tempdir().unwrap();
        let mut session = Session::new(
            ToolCtx::for_test(cwd.path().to_path_buf(), 1_000),
            PermissionMode::Default,
            PermissionRules::default(),
        );
        let data = cwd.path().join("data");
        let store = crate::store::SessionStore::create(&data, cwd.path()).unwrap();
        session.ledger.attach_sink(Box::new(store));
        // Read back what a resume would: the real log, not an in-memory mirror.
        let recorded = || {
            crate::store::SessionStore::resume(&data, None)
                .expect("the log exists")
                .progress
        };

        // Nothing open, nothing to say — and repeating the check is free.
        session.record_progress_file();
        session.record_progress_file();
        assert_eq!(recorded(), None);

        crate::progress::apply_call(
            &session.tool_ctx,
            &serde_json::json!({ "title": "Ship it",
                                 "phases": [{ "phase": "one", "status": "in_progress" }] }),
        )
        .unwrap();
        session.record_progress_file();
        session.record_progress_file();
        let path = recorded().expect("a resume finds the open plan");
        assert!(path.to_string_lossy().ends_with("ship-it.md"), "{path:?}");

        // Finishing it clears the current plan, and the log says so — resume
        // must not silently reopen the last plan it ever saw.
        crate::progress::apply_call(
            &session.tool_ctx,
            &serde_json::json!({ "state": "done",
                                 "phases": [{ "phase": "one", "status": "completed" }] }),
        )
        .unwrap();
        session.record_progress_file();
        assert_eq!(recorded(), None);

        // Even handed the finished file directly, a resume opens nothing.
        assert_eq!(
            session.restore_progress(Some(&path)),
            None,
            "a finished plan is not revived"
        );
    }

    #[test]
    fn session_replacement_changes_cache_scope_before_refreshing_variables() {
        let root = std::env::temp_dir();
        let mut session = Session::new(
            ToolCtx::for_test(root, 1_000),
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
        assert!(session
            .classifier_cache_scope()
            .starts_with("auto-classifier:main:resumed-session:"));
    }

    /// Registration is the whole mechanism: a frontend that registers its
    /// cwd-derived state gets it re-derived without remembering to, and only
    /// when the directory actually moved.
    #[test]
    fn change_cwd_rederives_registered_scopes() {
        struct Seen(Mutex<Vec<PathBuf>>);
        impl CwdScoped for Seen {
            fn rescope(&self, cwd: &Path) -> Vec<String> {
                self.0.lock().unwrap().push(cwd.to_path_buf());
                Vec::new()
            }
        }

        let root = std::env::temp_dir().join(format!("tcode-cd-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("child")).unwrap();
        let root = canonicalized(root);
        let child = canonicalized(root.join("child"));

        let mut session = Session::new(
            ToolCtx::for_test(root.clone(), 1_000),
            PermissionMode::Default,
            PermissionRules::default(),
        );
        let seen = Arc::new(Seen(Mutex::new(Vec::new())));
        session.register_cwd_scope(seen.clone());

        session.change_cwd("child").unwrap();
        // Same directory again: nothing moved, so nothing is re-derived.
        session.change_cwd(".").unwrap();
        session.change_cwd("..").unwrap();

        assert_eq!(*seen.0.lock().unwrap(), [child, root.clone()]);
        let _ = std::fs::remove_dir_all(&root);
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
        let root = canonicalized(root);
        let child = canonicalized(root.join("child"));

        let mut session = Session::new(
            ToolCtx::for_test(root.clone(), 1_000),
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
        session.environment = Some(EnvironmentSnapshot {
            cwd: child.display().to_string(),
            platform: "test".into(),
            os_version: None,
            command_shells: vec!["test shell".into()],
            git: Default::default(),
            date: "1970-01-01".into(),
        });
        session.delivered_environment = session.environment.clone();
        let change = session.change_cwd("..").unwrap();
        assert!(change.changed);
        assert!(!change.refresh_opening_context);
        assert_eq!(session.tool_ctx.cwd, root);
        assert_eq!(
            session.prompt_variables().expand("${TCODE_PROJECT_DIR}"),
            frozen_project
        );
        // `change_cwd` only changes state. Environment observations persist
        // immediately, while their combined model note waits for delivery.
        assert_eq!(session.ledger.len(), 1);
        session.sync_environment(
            EnvironmentSnapshot {
                cwd: root.display().to_string(),
                platform: "test".into(),
                os_version: None,
                command_shells: vec!["test shell".into()],
                git: Default::default(),
                date: "1970-01-01".into(),
            },
            change.memory_note,
        );
        assert_eq!(
            session.ledger.len(),
            1,
            "environment is metadata until delivery"
        );
        let entries = session.take_deferred_context_entries();
        assert!(matches!(
            entries.as_slice(),
            [Entry::Note(text)]
                if text.contains("Runtime environment changed")
                    && text.contains("Future relative tool paths")
        ));
        let entries_after_sync = session.ledger.len();
        session.sync_environment(
            EnvironmentSnapshot {
                cwd: root.display().to_string(),
                platform: "test".into(),
                os_version: None,
                command_shells: vec!["test shell".into()],
                git: Default::default(),
                date: "1970-01-01".into(),
            },
            None,
        );
        assert!(session.take_deferred_context_entries().is_empty());
        assert_eq!(session.ledger.len(), entries_after_sync);

        let entries = session.ledger.len();
        let change = session.change_cwd(".").unwrap();
        assert!(!change.changed);
        assert!(!change.refresh_opening_context);
        assert_eq!(session.ledger.len(), entries);

        let _ = std::fs::remove_dir_all(&root);
    }
}
