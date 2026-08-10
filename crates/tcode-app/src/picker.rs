//! What the composer's model panel offers: permission mode, model, effort,
//! preset, and what every sub-agent role runs on.
//!
//! None of the *decisions* live here. `tcode-frontend` already builds a
//! `ModelMenu`, a `PresetMenu` and an `AgentMenu` — lists of options plus the
//! closures that apply one, swapping the provider and writing `[tcode_state]` in
//! the selected config — and the TUI's `/model` and `/agents` render exactly the
//! same three structures. This module is the thin layer that turns them into
//! JSON the webview can draw and routes a choice back into the closure that was
//! always going to make it.
//!
//! That split is the point. A second implementation of "switch the model" is a
//! second place for precedence (CLI flag > `[tcode_state]` > preset > config) to
//! be almost right.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use tcode_core::config::Config;
use tcode_core::{AgentModels, ModelCell, PermissionMode};
use tcode_frontend::{AgentMenu, AgentModelChoice, ModelMenu, PresetDraft, PresetMenu};

/// The live menus, rebuilt whenever a choice replaces them.
///
/// Behind one lock rather than three: applying a preset returns a new model menu
/// *and* a new agent menu *and* a new preset list, and half-updated menus would
/// offer options that no longer describe anything.
pub struct Pickers {
    pub models: ModelMenu,
    pub presets: PresetMenu,
    /// Sub-agent and helper roles: what each runs on, and the pin closure.
    pub agents: AgentMenu,
    /// The selected personal config — where a mode choice is remembered.
    pub config_file: PathBuf,
}

pub type Menus = Arc<Mutex<Pickers>>;

impl Pickers {
    /// Menus with nothing in them, whose apply-closures say why.
    ///
    /// The honest representation of "this config offers no usable model" — the
    /// panel then shows an empty picker with a reason instead of a control that
    /// silently does nothing. It is also what a test that is not about model
    /// switching needs, which is a good sign it is the right fallback rather
    /// than a test fixture in disguise.
    pub fn unavailable(config_file: PathBuf, reason: &'static str) -> Self {
        Self {
            models: ModelMenu {
                options: Vec::new(),
                current: 0,
                switch: Box::new(move |_, _| Err(reason.to_string())),
            },
            presets: PresetMenu {
                options: Vec::new(),
                current: None,
                apply: Box::new(move |_| Err(reason.to_string())),
                save: Box::new(move |_, _, _| Err(reason.to_string())),
            },
            agents: AgentMenu {
                roles: Vec::new(),
                pins: Vec::new(),
                pin: Box::new(move |_, _| Err(reason.to_string())),
            },
            config_file,
        }
    }
}

/// One model the composer can switch to.
#[derive(Serialize)]
pub struct ModelChoice {
    pub profile: String,
    /// What the picker shows: the model's label, or its id when it has none.
    pub label: String,
    /// Reasoning efforts this model accepts, lowest first. Empty means the
    /// model has no effort dial — the chip is then absent rather than showing a
    /// control that does nothing.
    pub efforts: Vec<String>,
}

#[derive(Serialize)]
pub struct PresetChoice {
    pub key: String,
    pub label: String,
}

/// A permission mode as the chip offers it.
///
/// There is no separate `label`: the chip shows the key itself (`default`,
/// `accept-edits`, `auto`, `unsafe`) — the same words `/mode` uses, the same
/// words the config file uses. A friendlier second name for each mode read as a
/// different set of modes to anyone who had seen the first set, and the strip is
/// where you check what a conversation is armed to do.
#[derive(Serialize)]
pub struct ModeChoice {
    pub key: &'static str,
    /// One line on what it changes. The four modes differ in what they let
    /// through without asking, which is not something a name can carry.
    pub detail: &'static str,
}

/// What one role runs on, structured so the panel can mark the current row
/// rather than string-match a description of it.
///
/// Internally tagged, and the tag is the only thing the webview may invent: an
/// unknown one fails to deserialize and the pin is refused (AGENTS.md rule 3).
#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PinChoice {
    /// Only legal for roles that are off by default — today, `web-fetch`.
    Off,
    /// Follow the main model, live: this role moves with `/model`.
    Inherit,
    Model {
        /// Index into `PickerState::models`.
        index: usize,
        effort: Option<String>,
    },
}

/// One pinnable role: the sub-agents you delegate to, plus the helper models
/// around a turn.
#[derive(Serialize)]
pub struct RoleChoice {
    /// What the pin closure and `[agents.*]` call it.
    pub key: String,
    /// What the panel shows. Usually the key, but not always (`fetch` is
    /// `web-fetch` to a reader).
    pub label: String,
    /// Whether "off" is one of this role's answers.
    pub allows_off: bool,
    /// `false` = a sub-agent you hand work to; `true` = machinery that runs
    /// around your turn. The panel groups on it, because the two are configured
    /// identically but looked for at different moments.
    pub helper: bool,
    pub pin: PinChoice,
}

/// Everything the composer's model panel draws, in one round trip.
///
/// One command rather than five because they are read together and change
/// together: applying a preset moves the model, which moves the effort, and
/// repins every role at once.
#[derive(Serialize)]
pub struct PickerState {
    pub models: Vec<ModelChoice>,
    pub model: usize,
    /// The effort the agent is *running* on, read from the live `ModelCell`
    /// rather than from the model definition's default: this field is what the
    /// panel shows back after a pick, so anything else makes a working choice
    /// look like a dead control.
    pub effort: Option<String>,
    /// How many tokens the running model's window holds. Read from the same
    /// live `ModelCell` as `effort`, and for the same reason: the context meter
    /// divides by this, and dividing by the *configured* window while running on
    /// another model draws a percentage of the wrong thing.
    pub context_window: u64,
    pub presets: Vec<PresetChoice>,
    pub preset: Option<usize>,
    pub roles: Vec<RoleChoice>,
    pub modes: Vec<ModeChoice>,
    pub mode: &'static str,
    /// True while the chosen mode is waiting for a running turn to end.
    pub mode_staged: bool,
    /// Whether the running line-up can view images at all (the live main
    /// model, or the vision role's current resolution). The panel's vision row
    /// warns when false. Read live, not from the menus, because a `choose_model`
    /// does not rebuild them.
    pub can_view_images: bool,
}

pub const MODES: &[ModeChoice] = &[
    ModeChoice {
        key: "default",
        detail: "Rules decide; anything else asks you.",
    },
    ModeChoice {
        key: "accept-edits",
        detail: "File edits go through; commands still ask.",
    },
    ModeChoice {
        key: "auto",
        detail: "Runs without prompting; a safety classifier reviews the rest.",
    },
    ModeChoice {
        key: "unsafe",
        detail: "Nothing asks. Deny rules still apply. For isolated environments.",
    },
];

pub fn mode_from_key(key: &str) -> Option<PermissionMode> {
    match key {
        "default" => Some(PermissionMode::Default),
        "accept-edits" => Some(PermissionMode::AcceptEdits),
        "auto" => Some(PermissionMode::Auto),
        "unsafe" => Some(PermissionMode::Unsafe),
        _ => None,
    }
}

pub fn state_of(
    menus: &Pickers,
    model: &ModelCell,
    pinned: &AgentModels,
    mode: PermissionMode,
    mode_staged: bool,
) -> PickerState {
    PickerState {
        models: menus
            .models
            .options
            .iter()
            .map(|option| ModelChoice {
                profile: option.profile.clone(),
                label: option.def.display().to_string(),
                efforts: option.def.efforts.clone(),
            })
            .collect(),
        model: menus.models.current,
        effort: model.snapshot().effort,
        context_window: model.snapshot().context_window,
        presets: menus
            .presets
            .options
            .iter()
            .map(|option| PresetChoice {
                key: option.key.clone(),
                label: option.label.clone(),
            })
            .collect(),
        preset: menus.presets.current,
        roles: menus
            .agents
            .roles
            .iter()
            .zip(&menus.agents.pins)
            .map(|(role, pin)| RoleChoice {
                key: role.key.clone(),
                label: role.label.clone(),
                allows_off: role.allows_off,
                helper: matches!(role.section, tcode_frontend::RoleSection::Helper),
                pin: match pin {
                    AgentModelChoice::Off => PinChoice::Off,
                    AgentModelChoice::Inherit => PinChoice::Inherit,
                    AgentModelChoice::Model { option, effort } => PinChoice::Model {
                        index: *option,
                        effort: effort.clone(),
                    },
                },
            })
            .collect(),
        modes: MODES.iter().map(clone_mode).collect(),
        mode: mode.label(),
        mode_staged,
        can_view_images: pinned.can_view_images(model),
    }
}

fn clone_mode(mode: &ModeChoice) -> ModeChoice {
    ModeChoice {
        key: mode.key,
        detail: mode.detail,
    }
}

/// Switch the main model, live and in `[tcode_state]`.
///
/// The whole body is the frontend's `switch` closure; what is here is the index
/// check, because the index came from the webview and is therefore data — plus
/// the swap into the shared `ModelCell`, which is what makes the choice apply to
/// the next request instead of only to the next process. The TUI's `apply_model`
/// does exactly the same two steps around the same closure.
pub fn choose_model(
    menus: &mut Pickers,
    model: &ModelCell,
    index: usize,
    effort: Option<&str>,
) -> Result<(), String> {
    let option = menus
        .models
        .options
        .get(index)
        .ok_or_else(|| format!("no model at position {index}"))?;
    let active = (menus.models.switch)(option, effort)?;
    model.swap(active);
    menus.models.current = index;
    Ok(())
}

/// Apply a named preset: the main model plus every role, in one step.
///
/// All three menus are replaced, not two. A preset repins every role, so keeping
/// the old `agents` would leave the panel listing pins that no longer exist —
/// the frontend hands back a rebuilt `AgentMenu` precisely so nobody has to work
/// out which rows moved.
///
/// The live `ModelCell` is *not* swapped here: the preset's own `apply` closure
/// holds that cell and rebuilds through it, unlike `switch`. The two are
/// asymmetric on purpose (see `choose_model`), so this is the one path that must
/// not swap again.
pub fn choose_preset(menus: &mut Pickers, key: &str) -> Result<String, String> {
    let (models, agents, label, _warnings) = (menus.presets.apply)(key)?;
    menus.models = models;
    menus.agents = agents;
    menus.presets.current = menus
        .presets
        .options
        .iter()
        .position(|option| option.key == key);
    Ok(label)
}

/// Pin one role to a model, to the main model (`inherit`), or off.
///
/// Everything the pick does — rebuild that role's provider, hot-swap the shared
/// registry, persist `[agents.<kind>]` — is the frontend's `pin` closure. Here:
/// the role must exist, and `off` must be a legal answer for it. Both come from
/// the webview, so both are checked rather than coerced into the nearest thing
/// that would have worked.
pub fn pin_role(menus: &mut Pickers, kind: &str, choice: PinChoice) -> Result<String, String> {
    let at = menus
        .agents
        .roles
        .iter()
        .position(|role| role.key == kind)
        .ok_or_else(|| format!("'{kind}' is not a pinnable role"))?;
    if matches!(choice, PinChoice::Off) && !menus.agents.roles[at].allows_off {
        return Err(format!("'{kind}' cannot be turned off"));
    }
    let choice = match choice {
        PinChoice::Off => AgentModelChoice::Off,
        PinChoice::Inherit => AgentModelChoice::Inherit,
        PinChoice::Model { index, effort } => AgentModelChoice::Model {
            option: index,
            effort,
        },
    };
    let label = (menus.agents.pin)(kind, choice.clone())?;
    menus.agents.pins[at] = choice;
    Ok(label)
}

/// Write what is running out as `[presets.<name>]`.
///
/// The draft is assembled here because only this layer holds the live line-up:
/// the model menu's index, the effort from the shared cell, and every role's
/// current pin. Naming the profiles and models behind those indices is the
/// frontend's `save` closure, which is also where the name is validated — the
/// name came from the webview, and the rules for it live with the config writer
/// rather than in a second copy here.
pub fn save_preset(menus: &mut Pickers, model: &ModelCell, name: &str) -> Result<(), String> {
    let draft = PresetDraft {
        main: (!menus.models.options.is_empty()).then_some(menus.models.current),
        main_effort: model.snapshot().effort,
        roles: menus
            .agents
            .roles
            .iter()
            .zip(&menus.agents.pins)
            .map(|(role, pin)| (role.key.clone(), pin.clone()))
            .collect(),
    };
    let (options, current, _outcome) = (menus.presets.save)(name, &draft, &menus.models)?;
    menus.presets.options = options;
    // Saving switches to what was saved: the ad-hoc pins it was captured from
    // are now spelled out under a name, so the preset is what is in force.
    menus.presets.current = Some(current);
    Ok(())
}

/// Remember the mode for new sessions — every mode, `unsafe` included.
///
/// `unsafe` used to be excluded so a one-off bypass could not arm later
/// sessions. The cost of that was paid by the people who actually work that
/// way: one re-pick per session, of a choice they had already made. The mode
/// chip says what is in force at all times and switches in one click, which is
/// where that reminder belongs. The TUI's `/mode` persists the same way
/// (`persist_mode`), and the two must not drift.
pub fn remember_mode(config_file: &std::path::Path, mode: PermissionMode) {
    Config::update_tcode_state(config_file, move |state| {
        state.mode = Some(mode);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_the_core_has_is_offered_and_round_trips() {
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Auto,
            PermissionMode::Unsafe,
        ] {
            let offered = MODES
                .iter()
                .find(|choice| choice.key == mode.label())
                .unwrap_or_else(|| panic!("{} is not offered by the picker", mode.label()));
            assert_eq!(mode_from_key(offered.key), Some(mode));
        }
        assert_eq!(MODES.len(), 4, "a mode was added to core but not offered");
    }

    #[test]
    fn an_unknown_mode_from_the_webview_is_not_a_mode() {
        assert_eq!(mode_from_key("yolo"), None);
        assert_eq!(mode_from_key(""), None);
    }

    /// Two roles: one that may be turned off, one that may not — which is the
    /// only difference the pin path has to police.
    fn with_roles() -> Pickers {
        use tcode_frontend::{AgentRole, RoleSection};

        let mut menus = Pickers::unavailable(
            PathBuf::from("/nonexistent/config.toml"),
            "no provider is configured",
        );
        menus.agents = AgentMenu {
            roles: vec![
                AgentRole {
                    key: "explore".to_string(),
                    label: "explore".to_string(),
                    allows_off: false,
                    section: RoleSection::Task,
                },
                AgentRole {
                    key: "fetch".to_string(),
                    label: "web-fetch".to_string(),
                    allows_off: true,
                    section: RoleSection::Helper,
                },
            ],
            pins: vec![AgentModelChoice::Inherit, AgentModelChoice::Off],
            pin: Box::new(|_, _| Ok("pinned".to_string())),
        };
        menus
    }

    #[test]
    fn a_role_the_config_does_not_have_is_not_pinnable() {
        let mut menus = with_roles();

        let refused = pin_role(&mut menus, "definitely-not-a-role", PinChoice::Inherit)
            .expect_err("an unknown role is refused");

        assert!(refused.contains("not a pinnable role"), "{refused}");
    }

    #[test]
    fn only_a_role_that_may_be_off_can_be_turned_off() {
        let mut menus = with_roles();

        let refused =
            pin_role(&mut menus, "explore", PinChoice::Off).expect_err("explore cannot be off");
        assert!(refused.contains("cannot be turned off"), "{refused}");
        assert!(
            matches!(menus.agents.pins[0], AgentModelChoice::Inherit),
            "a refused pin leaves the live one alone"
        );

        pin_role(&mut menus, "fetch", PinChoice::Off).expect("web-fetch may be off");
    }

    #[test]
    fn an_applied_pin_is_what_the_panel_reads_back() {
        let mut menus = with_roles();

        pin_role(
            &mut menus,
            "explore",
            PinChoice::Model {
                index: 0,
                effort: Some("low".to_string()),
            },
        )
        .expect("the pin applies");

        // The panel re-reads state after every change, so a pin that is not
        // mirrored here shows the row snapping back to its old value.
        assert!(matches!(
            &menus.agents.pins[0],
            AgentModelChoice::Model { option: 0, effort } if effort.as_deref() == Some("low")
        ));
    }
}
