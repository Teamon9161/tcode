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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Anthropic,
    Openai,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub provider: ProviderKind,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Model context window in tokens; drives the context status line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

impl Profile {
    /// Resolve the API key from the environment; keys never live on disk.
    pub fn api_key(&self, profile_name: &str) -> Result<String, ConfigError> {
        let var = self.api_key_env.clone().unwrap_or_else(|| {
            match self.provider {
                ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
                ProviderKind::Openai => "OPENAI_API_KEY",
            }
            .to_string()
        });
        std::env::var(&var).map_err(|_| ConfigError::MissingApiKey {
            profile: profile_name.to_string(),
            var,
        })
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

const DEFAULT_CONFIG: &str = r#"# tcode global configuration
default_profile = "anthropic"

[watchdog]
idle_timeout_secs = 30
max_retries = 3
initial_backoff_ms = 1000

[limits]
tool_output_tokens = 2000

[permissions]
mode = "default"
allow = []
deny = []

[profiles.anthropic]
provider = "anthropic"
model = "claude-sonnet-5"
api_key_env = "ANTHROPIC_API_KEY"
# base_url = "https://api.anthropic.com"

# Any OpenAI-compatible endpoint (OpenAI, DeepSeek, OpenRouter, local...).
# Adjust model/base_url to your endpoint.
[profiles.openai]
provider = "openai"
model = "gpt-5.1"
api_key_env = "OPENAI_API_KEY"
# base_url = "https://api.openai.com/v1"
"#;

impl Config {
    pub fn global_path() -> Result<PathBuf, ConfigError> {
        Ok(dirs::home_dir().ok_or(ConfigError::NoHome)?.join(".tcode"))
    }

    /// Load global config (creating a commented default on first run),
    /// then overlay the project-level `.tcode/config.toml` if present.
    pub fn load(project_dir: &Path) -> Result<Self, ConfigError> {
        let global_dir = Self::global_path()?;
        let global_file = global_dir.join("config.toml");
        if !global_file.exists() {
            std::fs::create_dir_all(&global_dir).map_err(|e| ConfigError::Io {
                path: global_dir.clone(),
                source: e,
            })?;
            std::fs::write(&global_file, DEFAULT_CONFIG).map_err(|e| ConfigError::Io {
                path: global_file.clone(),
                source: e,
            })?;
        }
        let mut config = Self::read_file(&global_file)?;
        let project_file = project_dir.join(".tcode").join("config.toml");
        if project_file.exists() {
            config.overlay(Self::read_file(&project_file)?);
        }
        Ok(config)
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
}
