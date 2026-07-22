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
    #[error("failed to update {path}: {message}")]
    Update { path: PathBuf, message: String },
    #[error("unknown profile '{0}' (available: {1})")]
    UnknownProfile(String, String),
    #[error("profile '{profile}': environment variable {var} is not set")]
    MissingApiKey { profile: String, var: String },
    #[error("profile '{0}' has no models configured")]
    NoModels(String),
    #[error(
        "profile '{0}' has no provider (set `provider = \"anthropic\" | \"openai\" | \"codex\"`)"
    )]
    NoProvider(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Anthropic,
    /// `OpenAiProvider`: any OpenAI-compatible Chat Completions endpoint —
    /// OpenAI, DeepSeek, OpenRouter, local.
    Openai,
    /// ChatGPT subscription through the Codex backend (Responses API). Its
    /// credentials and runtime model catalogue are owned by the provider
    /// adapter; `alias` keeps pre-rename configs (`provider = "chatgpt"`)
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
    /// Explicitly mark text-only models so image paths can self-heal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
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
            vision: None,
        }
    }

    pub fn display(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    /// Absent in a *layer*: this file only overrides parts of a profile the
    /// layer below already defines. Every field here is optional for the same
    /// reason — a user config that only carries `api_key` must parse, and
    /// completeness is checked once, after all layers are merged
    /// (`Config::validate`), not per file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderKind>,
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
    /// Default vision capability for models in this profile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
}

impl Profile {
    /// An empty patch: overrides nothing. Layering a profile that only sets
    /// a field or two onto the catalogue is the normal case, so building one
    /// should not mean listing every field as `None`.
    pub fn patch() -> Self {
        Self {
            provider: None,
            model: None,
            models: Vec::new(),
            api_key: None,
            api_key_env: None,
            base_url: None,
            max_tokens: None,
            context_window: None,
            vision: None,
        }
    }

    /// Resolve the API key: inline value, then the configured (or
    /// provider-default) environment variable. ChatGPT profiles don't
    /// use API keys at all.
    pub fn api_key(&self, profile_name: &str) -> Result<String, ConfigError> {
        if self.provider == Some(ProviderKind::Codex) {
            return Ok(String::new());
        }
        if let Some(key) = &self.api_key {
            return Ok(key.clone());
        }
        let var = self.api_key_env.clone().unwrap_or_else(|| {
            match self.provider {
                Some(ProviderKind::Anthropic) | None => "ANTHROPIC_API_KEY",
                Some(ProviderKind::Openai) | Some(ProviderKind::Codex) => "OPENAI_API_KEY",
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
        defs
    }

    /// Overlay `over` onto self: scalar fields win when set; models merge
    /// by `name` (same name replaced, new name appended). This is how a
    /// user profile extends or overrides a same-named default profile.
    pub fn merge(&mut self, over: Profile) {
        if over.provider.is_some() {
            self.provider = over.provider;
        }
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
        if over.vision.is_some() {
            self.vision = over.vision;
        }
        for model in over.models {
            match self.models.iter_mut().find(|d| d.name == model.name) {
                Some(existing) => *existing = model,
                None => self.models.push(model),
            }
        }
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
    /// Whether the agent summarizes history before it reaches the model's
    /// configured context limit. Disable only when the provider manages
    /// compaction itself or an uninterrupted transcript is required.
    pub auto_compact: bool,
    /// Context occupancy percentage at which automatic compaction begins.
    /// Values outside 1..=100 are clamped at use time.
    pub auto_compact_percent: u8,
    /// Token budget per tool output before it is gated to the blob store.
    /// Sized to hold a normal shell command's output (a build/test failure)
    /// whole while still capping runaway logs. Locating/content tools
    /// (`read`/`grep`/`glob`/`web_search`) opt out of gating entirely.
    pub tool_output_tokens: usize,
    /// Model round-trips per user turn before the harness ends the turn
    /// (runaway guard; the user can always ask to continue).
    pub max_steps_per_turn: usize,
    /// Whether successful `shell` output passes through the declarative
    /// output filters (built-in, plus `~/.tcode/filters.toml` and
    /// `.tcode/filters.toml`). Lives under `limits` so it is settled by the
    /// user's own configuration: a checked-out repository cannot re-enable
    /// filtering the user turned off.
    pub shell_output_filters: bool,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            auto_compact: true,
            auto_compact_percent: 85,
            tool_output_tokens: 8000,
            max_steps_per_turn: crate::agent::DEFAULT_MAX_STEPS,
            shell_output_filters: true,
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
    /// Show provider reasoning summaries in the transcript. Reasoning remains
    /// in the persisted ledger for provider replay regardless of this setting.
    pub show_reasoning: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            suggest_next: false,
            show_reasoning: false,
        }
    }
}

/// Which key holds the microphone open.
///
/// There are three because no single choice survives every setup: `ctrl+space`
/// is the natural chord but input methods claim it (Microsoft Pinyin toggles
/// 中/英 with it, and the key never reaches the terminal at all); plain `space`
/// is unclaimed but can only work while the draft is empty; a function key is
/// unclaimed *and* always available, at the cost of being arbitrary. Whichever
/// is chosen, `/voice keys` shows what the terminal actually delivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceKey {
    CtrlSpace,
    Space,
    /// F1-F12. `F1` is usually help in a terminal; the rest are free.
    Function(u8),
}

impl VoiceKey {
    /// The label used in config, in hints, and in `/voice`'s own output — one
    /// spelling so what the user types is what the screen says back.
    pub fn label(self) -> String {
        match self {
            VoiceKey::CtrlSpace => "ctrl+space".into(),
            VoiceKey::Space => "space".into(),
            VoiceKey::Function(n) => format!("f{n}"),
        }
    }
}

impl std::str::FromStr for VoiceKey {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim().to_ascii_lowercase();
        match text.as_str() {
            "ctrl+space" => Ok(VoiceKey::CtrlSpace),
            "space" => Ok(VoiceKey::Space),
            _ => match text.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                Some(n @ 1..=12) => Ok(VoiceKey::Function(n)),
                _ => Err(format!(
                    "'{text}' is not a voice key: use ctrl+space, space, or f1-f12"
                )),
            },
        }
    }
}

impl Serialize for VoiceKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.label())
    }
}

impl<'de> Deserialize<'de> for VoiceKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// `[voice]`: push-to-talk dictation. The recogniser itself is an external
/// sidecar process, so everything here is either a gesture choice or a pointer
/// to where that sidecar and its model live.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    /// Starting value for `/voice`; `[tcode_state].voice` wins when set.
    pub enabled: bool,
    /// Hold this to record. `space` only triggers on an empty draft.
    pub key: VoiceKey,
    /// Which recognition model, by preset name. The sidecar owns the list and
    /// the default, so this is passed through untouched and a wrong name is
    /// answered by the sidecar with the menu — there is no second copy here to
    /// drift out of date.
    pub model: String,
    /// Words and phrases to bias recognition towards — names, library names,
    /// anything a general model has no reason to expect. Only models that
    /// support biasing use them; `/voice words` reports when the chosen one
    /// does not.
    pub hotwords: Vec<String>,
    /// `auto`, or one of `zh`, `en`, `ja`, `ko`, `yue`. Only the `sense-voice`
    /// model has a language switch; the bilingual models ignore this. Passed
    /// through to the sidecar untouched.
    pub language: String,
    /// A hold that never ends must not fill memory with audio.
    pub max_seconds: u64,
    /// Input device name; empty means the system default.
    pub device: String,
    /// Path to the sidecar executable. Empty means the standard locations
    /// (`~/.tcode/voice/`, then PATH).
    pub command: String,
    /// Where model files live. Empty means `~/.tcode/voice/models`.
    pub model_dir: String,
    /// Base URL the sidecar fetches models from, for mirrors. Empty means its
    /// own default.
    pub download_base: String,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            // Space, because the alternatives are worse in practice: input
            // methods claim ctrl+space, and a function key is something nobody
            // reaches for. Tapping it still types a space — see the provisional
            // take in `tcode-tui`'s voice module.
            key: VoiceKey::Space,
            // Empty, not a name: the sidecar's table decides which preset is
            // the default, and duplicating that choice here would mean two
            // places to change when it moves.
            model: String::new(),
            hotwords: Vec::new(),
            language: "auto".into(),
            max_seconds: 60,
            device: String::new(),
            command: String::new(),
            model_dir: String::new(),
            download_base: String::new(),
        }
    }
}

fn default_trusted_read_hosts() -> Vec<String> {
    vec!["api.github.com".into(), "raw.githubusercontent.com".into()]
}

/// Natural-language policy and narrowly-scoped read exceptions for Auto Mode.
/// This is user/global configuration only: a repository must not be able to
/// loosen the safety policy that protects a developer running it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoModeConfig {
    pub hard_deny: Vec<String>,
    pub soft_deny: Vec<String>,
    pub allow: Vec<String>,
    /// End-to-end deadline for Stage 1: the classifier must produce a usable
    /// `ALLOW` or `BLOCK`, not merely open an SSE stream.
    pub fast_timeout_secs: u64,
    /// End-to-end deadline for Stage 2, which returns a verdict and, for a
    /// block, one short explanation.
    pub reasoned_timeout_secs: u64,
    /// Retries after a failed classifier stage. This is capped to one retry so
    /// an unavailable safety model cannot hold an interactive approval hostage.
    pub retry_count: u32,
    /// Exact HTTPS hosts a tool has independently declared as anonymous,
    /// side-effect-free read targets. This is deliberately not a shell rule.
    #[serde(default = "default_trusted_read_hosts")]
    pub trusted_read_hosts: Vec<String>,
}

/// Timing and retry policy passed to each isolated Auto Mode classifier.
/// Kept separate from `WatchdogConfig`: a byte-level stream watchdog does not
/// bound time spent waiting for a parseable safety verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoClassifierConfig {
    pub fast_timeout: Duration,
    pub reasoned_timeout: Duration,
    pub retry_count: u32,
}

impl Default for AutoClassifierConfig {
    fn default() -> Self {
        Self {
            fast_timeout: Duration::from_secs(10),
            reasoned_timeout: Duration::from_secs(20),
            retry_count: 1,
        }
    }
}

impl AutoModeConfig {
    pub fn classifier_config(&self) -> AutoClassifierConfig {
        AutoClassifierConfig {
            fast_timeout: Duration::from_secs(self.fast_timeout_secs.max(1)),
            reasoned_timeout: Duration::from_secs(self.reasoned_timeout_secs.max(1)),
            retry_count: self.retry_count.min(1),
        }
    }
}

impl Default for AutoModeConfig {
    fn default() -> Self {
        let classifier = AutoClassifierConfig::default();
        Self {
            hard_deny: Vec::new(),
            soft_deny: Vec::new(),
            allow: Vec::new(),
            fast_timeout_secs: classifier.fast_timeout.as_secs(),
            reasoned_timeout_secs: classifier.reasoned_timeout.as_secs(),
            retry_count: classifier.retry_count,
            trusted_read_hosts: default_trusted_read_hosts(),
        }
    }
}

/// `[agents.<kind>]`: run an auxiliary role or sub-agent kind on a model other
/// than the one driving the conversation. For example, `compact` can use a
/// cheaper summarizer, while reconnaissance is bulk reading and summarizing
/// whose context never enters the parent's window. Unset model fields inherit
/// the parent's active selection.
/// `enabled` applies to opt-in roles such as `fetch`: `true` selects the parent
/// model, while absence leaves that role off by default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AgentConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl AgentConfig {
    /// The one-word forms a `[presets.*.agents]` or `[agents]` entry may use
    /// instead of a table. A line-up is mostly "this role runs that model",
    /// and writing eight four-key tables to say it is what makes hand-editing
    /// the file unpleasant enough to be skipped.
    pub fn from_shorthand(text: &str) -> Self {
        match text.trim() {
            "off" => Self {
                enabled: Some(false),
                ..Self::default()
            },
            "inherit" | "" => Self {
                enabled: Some(true),
                ..Self::default()
            },
            model => Self {
                model: Some(model.to_string()),
                ..Self::default()
            },
        }
    }

    /// Whether this assignment names a model at all. An entry that only says
    /// `enabled` selects the parent's model rather than one of its own.
    pub fn names_a_model(&self) -> bool {
        self.profile.is_some() || self.model.is_some() || self.effort.is_some()
    }
}

/// The shortest form that reads back as this assignment. `/model save` writes
/// into a file people hand-edit, so a captured line-up has to look like one
/// they would have written: eight four-key inline tables is technically the
/// same document and practically an unreadable one.
fn agent_value(agent: &AgentConfig) -> toml_edit::Value {
    match (
        agent.profile.as_deref(),
        agent.model.as_deref(),
        agent.effort.as_deref(),
        agent.enabled,
    ) {
        (None, None, None, Some(false)) => "off".into(),
        (None, None, None, Some(true)) => "inherit".into(),
        (None, Some(model), None, None) => model.into(),
        _ => {
            let mut table = toml_edit::InlineTable::new();
            for (key, value) in [
                ("profile", agent.profile.as_deref()),
                ("model", agent.model.as_deref()),
                ("effort", agent.effort.as_deref()),
            ] {
                if let Some(value) = value {
                    table.insert(key, value.into());
                }
            }
            if let Some(enabled) = agent.enabled {
                table.insert("enabled", enabled.into());
            }
            toml_edit::Value::InlineTable(table)
        }
    }
}

/// `[presets.<name>]` as a document table, with `agents` as its own sub-table
/// rather than one long inline map.
fn preset_table(preset: &Preset) -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    for (key, value) in [
        ("label", preset.label.as_deref()),
        ("profile", preset.profile.as_deref()),
        ("model", preset.model.as_deref()),
        ("effort", preset.effort.as_deref()),
    ] {
        if let Some(value) = value {
            table.insert(key, toml_edit::value(value));
        }
    }
    if !preset.agents.is_empty() {
        let mut agents = toml_edit::Table::new();
        for (kind, agent) in &preset.agents {
            agents.insert(kind, toml_edit::Item::Value(agent_value(agent)));
        }
        table.insert("agents", toml_edit::Item::Table(agents));
    }
    table
}

impl<'de> Deserialize<'de> for AgentConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct Table {
            profile: Option<String>,
            model: Option<String>,
            effort: Option<String>,
            enabled: Option<bool>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Short(String),
            Table(Table),
        }
        Ok(match Repr::deserialize(deserializer)? {
            Repr::Short(text) => AgentConfig::from_shorthand(&text),
            Repr::Table(table) => AgentConfig {
                profile: table.profile,
                model: table.model,
                effort: table.effort,
                enabled: table.enabled,
            },
        })
    }
}

/// A named model line-up: which model the conversation runs on, and what each
/// sub-agent and helper role runs on. `[profiles.*]` says *how to reach* a
/// provider; a preset says *which models to use* and is switched as one unit,
/// so moving from one provider family to another is one choice rather than
/// eight re-pins.
///
/// A preset is a layer *below* `[tcode_state]`: switching to one clears the
/// ad-hoc pins, so what remains in the state is exactly the tweaks made since
/// the switch. Presets themselves are declarative config — hand-written, or
/// captured from the live line-up by `/model save`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Preset {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Per-role assignments, same shape and shorthands as top-level `[agents]`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<String, AgentConfig>,
}

impl Preset {
    pub fn display<'a>(&'a self, key: &'a str) -> &'a str {
        self.label.as_deref().unwrap_or(key)
    }

    /// Preset names become TOML table keys and `/model preset <name>`
    /// arguments, so they are restricted to what both read back unambiguously.
    pub fn valid_name(name: &str) -> bool {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }
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
    pub voice: VoiceConfig,
    pub profiles: BTreeMap<String, Profile>,
    /// Named model line-ups, switched as a unit by `/model`. See `Preset`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub presets: BTreeMap<String, Preset>,
    /// Per-sub-agent model overrides, keyed by agent kind (`explore`,
    /// `general`). Absent = the sub-agent follows the parent's model,
    /// including later `/model` switches. The active preset overlays these,
    /// and `[tcode_state] agents` overlays both.
    pub agents: BTreeMap<String, AgentConfig>,
    /// External commands around tool calls, e.g. a formatter after edit:
    /// `[[hooks]] event = "post_tool_use", matcher = "edit|write",
    /// command = "cargo fmt"`.
    pub hooks: Vec<crate::hooks::HookDef>,
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    /// Runtime choices managed by tcode. This lives only in the selected
    /// user-level config file; project overlays are never allowed to supply it.
    #[serde(default, skip_serializing_if = "ModelState::is_empty")]
    pub tcode_state: ModelState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FolderTrust {
    Trusted,
    Untrusted,
}

/// Everything the program itself decides and must remember: the active
/// (profile, model, effort), the sub-agent pins, and the dogfood switch.
/// It is stored as `[tcode_state]`; runtime updates replace only that table
/// and preserve the rest of the hand-edited config document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelState {
    pub profile: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// The `[presets.<name>]` line-up in force. It supplies the main model and
    /// the role pins that the fields around it do not override; switching to
    /// one clears those overrides (`switch_preset`), so a preset always takes
    /// effect whole. A name that no longer exists is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// `/agents` picks, keyed by agent kind. Overlays the active preset and
    /// `[agents.*]` from config.toml; an entry with every field unset means
    /// "inherit", which is how the picker un-pins a kind config had pinned.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<String, AgentConfig>,
    /// `/dogfood`, so it survives a restart instead of being re-toggled.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dogfood: bool,
    /// `/suggest`. Absent = follow `[ui] suggest_next` from config.toml;
    /// the runtime toggle is what the user last chose, so it wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestions: Option<bool>,
    /// `/voice`. Absent = follow `[voice] enabled` from config.toml, same
    /// precedence as `suggestions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<bool>,
    /// `/voice key <name>`. Which key is free depends on the terminal and the
    /// input method in front of it, so it is found by trying — and a choice
    /// that has to be re-made every morning is one that gets abandoned.
    /// Absent = follow `[voice] key` from config.toml.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_key: Option<crate::config::VoiceKey>,
    /// `/voice model <name>`. Which model reads your voice best is found by
    /// trying them, and the trying is only worth doing once.
    /// Absent = follow `[voice] model` from config.toml.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_model: Option<String>,
    /// `/voice words`. Absent = follow `[voice] hotwords` from config.toml.
    /// Once edited from inside tcode the whole list lives here, seeded from
    /// config so that editing never silently drops what was written by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_words: Option<Vec<String>>,
    /// The permission mode last cycled to with Shift+Tab, so a session starts
    /// where the last one left off. `Unsafe` is deliberately never stored: a
    /// one-off flip to it must not silently arm every future session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<crate::permission::PermissionMode>,
    /// Per-canonical-folder trust decisions. This is machine-local state, never
    /// project configuration: checking out a repository cannot make it trusted.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub folder_trust: BTreeMap<String, FolderTrust>,
}

impl ModelState {
    pub fn folder_trust_for(&self, folder: &Path) -> Option<FolderTrust> {
        self.folder_trust
            .get(&folder.display().to_string())
            .copied()
    }

    pub fn set_folder_trust(&mut self, folder: &Path, trust: FolderTrust) {
        self.folder_trust
            .insert(folder.display().to_string(), trust);
    }

    pub fn is_empty(&self) -> bool {
        self.profile.is_none()
            && self.model.is_none()
            && self.effort.is_none()
            && self.preset.is_none()
            && self.agents.is_empty()
            && !self.dogfood
            && self.suggestions.is_none()
            && self.voice.is_none()
            && self.voice_key.is_none()
            && self.voice_model.is_none()
            && self.voice_words.is_none()
            && self.mode.is_none()
            && self.folder_trust.is_empty()
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

    pub fn exists_at(file: &Path) -> bool {
        file.exists()
    }

    /// The built-in provider/model catalog, parsed from the embedded
    /// `default.toml`. It is the base layer every runtime `load` starts
    /// from; the user's config only needs to add keys and overrides.
    pub fn defaults() -> Self {
        toml::from_str(include_str!("default.toml")).expect("embedded default.toml is valid")
    }

    /// Load only the selected user-level configuration — no built-in defaults,
    /// no project overlay. Setup reads and writes this, so the built-in catalog
    /// is never serialized onto the user's disk.
    pub fn load_global_at(file: &Path) -> Result<Self, ConfigError> {
        Self::read_file(file)
    }

    /// Runtime config: built-in defaults, then the selected user config, then
    /// the project-level `.tcode/config.toml`. Runtime state remains global to
    /// the selected user config and cannot be supplied by the project layer.
    pub fn load_at(file: &Path, project_dir: &Path) -> Result<Self, ConfigError> {
        let user = Self::load_global_at(file)?;
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
        for preset in project.presets.values_mut() {
            preset.agents.remove("auto");
        }
        project.auto_mode = AutoModeConfig::default();
        project.tcode_state = ModelState::default();
        project
    }

    /// Add a project-scoped allow rule without rewriting the user's hand-edited
    /// TOML. The document is read immediately before writing so a later approval
    /// merges with fields, comments and rules already on disk; the replacement
    /// itself is staged in the same directory and atomically renamed.
    pub async fn add_project_allow(
        project_dir: PathBuf,
        descriptor: String,
    ) -> Result<bool, ConfigError> {
        let error_path = project_dir.join(".tcode").join("config.toml");
        let write_dir = project_dir.clone();
        tokio::task::spawn_blocking(move || add_project_allow_blocking(&write_dir, &descriptor))
            .await
            .map_err(|error| ConfigError::Update {
                path: error_path,
                message: format!("project permission update task failed: {error}"),
            })?
    }

    /// Serialize a setup result to the selected user config file. Runtime
    /// changes never use this: they go through `update_tcode_state_checked`,
    /// which preserves the rest of the handwritten document.
    pub fn write_global_at(&self, file: &Path, header: &str) -> Result<PathBuf, ConfigError> {
        let dir = file
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir).map_err(|source| ConfigError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let body = toml::to_string_pretty(self).expect("config serializes");
        std::fs::write(file, format!("{header}{body}")).map_err(|source| ConfigError::Io {
            path: file.to_path_buf(),
            source,
        })?;
        Ok(file.to_path_buf())
    }

    /// Read the selected config, change only `[tcode_state]`, and atomically
    /// replace it. `toml_edit` retains all other keys, unknown tables, and
    /// comments owned by the user.
    pub fn update_tcode_state_checked(
        file: &Path,
        edit: impl FnOnce(&mut ModelState),
    ) -> Result<(), ConfigError> {
        use toml_edit::{DocumentMut, Item};

        let source = std::fs::read_to_string(file).map_err(|source| ConfigError::Io {
            path: file.to_path_buf(),
            source,
        })?;
        let mut state = Self::read_file(file)?.tcode_state;
        edit(&mut state);
        let mut document = source
            .parse::<DocumentMut>()
            .map_err(|error| ConfigError::Update {
                path: file.to_path_buf(),
                message: format!("invalid TOML: {error}"),
            })?;
        if state.is_empty() {
            document.remove("tcode_state");
        } else {
            let state_document =
                toml_edit::ser::to_document(&state).map_err(|error| ConfigError::Update {
                    path: file.to_path_buf(),
                    message: format!("cannot serialize tcode_state: {error}"),
                })?;
            document.insert(
                "tcode_state",
                Item::Table(state_document.as_table().clone()),
            );
        }
        write_document_atomically(file, document.to_string())
    }

    /// Best-effort updates are used for cosmetic preferences. Security-relevant
    /// callers use the checked variant so they can report a persistence error.
    pub fn update_tcode_state(file: &Path, edit: impl FnOnce(&mut ModelState)) {
        let _ = Self::update_tcode_state_checked(file, edit);
    }

    /// Move a legacy standalone state file into an otherwise state-less default
    /// config. Callers deliberately pass the legacy path only for the default
    /// config; an explicit `--config` is independent and never absorbs it.
    pub fn migrate_legacy_state_if_needed(
        config_file: &Path,
        legacy_state_file: &Path,
    ) -> Result<(), ConfigError> {
        let source = std::fs::read_to_string(config_file).map_err(|source| ConfigError::Io {
            path: config_file.to_path_buf(),
            source,
        })?;
        let document =
            source
                .parse::<toml_edit::DocumentMut>()
                .map_err(|error| ConfigError::Update {
                    path: config_file.to_path_buf(),
                    message: format!("invalid TOML: {error}"),
                })?;
        if document.get("tcode_state").is_some() || !legacy_state_file.exists() {
            return Ok(());
        }
        let legacy = std::fs::read_to_string(legacy_state_file)
            .ok()
            .and_then(|text| toml::from_str::<ModelState>(&text).ok())
            .unwrap_or_default();
        if !legacy.is_empty() {
            Self::update_tcode_state_checked(config_file, |state| *state = legacy)?;
        }
        Ok(())
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
        self.tcode_state = user.tcode_state.clone();
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
        self.presets.extend(over.presets);
        self.agents.extend(over.agents);
    }

    /// Fold the active preset and the runtime state into this config, and hand
    /// back the state the rest of startup should read.
    ///
    /// This is the single place the three layers meet, and their order is the
    /// whole point: `[agents.*]` from the config files, then the active
    /// `[presets.<name>]`, then the ad-hoc `[tcode_state] agents` picks. The
    /// main model resolves the same way — the preset supplies it only when the
    /// state names no model of its own, which is exactly the situation
    /// `switch_preset` leaves behind.
    pub fn apply_active_preset(&mut self) -> ModelState {
        let mut state = self.tcode_state.clone();
        if let Some(preset) = state
            .preset
            .as_deref()
            .and_then(|name| self.presets.get(name))
            .cloned()
        {
            if state.model.is_none() && state.profile.is_none() {
                // A bare `model = "..."` must find the profile that offers it,
                // for the same reason `[agents.*]` does: sending one vendor's
                // model id to another's endpoint fails at the first request,
                // long after the choice was made.
                state.profile = preset.profile.clone().or_else(|| {
                    let model = preset.model.as_deref()?;
                    self.profile_offering(model, self.default_profile.as_deref().unwrap_or(""))
                });
                state.model = preset.model.clone();
                state.effort = preset.effort.clone();
            }
            self.agents.extend(preset.agents);
        }
        self.agents.extend(state.agents.clone());
        state
    }

    /// Switch the line-up. Clearing the ad-hoc pins *and* the saved main model
    /// is what makes a preset apply whole: whatever was tweaked since the last
    /// switch belonged to the line-up being left, and leaving it behind would
    /// mean no preset ever fully described what is running.
    pub fn switch_preset(file: &Path, name: &str) -> Result<(), ConfigError> {
        let name = name.to_string();
        Self::update_tcode_state_checked(file, move |state| {
            state.preset = Some(name);
            state.profile = None;
            state.model = None;
            state.effort = None;
            state.agents.clear();
        })
    }

    /// Add or replace one `[presets.<name>]` table, leaving the rest of the
    /// hand-edited document alone. This is the only program write outside
    /// `[tcode_state]`, and it is additive by construction: `/model save`
    /// names a line-up the user just assembled.
    pub fn upsert_preset(file: &Path, name: &str, preset: &Preset) -> Result<(), ConfigError> {
        use toml_edit::{DocumentMut, Item, Table};

        if !Preset::valid_name(name) {
            return Err(ConfigError::Update {
                path: file.to_path_buf(),
                message: format!(
                    "'{name}' is not a usable preset name — letters, digits, '-' and '_' only"
                ),
            });
        }
        let source = std::fs::read_to_string(file).map_err(|source| ConfigError::Io {
            path: file.to_path_buf(),
            source,
        })?;
        let mut document = source
            .parse::<DocumentMut>()
            .map_err(|error| ConfigError::Update {
                path: file.to_path_buf(),
                message: format!("invalid TOML: {error}"),
            })?;
        let presets = document
            .entry("presets")
            .or_insert_with(|| {
                let mut table = Table::new();
                table.set_implicit(true);
                Item::Table(table)
            })
            .as_table_mut()
            .ok_or_else(|| ConfigError::Update {
                path: file.to_path_buf(),
                message: "[presets] exists but is not a table".to_string(),
            })?;
        presets.insert(name, Item::Table(preset_table(preset)));
        write_document_atomically(file, document.to_string())
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
        if !over.names_a_model() {
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
        let profile = self.complete_profile(&name)?;
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
        self.complete_profile(name).map(|p| (name.to_string(), p))
    }

    /// Look up a profile that is fit to be used. Completeness is checked here,
    /// on the merged result and only for the profile actually being selected:
    /// an incomplete entry the user never selects is their business, and must
    /// not keep the session from starting.
    fn complete_profile(&self, name: &str) -> Result<&Profile, ConfigError> {
        let profile = self.profiles.get(name).ok_or_else(|| {
            ConfigError::UnknownProfile(
                name.to_string(),
                self.profiles.keys().cloned().collect::<Vec<_>>().join(", "),
            )
        })?;
        match profile.provider {
            Some(_) => Ok(profile),
            None => Err(ConfigError::NoProvider(name.to_string())),
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
        // An incomplete profile is skipped rather than matched: it could not
        // be built, and silently landing in it would report the wrong problem.
        if let (None, Some(m)) = (profile_flag, model_flag) {
            for (name, p) in self.profiles.iter().filter(|(_, p)| p.provider.is_some()) {
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

/// Built-in provider profiles used by the first-run wizard. These are just
/// the entries from the embedded `default.toml` catalog with an inline API
/// key filled in, so the wizard and the runtime default layer never drift.
/// Not to be confused with `[presets.*]`, which are user-named model
/// line-ups (`Preset`).
pub mod catalog {
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

    /// ChatGPT subscription via the Codex backend: the provider adapter
    /// supplies its runtime model catalogue before selection.
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

fn write_document_atomically(path: &Path, text: String) -> Result<(), ConfigError> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary = dir.join(format!(
        ".{}.{}-{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config.toml"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::write(&temporary, text).map_err(|source| ConfigError::Io {
        path: temporary.clone(),
        source,
    })?;
    std::fs::rename(&temporary, path).map_err(|source| {
        let _ = std::fs::remove_file(&temporary);
        ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

fn add_project_allow_blocking(project_dir: &Path, descriptor: &str) -> Result<bool, ConfigError> {
    use toml_edit::{Array, DocumentMut, Item, Table, Value};

    let dir = project_dir.join(".tcode");
    let path = dir.join("config.toml");
    std::fs::create_dir_all(&dir).map_err(|source| ConfigError::Io {
        path: dir.clone(),
        source,
    })?;
    let source = if path.exists() {
        std::fs::read_to_string(&path).map_err(|source| ConfigError::Io {
            path: path.clone(),
            source,
        })?
    } else {
        String::new()
    };
    let mut document = source
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Update {
            path: path.clone(),
            message: format!("invalid TOML: {error}"),
        })?;
    if document.get("permissions").is_none() {
        document.insert("permissions", Item::Table(Table::new()));
    }
    let permissions = document
        .get_mut("permissions")
        .and_then(Item::as_table_mut)
        .ok_or_else(|| ConfigError::Update {
            path: path.clone(),
            message: "`permissions` must be a TOML table".into(),
        })?;
    if permissions.get("allow").is_none() {
        permissions.insert("allow", Item::Value(Value::Array(Array::new())));
    }
    let allow = permissions
        .get_mut("allow")
        .and_then(Item::as_array_mut)
        .ok_or_else(|| ConfigError::Update {
            path: path.clone(),
            message: "`permissions.allow` must be a TOML array".into(),
        })?;
    if allow.iter().any(|value| value.as_str() == Some(descriptor)) {
        return Ok(false);
    }
    allow.push(descriptor);

    let temporary = dir.join(format!(
        ".config.toml.{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::write(&temporary, document.to_string()).map_err(|source| ConfigError::Io {
        path: temporary.clone(),
        source,
    })?;
    std::fs::rename(&temporary, &path).map_err(|source| {
        let _ = std::fs::remove_file(&temporary);
        ConfigError::Io {
            path: path.clone(),
            source,
        }
    })?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spelling in config.toml is the spelling shown back on screen, and a
    /// typo has to name what is allowed rather than fail with a serde message
    /// about an unknown variant.
    #[test]
    fn voice_keys_round_trip_through_their_labels() {
        for (text, key) in [
            ("ctrl+space", VoiceKey::CtrlSpace),
            ("space", VoiceKey::Space),
            ("f4", VoiceKey::Function(4)),
            ("F12", VoiceKey::Function(12)),
        ] {
            let parsed: VoiceKey = text.parse().expect("parses");
            assert_eq!(parsed, key);
            assert_eq!(parsed.label(), text.to_ascii_lowercase());
        }
        let error = "ctrl+enter".parse::<VoiceKey>().expect_err("rejected");
        assert!(error.contains("f1-f12"), "{error}");
        assert!("f13".parse::<VoiceKey>().is_err());

        let config: VoiceConfig = toml::from_str("key = \"f4\"").expect("parses");
        assert_eq!(config.key, VoiceKey::Function(4));
    }

    #[tokio::test]
    async fn project_allow_update_preserves_existing_toml_and_deduplicates() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join(".tcode");
        std::fs::create_dir_all(&config_dir).unwrap();
        let file = config_dir.join("config.toml");
        std::fs::write(
            &file,
            "# keep this comment\n[permissions]\nallow = [\"edit(src/lib.rs)\"]\n\n[custom]\nvalue = 1\n",
        )
        .unwrap();

        assert!(
            Config::add_project_allow(dir.path().to_path_buf(), "run(cargo test)".into())
                .await
                .unwrap()
        );
        assert!(
            !Config::add_project_allow(dir.path().to_path_buf(), "run(cargo test)".into())
                .await
                .unwrap()
        );

        let text = std::fs::read_to_string(file).unwrap();
        assert!(text.contains("# keep this comment"));
        assert!(text.contains("edit(src/lib.rs)"));
        assert!(text.contains("run(cargo test)"));
        assert!(text.contains("[custom]"));
    }

    #[tokio::test]
    async fn project_allow_update_creates_a_new_config_file() {
        let dir = tempfile::tempdir().unwrap();

        assert!(
            Config::add_project_allow(dir.path().to_path_buf(), "web_search(*)".into())
                .await
                .unwrap()
        );

        assert_eq!(
            std::fs::read_to_string(dir.path().join(".tcode/config.toml")).unwrap(),
            "[permissions]\nallow = [\"web_search(*)\"]\n"
        );
    }

    #[test]
    fn runtime_state_updates_preserve_comments_and_unrelated_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("personal.toml");
        std::fs::write(
            &file,
            "# keep this comment\ndefault_profile = \"anthropic\"\n\n[custom]\nanswer = 42\n",
        )
        .unwrap();

        Config::update_tcode_state_checked(&file, |state| {
            state.profile = Some("work".into());
            state.dogfood = true;
        })
        .unwrap();
        Config::update_tcode_state_checked(&file, |state| {
            state.suggestions = Some(true);
        })
        .unwrap();

        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("# keep this comment"));
        assert!(text.contains("[custom]"));
        assert!(text.contains("answer = 42"));
        assert!(text.contains("[tcode_state]"));
        let config = Config::load_global_at(&file).unwrap();
        assert_eq!(config.tcode_state.profile.as_deref(), Some("work"));
        assert!(config.tcode_state.dogfood);
        assert_eq!(config.tcode_state.suggestions, Some(true));
    }

    #[test]
    fn legacy_state_migrates_only_when_selected_config_has_no_runtime_section() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let legacy = dir.path().join("state.toml");
        std::fs::write(
            &config,
            "# personal config\ndefault_profile = \"anthropic\"\n",
        )
        .unwrap();
        std::fs::write(&legacy, "profile = \"openai\"\nmodel = \"gpt-test\"\n").unwrap();

        Config::migrate_legacy_state_if_needed(&config, &legacy).unwrap();
        let migrated = Config::load_global_at(&config).unwrap();
        assert_eq!(migrated.tcode_state.profile.as_deref(), Some("openai"));
        assert_eq!(migrated.tcode_state.model.as_deref(), Some("gpt-test"));

        Config::update_tcode_state_checked(&config, |state| state.profile = Some("keep".into()))
            .unwrap();
        std::fs::write(&legacy, "profile = \"replace-me\"\n").unwrap();
        Config::migrate_legacy_state_if_needed(&config, &legacy).unwrap();
        assert_eq!(
            Config::load_global_at(&config)
                .unwrap()
                .tcode_state
                .profile
                .as_deref(),
            Some("keep")
        );
    }

    #[test]
    fn folder_trust_is_machine_local_state_and_round_trips() {
        let path = Path::new("C:/work/example");
        let mut state = ModelState::default();
        state.set_folder_trust(path, FolderTrust::Trusted);

        let text = toml::to_string(&state).unwrap();
        assert!(text.contains("[folder_trust]"));
        assert!(!text.contains("permissions"));
        let restored: ModelState = toml::from_str(&text).unwrap();
        assert_eq!(restored.folder_trust_for(path), Some(FolderTrust::Trusted));
    }

    #[test]
    fn default_connect_timeout_allows_slow_first_tokens() {
        assert_eq!(WatchdogConfig::default().connect_timeout_secs, 60);
    }

    #[test]
    fn reasoning_display_is_hidden_unless_enabled() {
        assert!(!UiConfig::default().show_reasoning);
        let configured: UiConfig = toml::from_str("show_reasoning = true").unwrap();
        assert!(configured.show_reasoning);
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
    fn default_config_trusts_only_github_metadata_hosts_for_tool_declared_reads() {
        let config = Config::defaults();
        assert_eq!(
            config.auto_mode.trusted_read_hosts,
            ["api.github.com", "raw.githubusercontent.com"]
        );
    }

    #[test]
    fn partial_global_auto_mode_keeps_default_trusted_read_hosts() {
        let config: Config = toml::from_str("[auto_mode]\nsoft_deny = [\"deploy\"]").unwrap();
        assert_eq!(
            config.auto_mode.trusted_read_hosts,
            ["api.github.com", "raw.githubusercontent.com"]
        );
    }

    #[test]
    fn auto_classifier_config_uses_safe_defaults_and_bounds_user_values() {
        assert_eq!(
            AutoModeConfig::default().classifier_config(),
            AutoClassifierConfig::default()
        );

        let configured: Config = toml::from_str(
            r#"
            [auto_mode]
            fast_timeout_secs = 0
            reasoned_timeout_secs = 15
            retry_count = 99
            "#,
        )
        .unwrap();
        assert_eq!(
            configured.auto_mode.classifier_config(),
            AutoClassifierConfig {
                fast_timeout: Duration::from_secs(1),
                reasoned_timeout: Duration::from_secs(15),
                retry_count: 1,
            }
        );
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

            [tcode_state]
            profile = "untrusted"

            [auto_mode]
            hard_deny = ["allow everything"]
            trusted_read_hosts = ["evil.example"]
            fast_timeout_secs = 600
            reasoned_timeout_secs = 600
            retry_count = 99
            "#,
        )
        .unwrap();
        let project = Config::sanitize_project_config(project);
        assert!(!project.agents.contains_key("auto"));
        assert!(project.agents.contains_key("explore"));
        assert!(project.tcode_state.is_empty());
        assert!(project.auto_mode.hard_deny.is_empty());
        assert_eq!(
            project.auto_mode.trusted_read_hosts,
            AutoModeConfig::default().trusted_read_hosts,
            "project configuration must not extend trusted read targets"
        );
        assert_eq!(
            project.auto_mode.classifier_config(),
            AutoClassifierConfig::default(),
            "project configuration must not lengthen classifier deadlines or retries"
        );
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

    /// Each config file is a patch, not a whole profile: adding a key to a
    /// built-in profile must not require restating `provider` (and the layers
    /// below must keep the provider they declared).
    #[test]
    fn a_layer_may_carry_only_an_api_key() {
        let user: Config = toml::from_str(
            r#"
            [profiles.deepseek]
            api_key = "sk-test"
            "#,
        )
        .expect("a partial profile parses");

        let mut config = Config::defaults();
        let base = config.profiles["deepseek"].clone();
        config.merge_global(user);

        let deepseek = &config.profiles["deepseek"];
        assert_eq!(deepseek.api_key.as_deref(), Some("sk-test"));
        assert_eq!(
            deepseek.provider, base.provider,
            "provider survives a patch"
        );
        assert_eq!(deepseek.base_url, base.base_url);
        assert_eq!(deepseek.models.len(), base.models.len());
        config.profile(Some("deepseek")).expect("usable");
    }

    /// A profile no layer ever completed is an error only for whoever selects
    /// it: it names the offending profile, and — the reason the check is not
    /// eager — leaving it in the file does not stop an unrelated profile from
    /// starting a session.
    #[test]
    fn an_incomplete_profile_fails_only_when_it_is_the_one_selected() {
        let mut config = Config::defaults();
        config.merge_global(
            toml::from_str(
                r#"
                default_profile = "deepseek"

                [profiles.mystery]
                api_key = "sk-test"
                "#,
            )
            .unwrap(),
        );

        assert!(matches!(
            config.profile(Some("mystery")),
            Err(ConfigError::NoProvider(name)) if name == "mystery"
        ));
        config
            .select(None, None, &ModelState::default())
            .expect("an unrelated incomplete profile must not block startup");
        // Nor may it be reached by a bare `--model` search.
        assert_eq!(
            config
                .select(None, Some("deepseek-v4-pro"), &ModelState::default())
                .unwrap()
                .profile,
            "deepseek"
        );
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

    /// The three layers in order, on one role each: a preset overrides the
    /// config file, and an ad-hoc `[tcode_state]` pick overrides the preset.
    #[test]
    fn a_preset_layers_between_the_config_file_and_the_runtime_state() {
        let mut config = Config::defaults();
        config.merge_global(
            toml::from_str(
                r#"
                [agents.explore]
                model = "deepseek-v4-flash"
                [agents.general]
                model = "deepseek-v4-flash"

                [presets.gpt]
                model = "gpt-5.6-terra"
                [presets.gpt.agents]
                explore = "gpt-5.6-luna"
                general = "gpt-5.6-luna"

                [tcode_state]
                preset = "gpt"
                [tcode_state.agents]
                general = { model = "deepseek-v4-flash" }
                "#,
            )
            .unwrap(),
        );

        let state = config.apply_active_preset();
        // The preset supplies the main model, and names the profile that
        // actually offers it rather than leaving it to the default.
        assert_eq!(state.model.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(state.profile.as_deref(), Some("openai"));
        assert_eq!(
            config.agents["explore"].model.as_deref(),
            Some("gpt-5.6-luna")
        );
        assert_eq!(
            config.agents["general"].model.as_deref(),
            Some("deepseek-v4-flash"),
            "an ad-hoc pick made since the switch outranks the preset"
        );
    }

    /// Switching is the moment the whole line-up changes, so it must not leave
    /// the previous one's tweaks behind — otherwise no preset ever fully
    /// describes what is running.
    #[test]
    fn switching_preset_drops_the_pins_belonging_to_the_one_left() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(
            &file,
            r#"
# hand-written, must survive
default_profile = "deepseek"

[presets.gpt]
model = "gpt-5.6-terra"

[tcode_state]
profile = "deepseek"
model = "deepseek-v4-flash"
[tcode_state.agents]
explore = { model = "deepseek-v4-flash" }
"#,
        )
        .unwrap();

        Config::switch_preset(&file, "gpt").unwrap();

        let written = std::fs::read_to_string(&file).unwrap();
        assert!(written.contains("hand-written, must survive"));
        let state = Config::load_global_at(&file).unwrap().tcode_state;
        assert_eq!(state.preset.as_deref(), Some("gpt"));
        assert!(state.agents.is_empty());
        assert!(state.model.is_none(), "the preset supplies the main model");
    }

    /// `/model save` writes one table and touches nothing else — it is the only
    /// program write outside `[tcode_state]`.
    #[test]
    fn saving_a_preset_adds_one_table_and_preserves_the_document() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(
            &file,
            "# keep me\n[profiles.deepseek]\napi_key = \"sk-x\"\n",
        )
        .unwrap();

        let preset = Preset {
            model: Some("gpt-5.6-terra".into()),
            agents: BTreeMap::from([("explore".to_string(), AgentConfig::from_shorthand("off"))]),
            ..Preset::default()
        };
        Config::upsert_preset(&file, "gpt", &preset).unwrap();

        let written = std::fs::read_to_string(&file).unwrap();
        assert!(written.contains("# keep me"));
        assert!(written.contains("sk-x"));
        let reloaded = Config::load_global_at(&file).unwrap();
        assert_eq!(
            reloaded.presets["gpt"].model.as_deref(),
            Some("gpt-5.6-terra")
        );
        assert_eq!(
            reloaded.presets["gpt"].agents["explore"].enabled,
            Some(false)
        );

        assert!(Config::upsert_preset(&file, "not ok", &preset).is_err());
    }

    /// The shorthand exists so a line-up reads as one line per role; it must
    /// mean exactly what the equivalent table means.
    #[test]
    fn agent_shorthand_matches_the_table_form() {
        let config: Config = toml::from_str(
            r#"
            [agents]
            explore = "deepseek-v4-flash"
            suggest = "inherit"
            fetch = "off"
            general = { model = "deepseek-v4-flash" }
            "#,
        )
        .unwrap();
        assert_eq!(config.agents["explore"], config.agents["general"]);
        assert_eq!(config.agents["suggest"].enabled, Some(true));
        assert!(!config.agents["suggest"].names_a_model());
        assert_eq!(config.agents["fetch"].enabled, Some(false));
    }

    /// A checked-out repository may pin task agents, but must not repoint the
    /// safety classifier — including through a preset.
    #[test]
    fn a_project_preset_cannot_repoint_the_classifier() {
        let project: Config = toml::from_str(
            "[presets.evil.agents]\nauto = \"some-model\"\nexplore = \"some-model\"\n",
        )
        .unwrap();
        let project = Config::sanitize_project_config(project);
        assert!(!project.presets["evil"].agents.contains_key("auto"));
        assert!(project.presets["evil"].agents.contains_key("explore"));
    }

    #[test]
    fn api_key_reflects_resolvable_credentials() {
        let defaults = Config::defaults();
        // No env var, no inline key → not usable.
        std::env::remove_var("DEEPSEEK_API_KEY");
        assert!(defaults.profiles["deepseek"].api_key("deepseek").is_err());

        let mut with_inline = defaults.profiles["deepseek"].clone();
        with_inline.api_key = Some("sk-x".into());
        assert!(with_inline.api_key("deepseek").is_ok());
    }
}
