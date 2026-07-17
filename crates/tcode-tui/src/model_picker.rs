//! `/model` and `/agents` share one picker: ↑↓ chooses a model across all
//! configured profiles, ←→ adjusts its reasoning effort, Enter applies, Esc
//! cancels. `/agents` wraps it in a first step that picks *which* sub-agent
//! the model is for, and adds explicit `inherit` (use the main model) and,
//! for opt-in capabilities such as `web-fetch`, `off` rows.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::text::{Line, Span};
use tcode_core::config::ModelDef;
use tcode_core::ActiveModel;

use crate::theme;

/// One selectable (profile, model) pair.
pub struct ModelOption {
    pub profile: String,
    pub def: ModelDef,
}

impl ModelOption {
    fn title(&self) -> String {
        format!("{} · {}", self.profile, self.def.display())
    }
}

/// Builds a provider for a picked option and persists the choice.
/// Lives in the binary crate so the TUI never depends on the concrete
/// provider implementations.
pub type SwitchFn =
    Box<dyn Fn(&ModelOption, Option<&str>) -> Result<ActiveModel, String> + Send + Sync>;

pub struct ModelMenu {
    pub options: Vec<ModelOption>,
    /// Index of the active option (for the picker's initial position).
    pub current: usize,
    pub switch: SwitchFn,
}

/// Apply a role's model mode, live and in state.toml. `Off` is available only
/// for opt-in capabilities such as the `web-fetch` summarizer.
pub type PinFn = Box<dyn Fn(&str, AgentModelChoice) -> Result<String, String> + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentModelChoice {
    Off,
    Inherit,
    Model {
        option: usize,
        effort: Option<String>,
    },
}

pub struct AgentRole {
    pub key: String,
    pub label: String,
    pub allows_off: bool,
}

/// `/agents`: auxiliary model roles (sub-agents plus Auto Mode), what each
/// currently runs on, and how to change it.
pub struct AgentMenu {
    pub roles: Vec<AgentRole>,
    pub pins: Vec<AgentModelChoice>,
    pub pin: PinFn,
}

impl AgentMenu {
    /// What a role runs on right now, for the kind list and the status line.
    pub fn describe(&self, index: usize, menu: &ModelMenu) -> String {
        match &self.pins[index] {
            AgentModelChoice::Off => "off".to_string(),
            AgentModelChoice::Inherit => "inherit (main model)".to_string(),
            AgentModelChoice::Model { option, effort } => {
                let name = menu
                    .options
                    .get(*option)
                    .map(|o| o.def.display().to_string())
                    .unwrap_or_else(|| "?".into());
                match effort {
                    Some(e) => format!("{name} ({e})"),
                    None => name,
                }
            }
        }
    }
}

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
    /// `/model`: every option, starting on the active one.
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

    /// `/agents` step 2: offers the role's non-model modes first, then the
    /// shared model grid. `inherit` always means use the live main model;
    /// opt-in roles additionally expose `off`.
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
            "  ↑↓ model · ←→ effort · enter apply · esc cancel",
            theme::dim(),
        ));
        out
    }
}

/// `/agents`: step 1 picks the sub-agent kind, step 2 is the model picker
/// above. Esc walks back out one step at a time.
pub struct AgentPicker {
    selected: usize,
    model: Option<Picker>,
    hovered: Option<usize>,
}

pub enum AgentPick {
    Pending,
    Cancelled,
    Picked {
        kind: String,
        choice: AgentModelChoice,
    },
}

impl AgentPicker {
    pub fn new(agents: &AgentMenu) -> Option<Self> {
        (!agents.roles.is_empty()).then_some(Self {
            selected: 0,
            model: None,
            hovered: None,
        })
    }

    pub fn handle_key(&mut self, key: KeyEvent, menu: &ModelMenu, agents: &AgentMenu) -> AgentPick {
        let Some(picker) = self.model.as_mut() else {
            match key.code {
                KeyCode::Esc => return AgentPick::Cancelled,
                KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                KeyCode::Down => self.selected = (self.selected + 1).min(agents.roles.len() - 1),
                KeyCode::Enter => {
                    let role = &agents.roles[self.selected];
                    self.model = Some(Picker::for_agent(menu, role, &agents.pins[self.selected]));
                }
                _ => {}
            }
            return AgentPick::Pending;
        };
        match picker.handle_key(key) {
            PickResult::Pending => AgentPick::Pending,
            // Esc in the model step backs out to the kind list, not out of
            // the picker: a wrong turn should cost one key, not the dialog.
            PickResult::Cancelled => {
                self.model = None;
                AgentPick::Pending
            }
            PickResult::Picked { .. } => AgentPick::Picked {
                kind: agents.roles[self.selected].key.clone(),
                choice: picker.agent_choice(),
            },
        }
    }

    pub fn handle_mouse_row(
        &mut self,
        row: usize,
        menu: &ModelMenu,
        agents: &AgentMenu,
    ) -> AgentPick {
        let Some(picker) = self.model.as_mut() else {
            let Some(index) = row
                .checked_sub(1)
                .filter(|index| *index < agents.roles.len())
            else {
                return AgentPick::Pending;
            };
            self.selected = index;
            let role = &agents.roles[index];
            self.model = Some(Picker::for_agent(menu, role, &agents.pins[index]));
            return AgentPick::Pending;
        };
        match picker.handle_mouse_row(row) {
            PickResult::Pending | PickResult::Cancelled => AgentPick::Pending,
            PickResult::Picked { .. } => AgentPick::Picked {
                kind: agents.roles[self.selected].key.clone(),
                choice: picker.agent_choice(),
            },
        }
    }

    pub fn set_hovered_row(&mut self, row: Option<usize>, agents: &AgentMenu) {
        if let Some(picker) = self.model.as_mut() {
            picker.set_hovered_row(row);
        } else {
            self.hovered = row
                .and_then(|row| row.checked_sub(1))
                .filter(|&index| index < agents.roles.len());
        }
    }

    pub fn render(&self, menu: &ModelMenu, agents: &AgentMenu) -> Vec<Line<'static>> {
        if let Some(picker) = &self.model {
            return picker.render(menu);
        }
        let mut out = vec![Line::from(vec![Span::styled(
            "◈ agents",
            theme::bold().fg(theme::ACCENT),
        )])];
        for (i, role) in agents.roles.iter().enumerate() {
            let is_sel = i == self.selected;
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
            out.push(Line::styled(
                format!(
                    "  {}{}  →  {}",
                    if is_sel { "▸ " } else { "  " },
                    role.label,
                    agents.describe(i, menu)
                ),
                style,
            ));
        }
        out.push(Line::styled(
            "  ↑↓ agent · enter choose its model · esc cancel",
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
                },
                AgentRole {
                    key: "general".into(),
                    label: "general".into(),
                    allows_off: false,
                },
            ],
            pins,
            pin: Box::new(|_, _| Err("not applied in tests".into())),
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

    #[test]
    fn agent_kind_hover_lifts_without_changing_the_selected_kind() {
        let m = menu();
        let a = agents(vec![AgentModelChoice::Inherit, AgentModelChoice::Inherit]);
        let mut p = AgentPicker::new(&a).unwrap();
        p.set_hovered_row(Some(2), &a);

        let lines = p.render(&m, &a);
        assert_eq!(p.selected, 0);
        assert_eq!(lines[2].style.fg, Some(theme::hover_color(theme::DIM)));
    }

    #[test]
    fn agent_picker_mouse_walks_kind_then_model_and_inherit() {
        let m = menu();
        let a = agents(vec![AgentModelChoice::Inherit, AgentModelChoice::Inherit]);
        let mut p = AgentPicker::new(&a).unwrap();

        // Content row zero is the title; clicking row two selects `general`.
        assert!(matches!(p.handle_mouse_row(2, &m, &a), AgentPick::Pending));
        let AgentPick::Picked { kind, choice } = p.handle_mouse_row(1, &m, &a) else {
            panic!("expected an inherit pick");
        };
        assert_eq!(kind, "general");
        assert_eq!(choice, AgentModelChoice::Inherit);
    }

    #[test]
    fn agent_picker_walks_kind_then_model_and_pins() {
        let m = menu();
        let a = agents(vec![AgentModelChoice::Inherit, AgentModelChoice::Inherit]);
        let mut p = AgentPicker::new(&a).unwrap();

        assert!(matches!(
            p.handle_key(KeyEvent::from(KeyCode::Enter), &m, &a),
            AgentPick::Pending
        ));
        // The inherit row is first, so Down lands on the first model.
        p.handle_key(KeyEvent::from(KeyCode::Down), &m, &a);
        p.handle_key(KeyEvent::from(KeyCode::Right), &m, &a); // auto → off
        let AgentPick::Picked { kind, choice } =
            p.handle_key(KeyEvent::from(KeyCode::Enter), &m, &a)
        else {
            panic!("expected a pick");
        };
        assert_eq!(kind, "explore");
        assert_eq!(
            choice,
            AgentModelChoice::Model {
                option: 0,
                effort: Some("off".into()),
            }
        );
    }

    #[test]
    fn agent_picker_can_inherit_and_esc_backs_out_one_step() {
        let m = menu();
        let a = agents(vec![
            AgentModelChoice::Model {
                option: 1,
                effort: None,
            },
            AgentModelChoice::Inherit,
        ]);
        let mut p = AgentPicker::new(&a).unwrap();
        p.handle_key(KeyEvent::from(KeyCode::Enter), &m, &a);

        assert!(matches!(
            p.handle_key(KeyEvent::from(KeyCode::Esc), &m, &a),
            AgentPick::Pending
        ));
        assert!(matches!(
            p.handle_key(KeyEvent::from(KeyCode::Esc), &m, &a),
            AgentPick::Cancelled
        ));

        p.handle_key(KeyEvent::from(KeyCode::Enter), &m, &a);
        p.handle_key(KeyEvent::from(KeyCode::Up), &m, &a);
        p.handle_key(KeyEvent::from(KeyCode::Up), &m, &a);
        let AgentPick::Picked { kind, choice } =
            p.handle_key(KeyEvent::from(KeyCode::Enter), &m, &a)
        else {
            panic!("expected a pick");
        };
        assert_eq!(kind, "explore");
        assert_eq!(choice, AgentModelChoice::Inherit);
    }

    #[test]
    fn web_fetch_role_off_and_inherit_are_distinct() {
        let m = menu();
        let role = AgentRole {
            key: "fetch".into(),
            label: "web-fetch".into(),
            allows_off: true,
        };
        let mut picker = Picker::for_agent(&m, &role, &AgentModelChoice::Off);
        assert!(picker.render(&m)[1].to_string().contains("off"));
        assert_eq!(picker.agent_choice(), AgentModelChoice::Off);
        picker.handle_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(picker.agent_choice(), AgentModelChoice::Inherit);
    }

    #[test]
    fn kind_list_shows_what_each_agent_runs_on() {
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
