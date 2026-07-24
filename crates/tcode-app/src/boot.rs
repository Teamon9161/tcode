//! The desktop app's composition root.
//!
//! Deliberately short: config loading, agent assembly and session opening are
//! all `tcode-frontend`'s, and the terminal binary reaches them the same way.
//! What is left here is what the app alone decides — which folder to open, and
//! that a missing provider is an error rather than a wizard, since there is no
//! terminal to draw one in.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;

use tcode_core::config::Config;
use tcode_core::{ModelCell, Session};
use tcode_tools::ShellFilters;

use crate::state::{SessionHandle, Supervisor};

/// The agent, one open session, and anything that degraded on the way up.
pub struct Startup {
    pub supervisor: Arc<Supervisor>,
    pub session: Arc<SessionHandle>,
    pub warnings: Vec<String>,
}

/// Opens further folders as sessions, after boot.
///
/// The launchpad can open any folder, and each one is its own conversation
/// with its own `ToolCtx` — but they all share the one `Arc<Agent>`, which is
/// stateless. What this holds is the small amount of per-app context that
/// `tcode_frontend::open_session` needs and that the agent does not carry.
///
/// Configuration is re-read per folder rather than reused, because
/// `.tcode/config.toml` is project-level: opening a second project must pick up
/// *its* hooks, permission rules and MCP servers, not the first one's.
pub struct SessionFactory {
    config_file: PathBuf,
    model_cell: ModelCell,
    shell_filters: Arc<ShellFilters>,
}

impl SessionFactory {
    /// `config_file` is the personal config selected at startup; it is re-read
    /// per folder so project-level overrides apply to the folder being opened.
    pub fn new(
        config_file: PathBuf,
        model_cell: ModelCell,
        shell_filters: Arc<ShellFilters>,
    ) -> Self {
        Self {
            config_file,
            model_cell,
            shell_filters,
        }
    }

    /// Open `cwd` as a conversation. `resume` names an existing session log to
    /// replay (by id prefix); `None` starts a fresh one.
    pub fn open(&self, cwd: &Path, resume: Option<String>) -> anyhow::Result<Session> {
        anyhow::ensure!(cwd.is_dir(), "{} is not a folder", cwd.display());
        let mut config = Config::load_at(&self.config_file, cwd)?;
        let state = config.apply_active_preset();
        tcode_frontend::open_session(tcode_frontend::SessionSpec {
            cwd: cwd.to_path_buf(),
            config: &config,
            state: &state,
            model_cell: self.model_cell.clone(),
            mode: tcode_frontend::startup_mode(None, &state, &config)?,
            rules: tcode_frontend::startup_rules(&config),
            resume: match resume {
                Some(id) => tcode_frontend::ResumeSpec::Resume { id: Some(id) },
                None => tcode_frontend::ResumeSpec::New,
            },
            shell_filters: self.shell_filters.clone(),
            opening_context: Arc::new(tcode_tools::startup_context_with_scratch),
            environment: Arc::new(tcode_tools::environment_snapshot),
        })
    }
}

/// Build the agent and open `cwd` as the first session.
pub async fn start(cwd: PathBuf) -> anyhow::Result<Startup> {
    let config_file = Config::global_file()?;
    anyhow::ensure!(
        Config::exists_at(&config_file),
        "no configuration at {} — run `tcode` in a terminal once to set up a provider",
        config_file.display()
    );

    let mut config = Config::load_at(&config_file, &cwd)?;
    tcode_providers::hydrate_codex_models(&mut config);
    let state = config.apply_active_preset();
    let selection = config.select(None, None, &state)?;
    let profile = config
        .profiles
        .get(&selection.profile)
        .context("selected profile disappeared")?;
    let active = tcode_providers::build_active(profile, &selection, &config.watchdog)?;
    let model_cell = ModelCell::new(active);

    let booted = tcode_frontend::boot(tcode_frontend::BootSpec {
        cwd: cwd.clone(),
        config: &mut config,
        selection,
        model_cell: model_cell.clone(),
        agent: None,
    })
    .await?;

    let factory = SessionFactory::new(config_file, model_cell, booted.shell_filters.clone());
    let session = factory.open(&cwd, None)?;

    let supervisor = Arc::new(Supervisor::new(booted.agent, factory));
    // The session id is the app's handle for this conversation, independent of
    // the JSONL log id: a resumed log and a fresh one are both just "a session"
    // to the frontend.
    let handle = Arc::new(SessionHandle::new(
        uuid::Uuid::new_v4().to_string(),
        cwd,
        session,
    ));
    supervisor.open(handle.clone());

    Ok(Startup {
        supervisor,
        session: handle,
        warnings: booted.warnings,
    })
}
