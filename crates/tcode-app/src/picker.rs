//! What the composer's chips offer: permission mode, model, preset.
//!
//! None of the *decisions* live here. `tcode-frontend` already builds a
//! `ModelMenu` and a `PresetMenu` — a list of options plus the closures that
//! apply one, swapping the provider and writing `[tcode_state]` in the selected
//! config — and the TUI's `/model` renders exactly the same two structures. This
//! module is the thin layer that turns them into JSON the webview can draw and
//! routes a choice back into the closure that was always going to make it.
//!
//! That split is the point. A second implementation of "switch the model" is a
//! second place for precedence (CLI flag > `[tcode_state]` > preset > config) to
//! be almost right.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use tcode_core::config::Config;
use tcode_core::PermissionMode;
use tcode_frontend::{ModelMenu, PresetMenu};

/// The live menus, rebuilt whenever a choice replaces them.
///
/// Behind one lock rather than two: applying a preset returns a new model menu
/// *and* a new preset list, and half-updated menus would offer options that no
/// longer describe anything.
pub struct Pickers {
    pub models: ModelMenu,
    pub presets: PresetMenu,
    /// The selected personal config — where a mode choice is remembered.
    pub config_file: PathBuf,
}

pub type Menus = Arc<Mutex<Pickers>>;

impl Pickers {
    /// Menus with nothing in them, whose apply-closures say why.
    ///
    /// The honest representation of "this config offers no usable model" — the
    /// chips then show an empty picker with a reason instead of a control that
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

#[derive(Serialize)]
pub struct ModeChoice {
    pub key: &'static str,
    pub label: &'static str,
    /// One line on what it changes. The four modes differ in what they let
    /// through without asking, which is not something a name can carry.
    pub detail: &'static str,
}

/// Everything the composer's chips draw, in one round trip.
///
/// One command rather than three because they are read together and change
/// together: applying a preset moves the model, which moves the effort.
#[derive(Serialize)]
pub struct PickerState {
    pub models: Vec<ModelChoice>,
    pub model: usize,
    pub effort: Option<String>,
    pub presets: Vec<PresetChoice>,
    pub preset: Option<usize>,
    pub modes: Vec<ModeChoice>,
    pub mode: &'static str,
    /// True while the chosen mode is waiting for a running turn to end.
    pub mode_staged: bool,
}

pub const MODES: &[ModeChoice] = &[
    ModeChoice {
        key: "default",
        label: "Ask first",
        detail: "Rules decide; anything else asks you.",
    },
    ModeChoice {
        key: "accept-edits",
        label: "Accept edits",
        detail: "File edits go through; commands still ask.",
    },
    ModeChoice {
        key: "auto",
        label: "Auto",
        detail: "Runs without prompting; a safety classifier reviews the rest.",
    },
    ModeChoice {
        key: "unsafe",
        label: "Bypass permissions",
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

pub fn state_of(menus: &Pickers, mode: PermissionMode, mode_staged: bool) -> PickerState {
    let current = menus.models.options.get(menus.models.current);
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
        effort: current.and_then(|option| option.def.default_effort.clone()),
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
        modes: MODES.iter().map(clone_mode).collect(),
        mode: mode.label(),
        mode_staged,
    }
}

fn clone_mode(mode: &ModeChoice) -> ModeChoice {
    ModeChoice {
        key: mode.key,
        label: mode.label,
        detail: mode.detail,
    }
}

/// Switch the main model, live and in `[tcode_state]`.
///
/// The whole body is the frontend's `switch` closure; what is here is the index
/// check, because the index came from the webview and is therefore data.
pub fn choose_model(menus: &mut Pickers, index: usize, effort: Option<&str>) -> Result<(), String> {
    let option = menus
        .models
        .options
        .get(index)
        .ok_or_else(|| format!("no model at position {index}"))?;
    (menus.models.switch)(option, effort)?;
    menus.models.current = index;
    Ok(())
}

/// Apply a named line-up. It replaces both menus, because a preset moves the
/// main model and every pinned role at once.
pub fn choose_preset(menus: &mut Pickers, key: &str) -> Result<String, String> {
    let (models, _agents, label, _warnings) = (menus.presets.apply)(key)?;
    menus.models = models;
    menus.presets.current = menus
        .presets
        .options
        .iter()
        .position(|option| option.key == key);
    Ok(label)
}

/// Remember the mode for new sessions — except `unsafe`, which is deliberately
/// not sticky: a one-off bypass must not silently arm every future session. The
/// TUI's `/mode` makes exactly the same exception, and the two must not drift.
pub fn remember_mode(config_file: &std::path::Path, mode: PermissionMode) {
    Config::update_tcode_state(config_file, move |state| {
        state.mode = (mode != PermissionMode::Unsafe).then_some(mode);
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
}
