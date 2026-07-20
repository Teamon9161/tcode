//! Building an `App` that paints into a buffer instead of a terminal.
//!
//! The point is that a test drives the *real* app: the same `on_term_event`
//! keys a user presses, the same `on_agent_event` the agent loop sends, the
//! same `redraw`. Only the leaf writes differ (see `crate::surface`). Nothing
//! here may reimplement app behaviour — a harness that paraphrases the thing it
//! tests proves nothing.
//!
//! No provider is ever called: these tests feed `AgentEvent`s directly, so the
//! stub below exists only to satisfy `Agent`'s shape.

use std::path::Path;
use std::sync::Arc;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use tcode_core::config::WatchdogConfig;
use tcode_core::{
    ActiveModel, Agent, AgentModels, CacheStrategy, EventStream, ModelCell, PermissionMode,
    PermissionRules, Provider, ProviderError, Request, Session, ToolCtx,
};
use tokio_util::sync::CancellationToken;

use super::App;
use crate::model_picker::{AgentMenu, ModelMenu};
use crate::surface::Surface;

/// Never reached: the tests drive `AgentEvent`s rather than turns.
struct StubProvider;

#[async_trait::async_trait]
impl Provider for StubProvider {
    fn name(&self) -> &str {
        "stub"
    }
    fn model(&self) -> &str {
        "stub"
    }
    fn cache_strategy(&self) -> CacheStrategy {
        CacheStrategy::ImplicitPrefix
    }
    async fn stream(
        &self,
        _req: Request,
        _cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        unreachable!("harness tests never run a turn")
    }
}

fn environment(cwd: &Path) -> tcode_core::EnvironmentSnapshot {
    tcode_core::EnvironmentSnapshot {
        cwd: cwd.display().to_string(),
        platform: "test".into(),
        os_version: None,
        command_shells: Vec::new(),
        git: Default::default(),
        date: "2026-01-01".into(),
    }
}

fn config() -> crate::TuiConfig {
    crate::TuiConfig {
        menu: ModelMenu {
            options: Vec::new(),
            current: 0,
            switch: Box::new(|_, _| Err("no switching in tests".into())),
        },
        agents: AgentMenu {
            roles: Vec::new(),
            pins: Vec::new(),
            pin: Box::new(|_, _| Err("no pinning in tests".into())),
        },
        provider_setup: crate::ProviderSetup {
            // Never the real ~/.tcode/config.toml: tests must neither depend
            // on this machine's providers nor be able to overwrite them.
            load: Box::new(|| Ok(tcode_core::config::Config::default())),
            apply: Box::new(|_, _| Err("no provider setup in tests".into())),
        },
        opening_context: Arc::new(|cwd: &Path, _| tcode_core::StartupContext {
            text: String::new(),
            environment: environment(cwd),
        }),
        environment: Arc::new(environment),
        show_reasoning: false,
        skills: Vec::new(),
    }
}

/// An app painting into a `width`x`height` buffer, rooted at `cwd`.
pub(super) fn app(cwd: &Path, width: u16, height: u16) -> App {
    app_with(cwd, width, height, config())
}

/// Same, with the `/provider` effects replaced so a test can observe what
/// setup produced without any of it reaching the disk.
pub(super) fn app_with_provider_setup(
    cwd: &Path,
    width: u16,
    height: u16,
    provider_setup: crate::ProviderSetup,
) -> App {
    app_with(
        cwd,
        width,
        height,
        crate::TuiConfig {
            provider_setup,
            ..config()
        },
    )
}

fn app_with(cwd: &Path, width: u16, height: u16, config: crate::TuiConfig) -> App {
    let agent = Agent {
        model: ModelCell::new(ActiveModel {
            provider: Arc::new(StubProvider),
            max_tokens: 1024,
            context_window: 200_000,
            effort: None,
        }),
        models: AgentModels::default(),
        tools: tcode_tools::builtin_tools(cwd),
        system: "test".into(),
        watchdog: WatchdogConfig::default(),
        hooks: Default::default(),
        safety_classifier: None,
        auto_policy: String::new(),
        max_steps: tcode_core::DEFAULT_MAX_STEPS,
        auto_compact: true,
        auto_compact_percent: 85,
    };
    let session = Session::new(
        ToolCtx::new(cwd.to_path_buf(), 2000),
        PermissionMode::Default,
        PermissionRules::default(),
    );
    App::on_surface(
        Arc::new(agent),
        session,
        config,
        Surface::Test(ratatui::backend::TestBackend::new(width, height)),
    )
    .expect("a test surface always builds")
}

impl App {
    /// Paint a frame and read it back — the "screenshot".
    pub(super) fn frame(&mut self) -> String {
        self.redraw().expect("painting a buffer cannot fail");
        self.terminal.backend().text()
    }

    /// Press a key, exactly as the terminal would deliver it.
    pub(super) fn press(&mut self, code: KeyCode) {
        self.on_term_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }
}
