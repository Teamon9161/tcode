//! Shared, UI-independent frontend logic.
//!
//! Core exposes the driver contract (`Agent::user_turn`, `AgentEvent`,
//! `Approver`); this crate holds the composition-root wiring that every
//! frontend would otherwise hand-roll — opening a session with persistence
//! attached, menu/preset/provider-setup data, and (later) a turn driver. It never
//! depends on a UI crate.

pub mod agent;
pub mod build;
pub mod menu;
pub mod session;

pub use agent::{build_agent, AgentBuild};
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
