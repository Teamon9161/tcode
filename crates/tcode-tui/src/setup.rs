//! Provider setup, as a state machine that draws nothing.
//!
//! Two renderers drive it: the standalone first-run wizard (`wizard.rs`,
//! which must work before an `App` exists because there is no usable model
//! yet) and the in-session `/provider` overlay. Keeping the decisions here
//! means the two agree on what a keystroke does and on what lands in
//! `config.toml`; they differ only in how a `View` is painted.

use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use tcode_core::config::{Config, ModelDef, ModelState, Profile, ProviderKind};
use tcode_providers::codex_auth_available;

/// How a status string should read. Renderers map this to their own palette
/// (ANSI escapes for the standalone wizard, `theme.rs` for the overlay).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Dim,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Checked,
    Unchecked,
    /// A row that carries no checkbox: a menu choice or a text field.
    None,
}

/// One rendered line. Text steps produce a single row too, so a renderer
/// never needs to know which step it is painting.
#[derive(Debug, Clone)]
pub struct Row {
    pub label: String,
    pub status: String,
    pub tone: Tone,
    pub mark: Mark,
    /// The cursor is on this row.
    pub active: bool,
}

/// Everything a renderer needs, and nothing about how to draw it.
#[derive(Debug, Clone)]
pub struct View {
    pub title: String,
    pub hint: String,
    pub rows: Vec<Row>,
    /// This view is a text field: renderers draw a caret after the value.
    /// A menu row and a half-typed URL look identical otherwise.
    pub caret: bool,
}

/// What the caller should do after feeding an event in.
pub enum Progress {
    /// Setup keeps the keyboard.
    Stay,
    /// Setup is over. `Some` = write these to disk; `None` = cancelled, and
    /// nothing the user had before may be touched.
    Done(Option<Box<(Config, ModelState)>>),
}

#[derive(Debug, Clone)]
struct Candidate {
    id: String,
    title: String,
    status: String,
    tone: Tone,
    detected: bool,
    key_env: Option<String>,
}

#[derive(Debug, Clone)]
struct Entry {
    selected: bool,
    key: Option<String>,
}

/// A custom endpoint being defined, filled field by field.
#[derive(Debug, Clone, Default)]
struct CustomDraft {
    name: String,
    provider: Option<ProviderKind>,
    base_url: String,
    models: Vec<ModelDef>,
}

/// A single-line text field. `masked` fields echo bullets: an API key is
/// read in the open in a shared terminal often enough to matter.
#[derive(Debug, Clone, Default)]
struct TextInput {
    buf: String,
    masked: bool,
    pasted: bool,
}

impl TextInput {
    fn masked() -> Self {
        Self {
            masked: true,
            ..Self::default()
        }
    }

    fn echo(&self) -> String {
        if self.masked {
            "•".repeat(self.buf.chars().count())
        } else {
            self.buf.clone()
        }
    }

    fn value(&self) -> String {
        self.buf.trim().to_string()
    }
}

/// Where the key being typed belongs once it is confirmed.
#[derive(Debug, Clone)]
enum KeyTarget {
    Candidate(usize),
    /// The last field of the custom-endpoint form.
    Custom(CustomDraft),
}

#[derive(Debug, Clone)]
enum Step {
    Providers {
        cursor: usize,
    },
    Key {
        target: KeyTarget,
        label: String,
        var: String,
        input: TextInput,
    },
    CustomName {
        input: TextInput,
    },
    CustomProtocol {
        draft: CustomDraft,
        cursor: usize,
    },
    CustomBaseUrl {
        draft: CustomDraft,
        input: TextInput,
    },
    CustomModels {
        draft: CustomDraft,
        input: TextInput,
    },
    Model {
        options: Vec<ModelChoice>,
        cursor: usize,
    },
}

#[derive(Debug, Clone)]
struct ModelChoice {
    profile: String,
    model: String,
    effort: Option<String>,
    label: String,
}

const PROTOCOLS: [(&str, ProviderKind); 2] = [
    (
        "openai (Chat Completions / OpenAI-compatible)",
        ProviderKind::Openai,
    ),
    (
        "anthropic (Messages / Anthropic-compatible)",
        ProviderKind::Anthropic,
    ),
];

pub struct Setup {
    /// The user's own global config — the layer that gets serialized. It
    /// stays free of catalogue entries so `config.toml` keeps holding only
    /// what the user actually set.
    config: Config,
    /// Defaults merged with the user's config: what is offered on screen.
    catalogue: BTreeMap<String, Profile>,
    cands: Vec<Candidate>,
    entries: Vec<Entry>,
    customs: Vec<(String, Profile)>,
    /// Set when setup was opened because one profile is unusable. Only
    /// credentials are being fixed, so the model selection is left alone.
    missing_profile: Option<String>,
    step: Step,
    /// The existing `~/.tcode/state.toml` at the moment setup was opened.
    /// The wizard only manages `profile`, `model`, and `effort` — every
    /// other field (folder trust, agent pins, dogfood, …) must survive
    /// the save that follows.
    existing_state: ModelState,
}

impl Setup {
    /// `config` is the user's global config (`Config::default()` on first
    /// run). `missing_profile` names the profile whose credentials sent us
    /// here, if any. `existing_state` is the current `~/.tcode/state.toml`;
    /// callers should pass `ModelState::load()` unless this is the first-run
    /// wizard (in which case there is nothing to preserve).
    pub fn new(config: Config, missing_profile: Option<&str>, existing_state: ModelState) -> Self {
        // Defaults as the base layer, the user's overrides on top — the same
        // order `Config::load` uses, so the wizard offers what would run.
        let mut catalogue = Config::defaults().profiles;
        for (key, profile) in &config.profiles {
            match catalogue.get_mut(key) {
                Some(existing) => existing.merge(profile.clone()),
                None => {
                    catalogue.insert(key.clone(), profile.clone());
                }
            }
        }

        let cands = candidates(&catalogue);
        let entries = cands
            .iter()
            .map(|cand| Entry {
                selected: missing_profile
                    .map(|profile| cand.id == profile || cand.detected)
                    .unwrap_or(cand.detected),
                key: catalogue.get(&cand.id).and_then(|p| p.api_key.clone()),
            })
            .collect();

        Self {
            config,
            catalogue,
            cands,
            entries,
            customs: Vec::new(),
            missing_profile: missing_profile.map(String::from),
            step: Step::Providers { cursor: 0 },
            existing_state,
        }
    }

    pub fn view(&self) -> View {
        match &self.step {
            Step::Providers { cursor } => self.providers_view(*cursor),
            Step::Key {
                label, var, input, ..
            } => text_view(
                format!("{label} API key"),
                format!("empty = ${var}"),
                input,
                "<type or paste key here>",
            ),
            Step::CustomName { input } => text_view(
                "profile name".into(),
                "e.g. openrouter, groq, local".into(),
                input,
                "<name>",
            ),
            Step::CustomProtocol { cursor, .. } => View {
                title: "wire protocol:".into(),
                hint: "↑↓ move · enter confirm · esc cancel".into(),
                caret: false,
                rows: PROTOCOLS
                    .iter()
                    .enumerate()
                    .map(|(i, (label, _))| Row {
                        label: (*label).into(),
                        status: String::new(),
                        tone: Tone::Dim,
                        mark: Mark::None,
                        active: i == *cursor,
                    })
                    .collect(),
            },
            Step::CustomBaseUrl { input, .. } => text_view(
                "base URL".into(),
                "e.g. https://openrouter.ai/api/v1".into(),
                input,
                "<url>",
            ),
            Step::CustomModels { input, .. } => text_view(
                "model id(s)".into(),
                "comma-separated, e.g. gpt-5.6, deepseek-v4-pro".into(),
                input,
                "<model ids>",
            ),
            Step::Model { options, cursor } => View {
                title: "default model:".into(),
                hint: "↑↓ move · enter confirm · esc cancel".into(),
                caret: false,
                rows: options
                    .iter()
                    .enumerate()
                    .map(|(i, option)| Row {
                        label: option.label.clone(),
                        status: String::new(),
                        tone: Tone::Dim,
                        mark: Mark::None,
                        active: i == *cursor,
                    })
                    .collect(),
            },
        }
    }

    fn providers_view(&self, cursor: usize) -> View {
        let mut rows: Vec<Row> = self
            .cands
            .iter()
            .zip(&self.entries)
            .enumerate()
            .map(|(i, (cand, entry))| {
                // What the key will actually be, in the order the runtime
                // resolves it: inline key, then environment variable.
                let (status, tone) = match &entry.key {
                    Some(key) => (format!("key set ({} chars)", key.len()), Tone::Ok),
                    None => match cand
                        .key_env
                        .as_deref()
                        .filter(|var| std::env::var(var).is_ok())
                    {
                        Some(var) => (format!("${var}"), Tone::Ok),
                        None => (cand.status.clone(), cand.tone),
                    },
                };
                Row {
                    label: cand.title.clone(),
                    status,
                    tone,
                    mark: if entry.selected {
                        Mark::Checked
                    } else {
                        Mark::Unchecked
                    },
                    active: i == cursor,
                }
            })
            .collect();
        rows.extend(self.customs.iter().map(|(name, profile)| Row {
            label: name.clone(),
            status: format!("custom · {} model(s)", profile.models.len()),
            tone: Tone::Dim,
            mark: Mark::Checked,
            active: false,
        }));
        View {
            title: match &self.missing_profile {
                Some(profile) => format!("providers to configure — '{profile}' is not configured"),
                None => "providers to configure".into(),
            },
            hint: "↑↓ move · space toggle · tab key · c add custom endpoint · enter confirm · esc cancel"
                .into(),
            rows,
            caret: false,
        }
    }

    /// Bracketed paste. Text steps take it; elsewhere it is ignored rather
    /// than being replayed as keystrokes, which is how a pasted key used to
    /// toggle checkboxes.
    pub fn on_paste(&mut self, text: String) -> Progress {
        let text = text.trim();
        if !text.is_empty() {
            if let Some(input) = self.input_mut() {
                input.buf.push_str(text);
                input.pasted = true;
            }
        }
        Progress::Stay
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Progress {
        if key.kind == KeyEventKind::Release {
            return Progress::Stay;
        }
        if is_cancel(&key) {
            return self.cancel();
        }
        match &mut self.step {
            Step::Providers { .. } => self.providers_key(key),
            // Tab confirms as Enter does: it is what opened the field, and
            // tabbing back out of a filled key should keep it.
            Step::Key { .. } if matches!(key.code, KeyCode::Enter | KeyCode::Tab) => {
                self.confirm_key()
            }
            Step::CustomName { .. }
            | Step::CustomBaseUrl { .. }
            | Step::CustomModels { .. }
            | Step::Key { .. }
                if key.code == KeyCode::Enter =>
            {
                self.confirm_text()
            }
            Step::CustomProtocol { cursor, .. } => {
                let last = PROTOCOLS.len() - 1;
                match key.code {
                    KeyCode::Up => *cursor = cursor.saturating_sub(1),
                    KeyCode::Down => *cursor = (*cursor + 1).min(last),
                    KeyCode::Enter => return self.confirm_protocol(),
                    _ => {}
                }
                Progress::Stay
            }
            Step::Model { options, cursor } => {
                let last = options.len().saturating_sub(1);
                match key.code {
                    KeyCode::Up => *cursor = cursor.saturating_sub(1),
                    KeyCode::Down => *cursor = (*cursor + 1).min(last),
                    KeyCode::Enter => return self.confirm_model(),
                    _ => {}
                }
                Progress::Stay
            }
            _ => {
                self.edit_text(key);
                Progress::Stay
            }
        }
    }

    fn input_mut(&mut self) -> Option<&mut TextInput> {
        match &mut self.step {
            Step::Key { input, .. }
            | Step::CustomName { input }
            | Step::CustomBaseUrl { input, .. }
            | Step::CustomModels { input, .. } => Some(input),
            _ => None,
        }
    }

    fn edit_text(&mut self, key: KeyEvent) {
        let Some(input) = self.input_mut() else {
            return;
        };
        match key.code {
            KeyCode::Backspace => {
                input.buf.pop();
                input.pasted = false;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.buf.push(c);
            }
            _ => {}
        }
    }

    fn providers_key(&mut self, key: KeyEvent) -> Progress {
        let Step::Providers { cursor } = &mut self.step else {
            return Progress::Stay;
        };
        let last = self.cands.len().saturating_sub(1);
        match key.code {
            KeyCode::Up => *cursor = cursor.saturating_sub(1),
            KeyCode::Down => *cursor = (*cursor + 1).min(last),
            KeyCode::Char(' ') => {
                let at = *cursor;
                if let Some(entry) = self.entries.get_mut(at) {
                    entry.selected = !entry.selected;
                }
            }
            KeyCode::Char('c') => {
                self.step = Step::CustomName {
                    input: TextInput::default(),
                }
            }
            KeyCode::Tab => {
                let at = *cursor;
                if let Some(cand) = self.cands.get(at) {
                    self.step = Step::Key {
                        target: KeyTarget::Candidate(at),
                        label: cand.id.clone(),
                        var: cand.key_env.clone().unwrap_or_else(|| "API_KEY".into()),
                        input: TextInput::masked(),
                    };
                }
            }
            KeyCode::Enter => return self.confirm_providers(),
            _ => {}
        }
        Progress::Stay
    }

    /// Esc/Ctrl+C. Inside a sub-step it backs out to the provider list —
    /// abandoning one field must not throw away the selections already made.
    fn cancel(&mut self) -> Progress {
        match std::mem::replace(&mut self.step, Step::Providers { cursor: 0 }) {
            Step::Providers { .. } | Step::Model { .. } => Progress::Done(None),
            // A cancelled key entry still counts as choosing the provider:
            // the user meant to configure it, just from the environment.
            Step::Key {
                target: KeyTarget::Custom(draft),
                ..
            } => {
                self.push_custom(draft, None);
                Progress::Stay
            }
            _ => Progress::Stay,
        }
    }

    fn confirm_key(&mut self) -> Progress {
        let Step::Key { target, input, .. } =
            std::mem::replace(&mut self.step, Step::Providers { cursor: 0 })
        else {
            return Progress::Stay;
        };
        let value = input.value();
        let key = (!value.is_empty()).then_some(value);
        match target {
            KeyTarget::Candidate(at) => {
                if let Some(entry) = self.entries.get_mut(at) {
                    entry.key = key;
                    // Typing (or deliberately skipping) a key is itself the
                    // choice to use this provider.
                    entry.selected = true;
                }
                self.step = Step::Providers { cursor: at };
            }
            KeyTarget::Custom(draft) => self.push_custom(draft, key),
        }
        Progress::Stay
    }

    /// A finished custom endpoint joins the list already selected — it only
    /// exists because the user typed it in.
    fn push_custom(&mut self, draft: CustomDraft, api_key: Option<String>) {
        let profile = Profile {
            provider: draft.provider,
            model: None,
            models: draft.models,
            api_key,
            // No inline key → fall back to <NAME>_API_KEY (uppercased).
            api_key_env: Some(format!("{}_API_KEY", draft.name.to_ascii_uppercase())),
            base_url: (!draft.base_url.is_empty()).then_some(draft.base_url),
            max_tokens: None,
            context_window: None,
            vision: None,
        };
        self.customs.push((draft.name, profile));
        self.step = Step::Providers { cursor: 0 };
    }

    fn confirm_text(&mut self) -> Progress {
        let step = std::mem::replace(&mut self.step, Step::Providers { cursor: 0 });
        self.step = match step {
            Step::CustomName { input } => {
                let name = input.value();
                // An unnamed profile has no key to live under; drop back out.
                if name.is_empty() {
                    return Progress::Stay;
                }
                Step::CustomProtocol {
                    draft: CustomDraft {
                        name,
                        ..CustomDraft::default()
                    },
                    cursor: 0,
                }
            }
            Step::CustomBaseUrl { mut draft, input } => {
                draft.base_url = input.value();
                Step::CustomModels {
                    draft,
                    input: TextInput::default(),
                }
            }
            Step::CustomModels { mut draft, input } => {
                draft.models = input
                    .value()
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ModelDef::bare)
                    .collect();
                Step::Key {
                    label: draft.name.clone(),
                    var: format!("{}_API_KEY", draft.name.to_ascii_uppercase()),
                    target: KeyTarget::Custom(draft),
                    input: TextInput::masked(),
                }
            }
            other => other,
        };
        Progress::Stay
    }

    fn confirm_protocol(&mut self) -> Progress {
        let Step::CustomProtocol { mut draft, cursor } =
            std::mem::replace(&mut self.step, Step::Providers { cursor: 0 })
        else {
            return Progress::Stay;
        };
        draft.provider = Some(PROTOCOLS[cursor].1);
        self.step = Step::CustomBaseUrl {
            draft,
            input: TextInput::default(),
        };
        Progress::Stay
    }

    /// Fold the selections into the config, then either finish (credentials
    /// were the whole job) or move on to picking a default model.
    fn confirm_providers(&mut self) -> Progress {
        let chosen: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.selected)
            .map(|(i, _)| i)
            .collect();
        if chosen.is_empty() && self.customs.is_empty() {
            return Progress::Done(None);
        }

        for &i in &chosen {
            let id = self.cands[i].id.clone();
            let key = self.entries[i].key.clone();
            match self.config.profiles.get_mut(&id) {
                Some(profile) => {
                    if key.is_some() {
                        profile.api_key = key;
                    }
                }
                // Catalogue-only profile: write the thinnest possible patch,
                // so models/base_url keep coming from the built-in layer and
                // the user's file stays readable.
                None => {
                    let mut patch = Profile::patch();
                    patch.api_key = key;
                    self.config.profiles.insert(id, patch);
                }
            }
        }
        for (name, profile) in std::mem::take(&mut self.customs) {
            self.config.profiles.insert(name, profile);
        }

        // Everything just configured, presets first, then custom endpoints.
        let mut configured: Vec<String> =
            chosen.iter().map(|&i| self.cands[i].id.clone()).collect();
        configured.extend(
            self.config
                .profiles
                .keys()
                .filter(|k| !self.cands.iter().any(|c| &c.id == *k))
                .cloned(),
        );

        if let Some(profile) = self.missing_profile.clone() {
            self.config.default_profile = Some(profile);
            return self.done(self.existing_state.clone());
        }

        // Models are offered from the *merged* view, the one the runtime will
        // resolve: the user's layer holds only patches, so asking it alone
        // would find no models on any builtin profile. Codex's list is a
        // runtime observation — needed to choose, never written back.
        let mut model_config = self.config.clone();
        model_config.profiles = self.merged_profiles();
        tcode_providers::hydrate_codex_models(&mut model_config);
        let options: Vec<ModelChoice> = configured
            .iter()
            .filter_map(|name| Some((name, model_config.profiles.get(name)?)))
            .flat_map(|(name, profile)| {
                profile
                    .model_defs()
                    .into_iter()
                    .map(move |def| ModelChoice {
                        label: format!("{name} · {}", def.display()),
                        profile: name.clone(),
                        model: def.name,
                        effort: def.default_effort,
                    })
            })
            .collect();

        match options.len() {
            // Nothing to choose from: keep the first configured profile as
            // the default and let the user name a model in config.toml.
            0 => {
                self.config.default_profile = configured.first().cloned();
                self.done(self.existing_state.clone())
            }
            // A single model is not a question worth asking.
            1 => self.pick_model(&options[0]),
            _ => {
                self.step = Step::Model { options, cursor: 0 };
                Progress::Stay
            }
        }
    }

    /// The catalogue with the user's edits applied — what `Config::load`
    /// will produce once this is written to disk.
    fn merged_profiles(&self) -> BTreeMap<String, Profile> {
        let mut merged = self.catalogue.clone();
        for (name, profile) in &self.config.profiles {
            match merged.get_mut(name) {
                Some(existing) => existing.merge(profile.clone()),
                None => {
                    merged.insert(name.clone(), profile.clone());
                }
            }
        }
        merged
    }

    fn confirm_model(&mut self) -> Progress {
        let Step::Model { options, cursor } = &self.step else {
            return Progress::Stay;
        };
        let choice = options[*cursor].clone();
        self.pick_model(&choice)
    }

    fn pick_model(&mut self, choice: &ModelChoice) -> Progress {
        self.config.default_profile = Some(choice.profile.clone());
        let mut state = self.existing_state.clone();
        state.profile = Some(choice.profile.clone());
        state.model = Some(choice.model.clone());
        state.effort = choice.effort.clone();
        self.done(state)
    }

    fn done(&mut self, state: ModelState) -> Progress {
        Progress::Done(Some(Box::new((self.config.clone(), state))))
    }
}

fn text_view(title: String, hint: String, input: &TextInput, placeholder: &str) -> View {
    let empty = input.buf.is_empty();
    View {
        caret: true,
        title,
        hint: if input.pasted {
            "pasted · enter confirm · esc back".into()
        } else if empty {
            format!("{hint} · enter confirm · esc back")
        } else {
            "enter confirm · esc back".into()
        },
        rows: vec![Row {
            label: if empty {
                placeholder.into()
            } else {
                input.echo()
            },
            status: String::new(),
            tone: if empty { Tone::Dim } else { Tone::Ok },
            mark: Mark::None,
            active: true,
        }],
    }
}

fn is_cancel(key: &KeyEvent) -> bool {
    key.code == KeyCode::Esc
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

fn candidates(profiles: &BTreeMap<String, Profile>) -> Vec<Candidate> {
    profiles
        .iter()
        .map(|(id, profile)| {
            let key_env = profile.api_key_env.clone();
            // Codex owns its credentials; every other provider is usable once
            // a key can be resolved.
            let (status, tone, detected) = if profile.provider == Some(ProviderKind::Codex) {
                match codex_auth_available() {
                    true => ("~/.codex/auth.json ✓".to_string(), Tone::Ok, true),
                    false => (
                        "not logged in — run `codex login` first".to_string(),
                        Tone::Dim,
                        false,
                    ),
                }
            } else if key_env.as_deref().is_some_and(|v| std::env::var(v).is_ok()) {
                (
                    format!("${} ✓", key_env.as_deref().unwrap_or_default()),
                    Tone::Ok,
                    true,
                )
            } else if profile.api_key.is_some() {
                ("inline key ✓".to_string(), Tone::Ok, true)
            } else {
                ("not configured".to_string(), Tone::Dim, false)
            };
            let provider_label = match profile.provider {
                Some(ProviderKind::Anthropic) => "Anthropic",
                Some(ProviderKind::Openai) => "OpenAI",
                Some(ProviderKind::Codex) => "Codex",
                None => "unset",
            };
            Candidate {
                id: id.clone(),
                title: format!("{id} ({provider_label})"),
                status,
                tone,
                detected,
                key_env,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press(setup: &mut Setup, codes: &[KeyCode]) -> Option<Box<(Config, ModelState)>> {
        let mut result = None;
        for &code in codes {
            if let Progress::Done(outcome) = setup.on_key(key(code)) {
                result = outcome;
            }
        }
        result
    }

    /// Walk to the row for `id` from the top of the list.
    fn go_to(setup: &mut Setup, id: &str) {
        let at = setup
            .cands
            .iter()
            .position(|c| c.id == id)
            .unwrap_or_else(|| panic!("no candidate '{id}'"));
        for _ in 0..at {
            setup.on_key(key(KeyCode::Down));
        }
    }

    /// Auto-detection reads the ambient environment (env keys, codex login),
    /// so tests that care about the outcome start from an empty selection.
    fn deselect_all(setup: &mut Setup) {
        for entry in &mut setup.entries {
            entry.selected = false;
        }
    }

    fn type_str(setup: &mut Setup, text: &str) {
        for c in text.chars() {
            setup.on_key(key(KeyCode::Char(c)));
        }
    }

    /// The wizard writes the user's layer, not the merged view: a builtin
    /// profile gets a key and nothing else, so `provider`/`base_url`/`models`
    /// keep coming from the catalogue and stay correct when it is updated.
    #[test]
    fn a_builtin_profile_is_saved_as_a_key_only_patch() {
        let mut setup = Setup::new(Config::default(), None, ModelState::default());
        // Which providers auto-detect depends on this machine's environment;
        // start from nothing so the walk below is the only thing under test.
        deselect_all(&mut setup);
        go_to(&mut setup, "deepseek");
        setup.on_key(key(KeyCode::Tab));
        type_str(&mut setup, "sk-typed");
        // key → back to the list, confirm the list, confirm the model.
        let saved = press(
            &mut setup,
            &[KeyCode::Enter, KeyCode::Enter, KeyCode::Enter],
        )
        .expect("setup completes");

        let (config, state) = *saved;
        let written = &config.profiles["deepseek"];
        assert_eq!(written.api_key.as_deref(), Some("sk-typed"));
        assert_eq!(written.provider, None, "provider stays in the catalogue");
        assert_eq!(written.base_url, None);
        assert!(written.models.is_empty());
        // What was written must still resolve once the layers are merged.
        let mut merged = Config::defaults();
        merged
            .profiles
            .get_mut("deepseek")
            .unwrap()
            .merge(config.profiles["deepseek"].clone());
        assert_eq!(
            merged.profiles["deepseek"].provider,
            Some(ProviderKind::Anthropic)
        );
        assert_eq!(state.profile.as_deref(), Some("deepseek"));
    }

    /// A custom endpoint is the one case that must carry a provider of its
    /// own: no layer below it knows the name.
    #[test]
    fn a_custom_endpoint_is_written_whole() {
        let mut setup = Setup::new(Config::default(), None, ModelState::default());
        deselect_all(&mut setup);
        setup.on_key(key(KeyCode::Char('c')));
        type_str(&mut setup, "groq");
        setup.on_key(key(KeyCode::Enter)); // name → protocol
        setup.on_key(key(KeyCode::Enter)); // openai
        type_str(&mut setup, "https://api.groq.com/openai/v1");
        setup.on_key(key(KeyCode::Enter)); // base url → models
        type_str(&mut setup, "llama-4, mixtral");
        setup.on_key(key(KeyCode::Enter)); // models → key
        type_str(&mut setup, "sk-groq");
        setup.on_key(key(KeyCode::Enter)); // key → back to the list

        let saved = press(&mut setup, &[KeyCode::Enter, KeyCode::Enter]).expect("setup completes");
        let profile = &saved.0.profiles["groq"];
        assert_eq!(profile.provider, Some(ProviderKind::Openai));
        assert_eq!(
            profile.base_url.as_deref(),
            Some("https://api.groq.com/openai/v1")
        );
        assert_eq!(profile.api_key.as_deref(), Some("sk-groq"));
        assert_eq!(profile.api_key_env.as_deref(), Some("GROQ_API_KEY"));
        let models: Vec<&str> = profile.models.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(models, ["llama-4", "mixtral"]);
    }

    /// Esc inside a field returns to the list; the selections made so far
    /// must survive it. Only Esc on the list itself abandons setup.
    #[test]
    fn escaping_a_field_keeps_the_selection() {
        let mut setup = Setup::new(Config::default(), None, ModelState::default());
        go_to(&mut setup, "deepseek");
        setup.on_key(key(KeyCode::Char(' ')));
        setup.on_key(key(KeyCode::Char('c')));
        type_str(&mut setup, "half-typed");
        setup.on_key(key(KeyCode::Esc));

        assert!(
            matches!(setup.step, Step::Providers { .. }),
            "esc backs out to the list"
        );
        assert!(setup.customs.is_empty(), "the abandoned draft is dropped");
        let row = setup
            .view()
            .rows
            .into_iter()
            .find(|r| r.label.starts_with("deepseek"))
            .expect("deepseek row");
        assert_eq!(row.mark, Mark::Checked);

        assert!(matches!(
            setup.on_key(key(KeyCode::Esc)),
            Progress::Done(None)
        ));
    }

    /// Reopening for one unusable profile only fixes credentials: it must not
    /// silently move the user's default model.
    #[test]
    fn reconfiguring_one_profile_leaves_the_model_choice_alone() {
        let mut setup = Setup::new(Config::default(), Some("openrouter"), ModelState::default());
        let row = setup
            .view()
            .rows
            .into_iter()
            .find(|r| r.label.starts_with("openrouter "))
            .expect("openrouter row");
        assert_eq!(row.mark, Mark::Checked, "the named profile starts selected");

        let saved = press(&mut setup, &[KeyCode::Enter]).expect("setup completes");
        let (config, state) = *saved;
        assert_eq!(config.default_profile.as_deref(), Some("openrouter"));
        assert_eq!(state.model, None, "no model was chosen here");
    }

    /// A paste is content, never commands: the API key is bracketed-pasted
    /// into the field, and on the list it must not act as keystrokes.
    #[test]
    fn paste_lands_in_the_field_and_is_inert_on_the_list() {
        let mut setup = Setup::new(Config::default(), None, ModelState::default());
        setup.on_paste("sk-pasted".into());
        assert!(setup.customs.is_empty());
        assert!(matches!(setup.step, Step::Providers { .. }));

        go_to(&mut setup, "deepseek");
        setup.on_key(key(KeyCode::Tab));
        setup.on_paste("  sk-pasted  ".into());
        let view = setup.view();
        assert_eq!(view.rows[0].label, "•".repeat(9), "keys echo masked");
        setup.on_key(key(KeyCode::Enter));

        let at = setup.cands.iter().position(|c| c.id == "deepseek").unwrap();
        assert_eq!(setup.entries[at].key.as_deref(), Some("sk-pasted"));
    }

    /// Confirming with nothing selected is a cancellation, not an empty
    /// config that would overwrite what is already on disk.
    #[test]
    fn confirming_an_empty_selection_cancels() {
        let mut setup = Setup::new(Config::default(), None, ModelState::default());
        for entry in &mut setup.entries {
            entry.selected = false;
        }
        assert!(matches!(
            setup.on_key(key(KeyCode::Enter)),
            Progress::Done(None)
        ));
    }
}
