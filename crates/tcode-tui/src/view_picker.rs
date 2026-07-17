//! Picker for top-level session views. It deliberately knows no `App` state:
//! today there is one parent session, and future concurrent sessions can use
//! the same focused/windowed interaction without mixing in task traces.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::text::Line;

use crate::theme;

const VISIBLE_ROWS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewId {
    Main,
    TaskRun(String),
}

#[derive(Clone)]
pub struct ViewEntry {
    pub id: ViewId,
    pub title: String,
    pub detail: String,
    pub active: bool,
}

pub struct Picker {
    entries: Vec<ViewEntry>,
    selected: usize,
    hovered: Option<usize>,
}

pub enum PickResult {
    Pending,
    Cancelled,
    Picked(ViewId),
}

impl Picker {
    pub fn new(entries: Vec<ViewEntry>, active: &ViewId) -> Option<Self> {
        (!entries.is_empty()).then(|| Self {
            selected: entries
                .iter()
                .position(|entry| &entry.id == active)
                .unwrap_or(0),
            entries,
            hovered: None,
        })
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PickResult {
        match key.code {
            KeyCode::Esc => PickResult::Cancelled,
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                PickResult::Pending
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.entries.len() - 1);
                PickResult::Pending
            }
            KeyCode::Enter => PickResult::Picked(self.entries[self.selected].id.clone()),
            _ => PickResult::Pending,
        }
    }

    pub fn handle_mouse_row(&mut self, row: usize) -> PickResult {
        let start = self.window_start();
        let Some(index) = row
            .checked_sub(1)
            .map(|offset| start + offset)
            .filter(|index| *index < self.entries.len())
        else {
            return PickResult::Pending;
        };
        self.selected = index;
        PickResult::Picked(self.entries[index].id.clone())
    }

    pub fn set_hovered_row(&mut self, row: Option<usize>) {
        let start = self.window_start();
        self.hovered = row
            .and_then(|row| row.checked_sub(1))
            .map(|offset| start + offset)
            .filter(|&index| index < self.entries.len());
    }

    pub fn render(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::styled("◈ sessions", theme::bold().fg(theme::ACCENT))];
        let start = self.window_start();
        for (index, entry) in self
            .entries
            .iter()
            .enumerate()
            .skip(start)
            .take(VISIBLE_ROWS)
        {
            let marker = if index == self.selected { "▸ " } else { "  " };
            let current = if entry.active { " ✓" } else { "" };
            let base_style = if index == self.selected {
                theme::accent()
            } else {
                theme::dim()
            };
            let style = if self.hovered == Some(index) {
                theme::hover_style(base_style)
            } else {
                base_style
            };
            lines.push(Line::styled(
                format!("  {marker}{} · {}{current}", entry.title, entry.detail),
                style,
            ));
        }
        let position = if self.entries.len() > VISIBLE_ROWS {
            format!("{}/{} · ", self.selected + 1, self.entries.len())
        } else {
            String::new()
        };
        lines.push(Line::styled(
            format!("  {position}↑↓ choose · enter switch · esc cancel"),
            theme::dim(),
        ));
        lines
    }

    fn window_start(&self) -> usize {
        self.selected
            .saturating_sub(VISIBLE_ROWS - 1)
            .min(self.entries.len().saturating_sub(VISIBLE_ROWS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hover_lifts_a_view_row_without_moving_keyboard_selection() {
        let active = ViewId::Main;
        let mut picker = Picker::new(
            vec![
                ViewEntry {
                    id: ViewId::Main,
                    title: "Main".into(),
                    detail: "current".into(),
                    active: true,
                },
                ViewEntry {
                    id: ViewId::TaskRun("t1".into()),
                    title: "Task".into(),
                    detail: "done".into(),
                    active: false,
                },
            ],
            &active,
        )
        .unwrap();
        picker.set_hovered_row(Some(2));

        let lines = picker.render();
        assert_eq!(picker.selected, 0);
        assert_eq!(lines[2].style.fg, Some(theme::hover_color(theme::DIM)));
    }

    #[test]
    fn picker_selects_task_trace_and_windows_rows() {
        let entries = (0..10)
            .map(|i| ViewEntry {
                id: if i == 0 {
                    ViewId::Main
                } else {
                    ViewId::TaskRun(format!("t{i}"))
                },
                title: format!("task {i}"),
                detail: "done".into(),
                active: i == 0,
            })
            .collect();
        let mut picker = Picker::new(entries, &ViewId::Main).unwrap();
        for _ in 0..9 {
            picker.handle_key(KeyEvent::from(KeyCode::Down));
        }
        assert!(
            matches!(picker.handle_key(KeyEvent::from(KeyCode::Enter)), PickResult::Picked(ViewId::TaskRun(id)) if id == "t9")
        );
        assert!(picker.render().len() <= VISIBLE_ROWS + 2);
    }
}
