//! Permission-mode picker opened from the status bar. It shares the existing
//! pending-mode semantics in `App`; this module only owns modal selection and
//! rendering.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::text::{Line, Span};
use tcode_core::PermissionMode;

use crate::theme;

const MODES: [PermissionMode; 5] = [
    PermissionMode::Plan,
    PermissionMode::Default,
    PermissionMode::AcceptEdits,
    PermissionMode::Auto,
    PermissionMode::Unsafe,
];

pub struct Picker {
    selected: usize,
    current: usize,
}

pub enum PickResult {
    Pending,
    Cancelled,
    Picked(PermissionMode),
}

impl Picker {
    pub fn new(current: PermissionMode) -> Self {
        let current = MODES.iter().position(|mode| *mode == current).unwrap_or(0);
        Self {
            selected: current,
            current,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PickResult {
        match key.code {
            KeyCode::Esc => PickResult::Cancelled,
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                PickResult::Pending
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(MODES.len() - 1);
                PickResult::Pending
            }
            KeyCode::Enter => PickResult::Picked(MODES[self.selected]),
            _ => PickResult::Pending,
        }
    }

    /// `row` is the border-free rendered content row. Row zero is the title;
    /// the following five rows correspond to `MODES`.
    pub fn handle_mouse_row(&mut self, row: usize) -> PickResult {
        let Some(index) = row.checked_sub(1).filter(|index| *index < MODES.len()) else {
            return PickResult::Pending;
        };
        self.selected = index;
        PickResult::Picked(MODES[index])
    }

    pub fn render(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(vec![Span::styled(
            "◈ permission mode",
            theme::bold().fg(theme::ACCENT),
        )])];
        for (index, mode) in MODES.iter().enumerate() {
            let selected = index == self.selected;
            let current = index == self.current;
            let marker = if selected { "▸ " } else { "  " };
            let current = if current { " ✓" } else { "" };
            let description = match mode {
                PermissionMode::Plan => "read-only tools only",
                PermissionMode::Default => "ask when rules require it",
                PermissionMode::AcceptEdits => "approve file edits automatically",
                PermissionMode::Auto => "classifier reviews routine actions",
                PermissionMode::Unsafe => "run without routine prompts",
            };
            let style = if selected {
                if *mode == PermissionMode::Unsafe {
                    theme::warn()
                } else {
                    theme::bold().fg(theme::ACCENT)
                }
            } else {
                theme::dim()
            };
            lines.push(Line::styled(
                format!("  {marker}{:<14} {description}{current}", mode.label()),
                style,
            ));
        }
        lines.push(Line::styled(
            "  ↑↓ choose · enter apply · click a mode · esc cancel",
            theme::dim(),
        ));
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_starts_on_the_current_mode_and_applies_keyboard_selection() {
        let mut picker = Picker::new(PermissionMode::AcceptEdits);
        let lines = picker.render();
        assert!(lines[3].spans.iter().any(|span| span.content.contains('✓')));

        picker.handle_key(KeyEvent::from(KeyCode::Down));
        assert!(matches!(
            picker.handle_key(KeyEvent::from(KeyCode::Enter)),
            PickResult::Picked(PermissionMode::Auto)
        ));
    }

    #[test]
    fn picker_applies_a_clicked_mode_row_and_ignores_non_mode_rows() {
        let mut picker = Picker::new(PermissionMode::Default);
        assert!(matches!(picker.handle_mouse_row(0), PickResult::Pending));
        assert!(matches!(
            picker.handle_mouse_row(5),
            PickResult::Picked(PermissionMode::Unsafe)
        ));
    }

    #[test]
    fn escape_cancels() {
        let mut picker = Picker::new(PermissionMode::Default);
        assert!(matches!(
            picker.handle_key(KeyEvent::from(KeyCode::Esc)),
            PickResult::Cancelled
        ));
    }
}
