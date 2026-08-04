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
    pub serve: ServeHandle,
}

/// The loopback origin for shown artifacts, or why there isn't one.
///
/// Deliberately not fatal to startup. Binding loopback essentially always
/// succeeds, but "essentially" on Windows includes security software that
/// blocks a process from listening at all — and an app that refuses to open
/// because one kind of file cannot be displayed would be trading the whole
/// tool for a feature. The failure is carried instead of dropped so the pane
/// that needs it can say what happened rather than staying blank.
pub struct ServeHandle(pub Result<Arc<crate::serve::Serve>, String>);

impl ServeHandle {
    pub fn get(&self) -> Result<&Arc<crate::serve::Serve>, String> {
        self.0.as_ref().map_err(|why| {
            format!(
                "the local viewer origin could not start ({why}), so HTML files cannot be displayed. \
Restarting tcode will try again; if it keeps failing, something on this machine is blocking loopback connections."
            )
        })
    }
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
    opening_context: tcode_core::commands::OpeningContextFn,
    environment: tcode_core::commands::EnvironmentFn,
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
            opening_context: Arc::new(tcode_tools::startup_context_with_scratch),
            environment: Arc::new(tcode_tools::environment_snapshot),
        }
    }

    pub fn command_context(
        &self,
    ) -> (
        tcode_core::commands::OpeningContextFn,
        tcode_core::commands::EnvironmentFn,
    ) {
        (self.opening_context.clone(), self.environment.clone())
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
            opening_context: self.opening_context.clone(),
            environment: self.environment.clone(),
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

    // `boot` consumes the selection; the model menu needs it to mark which
    // option is current.
    let selection_for_menu = selection.clone();
    let booted = tcode_frontend::boot(tcode_frontend::BootSpec {
        cwd: cwd.clone(),
        config: &mut config,
        selection,
        model_cell: model_cell.clone(),
        agent: None,
        // The one thing this frontend has that the terminal does not: somewhere
        // to put a rendered file. See `tcode_tools::ShowTool`.
        display_tools: vec![Arc::new(tcode_tools::ShowTool)],
    })
    .await?;

    // The same three menus `/model` and `/agents` render in the terminal, with
    // the same closures attached. Built here rather than in the command that
    // reads them: they carry the provider swap and the selected-config writer,
    // which are composition-root concerns (see `picker.rs`).
    let models = tcode_frontend::build_menu(&config, &selection_for_menu, config_file.clone());
    let menus = Arc::new(std::sync::Mutex::new(crate::picker::Pickers {
        presets: tcode_frontend::build_preset_menu(
            &config,
            &state,
            cwd.clone(),
            model_cell.clone(),
            booted.pinned.clone(),
            booted.agent_defs.clone(),
            config_file.clone(),
        ),
        // Reads the model menu to resolve each pin back to a row in it, so it is
        // built from the same list the panel will draw.
        agents: tcode_frontend::build_agent_menu(
            &config,
            &models,
            booted.pinned.clone(),
            booted.agent_defs.as_ref(),
            config_file.clone(),
        ),
        models,
        config_file: config_file.clone(),
    }));

    let factory = SessionFactory::new(config_file, model_cell, booted.shell_filters.clone());
    let session = factory.open(&cwd, None)?;

    let supervisor = Arc::new(Supervisor::new(booted.agent, factory, menus));
    // The session id is the app's handle for this conversation, independent of
    // the JSONL log id: a resumed log and a fresh one are both just "a session"
    // to the frontend.
    let handle = Arc::new(SessionHandle::new(
        uuid::Uuid::new_v4().to_string(),
        cwd,
        session,
    ));
    supervisor.open(handle.clone());

    let serve = ServeHandle(
        crate::serve::Serve::start()
            .await
            .map_err(|error| error.to_string()),
    );
    let mut warnings = booted.warnings;
    if let Err(why) = &serve.0 {
        warnings.push(format!("HTML files cannot be displayed: {why}"));
    }

    Ok(Startup {
        supervisor,
        session: handle,
        warnings,
        serve,
    })
}
