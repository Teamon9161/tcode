//! First-entry local folder-trust confirmation. This is frontend state only:
//! the selected value is persisted by `App` in the selected config's
//! `[tcode_state]` table.

use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::text::{Line, Span};
use tcode_core::FolderTrust;

use crate::theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    TrustSession,
    TrustAndRemember,
    RejectSession,
    RejectAndRemember,
}

const CHOICES: [(Choice, &str, &str); 4] = [
    (
        Choice::TrustSession,
        "Yes, trust for this session",
        "ordinary local development is trusted until tcode exits",
    ),
    (
        Choice::TrustAndRemember,
        "Yes, trust and remember",
        "save this decision locally for future sessions",
    ),
    (
        Choice::RejectSession,
        "No, do not trust this session",
        "keep Auto Mode conservative until tcode exits",
    ),
    (
        Choice::RejectAndRemember,
        "No, do not trust and remember",
        "save the conservative choice locally for future sessions",
    ),
];

pub struct Picker {
    path: String,
    selected: usize,
    hovered: Option<usize>,
}

pub enum PickResult {
    Pending,
    Cancelled,
    Picked(Choice),
}

impl Picker {
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.display().to_string(),
            selected: 0,
            hovered: None,
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
                self.selected = (self.selected + 1).min(CHOICES.len() - 1);
                PickResult::Pending
            }
            KeyCode::Enter => PickResult::Picked(CHOICES[self.selected].0),
            _ => PickResult::Pending,
        }
    }

    pub fn handle_mouse_row(&mut self, row: usize) -> PickResult {
        let Some(index) = row.checked_sub(2).filter(|index| *index < CHOICES.len()) else {
            return PickResult::Pending;
        };
        self.selected = index;
        PickResult::Picked(CHOICES[index].0)
    }

    pub fn set_hovered_row(&mut self, row: Option<usize>) {
        self.hovered = row
            .and_then(|row| row.checked_sub(2))
            .filter(|&index| index < CHOICES.len());
    }

    pub fn render(&self) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::from(vec![Span::styled(
                "◈ Trust this folder?",
                theme::bold().fg(theme::ACCENT),
            )]),
            Line::styled(format!("  {}", self.path), theme::dim()),
        ];
        for (index, (_, label, description)) in CHOICES.iter().enumerate() {
            let selected = index == self.selected;
            let marker = if selected { "▸ " } else { "  " };
            let style = if selected {
                theme::bold().fg(theme::ACCENT)
            } else {
                theme::dim()
            };
            let style = if self.hovered == Some(index) {
                theme::hover_style(style)
            } else {
                style
            };
            lines.push(Line::styled(
                format!("  {marker}{label:<36} {description}"),
                style,
            ));
        }
        lines.push(Line::styled(
            "  ↑↓ choose · enter apply · click an option · esc = do not trust this session",
            theme::dim(),
        ));
        lines
    }
}

pub fn outcome(choice: Choice) -> (FolderTrust, bool) {
    match choice {
        Choice::TrustSession => (FolderTrust::Trusted, false),
        Choice::TrustAndRemember => (FolderTrust::Trusted, true),
        Choice::RejectSession => (FolderTrust::Untrusted, false),
        Choice::RejectAndRemember => (FolderTrust::Untrusted, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_rows_select_the_fourth_choice_after_the_title_and_path() {
        let mut picker = Picker::new(Path::new("C:/repo"));
        assert!(matches!(
            picker.handle_mouse_row(5),
            PickResult::Picked(Choice::RejectAndRemember)
        ));
        assert_eq!(
            outcome(Choice::RejectAndRemember),
            (FolderTrust::Untrusted, true)
        );
    }
}
