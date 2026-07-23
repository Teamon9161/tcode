//! Shared, UI-independent frontend logic.
//!
//! Core exposes the driver contract (`Agent::user_turn`, `AgentEvent`,
//! `Approver`); this crate holds the composition-root wiring that every
//! frontend would otherwise hand-roll — opening a session with persistence
//! attached, and (later) menu/provider-setup data and a turn driver. It never
//! depends on a UI crate.

pub mod agent;
pub mod menu;
pub mod session;

pub use agent::{build_agent, AgentBuild};
pub use menu::{
    AgentMenu, AgentModelChoice, AgentRole, ApplyPresetFn, ModelMenu, ModelOption, PinFn,
    PresetDraft, PresetMenu, PresetOption, RoleSection, SavePresetFn, SwitchFn,
};
pub use session::{open_session, ResumeSpec, SessionSpec};
