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
        presets: crate::PresetMenu {
            options: Vec::new(),
            current: None,
            apply: Box::new(|_| Err("no presets in tests".into())),
            save: Box::new(|_, _, _| Err("no presets in tests".into())),
        },
        provider_setup: crate::ProviderSetup {
            // Never the real ~/.tcode/config.toml: tests must neither depend
            // on this machine's providers nor be able to overwrite them.
            load: Box::new(|| Ok(tcode_core::config::Config::default())),
            apply: Box::new(|_| Err("no provider setup in tests".into())),
            refresh: Box::new(|| Err("no provider setup in tests".into())),
        },
        // Tests never reach the network; a login test scripts this itself.
        codex_login: crate::CodexLogin(Arc::new(|_| Box::pin(async {}))),
        state_store: crate::StateStore::new(
            || Ok(tcode_core::config::ModelState::default()),
            |_| Ok(()),
        ),
        opening_context: Arc::new(|cwd: &Path, _| tcode_core::StartupContext {
            text: String::new(),
            environment: environment(cwd),
        }),
        environment: Arc::new(environment),
        show_reasoning: false,
        skills: Vec::new(),
        voice: Default::default(),
        // Tests never reach the network. A test that wants the install path
        // exercised scripts this itself.
        voice_install: crate::VoiceInstall(Arc::new(|_, _, _| Err("no downloads in tests".into()))),
    }
}

/// An app painting into a `width`x`height` buffer, rooted at `cwd`.
pub(super) fn app(cwd: &Path, width: u16, height: u16) -> App {
    app_with(cwd, width, height, config())
}

/// Same, with the sidecar download replaced. It is the one part of voice that
/// runs off the UI thread, so a test has to be able to script how it ends.
pub(super) fn app_with_voice_install(
    cwd: &Path,
    width: u16,
    height: u16,
    voice_install: crate::VoiceInstall,
) -> App {
    app_with(
        cwd,
        width,
        height,
        crate::TuiConfig {
            voice_install,
            ..config()
        },
    )
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
    let mut session = Session::new(
        ToolCtx::for_test(cwd.to_path_buf(), 2000),
        PermissionMode::Default,
        PermissionRules::default(),
    );
    // Otherwise every app starts behind the folder-trust dialog, which owns
    // the keyboard — no test here is about that decision.
    session.set_folder_trust(tcode_core::FolderTrust::Trusted);
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

    pub(super) fn press_with(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.on_term_event(Event::Key(KeyEvent::new(code, modifiers)));
    }

    /// Let a key go. Terminals that report this at all report whatever
    /// modifiers are *still* held, which is why the release carries its own
    /// modifier set.
    pub(super) fn release(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.on_term_event(Event::Key(KeyEvent::new_with_kind(
            code,
            modifiers,
            crossterm::event::KeyEventKind::Release,
        )));
    }

    /// Replace the voice backend with one the test scripts. Returns the log of
    /// commands the app sends it.
    pub(super) fn fake_voice(
        &mut self,
        key: tcode_core::config::VoiceKey,
    ) -> Arc<std::sync::Mutex<Vec<crate::voice::VoiceCmd>>> {
        use crate::voice::{BackendFactory, Voice, VoiceBackend, VoiceCmd};

        struct Fake(Arc<std::sync::Mutex<Vec<VoiceCmd>>>);
        impl VoiceBackend for Fake {
            fn send(&mut self, cmd: VoiceCmd) -> Result<(), String> {
                self.0.lock().expect("lock").push(cmd);
                Ok(())
            }
        }

        let log: Arc<std::sync::Mutex<Vec<VoiceCmd>>> = Arc::default();
        let handle = log.clone();
        let factory: BackendFactory = Box::new(move |_, _| Ok(Box::new(Fake(handle.clone()))));
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let cfg = tcode_core::config::VoiceConfig {
            key,
            ..Default::default()
        };
        self.voice = Voice::new(cfg, tx, factory);
        self.voice.use_injected_backend();
        self.voice.set_end_detect(crate::voice::EndDetect::Release);
        self.voice_rx = rx;
        // Straight past the config file: `set_voice` would persist to the real
        // [tcode_state] in the selected config on the machine running the tests.
        self.voice.turn_on().expect("the fake backend starts");
        self.on_voice_event(crate::voice::VoiceEvent::Ready);
        log
    }
}
