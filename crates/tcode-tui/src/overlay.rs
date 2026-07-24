//! The one modal that owns the bottom panel's primary content.
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
use tcode_core::{Approval, BatchApproval, PermissionMode};
use tcode_importers::{ExternalSessionInfo, ExternalSource};
use tokio::sync::oneshot;

use crate::approval::{Dialog, DialogResult};
use crate::model_picker::{self, AgentMenu, AgentModelChoice, HubCtx, ModelMenu, PresetMenu};
use crate::view_picker::{self, ViewId};
use crate::{folder_trust_picker, mode_picker, resume, voice_picker};

/// Shared state the overlays need but do not own. Passed in per event so the
/// overlay never has to borrow `App`.
pub struct OverlayCtx<'a> {
    pub menu: &'a ModelMenu,
    pub agents: &'a AgentMenu,
    pub presets: &'a PresetMenu,
    /// The running provider's reasoning effort. No menu holds it: it belongs
    /// to the live `ModelCell`, not to the list of what could be picked.
    pub effort: Option<String>,
    pub width: u16,
    pub height: u16,
}

impl OverlayCtx<'_> {
    fn hub(&self) -> HubCtx<'_> {
        HubCtx {
            menu: self.menu,
            agents: self.agents,
            presets: self.presets,
            effort: self.effort.as_deref(),
        }
    }
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
    /// `/voice model`. The name is a string because the list of them lives in
    /// the sidecar, not here.
    SetVoiceModel(String),
    FolderTrust(folder_trust_picker::Choice),
    ApplyAgentModel {
        kind: String,
        choice: AgentModelChoice,
    },
    /// Switch to a named line-up. Like `ApplySetup`, only the binary can carry
    /// it out: it rebuilds the provider and every pin behind it.
    ApplyPreset(String),
    /// Capture the live line-up under a new name.
    SavePreset(String),
    /// Suspends the terminal, so it cannot run inside the dialog's own key
    /// handler. The dialog stays open and takes the revision afterwards.
    EditPlan,
    /// A finished `/provider` run: persist it and rebuild everything derived
    /// from the config. Only the binary can do that, so it arrives as an
    /// action like every other.
    ApplySetup(Box<tcode_core::config::Config>),
    /// Only `Overlay::Approval` produces this, and the reply channel it needs
    /// lives in that variant — see `App::on_overlay_flow`.
    Approved(Approval),
    /// A combined review the reviewer took apart: retract its diffs and let the
    /// agent loop prompt for each change on its own. Same channel ownership as
    /// `Approved`.
    ReviewIndividually,
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
    /// `/model` and `/agents`: one hub over the whole line-up.
    Model(model_picker::Hub),
    Mode(mode_picker::Picker),
    VoiceModel(voice_picker::Picker),
    FolderTrust(folder_trust_picker::Picker),
    /// `/provider`. Boxed because it carries a whole `Config` being edited.
    Provider(Box<crate::setup::Setup>),
    Approval(Box<Dialog>, ApprovalReply),
}

/// Where a review's answer goes. A combined review can also be handed back for
/// per-call prompts — an outcome a single prompt has no way to express.
pub enum ApprovalReply {
    One(oneshot::Sender<Approval>),
    Batch(oneshot::Sender<BatchApproval>),
}

impl Overlay {
    pub fn approval(dialog: Dialog, reply: ApprovalReply) -> Self {
        Overlay::Approval(Box::new(dialog), reply)
    }

    /// An approval keeps the mode status visible: it is still a pending turn,
    /// so `shift+tab` can stage the mode that takes effect after the decision.
    pub fn keeps_status_hint(&self) -> bool {
        matches!(self, Overlay::Approval(..))
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
        match self {
            Overlay::Approval(dialog, _) => dialog.paste_text(text),
            // An API key is nearly always pasted; setup itself decides
            // whether the current step has a field to take it.
            Overlay::Provider(setup) => {
                setup.on_paste(text);
            }
            _ => {}
        }
    }

    pub fn render(&self, ctx: &OverlayCtx) -> Vec<Line<'static>> {
        match self {
            Overlay::View(picker) => picker.render(),
            Overlay::Resume(picker) => picker.render(),
            Overlay::Model(hub) => hub.render(&ctx.hub()),
            Overlay::Mode(picker) => picker.render(),
            Overlay::VoiceModel(picker) => picker.render(),
            Overlay::FolderTrust(picker) => picker.render(),
            Overlay::Provider(setup) => crate::provider_picker::render(&setup.view()),
            Overlay::Approval(dialog, _) => dialog.render(ctx.width, ctx.height.saturating_sub(7)),
        }
    }

    pub fn set_hovered_row(&mut self, row: Option<usize>, ctx: &OverlayCtx) {
        match self {
            Overlay::View(picker) => picker.set_hovered_row(row),
            Overlay::Model(hub) => hub.set_hovered_row(row, &ctx.hub()),
            Overlay::Mode(picker) => picker.set_hovered_row(row),
            Overlay::VoiceModel(picker) => picker.set_hovered_row(row),
            Overlay::FolderTrust(picker) => picker.set_hovered_row(row),
            // Setup is a keyboard form (fields, not just rows); hover would
            // have to move a text cursor to mean anything.
            Overlay::Resume(_) | Overlay::Provider(_) | Overlay::Approval(..) => {}
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
            Overlay::Model(hub) => hub_flow(hub.handle_key(key, &ctx.hub())),
            Overlay::Mode(picker) => match picker.handle_key(key) {
                mode_picker::PickResult::Pending => Flow::Stay,
                mode_picker::PickResult::Cancelled => Flow::Close,
                mode_picker::PickResult::Picked(mode) => Flow::Act(OverlayAction::SetMode(mode)),
            },
            Overlay::VoiceModel(picker) => match picker.handle_key(key) {
                voice_picker::PickResult::Pending => Flow::Stay,
                voice_picker::PickResult::Cancelled => Flow::Close,
                voice_picker::PickResult::Picked(name) => {
                    Flow::Act(OverlayAction::SetVoiceModel(name))
                }
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
            Overlay::Provider(setup) => match crate::setup::on_key(setup, key) {
                crate::setup::Progress::Stay => Flow::Stay,
                crate::setup::Progress::Done(None) => Flow::Close,
                crate::setup::Progress::Done(Some(done)) => {
                    Flow::Act(OverlayAction::ApplySetup(done))
                }
            },
            Overlay::Approval(dialog, _) => match dialog.handle_key(key) {
                DialogResult::Pending => Flow::Stay,
                DialogResult::EditPlan => Flow::ActInPlace(OverlayAction::EditPlan),
                DialogResult::Done(approval) => Flow::Act(OverlayAction::Approved(approval)),
                DialogResult::Individually => Flow::Act(OverlayAction::ReviewIndividually),
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
            Overlay::Model(hub) => match hub_flow(hub.handle_mouse_row(row, &ctx.hub())) {
                // A click on a header row picks nothing; it must not be read
                // as a dismissal.
                Flow::Close => Flow::Stay,
                flow => flow,
            },
            Overlay::Mode(picker) => match picker.handle_mouse_row(row) {
                mode_picker::PickResult::Picked(mode) => Flow::Act(OverlayAction::SetMode(mode)),
                _ => Flow::Stay,
            },
            Overlay::VoiceModel(picker) => match picker.handle_mouse_row(row) {
                voice_picker::PickResult::Picked(name) => {
                    Flow::Act(OverlayAction::SetVoiceModel(name))
                }
                _ => Flow::Stay,
            },
            Overlay::FolderTrust(picker) => match picker.handle_mouse_row(row) {
                folder_trust_picker::PickResult::Picked(choice) => {
                    Flow::Act(OverlayAction::FolderTrust(choice))
                }
                _ => Flow::Stay,
            },
            Overlay::Resume(_) | Overlay::Provider(_) | Overlay::Approval(..) => Flow::Stay,
        }
    }

    /// A click outside the panel. Dismisses the picker the way Esc would,
    /// except where dismissal needs an explicit decision.
    pub fn on_click_away(&self) -> Flow {
        match self {
            Overlay::View(_) | Overlay::Model(_) | Overlay::Mode(_) | Overlay::VoiceModel(_) => {
                Flow::Close
            }
            // The trust prompt and an approval must be answered, not dodged;
            // setup holds a half-typed key a stray click must not discard.
            Overlay::FolderTrust(_)
            | Overlay::Approval(..)
            | Overlay::Resume(_)
            | Overlay::Provider(_) => Flow::Stay,
        }
    }
}

/// Every hub pick but the dismissal keeps the dialog open: a visit is usually
/// about more than one row, and closing after each pick is what made
/// configuring a line-up mean reopening the dialog eight times.
fn hub_flow(pick: model_picker::HubPick) -> Flow {
    match pick {
        model_picker::HubPick::Pending => Flow::Stay,
        model_picker::HubPick::Cancelled => Flow::Close,
        model_picker::HubPick::Model { option, effort } => {
            Flow::ActInPlace(OverlayAction::ApplyModel {
                index: option,
                effort,
            })
        }
        model_picker::HubPick::Agent { kind, choice } => {
            Flow::ActInPlace(OverlayAction::ApplyAgentModel { kind, choice })
        }
        model_picker::HubPick::Preset(name) => Flow::ActInPlace(OverlayAction::ApplyPreset(name)),
        model_picker::HubPick::SavePreset(name) => {
            Flow::ActInPlace(OverlayAction::SavePreset(name))
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
    fn menus() -> (ModelMenu, AgentMenu, PresetMenu) {
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
            PresetMenu {
                options: Vec::new(),
                current: None,
                apply: Box::new(|_| Err("no presets in test".to_string())),
                save: Box::new(|_, _, _| Err("no presets in test".to_string())),
            },
        )
    }

    fn ctx<'a>(
        menu: &'a ModelMenu,
        agents: &'a AgentMenu,
        presets: &'a PresetMenu,
    ) -> OverlayCtx<'a> {
        OverlayCtx {
            menu,
            agents,
            presets,
            effort: None,
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
    fn approval_keeps_the_mode_status_visible() {
        let (reply, _rx) = oneshot::channel();
        let overlay = Overlay::approval(
            Dialog::new("summary".into(), "tool".into(), "call".into(), false, false),
            ApprovalReply::One(reply),
        );
        assert!(overlay.keeps_status_hint());
        assert!(!mode_overlay().keeps_status_hint());
    }

    #[test]
    fn escape_dismisses_a_picker_without_acting() {
        let (menu, agents, presets) = menus();
        let mut overlay = mode_overlay();
        assert!(matches!(
            overlay.handle_key(key(KeyCode::Esc), &ctx(&menu, &agents, &presets)),
            Flow::Close
        ));
    }

    #[test]
    fn picking_a_mode_closes_the_overlay_and_reports_the_choice() {
        let (menu, agents, presets) = menus();
        let mut overlay = mode_overlay();
        let flow = overlay.handle_key(key(KeyCode::Enter), &ctx(&menu, &agents, &presets));
        assert!(matches!(
            flow,
            Flow::Act(OverlayAction::SetMode(PermissionMode::Default))
        ));
    }

    /// Dismissing the trust prompt is a decision, not a no-op: the folder
    /// stays untrusted for the session rather than silently staying unknown.
    #[test]
    fn dismissing_the_folder_trust_prompt_rejects_for_the_session() {
        let (menu, agents, presets) = menus();
        let mut overlay = trust_overlay();
        let flow = overlay.handle_key(key(KeyCode::Esc), &ctx(&menu, &agents, &presets));
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
