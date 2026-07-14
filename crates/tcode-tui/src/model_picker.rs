//! `/model` and `/agents` share one picker: ↑↓ chooses a model across all
//! configured profiles, ←→ adjusts its reasoning effort, Enter applies, Esc
//! cancels. `/agents` wraps it in a first step that picks *which* sub-agent
//! the model is for, and adds an "inherit" row — a sub-agent may simply follow
//! the main model instead of pinning its own.

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

/// Pin a sub-agent kind to a model (`Some`) or let it inherit (`None`),
/// applying it live and persisting it. The binary owns this for the same
/// reason it owns `SwitchFn`.
pub type PinFn =
    Box<dyn Fn(&str, Option<(&ModelOption, Option<&str>)>) -> Result<String, String> + Send + Sync>;

/// `/agents`: auxiliary model roles (sub-agents plus Auto Mode), what each
/// currently runs on, and how to change it.
pub struct AgentMenu {
    pub kinds: Vec<String>,
    /// Per kind: the menu option it is pinned to and that pin's effort.
    /// Absent = inherit (follows `/model`).
    pub pins: Vec<Option<(usize, Option<String>)>>,
    pub pin: PinFn,
}

impl AgentMenu {
    /// What a kind runs on right now, for the kind list and the status line.
    fn describe(&self, index: usize, menu: &ModelMenu) -> String {
        match &self.pins[index] {
            None => "inherit".to_string(),
            Some((option, effort)) => {
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
    /// Menu option this row selects; `None` is the synthetic "inherit" row.
    option: Option<usize>,
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
        })
    }

    /// `/agents` step 2: the same grid, with "inherit" on top, opened on
    /// whatever the kind currently runs on.
    pub fn for_agent(menu: &ModelMenu, kind: &str, pin: Option<&(usize, Option<String>)>) -> Self {
        let mut rows = vec![Row {
            option: None,
            slots: vec![String::new()],
            slot: 0,
        }];
        for (i, opt) in menu.options.iter().enumerate() {
            let want = match pin {
                Some((pinned, effort)) if *pinned == i => effort.as_deref(),
                _ => opt.def.default_effort.as_deref(),
            };
            let (slots, slot) = slots_for(&opt.def, want);
            rows.push(Row {
                option: Some(i),
                slots,
                slot,
            });
        }
        let current = pin.map_or(0, |(option, _)| option + 1);
        Self {
            title: format!("◈ agent: {kind}"),
            rows,
            selected: current,
            current,
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
            let style = if is_sel {
                theme::bold().fg(theme::ACCENT)
            } else {
                theme::dim()
            };
            let Some(option) = row.option else {
                out.push(Line::styled(
                    format!("  {marker}inherit — follow the main model{current}"),
                    style,
                ));
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
}

pub enum AgentPick {
    Pending,
    Cancelled,
    /// A kind was assigned: `option` is `None` for inherit.
    Picked {
        kind: String,
        option: Option<usize>,
        effort: Option<String>,
    },
}

impl AgentPicker {
    pub fn new(agents: &AgentMenu) -> Option<Self> {
        (!agents.kinds.is_empty()).then_some(Self {
            selected: 0,
            model: None,
        })
    }

    pub fn handle_key(&mut self, key: KeyEvent, menu: &ModelMenu, agents: &AgentMenu) -> AgentPick {
        let Some(picker) = self.model.as_mut() else {
            match key.code {
                KeyCode::Esc => return AgentPick::Cancelled,
                KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                KeyCode::Down => self.selected = (self.selected + 1).min(agents.kinds.len() - 1),
                KeyCode::Enter => {
                    let kind = &agents.kinds[self.selected];
                    self.model = Some(Picker::for_agent(
                        menu,
                        kind,
                        agents.pins[self.selected].as_ref(),
                    ));
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
            PickResult::Picked { option, effort } => AgentPick::Picked {
                kind: agents.kinds[self.selected].clone(),
                option,
                effort,
            },
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
        for (i, kind) in agents.kinds.iter().enumerate() {
            let is_sel = i == self.selected;
            let style = if is_sel {
                theme::bold().fg(theme::ACCENT)
            } else {
                theme::dim()
            };
            out.push(Line::styled(
                format!(
                    "  {}{kind}  →  {}",
                    if is_sel { "▸ " } else { "  " },
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

    fn agents(pins: Vec<Option<(usize, Option<String>)>>) -> AgentMenu {
        AgentMenu {
            kinds: vec!["explore".into(), "general".into()],
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
    fn agent_picker_walks_kind_then_model_and_pins() {
        let m = menu();
        let a = agents(vec![None, None]);
        let mut p = AgentPicker::new(&a).unwrap();

        // Step 1: "explore" is first; Enter opens the model grid.
        assert!(matches!(
            p.handle_key(KeyEvent::from(KeyCode::Enter), &m, &a),
            AgentPick::Pending
        ));
        // Step 2: row 0 is "inherit", so Down lands on the first model.
        p.handle_key(KeyEvent::from(KeyCode::Down), &m, &a);
        p.handle_key(KeyEvent::from(KeyCode::Right), &m, &a); // auto → off
        let AgentPick::Picked {
            kind,
            option,
            effort,
        } = p.handle_key(KeyEvent::from(KeyCode::Enter), &m, &a)
        else {
            panic!("expected a pick");
        };
        assert_eq!(kind, "explore");
        assert_eq!(option, Some(0));
        assert_eq!(effort.as_deref(), Some("off"));
    }

    #[test]
    fn agent_picker_can_un_pin_and_esc_backs_out_one_step() {
        let m = menu();
        let a = agents(vec![Some((1, None)), None]);
        let mut p = AgentPicker::new(&a).unwrap();
        p.handle_key(KeyEvent::from(KeyCode::Enter), &m, &a);

        // Opens on the current pin (option 1 = row 2), not at the top.
        // Esc returns to the kind list rather than closing the dialog...
        assert!(matches!(
            p.handle_key(KeyEvent::from(KeyCode::Esc), &m, &a),
            AgentPick::Pending
        ));
        // ...and a second Esc closes it.
        assert!(matches!(
            p.handle_key(KeyEvent::from(KeyCode::Esc), &m, &a),
            AgentPick::Cancelled
        ));

        // Re-enter and choose the "inherit" row: the pin is dropped.
        p.handle_key(KeyEvent::from(KeyCode::Enter), &m, &a);
        p.handle_key(KeyEvent::from(KeyCode::Up), &m, &a);
        p.handle_key(KeyEvent::from(KeyCode::Up), &m, &a);
        let AgentPick::Picked { kind, option, .. } =
            p.handle_key(KeyEvent::from(KeyCode::Enter), &m, &a)
        else {
            panic!("expected a pick");
        };
        assert_eq!(kind, "explore");
        assert_eq!(option, None, "inherit un-pins the kind");
    }

    #[test]
    fn kind_list_shows_what_each_agent_runs_on() {
        let m = menu();
        let a = agents(vec![Some((0, Some("high".into()))), None]);
        assert_eq!(a.describe(0, &m), "deepseek-v4-flash[1m] (high)");
        assert_eq!(a.describe(1, &m), "inherit");
    }
}
