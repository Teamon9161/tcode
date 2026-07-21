//! `/voice model`: pick a recognition model from a list.
//!
//! The list is not written here. It comes from the sidecar (`--list-models`),
//! because that binary is the only thing that knows which presets it was built
//! with and what each one costs to download. This module renders whatever it is
//! handed and reports the name that was chosen.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::text::{Line, Span};

use crate::theme;
use crate::voice::VoiceModel;

pub struct Picker {
    models: Vec<VoiceModel>,
    selected: usize,
    /// Which row is in use now, if it is one of these. A model set in config to
    /// something this sidecar no longer offers has no row to mark.
    current: Option<usize>,
    hovered: Option<usize>,
}

pub enum PickResult {
    Pending,
    Cancelled,
    Picked(String),
}

impl Picker {
    /// `current` is the configured name; empty means "whatever the default is",
    /// which is the first row by the sidecar's own ordering.
    pub fn new(models: Vec<VoiceModel>, current: &str) -> Self {
        let current = if current.is_empty() {
            (!models.is_empty()).then_some(0)
        } else {
            models.iter().position(|model| model.name == current)
        };
        Self {
            models,
            selected: current.unwrap_or(0),
            current,
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
                self.selected = (self.selected + 1).min(self.models.len().saturating_sub(1));
                PickResult::Pending
            }
            KeyCode::Enter => self.pick(self.selected),
            _ => PickResult::Pending,
        }
    }

    /// `row` is the border-free rendered content row. Row zero is the title.
    pub fn handle_mouse_row(&mut self, row: usize) -> PickResult {
        let Some(index) = row.checked_sub(1).filter(|i| *i < self.models.len()) else {
            return PickResult::Pending;
        };
        self.selected = index;
        self.pick(index)
    }

    fn pick(&self, index: usize) -> PickResult {
        match self.models.get(index) {
            Some(model) => PickResult::Picked(model.name.clone()),
            None => PickResult::Cancelled,
        }
    }

    pub fn set_hovered_row(&mut self, row: Option<usize>) {
        self.hovered = row
            .and_then(|row| row.checked_sub(1))
            .filter(|&index| index < self.models.len());
    }

    pub fn render(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(vec![Span::styled(
            "◈ voice model",
            theme::bold().fg(theme::ACCENT),
        )])];
        // Names are padded to the longest so the notes line up; the notes are
        // where the actual decision gets made.
        let width = self
            .models
            .iter()
            .map(|model| model.name.chars().count())
            .max()
            .unwrap_or(0);
        for (index, model) in self.models.iter().enumerate() {
            let selected = index == self.selected;
            let marker = if selected { "▸ " } else { "  " };
            let current = if self.current == Some(index) {
                " ✓"
            } else {
                ""
            };
            let base = if selected {
                theme::bold().fg(theme::ACCENT)
            } else {
                theme::dim()
            };
            let style = if self.hovered == Some(index) {
                theme::hover_style(base)
            } else {
                base
            };
            lines.push(Line::styled(
                format!(
                    "  {marker}{:<width$}  {}{current}",
                    model.name,
                    model.note,
                    width = width
                ),
                style,
            ));
        }
        lines.push(Line::styled(
            "  ↑↓ choose · enter apply · esc cancel · downloads on first use",
            theme::dim(),
        ));
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models() -> Vec<VoiceModel> {
        [
            ("zh-en", "136MB"),
            ("sense-voice", "163MB"),
            ("qwen3", "879MB"),
        ]
        .into_iter()
        .map(|(name, note)| VoiceModel {
            name: name.into(),
            note: note.into(),
        })
        .collect()
    }

    #[test]
    fn the_picker_opens_on_the_model_in_use_and_marks_it() {
        let mut picker = Picker::new(models(), "sense-voice");
        assert!(picker.render()[2]
            .spans
            .iter()
            .any(|s| s.content.contains('✓')));

        picker.handle_key(KeyEvent::from(KeyCode::Down));
        assert!(matches!(
            picker.handle_key(KeyEvent::from(KeyCode::Enter)),
            PickResult::Picked(name) if name == "qwen3"
        ));
    }

    /// An empty setting means the default, which is the sidecar's first entry —
    /// this side never spells out which model that is.
    #[test]
    fn an_unset_model_opens_on_the_first_row() {
        let picker = Picker::new(models(), "");
        assert_eq!(picker.current, Some(0));
        assert!(picker.render()[1]
            .spans
            .iter()
            .any(|s| s.content.contains('✓')));
    }

    /// A name the installed sidecar no longer offers still opens a usable
    /// picker; it just has nothing to tick.
    #[test]
    fn a_model_this_sidecar_does_not_have_marks_nothing() {
        let picker = Picker::new(models(), "whisper");
        assert_eq!(picker.current, None);
        assert!(!picker
            .render()
            .iter()
            .any(|line| line.spans.iter().any(|s| s.content.contains('✓'))));
    }

    #[test]
    fn a_click_on_a_model_row_picks_it_and_the_title_row_does_not() {
        let mut picker = Picker::new(models(), "zh-en");
        assert!(matches!(picker.handle_mouse_row(0), PickResult::Pending));
        assert!(matches!(
            picker.handle_mouse_row(3),
            PickResult::Picked(name) if name == "qwen3"
        ));
    }

    #[test]
    fn escape_cancels() {
        let mut picker = Picker::new(models(), "zh-en");
        assert!(matches!(
            picker.handle_key(KeyEvent::from(KeyCode::Esc)),
            PickResult::Cancelled
        ));
    }
}
