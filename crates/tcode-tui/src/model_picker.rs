//! `/model` picker: ↑↓ chooses a model across all configured profiles,
//! ←→ adjusts its reasoning effort, Enter applies (hot-swaps the shared
//! ModelCell and persists the choice), Esc cancels.

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

struct Row {
    /// Effort slots: "auto" plus the model's declared levels.
    slots: Vec<String>,
    slot: usize,
}

pub struct Picker {
    rows: Vec<Row>,
    selected: usize,
}

pub enum PickResult {
    Pending,
    Cancelled,
    Picked {
        index: usize,
        effort: Option<String>,
    },
}

const AUTO: &str = "auto";

impl Picker {
    pub fn new(menu: &ModelMenu, current_effort: Option<&str>) -> Option<Self> {
        if menu.options.is_empty() {
            return None;
        }
        let rows = menu
            .options
            .iter()
            .enumerate()
            .map(|(i, opt)| {
                let mut slots = vec![AUTO.to_string()];
                slots.extend(opt.def.efforts.iter().cloned());
                // Start at the live effort for the active model, at the
                // configured default for the others.
                let want = if i == menu.current {
                    current_effort
                } else {
                    opt.def.default_effort.as_deref()
                };
                let slot = want
                    .and_then(|w| slots.iter().position(|s| s == w))
                    .unwrap_or(0);
                Row { slots, slot }
            })
            .collect();
        Some(Self {
            rows,
            selected: menu.current,
        })
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
                let effort = (row.slot > 0).then(|| row.slots[row.slot].clone());
                return PickResult::Picked {
                    index: self.selected,
                    effort,
                };
            }
            _ => {}
        }
        PickResult::Pending
    }

    pub fn render(&self, menu: &ModelMenu) -> Vec<Line<'static>> {
        let mut out = vec![Line::from(vec![Span::styled(
            "◈ model",
            theme::bold().fg(theme::ACCENT),
        )])];
        let total = self.rows.len();
        let window = 8usize;
        let start = self
            .selected
            .saturating_sub(window / 2)
            .min(total.saturating_sub(window));
        for i in start..(start + window).min(total) {
            let opt = &menu.options[i];
            let row = &self.rows[i];
            let is_sel = i == self.selected;
            let marker = if is_sel { "▸ " } else { "  " };
            let current = if i == menu.current { " ✓" } else { "" };
            let style = if is_sel {
                theme::bold().fg(theme::ACCENT)
            } else {
                theme::dim()
            };
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

    #[test]
    fn arrows_adjust_effort_and_enter_picks() {
        let m = menu();
        let mut p = Picker::new(&m, Some("low")).unwrap();
        // Starts at the live effort ("low" = slot 2 of auto/off/low/high).
        p.handle_key(KeyEvent::from(KeyCode::Right));
        let PickResult::Picked { index, effort } = p.handle_key(KeyEvent::from(KeyCode::Enter))
        else {
            panic!("expected pick");
        };
        assert_eq!(index, 0);
        assert_eq!(effort.as_deref(), Some("high"));
    }

    #[test]
    fn auto_slot_means_no_effort() {
        let m = menu();
        let mut p = Picker::new(&m, None).unwrap();
        p.handle_key(KeyEvent::from(KeyCode::Down));
        let PickResult::Picked { index, effort } = p.handle_key(KeyEvent::from(KeyCode::Enter))
        else {
            panic!("expected pick");
        };
        assert_eq!(index, 1);
        assert_eq!(effort, None);
    }
}
