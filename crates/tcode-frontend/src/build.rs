//! UI-independent construction of model, agent, preset, and provider menus.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tcode_core::config::{AgentConfig, Config, ConfigError, ModelState, Preset, Selection};
use tcode_core::{ActiveModel, AgentModels, AgentRole, ModelCell};

use crate::menu::{
    AgentMenu, AgentModelChoice, AgentRole as MenuAgentRole, ApplyPresetFn, ModelMenu, ModelOption,
    PinFn, PresetMenu, PresetOption, PresetSaveOutcome, ProviderSetup, RoleSection, SavePresetFn,
};

/// The selected model and menus rebuilt from a config-file change.
pub struct RebuiltMenus {
    pub selection: Selection,
    pub active: ActiveModel,
    pub menu: ModelMenu,
    pub agents: AgentMenu,
    pub warnings: Vec<String>,
}

/// Build the provider for one sub-agent pin.
fn build_agent_model(
    config: &Config,
    kind: &str,
    parent: &Selection,
) -> Option<Result<ActiveModel, ConfigError>> {
    let resolved = config.agent_selection(kind, parent)?;
    Some(resolved.and_then(|selection| {
        let profile = config
            .profiles
            .get(&selection.profile)
            .expect("agent_selection validated the profile");
        tcode_providers::build_active(profile, &selection, &config.watchdog)
    }))
}

/// Resolve every `[agents.<kind>]` / `/agents` pin into the live registry the
/// tools read. `fetch` is opt-in: its explicit `enabled = true` assignment
/// inherits the main model, while an absent assignment leaves it off.
///
/// Invalid pins remain ignored, but their explanations travel to the caller so
/// each frontend can present them without this shared crate doing terminal I/O.
pub fn agent_models(config: &Config, parent: &Selection) -> (AgentModels, Vec<String>) {
    let pinned = AgentModels::default();
    let mut warnings = Vec::new();
    for (kind, assignment) in &config.agents {
        if assignment.enabled == Some(false) {
            continue;
        }
        match build_agent_model(config, kind, parent) {
            Some(Ok(model)) => pinned.pin(kind, model),
            Some(Err(error)) => warnings.push(format!("[agents.{kind}] ignored: {error}")),
            None if AgentRole::from_key(kind)
                .is_some_and(|role| role.allows_off() && assignment.enabled == Some(true)) =>
            {
                pinned.pin_inherit(kind)
            }
            None => {}
        }
    }
    (pinned, warnings)
}

/// The `/agents` menu: the pinnable kinds, what each runs on now, and the
/// action that applies a pick — hot-swap the shared registry, then persist to
/// `[tcode_state]` in the selected config file.
pub fn build_agent_menu(
    config: &Config,
    menu: &ModelMenu,
    pinned: AgentModels,
    agent_defs: &tcode_tools::AgentRegistry,
    config_file: PathBuf,
) -> AgentMenu {
    let roles: Vec<MenuAgentRole> = agent_defs
        .visible_defs(None)
        .map(|def| MenuAgentRole {
            key: def.name.clone(),
            label: def.name.clone(),
            allows_off: false,
            section: RoleSection::Task,
        })
        .chain(AgentRole::ALL.iter().map(|role| MenuAgentRole {
            key: role.key().to_string(),
            label: role.label().to_string(),
            allows_off: role.allows_off(),
            section: RoleSection::Helper,
        }))
        .collect();
    let watchdog = config.watchdog.clone();
    let profiles = config.profiles.clone();
    let options: Vec<(String, tcode_core::config::ModelDef)> = menu
        .options
        .iter()
        .map(|option| (option.profile.clone(), option.def.clone()))
        .collect();
    let pins = roles
        .iter()
        .map(|role| {
            if let Some(model) = pinned.get(&role.key) {
                let Some(option) = menu
                    .options
                    .iter()
                    .position(|opt| opt.def.name == model.provider.model())
                else {
                    return AgentModelChoice::Inherit;
                };
                AgentModelChoice::Model {
                    option,
                    effort: model.effort.clone(),
                }
            } else if pinned.inherits(&role.key) {
                AgentModelChoice::Inherit
            } else if role.allows_off {
                AgentModelChoice::Off
            } else {
                AgentModelChoice::Inherit
            }
        })
        .collect();

    let off_by_default: BTreeSet<String> = roles
        .iter()
        .filter(|role| role.allows_off)
        .map(|role| role.key.clone())
        .collect();
    let pin: PinFn = Box::new(move |kind, choice| {
        let allows_off = off_by_default.contains(kind);
        match choice {
            AgentModelChoice::Off => {
                pinned.unpin(kind);
                persist_agent_pin(&config_file, kind, allows_off, None, false);
                Ok("off".to_string())
            }
            AgentModelChoice::Inherit => {
                pinned.pin_inherit(kind);
                persist_agent_pin(&config_file, kind, allows_off, None, true);
                Ok("inherit (main model)".to_string())
            }
            AgentModelChoice::Model { option, effort } => {
                let (profile_name, model) = options
                    .get(option)
                    .ok_or_else(|| "selected model disappeared".to_string())?;
                let profile = profiles
                    .get(profile_name)
                    .ok_or_else(|| format!("unknown profile '{profile_name}'"))?;
                let selection = Selection {
                    profile: profile_name.clone(),
                    model: model.clone(),
                    effort,
                };
                let active = tcode_providers::build_active(profile, &selection, &watchdog)
                    .map_err(|error| error.to_string())?;
                let label = active.describe();
                pinned.pin(kind, active);
                persist_agent_pin(&config_file, kind, allows_off, Some(&selection), true);
                Ok(label)
            }
        }
    });
    AgentMenu { roles, pins, pin }
}

/// State entries override `[agents.*]`: an explicit `enabled` lets opt-in
/// roles preserve the distinction between "off" and "inherit main model".
fn persist_agent_pin(
    config_file: &Path,
    kind: &str,
    allows_off: bool,
    selection: Option<&Selection>,
    enabled: bool,
) {
    let enabled = allows_off.then_some(enabled);
    Config::update_tcode_state(config_file, |state| {
        state.agents.insert(
            kind.to_string(),
            match selection {
                Some(selection) => AgentConfig {
                    profile: Some(selection.profile.clone()),
                    model: Some(selection.model.name.clone()),
                    effort: selection.effort.clone(),
                    enabled,
                },
                None => AgentConfig {
                    enabled,
                    ..AgentConfig::default()
                },
            },
        );
    });
}

/// Front-matter `model:` hints become `[agents.<name>]` defaults for the
/// definitions that survived capability validation — but only where nothing
/// else claimed the kind, so hand-written config, presets and `/agents` picks
/// all still win.
pub fn apply_agent_def_hints(config: &mut Config, agent_defs: &tcode_tools::AgentRegistry) {
    for def in agent_defs.visible_defs(None) {
        if let Some(hint) = &def.model {
            config
                .agents
                .entry(def.name.clone())
                .or_insert_with(|| AgentConfig {
                    profile: hint.profile.clone(),
                    model: hint.model.clone(),
                    effort: hint.effort.clone(),
                    enabled: None,
                });
        }
    }
}

/// Everything downstream of the selected config file, rebuilt from scratch:
/// reload all three layers, fold in the active preset and runtime state, swap
/// the provider into the shared cell, replace every sub-agent pin, and build
/// both menus.
pub fn rebuild_from_config(
    cwd: &Path,
    config_file: &Path,
    model_cell: &ModelCell,
    pinned: &AgentModels,
    agent_defs: &tcode_tools::AgentRegistry,
) -> Result<RebuiltMenus, String> {
    let mut config = Config::load_at(config_file, cwd).map_err(|error| error.to_string())?;
    tcode_providers::hydrate_codex_models(&mut config);
    let state = config.apply_active_preset();
    apply_agent_def_hints(&mut config, agent_defs);
    let selection = config
        .select(None, None, &state)
        .map_err(|error| error.to_string())?;
    let profile = config
        .profiles
        .get(&selection.profile)
        .ok_or_else(|| format!("profile '{}' is not configured", selection.profile))?;
    let active = tcode_providers::build_active(profile, &selection, &config.watchdog)
        .map_err(|error| error.to_string())?;
    model_cell.swap(active.clone());
    let (models, warnings) = agent_models(&config, &selection);
    pinned.replace_all(&models);

    let menu = build_menu(&config, &selection, config_file.to_path_buf());
    let agents = build_agent_menu(
        &config,
        &menu,
        pinned.clone(),
        agent_defs,
        config_file.to_path_buf(),
    );
    Ok(RebuiltMenus {
        selection,
        active,
        menu,
        agents,
        warnings,
    })
}

/// Persist what `/provider` produced, then rebuild everything downstream.
pub fn build_provider_setup(
    cwd: PathBuf,
    model_cell: ModelCell,
    pinned: AgentModels,
    agent_defs: Arc<tcode_tools::AgentRegistry>,
    config_file: PathBuf,
    config_header: &'static str,
) -> ProviderSetup {
    let apply_file = config_file.clone();
    let apply_cwd = cwd.clone();
    let apply_cell = model_cell.clone();
    let apply_pinned = pinned.clone();
    let apply_defs = agent_defs.clone();
    let apply = move |updated: Config| {
        updated
            .write_global_at(&apply_file, config_header)
            .map_err(|error| error.to_string())?;
        let rebuilt = rebuild_from_config(
            &apply_cwd,
            &apply_file,
            &apply_cell,
            &apply_pinned,
            apply_defs.as_ref(),
        )?;
        Ok((rebuilt.menu, rebuilt.agents, rebuilt.warnings))
    };
    // Refresh rebuilds the menus from the config already on disk, persisting
    // nothing — for after a `/login` changes provider availability.
    let refresh_file = config_file.clone();
    let refresh = move || {
        let rebuilt = rebuild_from_config(
            &cwd,
            &refresh_file,
            &model_cell,
            &pinned,
            agent_defs.as_ref(),
        )?;
        Ok((rebuilt.menu, rebuilt.agents, rebuilt.warnings))
    };
    ProviderSetup {
        load: Box::new(move || {
            Config::load_global_at(&config_file).map_err(|error| error.to_string())
        }),
        apply: Box::new(apply),
        refresh: Box::new(refresh),
    }
}

/// The named line-ups as the hub lists them, newest config read wins.
fn preset_options(config: &Config) -> Vec<PresetOption> {
    config
        .presets
        .iter()
        .map(|(key, preset)| PresetOption {
            key: key.clone(),
            label: preset.display(key).to_string(),
        })
        .collect()
}

/// How one role's entry in a preset reads in a one-line summary. A role the
/// preset stays silent about inherits at apply time, so `None` reads "inherit".
fn role_assignment(preset: Option<&AgentConfig>) -> String {
    let Some(agent) = preset else {
        return "inherit".into();
    };
    match (&agent.model, agent.enabled) {
        (None, Some(false)) => "off".into(),
        (None, _) => "inherit".into(),
        (Some(model), _) => match &agent.effort {
            Some(effort) => format!("{model} ({effort})"),
            None => model.clone(),
        },
    }
}

/// The main model line of a preset, in the same shape as `role_assignment`.
fn main_assignment(preset: &Preset) -> String {
    match &preset.model {
        Some(model) => match &preset.effort {
            Some(effort) => format!("{model} ({effort})"),
            None => model.clone(),
        },
        None => "inherit".into(),
    }
}

/// One line per role whose assignment changed between two line-ups, `main:`
/// first. Empty when they describe the same line-up.
fn preset_changes(old: &Preset, new: &Preset) -> Vec<String> {
    let mut out = Vec::new();
    let old_main = main_assignment(old);
    let new_main = main_assignment(new);
    if old_main != new_main {
        out.push(format!("main: {old_main} → {new_main}"));
    }
    for (kind, agent) in &new.agents {
        let before = role_assignment(old.agents.get(kind));
        let after = role_assignment(Some(agent));
        if before != after {
            out.push(format!("{kind}: {before} → {after}"));
        }
    }
    out
}

/// `/model`'s preset strip: switch to a line-up, or capture the live one as a
/// new preset. Both are config writes plus a rebuild, so both live here.
pub fn build_preset_menu(
    config: &Config,
    state: &ModelState,
    cwd: PathBuf,
    model_cell: ModelCell,
    pinned: AgentModels,
    agent_defs: Arc<tcode_tools::AgentRegistry>,
    config_file: PathBuf,
) -> PresetMenu {
    let options = preset_options(config);
    let current = state
        .preset
        .as_deref()
        .and_then(|name| options.iter().position(|option| option.key == name));

    let apply_file = config_file.clone();
    let (apply_cwd, apply_cell, apply_pinned, apply_defs) = (
        cwd.clone(),
        model_cell.clone(),
        pinned.clone(),
        agent_defs.clone(),
    );
    let apply: ApplyPresetFn = Box::new(move |name| {
        Config::switch_preset(&apply_file, name).map_err(|error| error.to_string())?;
        let rebuilt = rebuild_from_config(
            &apply_cwd,
            &apply_file,
            &apply_cell,
            &apply_pinned,
            apply_defs.as_ref(),
        )?;
        let label = match &rebuilt.selection.effort {
            Some(effort) => format!(
                "{} · {} ({effort})",
                rebuilt.selection.profile,
                rebuilt.selection.model.display()
            ),
            None => format!(
                "{} · {}",
                rebuilt.selection.profile,
                rebuilt.selection.model.display()
            ),
        };
        Ok((
            rebuilt.menu,
            rebuilt.agents,
            rebuilt.active,
            label,
            rebuilt.warnings,
        ))
    });

    let save: SavePresetFn = Box::new(move |name, draft, main_menu, role_options| {
        let main_named = |option: usize| -> Result<&ModelOption, String> {
            main_menu
                .options
                .get(option)
                .ok_or_else(|| "selected main model disappeared".to_string())
        };
        let role_named = |option: usize| -> Result<&ModelOption, String> {
            role_options
                .get(option)
                .ok_or_else(|| "selected role model disappeared".to_string())
        };
        let mut preset = Preset::default();
        if let Some(option) = draft.main {
            let option = main_named(option)?;
            preset.profile = Some(option.profile.clone());
            preset.model = Some(option.def.name.clone());
            preset.effort = draft.main_effort.clone();
        }
        // Every role is written out, including the ones merely inheriting: a
        // preset that stayed silent about a role would let `[agents.*]` leak
        // back in, and then switching to it would not describe what runs.
        for (kind, choice) in &draft.roles {
            let entry = match choice {
                AgentModelChoice::Off => AgentConfig::from_shorthand("off"),
                AgentModelChoice::Inherit => AgentConfig::from_shorthand("inherit"),
                AgentModelChoice::Model { option, effort } => {
                    let option = role_named(*option)?;
                    AgentConfig {
                        profile: Some(option.profile.clone()),
                        model: Some(option.def.name.clone()),
                        effort: effort.clone(),
                        enabled: None,
                    }
                }
            };
            preset.agents.insert(kind.clone(), entry);
        }
        // The name may already name a line-up: `/model save` replaces that
        // table, and the reply should say the update happened and what moved.
        let before = Config::load_at(&config_file, &cwd).map_err(|error| error.to_string())?;
        let old = before.presets.get(name);
        let outcome = PresetSaveOutcome {
            replaced: old.is_some(),
            changes: old
                .map(|old| preset_changes(old, &preset))
                .unwrap_or_default(),
        };
        Config::upsert_preset(&config_file, name, &preset).map_err(|error| error.to_string())?;
        // The line-up just saved becomes the one in force, so the ad-hoc pins
        // it was captured from can go: the preset now says the same thing.
        Config::switch_preset(&config_file, name).map_err(|error| error.to_string())?;
        let config = Config::load_at(&config_file, &cwd).map_err(|error| error.to_string())?;
        let options = preset_options(&config);
        let current = options
            .iter()
            .position(|option| option.key == name)
            .ok_or_else(|| "the saved preset is not readable back".to_string())?;
        Ok((options, current, outcome))
    });

    PresetMenu {
        options,
        current,
        apply,
        save,
    }
}

/// Flatten every profile's models into the `/model` menu, wiring the switch
/// action that rebuilds a provider and persists the selected choice.
pub fn build_menu(config: &Config, selection: &Selection, config_file: PathBuf) -> ModelMenu {
    let mut options = Vec::new();
    let mut current = 0;
    for (profile_name, profile) in &config.profiles {
        // The built-in catalog always contributes every provider; hide the
        // ones the user has no credentials for so the picker stays short.
        // The active profile is always shown so `current` stays valid.
        if !tcode_providers::profile_is_usable(profile_name, profile)
            && profile_name != &selection.profile
        {
            continue;
        }
        for def in config.resolved_model_defs(profile) {
            if profile_name == &selection.profile && def.name == selection.model.name {
                current = options.len();
            }
            options.push(ModelOption {
                profile: profile_name.clone(),
                def,
            });
        }
    }
    let cfg = config.clone();
    let watchdog = config.watchdog.clone();
    let switch = Box::new(move |option: &ModelOption, effort: Option<&str>| {
        let profile = cfg
            .profiles
            .get(&option.profile)
            .ok_or_else(|| format!("profile '{}' not found", option.profile))?;
        let selection = Selection {
            profile: option.profile.clone(),
            model: option.def.clone(),
            effort: effort.map(String::from),
        };
        let active = tcode_providers::build_active(profile, &selection, &watchdog)
            .map_err(|error| error.to_string())?;
        // Read-modify-write preserves the other runtime choices and all
        // handwritten TOML outside `[tcode_state]`.
        Config::update_tcode_state(&config_file, |state| {
            state.profile = Some(option.profile.clone());
            state.model = Some(option.def.name.clone());
            state.effort = effort.map(String::from);
        });
        Ok(active)
    });
    ModelMenu {
        options,
        current,
        switch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tcode_core::config::ModelDef;

    #[test]
    fn invalid_agent_pin_is_returned_as_a_warning_and_not_pinned() {
        let mut config = Config::defaults();
        config.agents.insert(
            "explore".to_string(),
            AgentConfig {
                profile: Some("typo".to_string()),
                model: None,
                effort: None,
                enabled: None,
            },
        );
        let parent = Selection {
            profile: "anthropic".to_string(),
            model: ModelDef::bare("claude-opus-4-8"),
            effort: None,
        };

        let (pinned, warnings) = agent_models(&config, &parent);

        assert!(pinned.get("explore").is_none());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("[agents.explore] ignored: unknown profile 'typo'"));
    }

    /// The overwrite reply lists only what actually moved: a role that stayed
    /// silent becomes "inherit" and a repeated save of the same line-up is
    /// empty — the write still happened, but there is nothing to narrate.
    #[test]
    fn preset_changes_report_only_the_roles_that_moved() {
        let old = Preset {
            model: Some("deepseek-v4-plus".into()),
            effort: Some("medium".into()),
            agents: std::collections::BTreeMap::from([(
                "explore".to_string(),
                AgentConfig::from_shorthand("deepseek-v4-plus"),
            )]),
            ..Preset::default()
        };
        let mut new = old.clone();
        new.agents.insert(
            "explore".to_string(),
            AgentConfig::from_shorthand("deepseek-v4-flash"),
        );
        new.agents.insert(
            "general".to_string(),
            AgentConfig::from_shorthand("inherit"),
        );
        new.effort = Some("high".into());

        assert_eq!(
            preset_changes(&old, &new),
            vec![
                "main: deepseek-v4-plus (medium) → deepseek-v4-plus (high)".to_string(),
                "explore: deepseek-v4-plus → deepseek-v4-flash".to_string(),
            ]
        );
        // The freshly-written `general = "inherit"` spells out what the old
        // silence already meant, so it is not a change.
        assert!(preset_changes(&old, &old).is_empty());
    }
}
