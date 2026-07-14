use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot locate home directory")]
    NoHome,
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    #[error("unknown profile '{0}' (available: {1})")]
    UnknownProfile(String, String),
    #[error("profile '{profile}': environment variable {var} is not set")]
    MissingApiKey { profile: String, var: String },
    #[error("profile '{0}' has no models configured")]
    NoModels(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Anthropic,
    /// `OpenAiProvider`: any OpenAI-compatible Chat Completions endpoint —
    /// OpenAI, DeepSeek, OpenRouter, local.
    Openai,
    /// ChatGPT subscription through the Codex backend (Responses API,
    /// `CodexProvider`). Credentials come from `~/.codex/auth.json`, not an
    /// API key. `alias` keeps pre-rename configs (`provider = "chatgpt"`)
    /// loading.
    #[serde(alias = "chatgpt")]
    Codex,
}

/// One selectable model within a profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDef {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Reasoning effort levels the model accepts, lowest → highest.
    /// Empty = effort not adjustable. The selector always offers "auto"
    /// (send nothing) in addition to these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
}

impl ModelDef {
    pub fn bare(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            label: None,
            context_window: None,
            max_tokens: None,
            efforts: Vec::new(),
            default_effort: None,
        }
    }

    pub fn display(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub provider: ProviderKind,
    /// Single-model shorthand; merged after `models` entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelDef>,
    /// Inline key. Prefer `api_key_env` if you don't want keys on disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Fallback context window for models that don't set their own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

impl Profile {
    /// Resolve the API key: inline value, then the configured (or
    /// provider-default) environment variable. ChatGPT profiles don't
    /// use API keys at all.
    pub fn api_key(&self, profile_name: &str) -> Result<String, ConfigError> {
        if self.provider == ProviderKind::Codex {
            return Ok(String::new());
        }
        if let Some(key) = &self.api_key {
            return Ok(key.clone());
        }
        let var = self.api_key_env.clone().unwrap_or_else(|| {
            match self.provider {
                ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
                ProviderKind::Openai | ProviderKind::Codex => "OPENAI_API_KEY",
            }
            .to_string()
        });
        std::env::var(&var).map_err(|_| ConfigError::MissingApiKey {
            profile: profile_name.to_string(),
            var,
        })
    }

    /// All selectable models: `models` entries, plus the `model`
    /// shorthand, plus (for ChatGPT profiles with nothing configured)
    /// the list Codex has cached locally.
    pub fn model_defs(&self) -> Vec<ModelDef> {
        let mut defs = self.models.clone();
        if let Some(name) = &self.model {
            if !defs.iter().any(|d| &d.name == name) {
                defs.push(ModelDef::bare(name.clone()));
            }
        }
        if defs.is_empty() && self.provider == ProviderKind::Codex {
            defs = crate::codex::cached_models();
        }
        defs
    }

    /// Overlay `over` onto self: scalar fields win when set; models merge
    /// by `name` (same name replaced, new name appended). This is how a
    /// user profile extends or overrides a same-named default profile.
    fn merge(&mut self, over: Profile) {
        self.provider = over.provider;
        if over.model.is_some() {
            self.model = over.model;
        }
        if over.api_key.is_some() {
            self.api_key = over.api_key;
        }
        if over.api_key_env.is_some() {
            self.api_key_env = over.api_key_env;
        }
        if over.base_url.is_some() {
            self.base_url = over.base_url;
        }
        if over.max_tokens.is_some() {
            self.max_tokens = over.max_tokens;
        }
        if over.context_window.is_some() {
            self.context_window = over.context_window;
        }
        for model in over.models {
            match self.models.iter_mut().find(|d| d.name == model.name) {
                Some(existing) => *existing = model,
                None => self.models.push(model),
            }
        }
    }

    /// Whether credentials for this profile resolve right now. Used to hide
    /// unconfigured built-in profiles from the `/model` picker so the
    /// always-present default catalog doesn't clutter it.
    pub fn is_usable(&self, name: &str) -> bool {
        if self.provider == ProviderKind::Codex {
            return crate::codex::auth_available();
        }
        self.api_key(name).is_ok()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchdogConfig {
    pub idle_timeout_secs: u64,
    /// Cap on the wait for a connection's response headers. `connect_timeout`
    /// on the HTTP client only bounds TCP setup; this bounds the "time to
    /// first byte" so a server that accepts but never replies is retried
    /// instead of hanging for minutes.
    ///
    /// It must stay well above the slowest *legitimate* first byte, not just
    /// above a healthy one: many gateways flush no headers until the model's
    /// first token, so a reasoning model chewing on a large prompt can take
    /// tens of seconds to answer at all. Cutting that off does not rescue a
    /// stuck request — it kills a live one, throws away the prompt processing
    /// the server already billed, and retries into the same wait.
    pub connect_timeout_secs: u64,
    pub max_retries: u32,
    pub initial_backoff_ms: u64,
    /// Ceiling for the exponential backoff so a long outage doesn't wait
    /// minutes between attempts.
    pub max_backoff_ms: u64,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: 30,
            connect_timeout_secs: 60,
            max_retries: 5,
            initial_backoff_ms: 1000,
            max_backoff_ms: 30_000,
        }
    }
}

impl WatchdogConfig {
    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.idle_timeout_secs)
    }
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(self.connect_timeout_secs)
    }
    pub fn initial_backoff(&self) -> Duration {
        Duration::from_millis(self.initial_backoff_ms)
    }
    /// Exponential backoff before the Nth retry (1-based): initial · 2^(n-1),
    /// capped at `max_backoff_ms`. Short at first, then progressively longer.
    pub fn backoff(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(20);
        let ms = self
            .initial_backoff_ms
            .saturating_mul(1u64 << shift)
            .min(self.max_backoff_ms);
        Duration::from_millis(ms)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitsConfig {
    /// Token budget per tool output before it is gated to the blob store.
    /// Sized to hold a normal shell command's output (a build/test failure)
    /// whole while still capping runaway logs. Locating/content tools
    /// (`read`/`grep`/`glob`/`web_search`) opt out of gating entirely.
    pub tool_output_tokens: usize,
    /// Model round-trips per user turn before the harness ends the turn
    /// (runaway guard; the user can always ask to continue).
    pub max_steps_per_turn: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            tool_output_tokens: 8000,
            max_steps_per_turn: crate::agent::DEFAULT_MAX_STEPS,
        }
    }
}

/// One MCP server over stdio: `[mcp_servers.name]` with a command to
/// spawn. Its tools register as `mcp__name__tool`, which is also the
/// descriptor permission rules match against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionsConfig {
    pub mode: crate::permission::PermissionMode,
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
}

/// `[ui]`: frontend behaviour that costs tokens, so it must be refusable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// Offer a greyed-out guess at the next instruction when the turn ends
    /// (→ accepts it). It rides the turn's cached prefix, so it is cheap — but
    /// it is still one extra request per turn, and not everyone wants that.
    pub suggest_next: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self { suggest_next: true }
    }
}

/// Natural-language policy supplied to the independent Auto Mode classifier.
/// This is user/global configuration only: a repository must not be able to
/// loosen the safety policy that protects a developer running it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoModeConfig {
    pub hard_deny: Vec<String>,
    pub soft_deny: Vec<String>,
    pub allow: Vec<String>,
}

/// `[agents.explore]`: run a sub-agent kind on a model other than the one
/// driving the conversation. Reconnaissance is bulk reading and summarizing —
/// a cheap, fast model does it well, and its context never enters the parent's
/// window anyway. Unset fields inherit the parent's active selection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub profile: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub default_profile: Option<String>,
    pub watchdog: WatchdogConfig,
    pub limits: LimitsConfig,
    pub permissions: PermissionsConfig,
    pub auto_mode: AutoModeConfig,
    pub ui: UiConfig,
    pub profiles: BTreeMap<String, Profile>,
    /// Per-sub-agent model overrides, keyed by agent kind (`explore`,
    /// `general`). Absent = the sub-agent follows the parent's model,
    /// including later `/model` switches.
    pub agents: BTreeMap<String, AgentConfig>,
    /// External commands around tool calls, e.g. a formatter after edit:
    /// `[[hooks]] event = "post_tool_use", matcher = "edit|write",
    /// command = "cargo fmt"`.
    pub hooks: Vec<crate::hooks::HookDef>,
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
}

/// Everything the program itself decides and must remember: the active
/// (profile, model, effort), the sub-agent pins, and the dogfood switch.
/// It is written here precisely so the hand-edited config.toml is never
/// rewritten by the program.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelState {
    pub profile: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// `/agents` picks, keyed by agent kind. Overlays `[agents.*]` from
    /// config.toml; an entry with every field unset means "inherit", which is
    /// how the picker un-pins a kind the config file had pinned.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<String, AgentConfig>,
    /// `/dogfood`, so it survives a restart instead of being re-toggled.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dogfood: bool,
    /// `/suggestions`. Absent = follow `[ui] suggest_next` from config.toml;
    /// the runtime toggle is what the user last chose, so it wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestions: Option<bool>,
    /// The permission mode last cycled to with Shift+Tab, so a session starts
    /// where the last one left off. `Unsafe` is deliberately never stored: a
    /// one-off flip to it must not silently arm every future session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<crate::permission::PermissionMode>,
}

impl ModelState {
    /// Read the file, apply `edit`, write it back. Callers that persist one
    /// field must not clobber the others (the picker's model choice and the
    /// dogfood switch are written from different places).
    pub fn update(edit: impl FnOnce(&mut Self)) {
        let mut state = Self::load();
        edit(&mut state);
        state.save();
    }
}

impl ModelState {
    fn path() -> Option<PathBuf> {
        Config::global_path().ok().map(|d| d.join("state.toml"))
    }

    /// Missing or corrupt state is not an error; it just means defaults.
    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = toml::to_string_pretty(self) {
            let _ = std::fs::write(path, text);
        }
    }
}

/// A fully resolved model choice, ready to build a provider from.
#[derive(Debug, Clone)]
pub struct Selection {
    pub profile: String,
    pub model: ModelDef,
    pub effort: Option<String>,
}

impl Config {
    pub fn global_path() -> Result<PathBuf, ConfigError> {
        Ok(dirs::home_dir().ok_or(ConfigError::NoHome)?.join(".tcode"))
    }

    pub fn global_file() -> Result<PathBuf, ConfigError> {
        Ok(Self::global_path()?.join("config.toml"))
    }

    pub fn exists() -> bool {
        Self::global_file().map(|p| p.exists()).unwrap_or(false)
    }

    /// The built-in provider/model catalog, parsed from the embedded
    /// `default.toml`. It is the base layer every runtime `load` starts
    /// from; the user's config only needs to add keys and overrides.
    pub fn defaults() -> Self {
        toml::from_str(include_str!("default.toml")).expect("embedded default.toml is valid")
    }

    /// Load only the hand-written user-level configuration — no built-in
    /// defaults, no project overlay. Setup reads and writes this, so the
    /// built-in catalog is never serialized onto the user's disk.
    pub fn load_global() -> Result<Self, ConfigError> {
        let global_file = Self::global_file()?;
        Self::read_file(&global_file)
    }

    /// Runtime config: built-in defaults, then the user's global config,
    /// then the project-level `.tcode/config.toml`. Errors if no global
    /// config exists — first-run setup (the wizard or `Config::write_global`)
    /// must run before this.
    pub fn load(project_dir: &Path) -> Result<Self, ConfigError> {
        let user = Self::load_global()?;
        let mut config = Self::defaults();
        config.merge_global(user);
        let project_file = project_dir.join(".tcode").join("config.toml");
        if project_file.exists() {
            config.overlay(Self::sanitize_project_config(Self::read_file(
                &project_file,
            )?));
        }
        Ok(config)
    }

    /// A checked-out repository may customize tool permissions, hooks, MCP and
    /// task-agent pins, but must never choose its own safety classifier or
    /// natural-language classifier policy.
    fn sanitize_project_config(mut project: Config) -> Config {
        project.agents.remove("auto");
        project.auto_mode = AutoModeConfig::default();
        project
    }

    /// Serialize to the global config file (used by the setup wizard).
    pub fn write_global(&self, header: &str) -> Result<PathBuf, ConfigError> {
        let dir = Self::global_path()?;
        std::fs::create_dir_all(&dir).map_err(|e| ConfigError::Io {
            path: dir.clone(),
            source: e,
        })?;
        let file = dir.join("config.toml");
        let body = toml::to_string_pretty(self).expect("config serializes");
        std::fs::write(&file, format!("{header}{body}")).map_err(|e| ConfigError::Io {
            path: file.clone(),
            source: e,
        })?;
        Ok(file)
    }

    fn read_file(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        toml::from_str(&text).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            source: Box::new(e),
        })
    }

    /// Overlay the user's global config onto the built-in defaults. The
    /// user file is authoritative for watchdog/limits/permission mode;
    /// profiles merge by key (see `Profile::merge`).
    fn merge_global(&mut self, user: Config) {
        self.watchdog = user.watchdog.clone();
        self.limits = user.limits.clone();
        self.permissions.mode = user.permissions.mode;
        self.auto_mode = user.auto_mode.clone();
        self.overlay(user);
    }

    /// Overlay project (or user) config: `default_profile` wins when set,
    /// profiles merge by key, permission/hook/MCP lists concatenate.
    /// Unlike `merge_global` this leaves watchdog/limits/mode untouched.
    fn overlay(&mut self, over: Config) {
        if over.default_profile.is_some() {
            self.default_profile = over.default_profile;
        }
        for (name, profile) in over.profiles {
            match self.profiles.get_mut(&name) {
                Some(existing) => existing.merge(profile),
                None => {
                    self.profiles.insert(name, profile);
                }
            }
        }
        self.permissions.allow.extend(over.permissions.allow);
        self.permissions.ask.extend(over.permissions.ask);
        self.permissions.deny.extend(over.permissions.deny);
        self.hooks.extend(over.hooks);
        self.mcp_servers.extend(over.mcp_servers);
        self.agents.extend(over.agents);
    }

    /// The model a sub-agent kind runs on. `None` = no override configured;
    /// the caller keeps sharing the parent's model handle (and its `/model`
    /// switches). Fields left unset in `[agents.<kind>]` inherit from `parent`.
    pub fn agent_selection(
        &self,
        kind: &str,
        parent: &Selection,
    ) -> Option<Result<Selection, ConfigError>> {
        let over = self.agents.get(kind)?;
        if over.profile.is_none() && over.model.is_none() && over.effort.is_none() {
            return None;
        }
        Some(self.resolve_agent(over, parent))
    }

    /// The profile that offers `model`, preferring `parent` when it does. This
    /// is what makes a bare `model = "..."` pin work: without it, naming a
    /// model that lives in another profile would keep the parent's profile and
    /// send, say, a DeepSeek model id to a ChatGPT endpoint — an error the user
    /// would only meet at the first sub-agent call. `--model` without
    /// `--profile` resolves the same way.
    fn profile_offering(&self, model: &str, parent: &str) -> Option<String> {
        let offers = |name: &str| {
            self.profiles
                .get(name)
                .is_some_and(|p| p.model_defs().iter().any(|d| d.name == model))
        };
        if offers(parent) {
            return Some(parent.to_string());
        }
        self.profiles
            .keys()
            .find(|name| offers(name))
            .map(String::from)
    }

    fn resolve_agent(
        &self,
        over: &AgentConfig,
        parent: &Selection,
    ) -> Result<Selection, ConfigError> {
        let name = match (&over.profile, &over.model) {
            (Some(profile), _) => profile.clone(),
            // An uncatalogued model name stays in the parent's profile and is
            // passed through verbatim (the endpoint may know models we do not).
            (None, Some(model)) => self
                .profile_offering(model, &parent.profile)
                .unwrap_or_else(|| parent.profile.clone()),
            (None, None) => parent.profile.clone(),
        };
        let profile = self.profiles.get(&name).ok_or_else(|| {
            ConfigError::UnknownProfile(
                name.clone(),
                self.profiles.keys().cloned().collect::<Vec<_>>().join(", "),
            )
        })?;
        let defs = profile.model_defs();
        let model = match &over.model {
            // An unknown name is passed through verbatim, as `select` does:
            // the endpoint may know models we have not catalogued.
            Some(wanted) => defs
                .iter()
                .find(|d| &d.name == wanted)
                .cloned()
                .unwrap_or_else(|| ModelDef::bare(wanted.clone())),
            // Same profile as the parent and no model named: keep the
            // parent's model and only the effort changes.
            None if name == parent.profile => parent.model.clone(),
            None => defs
                .first()
                .cloned()
                .ok_or_else(|| ConfigError::NoModels(name.clone()))?,
        };
        let effort = over
            .effort
            .clone()
            .or_else(|| model.default_effort.clone())
            // Carrying the parent's effort across a model switch would be a
            // guess; only an unchanged model keeps it.
            .or_else(|| {
                (model.name == parent.model.name)
                    .then(|| parent.effort.clone())
                    .flatten()
            });
        Ok(Selection {
            profile: name,
            model,
            effort,
        })
    }

    pub fn profile(&self, name: Option<&str>) -> Result<(String, &Profile), ConfigError> {
        let name = name
            .or(self.default_profile.as_deref())
            .or_else(|| self.profiles.keys().next().map(|s| s.as_str()))
            .unwrap_or("anthropic");
        match self.profiles.get(name) {
            Some(p) => Ok((name.to_string(), p)),
            None => Err(ConfigError::UnknownProfile(
                name.to_string(),
                self.profiles.keys().cloned().collect::<Vec<_>>().join(", "),
            )),
        }
    }

    /// Resolve the active model. Priority: CLI flags > saved state >
    /// config defaults. A stale state file (profile or model no longer
    /// configured) silently falls back.
    pub fn select(
        &self,
        profile_flag: Option<&str>,
        model_flag: Option<&str>,
        state: &ModelState,
    ) -> Result<Selection, ConfigError> {
        // --model without --profile searches every profile for the name.
        if let (None, Some(m)) = (profile_flag, model_flag) {
            for (name, p) in &self.profiles {
                if let Some(def) = p.model_defs().into_iter().find(|d| d.name == m) {
                    return Ok(self.finish(name.clone(), def, None, state));
                }
            }
        }

        let state_usable = profile_flag.is_none()
            && model_flag.is_none()
            && state
                .profile
                .as_deref()
                .is_some_and(|p| self.profiles.contains_key(p));
        let (name, profile) = if state_usable {
            self.profile(state.profile.as_deref())?
        } else {
            self.profile(profile_flag)?
        };

        let defs = profile.model_defs();
        let wanted = model_flag.or(if state_usable {
            state.model.as_deref()
        } else {
            None
        });
        let def = match wanted {
            Some(m) => defs
                .iter()
                .find(|d| d.name == m)
                .cloned()
                // An unknown --model name is passed through verbatim: the
                // endpoint may well know models we haven't configured.
                .unwrap_or_else(|| ModelDef::bare(m)),
            None => defs
                .first()
                .cloned()
                .ok_or_else(|| ConfigError::NoModels(name.clone()))?,
        };
        Ok(self.finish(name, def, model_flag, state))
    }

    fn finish(
        &self,
        profile: String,
        model: ModelDef,
        model_flag: Option<&str>,
        state: &ModelState,
    ) -> Selection {
        // Saved effort applies only if it's valid for the chosen model
        // and no CLI override changed the model out from under it.
        let effort = state
            .effort
            .as_deref()
            .filter(|_| model_flag.is_none())
            .filter(|e| model.efforts.iter().any(|x| x == e))
            .map(String::from)
            .or_else(|| model.default_effort.clone());
        Selection {
            profile,
            model,
            effort,
        }
    }
}

/// Built-in profile presets used by the first-run wizard. These are just
/// the entries from the embedded `default.toml` catalog with an inline API
/// key filled in, so the wizard and the runtime default layer never drift.
pub mod presets {
    use super::{Config, Profile};

    fn from_catalog(id: &str) -> Profile {
        Config::defaults()
            .profiles
            .remove(id)
            .unwrap_or_else(|| panic!("default.toml is missing the '{id}' profile"))
    }

    fn with_key(id: &str, api_key: Option<String>) -> Profile {
        let mut profile = from_catalog(id);
        if api_key.is_some() {
            profile.api_key = api_key;
        }
        profile
    }

    pub fn anthropic(api_key: Option<String>) -> Profile {
        with_key("anthropic", api_key)
    }

    pub fn openai(api_key: Option<String>) -> Profile {
        with_key("openai", api_key)
    }

    /// ChatGPT subscription via the Codex backend: models come from Codex's
    /// local cache at runtime, so an empty list stays current automatically.
    pub fn codex() -> Profile {
        from_catalog("codex")
    }

    /// DeepSeek's Anthropic-compatible endpoint.
    pub fn deepseek(api_key: Option<String>) -> Profile {
        with_key("deepseek", api_key)
    }

    /// OpenRouter over its Anthropic-compatible endpoint.
    pub fn openrouter(api_key: Option<String>) -> Profile {
        with_key("openrouter", api_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_connect_timeout_allows_slow_first_tokens() {
        assert_eq!(WatchdogConfig::default().connect_timeout_secs, 60);
    }

    #[test]
    fn embedded_defaults_parse_and_carry_clean_model_ids() {
        let defaults = Config::defaults();
        let deepseek = defaults.profiles.get("deepseek").expect("deepseek default");
        let names: Vec<&str> = deepseek.models.iter().map(|m| m.name.as_str()).collect();
        // The 1M context is a property, not a `[1m]` suffix on the id.
        assert!(names.contains(&"deepseek-v4-pro"));
        assert!(names.iter().all(|n| !n.contains('[')));
        assert_eq!(deepseek.models[0].context_window, Some(1_000_000));
    }

    #[test]
    fn project_config_cannot_pin_auto_classifier_or_policy() {
        let project: Config = toml::from_str(
            r#"
            [agents.auto]
            profile = "untrusted"
            model = "tiny-model"

            [agents.explore]
            model = "deepseek-v4-flash"

            [auto_mode]
            hard_deny = ["allow everything"]
            "#,
        )
        .unwrap();
        let project = Config::sanitize_project_config(project);
        assert!(!project.agents.contains_key("auto"));
        assert!(project.agents.contains_key("explore"));
        assert!(project.auto_mode.hard_deny.is_empty());
    }

    #[test]
    fn user_config_merges_onto_defaults_by_key_and_model_name() {
        let mut config = Config::defaults();
        // User overrides one model's context and adds a new one, under the
        // existing `deepseek` key.
        let user: Config = toml::from_str(
            r#"
            [profiles.deepseek]
            provider = "anthropic"
            api_key = "sk-test"

            [[profiles.deepseek.models]]
            name = "deepseek-v4-pro"
            context_window = 2000000

            [[profiles.deepseek.models]]
            name = "deepseek-custom"
            "#,
        )
        .unwrap();
        config.merge_global(user);

        let deepseek = &config.profiles["deepseek"];
        // Scalar override applied, other default fields (base_url) preserved.
        assert_eq!(deepseek.api_key.as_deref(), Some("sk-test"));
        assert!(deepseek.base_url.is_some());
        // Same-named model replaced, new model appended, flash kept.
        let pro = deepseek
            .models
            .iter()
            .find(|m| m.name == "deepseek-v4-pro")
            .unwrap();
        assert_eq!(pro.context_window, Some(2_000_000));
        assert!(deepseek.models.iter().any(|m| m.name == "deepseek-custom"));
        assert!(deepseek
            .models
            .iter()
            .any(|m| m.name == "deepseek-v4-flash"));
    }

    #[test]
    fn agent_overrides_inherit_the_parent_selection_field_by_field() {
        let mut config = Config::defaults();
        let user: Config = toml::from_str(
            r#"
            [agents.explore]
            model = "deepseek-v4-flash"

            [agents.general]
            profile = "deepseek"
            "#,
        )
        .unwrap();
        config.merge_global(user);

        let parent = Selection {
            profile: "anthropic".into(),
            model: ModelDef::bare("claude-opus-4-8"),
            effort: Some("high".into()),
        };

        // Only `model` set: resolves to the profile that actually offers it,
        // not the parent's (which would send this id to the wrong endpoint).
        // The parent's effort is dropped — it was another model's dial.
        let explore = config.agent_selection("explore", &parent).unwrap().unwrap();
        assert_eq!(explore.profile, "deepseek");
        assert_eq!(explore.model.name, "deepseek-v4-flash");
        assert_eq!(explore.effort, None);

        // Only `profile` set: takes that profile's first model.
        let general = config.agent_selection("general", &parent).unwrap().unwrap();
        assert_eq!(general.profile, "deepseek");
        assert_eq!(
            general.model.name,
            config.profiles["deepseek"].models[0].name
        );

        // Nothing configured for a kind: no override, the sub-agent shares the
        // parent's live model handle.
        assert!(config.agent_selection("nobody", &parent).is_none());
    }

    #[test]
    fn agent_pin_of_an_uncatalogued_model_stays_in_the_parent_profile() {
        let mut config = Config::defaults();
        let user: Config =
            toml::from_str("[agents.explore]\nmodel = \"some-unreleased-model\"\n").unwrap();
        config.merge_global(user);
        let parent = Selection {
            profile: "openai".into(),
            model: ModelDef::bare("gpt-5.4"),
            effort: None,
        };

        let explore = config.agent_selection("explore", &parent).unwrap().unwrap();
        assert_eq!(explore.profile, "openai");
        assert_eq!(explore.model.name, "some-unreleased-model");
    }

    #[test]
    fn agent_override_with_an_unknown_profile_is_an_error_not_a_panic() {
        let mut config = Config::defaults();
        let user: Config = toml::from_str("[agents.explore]\nprofile = \"typo\"\n").unwrap();
        config.merge_global(user);
        let parent = Selection {
            profile: "anthropic".into(),
            model: ModelDef::bare("claude-opus-4-8"),
            effort: None,
        };
        assert!(matches!(
            config.agent_selection("explore", &parent),
            Some(Err(ConfigError::UnknownProfile(name, _))) if name == "typo"
        ));
    }

    #[test]
    fn is_usable_reflects_resolvable_credentials() {
        let defaults = Config::defaults();
        // No env var, no inline key → not usable.
        std::env::remove_var("DEEPSEEK_API_KEY");
        assert!(!defaults.profiles["deepseek"].is_usable("deepseek"));

        let mut with_inline = defaults.profiles["deepseek"].clone();
        with_inline.api_key = Some("sk-x".into());
        assert!(with_inline.is_usable("deepseek"));
    }
}
