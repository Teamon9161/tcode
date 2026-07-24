//! Open a live `Session` with persistence attached.
//!
//! This is the exact wiring the composition root used to inline (session
//! creation, folder-trust/suggestion/dogfood seeding, and JSONL log
//! create/resume). Every frontend needs it per conversation; the desktop app's
//! supervisor calls it once per open folder, so it lives here rather than in
//! any one binary.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;

use tcode_core::commands::{EnvironmentFn, OpeningContextFn};
use tcode_core::config::{Config, ModelState};
use tcode_core::{
    CheckpointStore, CwdScoped, ModelCell, PermissionMode, PermissionRules, Resumed, Session,
    SessionStore, ToolCtx,
};

/// How a session's ledger is seeded.
pub enum ResumeSpec {
    /// A fresh conversation with a new JSONL log.
    New,
    /// Replay the most recent (or `id`-prefixed) session log for this project.
    Resume { id: Option<String> },
}

/// Everything a frontend decides before a session exists. The composition root
/// fills this in once; `open_session` turns it into a live `Session` with
/// persistence attached.
pub struct SessionSpec<'a> {
    pub cwd: PathBuf,
    pub config: &'a Config,
    pub state: &'a ModelState,
    /// The main model handle; the session's `ToolCtx` reads it for token math.
    pub model_cell: ModelCell,
    pub mode: PermissionMode,
    pub rules: PermissionRules,
    pub resume: ResumeSpec,
    /// The `/cd`-aware shell filter chain the tools already hold; registering it
    /// is what makes `/cd` re-read the new directory's `.tcode/filters.toml`.
    pub shell_filters: Arc<dyn CwdScoped>,
    pub opening_context: OpeningContextFn,
    pub environment: EnvironmentFn,
}

/// Build a session, seed runtime toggles from `[tcode_state]`, and attach the
/// JSONL persistence sink (creating or resuming per `spec.resume`). Returns a
/// session ready for its first `Agent::user_turn`.
pub fn open_session(spec: SessionSpec<'_>) -> anyhow::Result<Session> {
    let SessionSpec {
        cwd,
        config,
        state,
        model_cell,
        mode,
        rules,
        resume,
        shell_filters,
        opening_context,
        environment,
    } = spec;

    let mut session = Session::new(
        ToolCtx::new(cwd.clone(), config.limits.tool_output_tokens).with_model(model_cell),
        mode,
        rules,
    );
    session.set_dogfood(state.dogfood);
    session.register_cwd_scope(shell_filters);
    if let Some(trust) = state.folder_trust_for(&cwd) {
        session.set_folder_trust(trust);
    }
    // `/suggest` last, else the config default: what the user last chose beats
    // what the file says.
    session.set_suggestions(state.suggestions.unwrap_or(config.ui.suggest_next));

    // Persistence: every ledger mutation is recorded to a JSONL session log;
    // resume replays it.
    let Some(data_dir) = tcode_core::store::project_data_dir(&cwd) else {
        session.set_startup_context((opening_context)(&cwd, &session.tool_ctx.scratch_dir));
        return Ok(session);
    };
    // Before this run's log exists, so the empty log we are about to create is
    // not mistaken for one of the abandoned ones it collects.
    tcode_core::store::sweep_old_sessions(&data_dir);
    match resume {
        ResumeSpec::Resume { id } => {
            let resumed =
                SessionStore::resume(&data_dir, id.as_deref()).context("cannot resume session")?;
            let Resumed {
                store,
                ledger,
                checkpoints,
                startup,
                environment: previous_environment,
                delivered_environment,
            } = resumed;
            let session_id = store.id.clone();
            let ckpt_dir = data_dir.join("checkpoints").join(&session_id);
            session.checkpoints = CheckpointStore::load(ckpt_dir, checkpoints);
            session.ledger = ledger;
            session.ledger.attach_sink(Box::new(store));
            session.bind_scratch_session(&session_id);
            let startup =
                startup.unwrap_or_else(|| (opening_context)(&cwd, &session.tool_ctx.scratch_dir));
            session.restore_startup_context(startup, previous_environment, delivered_environment);
            session.sync_environment((environment)(&cwd), None);
        }
        ResumeSpec::New => {
            let store =
                SessionStore::create(&data_dir, &cwd).context("cannot create session log")?;
            let session_id = store.id.clone();
            session.checkpoints =
                CheckpointStore::new(data_dir.join("checkpoints").join(&session_id));
            session.ledger.attach_sink(Box::new(store));
            session.bind_scratch_session(&session_id);
            session.set_startup_context((opening_context)(&cwd, &session.tool_ctx.scratch_dir));
        }
    }
    Ok(session)
}
