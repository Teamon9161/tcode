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
    Openai,
    /// ChatGPT subscription through the Codex backend (Responses API).
    /// Credentials come from `~/.codex/auth.json`, not an API key.
    Chatgpt,
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
        if self.provider == ProviderKind::Chatgpt {
            return Ok(String::new());
        }
        if let Some(key) = &self.api_key {
            return Ok(key.clone());
        }
        let var = self.api_key_env.clone().unwrap_or_else(|| {
            match self.provider {
                ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
                ProviderKind::Openai | ProviderKind::Chatgpt => "OPENAI_API_KEY",
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
        if defs.is_empty() && self.provider == ProviderKind::Chatgpt {
            defs = crate::codex::cached_models();
        }
        defs
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchdogConfig {
    pub idle_timeout_secs: u64,
    pub max_retries: u32,
    pub initial_backoff_ms: u64,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: 30,
            max_retries: 3,
            initial_backoff_ms: 1000,
        }
    }
}

impl WatchdogConfig {
    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.idle_timeout_secs)
    }
    pub fn initial_backoff(&self) -> Duration {
        Duration::from_millis(self.initial_backoff_ms)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitsConfig {
    /// Token budget per tool output before it is gated to the blob store.
    pub tool_output_tokens: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            tool_output_tokens: 2000,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionsConfig {
    pub mode: crate::permission::PermissionMode,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub default_profile: Option<String>,
    pub watchdog: WatchdogConfig,
    pub limits: LimitsConfig,
    pub permissions: PermissionsConfig,
    pub profiles: BTreeMap<String, Profile>,
    /// External commands around tool calls, e.g. a formatter after edit:
    /// `[[hooks]] event = "post_tool_use", matcher = "edit|write",
    /// command = "cargo fmt"`.
    pub hooks: Vec<crate::hooks::HookDef>,
}

/// The active (profile, model, effort) choice. `/model` writes it here so
/// the hand-edited config.toml is never rewritten by the program.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelState {
    pub profile: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
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

    /// Load global config, then overlay the project-level
    /// `.tcode/config.toml` if present. Errors if no global config
    /// exists — first-run setup (the wizard or `Config::write_global`)
    /// must run before this.
    pub fn load(project_dir: &Path) -> Result<Self, ConfigError> {
        let global_file = Self::global_file()?;
        let mut config = Self::read_file(&global_file)?;
        let project_file = project_dir.join(".tcode").join("config.toml");
        if project_file.exists() {
            config.overlay(Self::read_file(&project_file)?);
        }
        Ok(config)
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

    /// Project-level values win; profiles merge by name; permission
    /// rule lists concatenate (both levels apply).
    fn overlay(&mut self, project: Config) {
        if project.default_profile.is_some() {
            self.default_profile = project.default_profile;
        }
        self.profiles.extend(project.profiles);
        self.permissions.allow.extend(project.permissions.allow);
        self.permissions.deny.extend(project.permissions.deny);
        self.hooks.extend(project.hooks);
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
                self.profiles
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
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

/// Built-in profile presets used by the first-run wizard (and handy as
/// documentation of known-good endpoints).
pub mod presets {
    use super::{ModelDef, Profile, ProviderKind};

    fn model(
        name: &str,
        label: &str,
        ctx: u64,
        efforts: &[&str],
        default_effort: Option<&str>,
    ) -> ModelDef {
        ModelDef {
            name: name.into(),
            label: Some(label.into()),
            context_window: Some(ctx),
            max_tokens: None,
            efforts: efforts.iter().map(|s| s.to_string()).collect(),
            default_effort: default_effort.map(String::from),
        }
    }

    pub fn anthropic(api_key: Option<String>) -> Profile {
        Profile {
            provider: ProviderKind::Anthropic,
            model: None,
            models: vec![
                model(
                    "claude-sonnet-5",
                    "Claude Sonnet 5",
                    200_000,
                    &["off", "low", "medium", "high"],
                    None,
                ),
                model(
                    "claude-opus-4-8",
                    "Claude Opus 4.8",
                    200_000,
                    &["off", "low", "medium", "high"],
                    None,
                ),
                model(
                    "claude-haiku-4-5-20251001",
                    "Claude Haiku 4.5",
                    200_000,
                    &["off", "low", "medium", "high"],
                    None,
                ),
            ],
            api_key,
            api_key_env: Some("ANTHROPIC_API_KEY".into()),
            base_url: None,
            max_tokens: None,
            context_window: None,
        }
    }

    pub fn openai(api_key: Option<String>) -> Profile {
        Profile {
            provider: ProviderKind::Openai,
            model: None,
            models: vec![
                model(
                    "gpt-5.4",
                    "GPT-5.4",
                    272_000,
                    &["low", "medium", "high", "xhigh"],
                    Some("medium"),
                ),
                model(
                    "gpt-5.4-mini",
                    "GPT-5.4 mini",
                    272_000,
                    &["low", "medium", "high", "xhigh"],
                    Some("medium"),
                ),
            ],
            api_key,
            api_key_env: Some("OPENAI_API_KEY".into()),
            base_url: None,
            max_tokens: None,
            context_window: None,
        }
    }

    /// ChatGPT subscription: models come from Codex's local cache at
    /// runtime, so an empty list stays current automatically.
    pub fn chatgpt() -> Profile {
        Profile {
            provider: ProviderKind::Chatgpt,
            model: None,
            models: Vec::new(),
            api_key: None,
            api_key_env: None,
            base_url: None,
            max_tokens: None,
            context_window: None,
        }
    }

    /// DeepSeek's Anthropic-compatible endpoint. The `[1m]` suffix
    /// selects the 1M-context variants. Thinking is ON by default
    /// server-side; "off" disables it.
    pub fn deepseek(api_key: Option<String>) -> Profile {
        Profile {
            provider: ProviderKind::Anthropic,
            model: None,
            models: vec![
                model(
                    "deepseek-v4-pro[1m]",
                    "DeepSeek V4 Pro (1M)",
                    1_000_000,
                    &["off", "low", "medium", "high"],
                    None,
                ),
                model(
                    "deepseek-v4-flash[1m]",
                    "DeepSeek V4 Flash (1M)",
                    1_000_000,
                    &["off", "low", "medium", "high"],
                    None,
                ),
            ],
            api_key,
            api_key_env: Some("DEEPSEEK_API_KEY".into()),
            base_url: Some("https://api.deepseek.com/anthropic".into()),
            max_tokens: None,
            context_window: None,
        }
    }
}
