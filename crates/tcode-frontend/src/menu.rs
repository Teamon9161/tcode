//! Model / preset / agent menu data and construction — UI-independent.
//!
//! These are the structs a frontend renders and the closures it invokes to
//! apply a choice (switch the main model, pin a role, apply/save a preset).
//! The closures keep the provider swap and selected-config mutation at the
//! composition boundary, so renderers only consume their results. The TUI's
//! ratatui `Picker`/`Hub` widgets render these; the desktop app renders its
//! own UI over the same data.

use tcode_core::config::{Config, ModelDef};
use tcode_core::ActiveModel;

/// One selectable (profile, model) pair.
pub struct ModelOption {
    pub profile: String,
    pub def: ModelDef,
}

impl ModelOption {
    pub fn title(&self) -> String {
        format!("{} · {}", self.profile, self.def.display())
    }
}

/// Builds a provider for a picked option and persists the choice.
pub type SwitchFn =
    Box<dyn Fn(&ModelOption, Option<&str>) -> Result<ActiveModel, String> + Send + Sync>;

pub struct ModelMenu {
    pub options: Vec<ModelOption>,
    /// Index of the active option (for the picker's initial position).
    pub current: usize,
    pub switch: SwitchFn,
}

/// Apply a role's model mode, live and in the selected config's
/// `[tcode_state]`. `Off` is available only for opt-in capabilities such as
/// the `web-fetch` summarizer.
pub type PinFn = Box<dyn Fn(&str, AgentModelChoice) -> Result<String, String> + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentModelChoice {
    Off,
    Inherit,
    Model {
        option: usize,
        effort: Option<String>,
    },
}

/// Which part of the hub a role belongs under. The two kinds are configured
/// identically but are looked for at different moments: `Task` is "who do I
/// delegate work to", `Helper` is "what runs the machinery around my turn".
/// A single flat list of both is what made the old `/agents` list hard to scan.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RoleSection {
    Task,
    Helper,
}

pub struct AgentRole {
    pub key: String,
    pub label: String,
    pub allows_off: bool,
    pub section: RoleSection,
}

/// One named line-up, as the hub lists it.
pub struct PresetOption {
    pub key: String,
    pub label: String,
}

/// What `save as preset` captures, in menu terms so the builder can name the
/// profiles and models behind the indices.
pub struct PresetDraft {
    /// Index into `ModelMenu::options`; `None` when nothing is configured.
    pub main: Option<usize>,
    pub main_effort: Option<String>,
    pub roles: Vec<(String, AgentModelChoice)>,
}

/// The menus rebuilt after a config, provider, or credential change, plus
/// non-fatal agent-pin warnings discovered while resolving that config.
pub type MenuUpdate = (ModelMenu, AgentMenu, Vec<String>);

/// Switch to a named line-up: persist the choice, rebuild the provider and
/// every pin, and hand back the menus that describe the result.
pub type PresetUpdate = (ModelMenu, AgentMenu, String, Vec<String>);

/// Apply a named preset.
pub type ApplyPresetFn = Box<dyn Fn(&str) -> Result<PresetUpdate, String> + Send + Sync>;

/// Write the live line-up out as `[presets.<name>]` and hand back the updated
/// list plus the index of the preset now in force. The menu travels with the
/// draft because the draft is expressed in its indices: they mean nothing
/// against a menu rebuilt since.
pub type SavePresetFn = Box<
    dyn Fn(&str, &PresetDraft, &ModelMenu) -> Result<(Vec<PresetOption>, usize), String>
        + Send
        + Sync,
>;

/// The named line-ups and the two things that can be done with them.
pub struct PresetMenu {
    pub options: Vec<PresetOption>,
    /// Which one is in force; `None` = none, i.e. the line-up is whatever the
    /// config file and the ad-hoc pins say.
    pub current: Option<usize>,
    pub apply: ApplyPresetFn,
    pub save: SavePresetFn,
}

/// The two effects `/provider` needs. The selected config and concrete
/// provider construction remain hidden behind these closures, so renderers do
/// not need either concern.
pub struct ProviderSetup {
    /// The selected user config, to seed the form. Never the merged runtime
    /// config: a project overlay must not be copied into the selected file by
    /// saving.
    pub load: Box<dyn Fn() -> Result<Config, String> + Send + Sync>,
    /// Persist the result and rebuild the active provider and both menus.
    pub apply: Box<dyn Fn(Config) -> Result<MenuUpdate, String> + Send + Sync>,
    /// Rebuild menus from the config already on disk after a credential change.
    pub refresh: Box<dyn Fn() -> Result<MenuUpdate, String> + Send + Sync>,
}

/// The pinnable roles — sub-agents plus the helper roles around a turn — what
/// each currently runs on, and how to change it.
pub struct AgentMenu {
    pub roles: Vec<AgentRole>,
    pub pins: Vec<AgentModelChoice>,
    pub pin: PinFn,
}

impl AgentMenu {
    /// What a role runs on right now, for the hub's rows and the status line.
    pub fn describe(&self, index: usize, menu: &ModelMenu) -> String {
        match &self.pins[index] {
            AgentModelChoice::Off => "off".to_string(),
            AgentModelChoice::Inherit => "inherit (main model)".to_string(),
            AgentModelChoice::Model { option, effort } => {
                let name = menu
                    .options
                    .get(*option)
                    .map(|o| o.def.display().to_string())
                    .unwrap_or_else(|| "?".into());
                match effort {
                    Some(e) => format!("{name} ({e})"),
                    None => name,
                }
            }
        }
    }
}
