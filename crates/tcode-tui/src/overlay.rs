//! The one modal that can own the bottom panel.
//!
//! Pickers and the approval dialog are mutually exclusive by construction:
//! only one of them can hold keyboard focus, render into the panel, or place
//! the terminal cursor. Modelling that as one `Option<Overlay>` rather than
//! seven parallel `Option<Picker>` fields is what keeps `app.rs` from growing
//! a "check every picker" clause at each consumer — historically four such
//! clauses existed and none of them listed all seven.
//!
//! Adding a modal means adding a variant here; the compiler then points at
//! every match that has to account for it.

use crossterm::event::KeyEvent;
use ratatui::text::Line;
use tcode_core::{Approval, PermissionMode};
use tcode_importers::{ExternalSessionInfo, ExternalSource};
use tokio::sync::oneshot;

use crate::approval::{Dialog, DialogResult};
use crate::model_picker::{self, AgentMenu, AgentModelChoice, ModelMenu};
use crate::view_picker::{self, ViewId};
use crate::{folder_trust_picker, mode_picker, resume};

/// Shared state the overlays need but do not own. Passed in per event so the
/// overlay never has to borrow `App`.
pub struct OverlayCtx<'a> {
    pub menu: &'a ModelMenu,
    pub agents: &'a AgentMenu,
    pub width: u16,
    pub height: u16,
}

/// Work an overlay asks the app to perform. Every variant is something only
/// `App` can do (touch the session, swap the model, start a turn).
pub enum OverlayAction {
    OpenView(ViewId),
    ResumeSession(String),
    ShowImportSources,
    OpenExternalSource(ExternalSource),
    ImportExternal(ExternalSessionInfo),
    ApplyModel {
        index: usize,
        effort: Option<String>,
    },
    SetMode(PermissionMode),
    FolderTrust(folder_trust_picker::Choice),
    ApplyAgentModel {
        kind: String,
        choice: AgentModelChoice,
    },
    /// Suspends the terminal, so it cannot run inside the dialog's own key
    /// handler. The dialog stays open and takes the revision afterwards.
    EditPlan,
    /// Only `Overlay::Approval` produces this, and the reply channel it needs
    /// lives in that variant — see `App::on_overlay_flow`.
    Approved(Approval),
}

/// What the app should do with the overlay after it handled an event.
pub enum Flow {
    /// The overlay keeps focus.
    Stay,
    /// Dismiss the overlay; nothing else happens.
    Close,
    /// Dismiss the overlay and run this action.
    Act(OverlayAction),
    /// Run this action; the overlay stays open and is mutated in place.
    ActInPlace(OverlayAction),
}

pub enum Overlay {
    View(view_picker::Picker),
    Resume(resume::Picker),
    Model(model_picker::Picker),
    Mode(mode_picker::Picker),
    FolderTrust(folder_trust_picker::Picker),
    Agent(model_picker::AgentPicker),
    Approval(Box<Dialog>, oneshot::Sender<Approval>),
}

impl Overlay {
    pub fn approval(dialog: Dialog, reply: oneshot::Sender<Approval>) -> Self {
        Overlay::Approval(Box::new(dialog), reply)
    }

    /// The approval dialog is the only overlay with editable text, so it is
    /// the only one that can place a caret for the OS IME.
    pub fn cursor_cell(&self) -> Option<(u16, u16)> {
        match self {
            Overlay::Approval(dialog, _) => dialog.cursor_cell(),
            _ => None,
        }
    }

    pub fn as_dialog_mut(&mut self) -> Option<&mut Dialog> {
        match self {
            Overlay::Approval(dialog, _) => Some(dialog),
            _ => None,
        }
    }

    pub fn as_dialog(&self) -> Option<&Dialog> {
        match self {
            Overlay::Approval(dialog, _) => Some(dialog),
            _ => None,
        }
    }

    /// Whether this overlay handles mouse input itself. The resume picker is
    /// keyboard-only, so the wheel keeps reaching the transcript behind it.
    pub fn owns_mouse(&self) -> bool {
        !matches!(self, Overlay::Resume(_))
    }

    /// Bracketed paste. The approval dialog consumes the text; the pickers
    /// swallow it so a multiline paste cannot leak into the hidden editor and
    /// make the panel jump when the picker closes.
    pub fn paste_text(&mut self, text: String) {
        if let Overlay::Approval(dialog, _) = self {
            dialog.paste_text(text);
        }
    }

    pub fn render(&self, ctx: &OverlayCtx) -> Vec<Line<'static>> {
        match self {
            Overlay::View(picker) => picker.render(),
            Overlay::Resume(picker) => picker.render(),
            Overlay::Model(picker) => picker.render(ctx.menu),
            Overlay::Mode(picker) => picker.render(),
            Overlay::FolderTrust(picker) => picker.render(),
            Overlay::Agent(picker) => picker.render(ctx.menu, ctx.agents),
            Overlay::Approval(dialog, _) => dialog.render(ctx.width, ctx.height.saturating_sub(6)),
        }
    }

    pub fn set_hovered_row(&mut self, row: Option<usize>, ctx: &OverlayCtx) {
        match self {
            Overlay::View(picker) => picker.set_hovered_row(row),
            Overlay::Model(picker) => picker.set_hovered_row(row),
            Overlay::Mode(picker) => picker.set_hovered_row(row),
            Overlay::FolderTrust(picker) => picker.set_hovered_row(row),
            Overlay::Agent(picker) => picker.set_hovered_row(row, ctx.agents),
            Overlay::Resume(_) | Overlay::Approval(..) => {}
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, ctx: &OverlayCtx) -> Flow {
        match self {
            Overlay::View(picker) => match picker.handle_key(key) {
                view_picker::PickResult::Pending => Flow::Stay,
                view_picker::PickResult::Cancelled => Flow::Close,
                view_picker::PickResult::Picked(id) => Flow::Act(OverlayAction::OpenView(id)),
            },
            Overlay::Resume(picker) => match picker.handle_key(key) {
                resume::PickResult::Pending => Flow::Stay,
                resume::PickResult::Cancelled => Flow::Close,
                resume::PickResult::Import => Flow::Act(OverlayAction::ShowImportSources),
                resume::PickResult::Current(id) => Flow::Act(OverlayAction::ResumeSession(id)),
                resume::PickResult::Source(source) => {
                    Flow::Act(OverlayAction::OpenExternalSource(source))
                }
                resume::PickResult::External(external) => {
                    Flow::Act(OverlayAction::ImportExternal(external))
                }
            },
            Overlay::Model(picker) => model_flow(picker.handle_key(key)),
            Overlay::Mode(picker) => match picker.handle_key(key) {
                mode_picker::PickResult::Pending => Flow::Stay,
                mode_picker::PickResult::Cancelled => Flow::Close,
                mode_picker::PickResult::Picked(mode) => Flow::Act(OverlayAction::SetMode(mode)),
            },
            Overlay::FolderTrust(picker) => match picker.handle_key(key) {
                folder_trust_picker::PickResult::Pending => Flow::Stay,
                // Dismissing the trust prompt is a decision, not a no-op: an
                // unanswered folder stays untrusted for this session.
                folder_trust_picker::PickResult::Cancelled => Flow::Act(
                    OverlayAction::FolderTrust(folder_trust_picker::Choice::RejectSession),
                ),
                folder_trust_picker::PickResult::Picked(choice) => {
                    Flow::Act(OverlayAction::FolderTrust(choice))
                }
            },
            Overlay::Agent(picker) => agent_flow(picker.handle_key(key, ctx.menu, ctx.agents)),
            Overlay::Approval(dialog, _) => match dialog.handle_key(key) {
                DialogResult::Pending => Flow::Stay,
                DialogResult::EditPlan => Flow::ActInPlace(OverlayAction::EditPlan),
                DialogResult::Done(approval) => Flow::Act(OverlayAction::Approved(approval)),
            },
        }
    }

    /// A click on a panel content row. The approval dialog has richer mouse
    /// behaviour (plan panes, note carets, drags) and is driven separately.
    pub fn handle_mouse_row(&mut self, row: usize, ctx: &OverlayCtx) -> Flow {
        match self {
            Overlay::View(picker) => match picker.handle_mouse_row(row) {
                view_picker::PickResult::Picked(id) => Flow::Act(OverlayAction::OpenView(id)),
                _ => Flow::Stay,
            },
            Overlay::Model(picker) => match model_flow(picker.handle_mouse_row(row)) {
                // A click on the inherit row of `/model` picks nothing.
                Flow::Close => Flow::Stay,
                flow => flow,
            },
            Overlay::Mode(picker) => match picker.handle_mouse_row(row) {
                mode_picker::PickResult::Picked(mode) => Flow::Act(OverlayAction::SetMode(mode)),
                _ => Flow::Stay,
            },
            Overlay::FolderTrust(picker) => match picker.handle_mouse_row(row) {
                folder_trust_picker::PickResult::Picked(choice) => {
                    Flow::Act(OverlayAction::FolderTrust(choice))
                }
                _ => Flow::Stay,
            },
            Overlay::Agent(picker) => {
                match agent_flow(picker.handle_mouse_row(row, ctx.menu, ctx.agents)) {
                    Flow::Close => Flow::Stay,
                    flow => flow,
                }
            }
            Overlay::Resume(_) | Overlay::Approval(..) => Flow::Stay,
        }
    }

    /// A click outside the panel. Dismisses the picker the way Esc would,
    /// except where dismissal needs an explicit decision.
    pub fn on_click_away(&self) -> Flow {
        match self {
            Overlay::View(_) | Overlay::Model(_) | Overlay::Mode(_) | Overlay::Agent(_) => {
                Flow::Close
            }
            // The trust prompt and an approval must be answered, not dodged.
            Overlay::FolderTrust(_) | Overlay::Approval(..) | Overlay::Resume(_) => Flow::Stay,
        }
    }
}

fn model_flow(result: model_picker::PickResult) -> Flow {
    match result {
        model_picker::PickResult::Pending => Flow::Stay,
        model_picker::PickResult::Cancelled => Flow::Close,
        // `/model` has no inherit row, so a pick without an option only comes
        // from the agent picker's shared rendering path.
        model_picker::PickResult::Picked {
            option: None,
            effort: _,
        } => Flow::Close,
        model_picker::PickResult::Picked {
            option: Some(index),
            effort,
        } => Flow::Act(OverlayAction::ApplyModel { index, effort }),
    }
}

fn agent_flow(pick: model_picker::AgentPick) -> Flow {
    match pick {
        model_picker::AgentPick::Pending => Flow::Stay,
        model_picker::AgentPick::Cancelled => Flow::Close,
        model_picker::AgentPick::Picked { kind, choice } => {
            Flow::Act(OverlayAction::ApplyAgentModel { kind, choice })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    use std::path::Path;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// The menus carry callbacks into `App`, so tests build empty ones rather
    /// than deriving `Default` on a struct that holds behaviour.
    fn menus() -> (ModelMenu, AgentMenu) {
        (
            ModelMenu {
                options: Vec::new(),
                current: 0,
                switch: Box::new(|_, _| Err("no models in test".to_string())),
            },
            AgentMenu {
                roles: Vec::new(),
                pins: Vec::new(),
                pin: Box::new(|_, _| Err("no pins in test".to_string())),
            },
        )
    }

    fn ctx<'a>(menu: &'a ModelMenu, agents: &'a AgentMenu) -> OverlayCtx<'a> {
        OverlayCtx {
            menu,
            agents,
            width: 80,
            height: 24,
        }
    }

    fn mode_overlay() -> Overlay {
        Overlay::Mode(mode_picker::Picker::new(PermissionMode::Default))
    }

    fn trust_overlay() -> Overlay {
        Overlay::FolderTrust(folder_trust_picker::Picker::new(Path::new("/tmp")))
    }

    #[test]
    fn escape_dismisses_a_picker_without_acting() {
        let (menu, agents) = menus();
        let mut overlay = mode_overlay();
        assert!(matches!(
            overlay.handle_key(key(KeyCode::Esc), &ctx(&menu, &agents)),
            Flow::Close
        ));
    }

    #[test]
    fn picking_a_mode_closes_the_overlay_and_reports_the_choice() {
        let (menu, agents) = menus();
        let mut overlay = mode_overlay();
        let flow = overlay.handle_key(key(KeyCode::Enter), &ctx(&menu, &agents));
        assert!(matches!(
            flow,
            Flow::Act(OverlayAction::SetMode(PermissionMode::Default))
        ));
    }

    /// Dismissing the trust prompt is a decision, not a no-op: the folder
    /// stays untrusted for the session rather than silently staying unknown.
    #[test]
    fn dismissing_the_folder_trust_prompt_rejects_for_the_session() {
        let (menu, agents) = menus();
        let mut overlay = trust_overlay();
        let flow = overlay.handle_key(key(KeyCode::Esc), &ctx(&menu, &agents));
        assert!(matches!(
            flow,
            Flow::Act(OverlayAction::FolderTrust(
                folder_trust_picker::Choice::RejectSession
            ))
        ));
    }

    /// A click outside dismisses the casual pickers, but a pending decision
    /// (trust, approval) has to be answered rather than dodged.
    #[test]
    fn click_away_only_dismisses_pickers_without_a_pending_decision() {
        assert!(matches!(mode_overlay().on_click_away(), Flow::Close));
        assert!(matches!(trust_overlay().on_click_away(), Flow::Stay));
    }

    /// Only the approval dialog has editable text, so only it can move the
    /// hardware cursor that the OS IME follows.
    #[test]
    fn pickers_never_claim_the_terminal_cursor() {
        assert!(mode_overlay().cursor_cell().is_none());
        assert!(trust_overlay().cursor_cell().is_none());
    }

    /// A picker must swallow a bracketed paste rather than let it reach the
    /// hidden editor and make the panel jump when the picker closes.
    #[test]
    fn a_picker_swallows_pastes_instead_of_leaking_them() {
        let mut overlay = mode_overlay();
        overlay.paste_text("many\nlines\nof\ntext".to_string());
        assert!(overlay.as_dialog().is_none());
    }
}
