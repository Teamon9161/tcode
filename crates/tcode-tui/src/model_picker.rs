//! `/model` opens one hub (`Hub`) over the whole model line-up: the main
//! model, the named presets that switch the line-up as a unit, and every
//! sub-agent and helper role. Drilling into any of them opens the same
//! `Picker`: ↑↓ chooses a model across all configured profiles, ←→ adjusts
//! its reasoning effort, Enter applies, Esc backs out one step. A role's
//! picker also offers explicit `inherit` (use the main model) and, for opt-in
//! capabilities such as `web-fetch`, `off`.
//!
//! `/agents` is the same hub, opened on the sub-agent section: pinning one
//! role and switching the whole line-up are the same task seen at two
//! altitudes, and splitting them across two dialogs is what made re-pinning
//! eight roles the only way to change provider family.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::text::{Line, Span};
use tcode_core::config::ModelDef;

use crate::theme;

// The menu *data* (ModelMenu/AgentMenu/PresetMenu and their closures) lives in
// `tcode-frontend` so a non-TUI frontend can render the same line-up without
// linking ratatui. This module keeps only the widgets that draw them.
pub use tcode_frontend::menu::{
    AgentMenu, AgentModelChoice, AgentRole, ApplyPresetFn, ModelMenu, ModelOption, PinFn,
    PresetDraft, PresetMenu, PresetOption, RoleSection, SavePresetFn, SwitchFn,
};

struct Row {
    /// Menu option this row selects; `None` identifies a synthetic mode row.
    option: Option<usize>,
    mode: Option<AgentModelChoice>,
    /// Effort slots: "auto" plus the model's declared levels.
    slots: Vec<String>,
    slot: usize,
}

pub struct Picker {
    title: String,
    rows: Vec<Row>,
    selected: usize,
    /// Row marked as the one in force (`✓`).
    current: usize,
    hovered: Option<usize>,
}

pub enum PickResult {
    Pending,
    Cancelled,
    /// `option` is `None` for the inherit row (agent picker only).
    Picked {
        option: Option<usize>,
        effort: Option<String>,
    },
}

const AUTO: &str = "auto";

fn slots_for(def: &ModelDef, want: Option<&str>) -> (Vec<String>, usize) {
    let mut slots = vec![AUTO.to_string()];
    slots.extend(def.efforts.iter().cloned());
    let slot = want
        .and_then(|w| slots.iter().position(|s| s == w))
        .unwrap_or(0);
    (slots, slot)
}

impl Picker {
    /// The main model: every option, starting on the active one.
    pub fn new(menu: &ModelMenu, current_effort: Option<&str>) -> Option<Self> {
        if menu.options.is_empty() {
            return None;
        }
        let rows = menu
            .options
            .iter()
            .enumerate()
            .map(|(i, opt)| {
                // Start at the live effort for the active model, at the
                // configured default for the others.
                let want = if i == menu.current {
                    current_effort
                } else {
                    opt.def.default_effort.as_deref()
                };
                let (slots, slot) = slots_for(&opt.def, want);
                Row {
                    option: Some(i),
                    mode: None,
                    slots,
                    slot,
                }
            })
            .collect();
        Some(Self {
            title: "◈ model".into(),
            rows,
            selected: menu.current,
            current: menu.current,
            hovered: None,
        })
    }

    /// A role's picker: its non-model modes first, then the shared model grid.
    /// `inherit` always means use the live main model; opt-in roles
    /// additionally expose `off`.
    pub fn for_agent(menu: &ModelMenu, role: &AgentRole, pin: &AgentModelChoice) -> Self {
        let mut rows = Vec::new();
        if role.allows_off {
            rows.push(Row {
                option: None,
                mode: Some(AgentModelChoice::Off),
                slots: vec![String::new()],
                slot: 0,
            });
        }
        rows.push(Row {
            option: None,
            mode: Some(AgentModelChoice::Inherit),
            slots: vec![String::new()],
            slot: 0,
        });
        for (i, opt) in menu.options.iter().enumerate() {
            let want = match pin {
                AgentModelChoice::Model { option, effort } if *option == i => effort.as_deref(),
                _ => opt.def.default_effort.as_deref(),
            };
            let (slots, slot) = slots_for(&opt.def, want);
            rows.push(Row {
                option: Some(i),
                mode: None,
                slots,
                slot,
            });
        }
        let current = rows
            .iter()
            .position(|row| match (&row.mode, &row.option, pin) {
                (Some(mode), _, _) => mode == pin,
                (None, Some(option), AgentModelChoice::Model { option: pinned, .. }) => {
                    option == pinned
                }
                _ => false,
            })
            .unwrap_or(0);
        Self {
            title: format!("◈ agent: {}", role.label),
            rows,
            selected: current,
            current,
            hovered: None,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PickResult {
        match key.code {
            KeyCode::Esc => return PickResult::Cancelled,
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(self.rows.len() - 1),
            // Effort cycles: past either end wraps around.
            KeyCode::Left => {
                let row = &mut self.rows[self.selected];
                row.slot = row.slot.checked_sub(1).unwrap_or(row.slots.len() - 1);
            }
            KeyCode::Right => {
                let row = &mut self.rows[self.selected];
                row.slot = (row.slot + 1) % row.slots.len();
            }
            KeyCode::Enter => {
                let row = &self.rows[self.selected];
                let effort =
                    (row.option.is_some() && row.slot > 0).then(|| row.slots[row.slot].clone());
                return PickResult::Picked {
                    option: row.option,
                    effort,
                };
            }
            _ => {}
        }
        PickResult::Pending
    }

    /// `row` is the border-free rendered content row. The title occupies row
    /// zero; following rows are the visible model window. Clicking a row both
    /// selects and applies it, preserving the keyboard Enter behaviour.
    fn agent_choice(&self) -> AgentModelChoice {
        let row = &self.rows[self.selected];
        row.mode.clone().unwrap_or_else(|| AgentModelChoice::Model {
            option: row.option.expect("model row has an option"),
            effort: (row.slot > 0).then(|| row.slots[row.slot].clone()),
        })
    }

    pub fn handle_mouse_row(&mut self, row: usize) -> PickResult {
        const WINDOW: usize = 8;
        let start = self
            .selected
            .saturating_sub(WINDOW / 2)
            .min(self.rows.len().saturating_sub(WINDOW));
        let Some(index) = row
            .checked_sub(1)
            .map(|offset| start + offset)
            .filter(|index| *index < (start + WINDOW).min(self.rows.len()))
        else {
            return PickResult::Pending;
        };
        self.selected = index;
        let row = &self.rows[index];
        let effort = (row.option.is_some() && row.slot > 0).then(|| row.slots[row.slot].clone());
        PickResult::Picked {
            option: row.option,
            effort,
        }
    }

    pub fn set_hovered_row(&mut self, row: Option<usize>) {
        const WINDOW: usize = 8;
        let start = self
            .selected
            .saturating_sub(WINDOW / 2)
            .min(self.rows.len().saturating_sub(WINDOW));
        self.hovered = row
            .and_then(|row| row.checked_sub(1))
            .map(|offset| start + offset)
            .filter(|&index| index < (start + WINDOW).min(self.rows.len()));
    }

    pub fn render(&self, menu: &ModelMenu) -> Vec<Line<'static>> {
        let mut out = vec![Line::from(vec![Span::styled(
            self.title.clone(),
            theme::bold().fg(theme::ACCENT),
        )])];
        let total = self.rows.len();
        let window = 8usize;
        let start = self
            .selected
            .saturating_sub(window / 2)
            .min(total.saturating_sub(window));
        for i in start..(start + window).min(total) {
            let row = &self.rows[i];
            let is_sel = i == self.selected;
            let marker = if is_sel { "▸ " } else { "  " };
            let current = if i == self.current { " ✓" } else { "" };
            let base_style = if is_sel {
                theme::bold().fg(theme::ACCENT)
            } else {
                theme::dim()
            };
            let style = if self.hovered == Some(i) {
                theme::hover_style(base_style)
            } else {
                base_style
            };
            let Some(option) = row.option else {
                let label = match row.mode.as_ref() {
                    Some(AgentModelChoice::Off) => "off — do not run this capability",
                    Some(AgentModelChoice::Inherit) => "inherit — use the main model",
                    _ => unreachable!("synthetic agent row has a mode"),
                };
                out.push(Line::styled(format!("  {marker}{label}{current}"), style));
                continue;
            };
            let opt = &menu.options[option];
            let effort = if row.slots.len() > 1 {
                let e = &row.slots[row.slot];
                if is_sel {
                    format!("  ‹ {e} ›")
                } else {
                    format!("  ({e})")
                }
            } else {
                String::new()
            };
            let ctx = opt
                .def
                .context_window
                .map(|c| format!("  {}k", c / 1000))
                .unwrap_or_default();
            out.push(Line::styled(
                format!("  {marker}{}{ctx}{effort}{current}", opt.title()),
                style,
            ));
        }
        out.push(Line::styled(
            "  ↑↓ model · ←→ effort · enter apply · esc back",
            theme::dim(),
        ));
        out
    }
}

/// Everything the hub lists, in display order. Headers are rendered but never
/// selected; the arrow keys step over them.
enum Entry {
    Header(&'static str),
    Main,
    Preset(usize),
    SavePreset,
    Agent(usize),
}

impl Entry {
    fn selectable(&self) -> bool {
        !matches!(self, Entry::Header(_))
    }
}

/// The hub's rows, rebuilt from the menus on every event rather than cached:
/// applying a pick from inside the hub changes what the rows say, and a stale
/// copy would show the user the state they just left.
fn entries(agents: &AgentMenu, presets: &PresetMenu) -> Vec<Entry> {
    let mut out = vec![Entry::Main, Entry::Header("presets")];
    out.extend((0..presets.options.len()).map(Entry::Preset));
    out.push(Entry::SavePreset);
    for (header, section) in [
        ("sub-agents", RoleSection::Task),
        ("roles", RoleSection::Helper),
    ] {
        let mut members = agents
            .roles
            .iter()
            .enumerate()
            .filter(|(_, role)| role.section == section)
            .peekable();
        if members.peek().is_none() {
            continue;
        }
        out.push(Entry::Header(header));
        out.extend(members.map(|(i, _)| Entry::Agent(i)));
    }
    out
}

/// The menus a hub row reads, plus the one fact no menu can hold: the
/// reasoning effort of the *running* provider.
pub struct HubCtx<'a> {
    pub menu: &'a ModelMenu,
    pub agents: &'a AgentMenu,
    pub presets: &'a PresetMenu,
    pub effort: Option<&'a str>,
}

/// Where a drilled-into `Picker` sends its result.
enum Target {
    Main,
    Agent(usize),
}

/// `/model`: the whole line-up on one screen. Owns the drill-down picker and
/// the one text field in the dialog (naming a preset).
pub struct Hub {
    selected: usize,
    hovered: Option<usize>,
    open: Option<(Target, Picker)>,
    /// Typing a name for `save as preset`. `Some("")` is a live empty field,
    /// which is why this is not just a `String`.
    naming: Option<String>,
}

pub enum HubPick {
    Pending,
    Cancelled,
    /// The main model changed; same payload as the standalone model pick.
    Model {
        option: usize,
        effort: Option<String>,
    },
    Agent {
        kind: String,
        choice: AgentModelChoice,
    },
    Preset(String),
    SavePreset(String),
}

const WINDOW: usize = 12;

impl Hub {
    /// `focus_agents` starts on the first sub-agent instead of the main model,
    /// which is all `/agents` still means.
    pub fn new(agents: &AgentMenu, presets: &PresetMenu, focus_agents: bool) -> Self {
        let rows = entries(agents, presets);
        let selected = focus_agents
            .then(|| {
                rows.iter()
                    .position(|entry| matches!(entry, Entry::Agent(_)))
            })
            .flatten()
            .unwrap_or(0);
        Self {
            selected,
            hovered: None,
            open: None,
            naming: None,
        }
    }

    fn start(&self, total: usize) -> usize {
        self.selected
            .saturating_sub(WINDOW / 2)
            .min(total.saturating_sub(WINDOW))
    }

    /// Step to the next selectable row in `forward` direction, staying put at
    /// the ends rather than landing on a header that selects nothing.
    fn step(&mut self, rows: &[Entry], forward: bool) {
        let mut index = self.selected;
        loop {
            index = match forward {
                true if index + 1 < rows.len() => index + 1,
                false if index > 0 => index - 1,
                _ => return,
            };
            if rows[index].selectable() {
                self.selected = index;
                return;
            }
        }
    }

    /// Open the picker a row leads to, or report the choice a row *is*.
    fn activate(&mut self, index: usize, ctx: &HubCtx) -> HubPick {
        let HubCtx {
            menu,
            agents,
            presets,
            effort,
        } = ctx;
        let rows = entries(agents, presets);
        let Some(entry) = rows.get(index).filter(|entry| entry.selectable()) else {
            return HubPick::Pending;
        };
        self.selected = index;
        match entry {
            Entry::Header(_) => HubPick::Pending,
            Entry::Main => {
                if let Some(picker) = Picker::new(menu, *effort) {
                    self.open = Some((Target::Main, picker));
                }
                HubPick::Pending
            }
            Entry::Agent(role) => {
                let picker = Picker::for_agent(menu, &agents.roles[*role], &agents.pins[*role]);
                self.open = Some((Target::Agent(*role), picker));
                HubPick::Pending
            }
            Entry::Preset(option) => HubPick::Preset(presets.options[*option].key.clone()),
            Entry::SavePreset => {
                self.naming = Some(String::new());
                HubPick::Pending
            }
        }
    }

    /// The drill-down picker resolved. Esc there backs out to the hub, not out
    /// of the dialog: a wrong turn should cost one key, not the whole visit.
    fn resolve(&mut self, result: PickResult, agents: &AgentMenu) -> HubPick {
        let Some((target, picker)) = self.open.as_mut() else {
            return HubPick::Pending;
        };
        match result {
            PickResult::Pending => HubPick::Pending,
            PickResult::Cancelled => {
                self.open = None;
                HubPick::Pending
            }
            PickResult::Picked { option, effort } => {
                let pick = match target {
                    Target::Main => match option {
                        Some(option) => HubPick::Model { option, effort },
                        // The main model's rows always carry an option; the
                        // inherit and off rows only exist in a role's picker.
                        None => HubPick::Pending,
                    },
                    Target::Agent(role) => HubPick::Agent {
                        kind: agents.roles[*role].key.clone(),
                        choice: picker.agent_choice(),
                    },
                };
                self.open = None;
                pick
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, ctx: &HubCtx) -> HubPick {
        if let Some(name) = self.naming.as_mut() {
            match key.code {
                KeyCode::Esc => self.naming = None,
                KeyCode::Backspace => {
                    name.pop();
                }
                KeyCode::Char(c) => name.push(c),
                KeyCode::Enter if !name.is_empty() => {
                    let name = self.naming.take().unwrap_or_default();
                    return HubPick::SavePreset(name);
                }
                _ => {}
            }
            return HubPick::Pending;
        }
        if self.open.is_some() {
            let result = match self.open.as_mut() {
                Some((_, picker)) => picker.handle_key(key),
                None => PickResult::Pending,
            };
            return self.resolve(result, ctx.agents);
        }
        let rows = entries(ctx.agents, ctx.presets);
        match key.code {
            KeyCode::Esc => return HubPick::Cancelled,
            KeyCode::Up => self.step(&rows, false),
            KeyCode::Down => self.step(&rows, true),
            KeyCode::Enter => return self.activate(self.selected, ctx),
            _ => {}
        }
        HubPick::Pending
    }

    pub fn handle_mouse_row(&mut self, row: usize, ctx: &HubCtx) -> HubPick {
        if self.naming.is_some() {
            return HubPick::Pending;
        }
        if self.open.is_some() {
            let result = match self.open.as_mut() {
                Some((_, picker)) => picker.handle_mouse_row(row),
                None => PickResult::Pending,
            };
            return self.resolve(result, ctx.agents);
        }
        let rows = entries(ctx.agents, ctx.presets);
        let start = self.start(rows.len());
        let Some(index) = row
            .checked_sub(1)
            .map(|offset| start + offset)
            .filter(|index| *index < (start + WINDOW).min(rows.len()))
        else {
            return HubPick::Pending;
        };
        self.activate(index, ctx)
    }

    pub fn set_hovered_row(&mut self, row: Option<usize>, ctx: &HubCtx) {
        if let Some((_, picker)) = self.open.as_mut() {
            picker.set_hovered_row(row);
            return;
        }
        let rows = entries(ctx.agents, ctx.presets);
        let start = self.start(rows.len());
        self.hovered = row
            .and_then(|row| row.checked_sub(1))
            .map(|offset| start + offset)
            .filter(|&index| index < (start + WINDOW).min(rows.len()) && rows[index].selectable());
    }

    /// What the main row says, given the live reasoning effort — which the
    /// menu cannot know, since it is a property of the running provider.
    fn main_line(menu: &ModelMenu, effort: Option<&str>) -> String {
        let Some(option) = menu.options.get(menu.current) else {
            return "no models configured".to_string();
        };
        match effort {
            Some(effort) => format!("{} ({effort})", option.title()),
            None => option.title(),
        }
    }

    pub fn render(&self, ctx: &HubCtx) -> Vec<Line<'static>> {
        let HubCtx {
            menu,
            agents,
            presets,
            effort,
        } = ctx;
        if let Some((_, picker)) = &self.open {
            return picker.render(menu);
        }
        let mut out = vec![Line::from(vec![Span::styled(
            "◈ model",
            theme::bold().fg(theme::ACCENT),
        )])];
        let rows = entries(agents, presets);
        let start = self.start(rows.len());
        for (index, entry) in rows.iter().enumerate().skip(start).take(WINDOW) {
            let is_sel = index == self.selected;
            let marker = if is_sel { "▸ " } else { "  " };
            let base_style = if is_sel {
                theme::bold().fg(theme::ACCENT)
            } else {
                theme::dim()
            };
            let style = if self.hovered == Some(index) {
                theme::hover_style(base_style)
            } else {
                base_style
            };
            out.push(match entry {
                Entry::Header(text) => Line::styled(format!("  {text}"), theme::dim()),
                // The main model is what most visits are about, so it stays
                // accented even when the cursor is elsewhere.
                Entry::Main => Line::styled(
                    format!("  {marker}main    {}", Self::main_line(menu, *effort)),
                    if is_sel {
                        style
                    } else {
                        theme::bold().fg(theme::ACCENT)
                    },
                ),
                Entry::Preset(option) => {
                    let current = if presets.current == Some(*option) {
                        " ✓"
                    } else {
                        ""
                    };
                    Line::styled(
                        format!("    {marker}{}{current}", presets.options[*option].label),
                        style,
                    )
                }
                Entry::SavePreset => Line::styled(
                    match &self.naming {
                        Some(name) => format!("    {marker}name it: {name}▏"),
                        None => format!("    {marker}save this line-up as a preset…"),
                    },
                    style,
                ),
                Entry::Agent(role) => Line::styled(
                    format!(
                        "    {marker}{:<12}{}",
                        agents.roles[*role].label,
                        agents.describe(*role, menu)
                    ),
                    style,
                ),
            });
        }
        out.push(Line::styled(
            match self.naming {
                Some(_) => "  type a name · enter save · esc cancel",
                None => "  ↑↓ move · enter open · esc close",
            },
            theme::dim(),
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu() -> ModelMenu {
        let mut flash = ModelDef::bare("deepseek-v4-flash[1m]");
        flash.efforts = vec!["off".into(), "low".into(), "high".into()];
        ModelMenu {
            options: vec![
                ModelOption {
                    profile: "deepseek".into(),
                    def: flash,
                },
                ModelOption {
                    profile: "codex".into(),
                    def: ModelDef::bare("gpt-5.5"),
                },
            ],
            current: 0,
            switch: Box::new(|_, _| Err("not built in tests".into())),
        }
    }

    fn agents(pins: Vec<AgentModelChoice>) -> AgentMenu {
        AgentMenu {
            roles: vec![
                AgentRole {
                    key: "explore".into(),
                    label: "explore".into(),
                    allows_off: false,
                    section: RoleSection::Task,
                },
                AgentRole {
                    key: "general".into(),
                    label: "general".into(),
                    allows_off: false,
                    section: RoleSection::Task,
                },
            ],
            pins,
            pin: Box::new(|_, _| Err("not applied in tests".into())),
        }
    }

    fn presets(names: &[&str], current: Option<usize>) -> PresetMenu {
        PresetMenu {
            options: names
                .iter()
                .map(|name| PresetOption {
                    key: (*name).into(),
                    label: (*name).into(),
                })
                .collect(),
            current,
            apply: Box::new(|_| Err("not applied in tests".into())),
            save: Box::new(|_, _, _| Err("not saved in tests".into())),
        }
    }

    fn ctx<'a>(menu: &'a ModelMenu, agents: &'a AgentMenu, presets: &'a PresetMenu) -> HubCtx<'a> {
        HubCtx {
            menu,
            agents,
            presets,
            effort: None,
        }
    }

    #[test]
    fn arrows_adjust_effort_and_enter_picks() {
        let m = menu();
        let mut p = Picker::new(&m, Some("low")).unwrap();
        // Starts at the live effort ("low" = slot 2 of auto/off/low/high).
        p.handle_key(KeyEvent::from(KeyCode::Right));
        let PickResult::Picked { option, effort } = p.handle_key(KeyEvent::from(KeyCode::Enter))
        else {
            panic!("expected pick");
        };
        assert_eq!(option, Some(0));
        assert_eq!(effort.as_deref(), Some("high"));
    }

    #[test]
    fn auto_slot_means_no_effort() {
        let m = menu();
        let mut p = Picker::new(&m, None).unwrap();
        p.handle_key(KeyEvent::from(KeyCode::Down));
        let PickResult::Picked { option, effort } = p.handle_key(KeyEvent::from(KeyCode::Enter))
        else {
            panic!("expected pick");
        };
        assert_eq!(option, Some(1));
        assert_eq!(effort, None);
    }

    #[test]
    fn hover_lifts_a_model_row_without_moving_keyboard_selection() {
        let m = menu();
        let mut p = Picker::new(&m, None).unwrap();
        p.set_hovered_row(Some(2));

        let lines = p.render(&m);
        assert_eq!(p.selected, 0);
        assert_eq!(lines[2].style.fg, Some(theme::hover_color(theme::DIM)));
    }

    #[test]
    fn mouse_row_applies_the_visible_model() {
        let m = menu();
        let mut p = Picker::new(&m, None).unwrap();
        let PickResult::Picked { option, effort } = p.handle_mouse_row(2) else {
            panic!("expected a model pick");
        };
        assert_eq!(option, Some(1));
        assert_eq!(effort, None);
    }

    /// The hub is the whole line-up on one screen, so every part of it must be
    /// reachable without leaving: main model, presets, and each role.
    #[test]
    fn the_hub_lists_the_main_model_the_presets_and_every_role() {
        let m = menu();
        let a = agents(vec![AgentModelChoice::Inherit, AgentModelChoice::Inherit]);
        let p = presets(&["deepseek", "gpt"], Some(0));
        let hub = Hub::new(&a, &p, false);

        let text = hub
            .render(&ctx(&m, &a, &p))
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("main    deepseek · deepseek-v4-flash[1m]"));
        assert!(text.contains("deepseek ✓"), "the preset in force is marked");
        assert!(text.contains("gpt"));
        assert!(text.contains("save this line-up as a preset"));
        assert!(text.contains("explore") && text.contains("general"));
    }

    /// Enter opens the focused role's picker, Down moves off `inherit` onto
    /// the first model, Enter applies it.
    fn drill_into_focused_role(
        hub: &mut Hub,
        m: &ModelMenu,
        a: &AgentMenu,
        p: &PresetMenu,
    ) -> (String, AgentModelChoice) {
        let ctx = ctx(m, a, p);
        hub.handle_key(KeyEvent::from(KeyCode::Enter), &ctx);
        hub.handle_key(KeyEvent::from(KeyCode::Down), &ctx);
        match hub.handle_key(KeyEvent::from(KeyCode::Enter), &ctx) {
            HubPick::Agent { kind, choice } => (kind, choice),
            _ => panic!("expected an agent pick"),
        }
    }

    /// `/agents` is the same hub, only opened further down it.
    #[test]
    fn agents_opens_the_hub_on_the_first_sub_agent() {
        let m = menu();
        let a = agents(vec![AgentModelChoice::Inherit, AgentModelChoice::Inherit]);
        let p = presets(&["gpt"], None);
        let mut hub = Hub::new(&a, &p, true);

        let (kind, _) = drill_into_focused_role(&mut hub, &m, &a, &p);
        assert_eq!(kind, "explore");
    }

    #[test]
    fn a_role_row_drills_into_the_shared_model_picker() {
        let m = menu();
        let a = agents(vec![AgentModelChoice::Inherit, AgentModelChoice::Inherit]);
        let p = presets(&[], None);
        let mut hub = Hub::new(&a, &p, true);

        let (kind, choice) = drill_into_focused_role(&mut hub, &m, &a, &p);
        assert_eq!(kind, "explore");
        assert_eq!(
            choice,
            AgentModelChoice::Model {
                option: 0,
                effort: None,
            }
        );
    }

    /// Esc in the drill-down is one step back, not a dismissal: a wrong turn
    /// must not cost the whole visit.
    #[test]
    fn esc_backs_out_of_a_drill_down_before_it_closes_the_hub() {
        let m = menu();
        let a = agents(vec![AgentModelChoice::Inherit, AgentModelChoice::Inherit]);
        let p = presets(&[], None);
        let mut hub = Hub::new(&a, &p, true);
        let c = ctx(&m, &a, &p);

        hub.handle_key(KeyEvent::from(KeyCode::Enter), &c);
        assert!(matches!(
            hub.handle_key(KeyEvent::from(KeyCode::Esc), &c),
            HubPick::Pending
        ));
        assert!(matches!(
            hub.handle_key(KeyEvent::from(KeyCode::Esc), &c),
            HubPick::Cancelled
        ));
    }

    /// The arrows must never land on a section header — it selects nothing,
    /// and Enter on it would be a dead key.
    #[test]
    fn arrows_step_over_section_headers() {
        let m = menu();
        let a = agents(vec![AgentModelChoice::Inherit, AgentModelChoice::Inherit]);
        let p = presets(&["gpt"], None);
        let mut hub = Hub::new(&a, &p, false);
        let c = ctx(&m, &a, &p);

        let rows = entries(&a, &p);
        for _ in 0..rows.len() + 2 {
            assert!(rows[hub.selected].selectable());
            hub.handle_key(KeyEvent::from(KeyCode::Down), &c);
        }
        assert!(rows[hub.selected].selectable());
    }

    #[test]
    fn choosing_a_preset_row_reports_it_by_name() {
        let m = menu();
        let a = agents(vec![AgentModelChoice::Inherit, AgentModelChoice::Inherit]);
        let p = presets(&["deepseek", "gpt"], Some(0));
        let mut hub = Hub::new(&a, &p, false);
        let c = ctx(&m, &a, &p);

        // main → (presets header) → deepseek → gpt
        hub.handle_key(KeyEvent::from(KeyCode::Down), &c);
        hub.handle_key(KeyEvent::from(KeyCode::Down), &c);
        assert!(matches!(
            hub.handle_key(KeyEvent::from(KeyCode::Enter), &c),
            HubPick::Preset(name) if name == "gpt"
        ));
    }

    /// Naming a preset is the one text field in the dialog; while it is open
    /// the keys that navigate the hub must all reach the field instead.
    #[test]
    fn saving_a_preset_takes_a_typed_name() {
        let m = menu();
        let a = agents(vec![AgentModelChoice::Inherit, AgentModelChoice::Inherit]);
        let p = presets(&[], None);
        let mut hub = Hub::new(&a, &p, false);
        let c = ctx(&m, &a, &p);

        // main → (presets header) → save row
        hub.handle_key(KeyEvent::from(KeyCode::Down), &c);
        hub.handle_key(KeyEvent::from(KeyCode::Enter), &c);
        for ch in "gptx".chars() {
            hub.handle_key(KeyEvent::from(KeyCode::Char(ch)), &c);
        }
        hub.handle_key(KeyEvent::from(KeyCode::Backspace), &c);
        assert!(hub
            .render(&c)
            .iter()
            .any(|line| line.to_string().contains("name it: gpt")));
        assert!(matches!(
            hub.handle_key(KeyEvent::from(KeyCode::Enter), &c),
            HubPick::SavePreset(name) if name == "gpt"
        ));
    }

    #[test]
    fn hub_hover_lifts_a_row_without_moving_keyboard_selection() {
        let m = menu();
        let a = agents(vec![AgentModelChoice::Inherit, AgentModelChoice::Inherit]);
        let p = presets(&["gpt"], None);
        let mut hub = Hub::new(&a, &p, false);
        hub.set_hovered_row(Some(3), &ctx(&m, &a, &p));

        let lines = hub.render(&ctx(&m, &a, &p));
        assert_eq!(hub.selected, 0);
        assert_eq!(lines[3].style.fg, Some(theme::hover_color(theme::DIM)));
    }

    #[test]
    fn web_fetch_role_off_and_inherit_are_distinct() {
        let m = menu();
        let role = AgentRole {
            key: "fetch".into(),
            label: "web-fetch".into(),
            allows_off: true,
            section: RoleSection::Helper,
        };
        let mut picker = Picker::for_agent(&m, &role, &AgentModelChoice::Off);
        assert!(picker.render(&m)[1].to_string().contains("off"));
        assert_eq!(picker.agent_choice(), AgentModelChoice::Off);
        picker.handle_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(picker.agent_choice(), AgentModelChoice::Inherit);
    }

    #[test]
    fn role_rows_show_what_each_agent_runs_on() {
        let m = menu();
        let a = agents(vec![
            AgentModelChoice::Model {
                option: 0,
                effort: Some("high".into()),
            },
            AgentModelChoice::Inherit,
        ]);
        assert_eq!(a.describe(0, &m), "deepseek-v4-flash[1m] (high)");
        assert_eq!(a.describe(1, &m), "inherit (main model)");
    }
}
