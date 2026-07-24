//! Shared, UI-independent frontend logic.
//!
//! Core exposes the driver contract (`Agent::user_turn`, `AgentEvent`,
//! `Approver`); this crate holds the composition-root wiring that every
//! frontend would otherwise hand-roll — opening a session with persistence
//! attached, menu/preset/provider-setup data, and (later) a turn driver. It never
//! depends on a UI crate.

pub mod agent;
pub mod boot;
pub mod build;
pub mod menu;
pub mod session;
pub mod setup;

pub use agent::{build_agent, AgentBuild};
pub use boot::{boot, startup_mode, startup_rules, BootSpec, Booted, INTERACTIVE_AGENT_SYSTEM};
pub use build::{
    agent_models, apply_agent_def_hints, build_agent_menu, build_menu, build_preset_menu,
    build_provider_setup, rebuild_from_config, RebuiltMenus,
};
pub use menu::{
    AgentMenu, AgentModelChoice, AgentRole, ApplyPresetFn, MenuUpdate, ModelMenu, ModelOption,
    PinFn, PresetDraft, PresetMenu, PresetOption, PresetUpdate, ProviderSetup, RoleSection,
    SavePresetFn, SwitchFn,
};
pub use session::{open_session, ResumeSpec, SessionSpec};
// `Key`/`View`/`Progress` etc. stay behind `setup::` — they only read right
// next to the state machine they belong to.
pub use setup::{CodexLogin, LoginUpdate, Setup};
