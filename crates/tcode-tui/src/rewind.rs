//! Double-Esc rewind picker: choose an earlier user input, optionally
//! restore files, and get the original text back into the editor.
//! Truncating the tail never touches the prompt prefix, so provider
//! caches still hit after a rewind.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::text::{Line, Span};

use crate::theme;

pub struct Candidate {
    /// Ledger index of the user entry (truncate target).
    pub index: usize,
    /// Full original input, prefilled into the editor after rewind.
    pub text: String,
    /// Files changed at/after this point → ask about restoring them.
    pub dirty: bool,
}

enum Stage {
    ChooseEntry,
    /// Only reached when files changed after the chosen point.
    ChooseScope,
}

pub struct Picker {
    candidates: Vec<Candidate>,
    selected: usize,
    stage: Stage,
    scope_files: bool,
}

pub enum PickResult {
    Pending,
    Cancelled,
    Rewind {
        index: usize,
        restore_files: bool,
        text: String,
    },
}

impl Picker {
    pub fn new(candidates: Vec<Candidate>) -> Option<Self> {
        if candidates.is_empty() {
            return None;
        }
        let selected = candidates.len() - 1; // most recent first
        Some(Self {
            candidates,
            selected,
            stage: Stage::ChooseEntry,
            scope_files: true,
        })
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PickResult {
        match self.stage {
            Stage::ChooseEntry => match key.code {
                KeyCode::Esc => return PickResult::Cancelled,
                KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                KeyCode::Down => self.selected = (self.selected + 1).min(self.candidates.len() - 1),
                KeyCode::Enter => {
                    if self.candidates[self.selected].dirty {
                        self.stage = Stage::ChooseScope;
                    } else {
                        return self.confirm(false);
                    }
                }
                _ => {}
            },
            Stage::ChooseScope => match key.code {
                KeyCode::Esc => self.stage = Stage::ChooseEntry,
                KeyCode::Up | KeyCode::Down => self.scope_files = !self.scope_files,
                KeyCode::Char('1') => self.scope_files = true,
                KeyCode::Char('2') => self.scope_files = false,
                KeyCode::Enter => return self.confirm(self.scope_files),
                _ => {}
            },
        }
        PickResult::Pending
    }

    fn confirm(&self, restore_files: bool) -> PickResult {
        let c = &self.candidates[self.selected];
        PickResult::Rewind {
            index: c.index,
            restore_files,
            text: c.text.clone(),
        }
    }

    pub fn render(&self) -> Vec<Line<'static>> {
        let mut out = vec![Line::from(vec![Span::styled(
            "↺ rewind to…",
            theme::bold().fg(theme::ACCENT),
        )])];
        match self.stage {
            Stage::ChooseEntry => {
                // Window of up to 6 candidates around the selection.
                let total = self.candidates.len();
                let start = self.selected.saturating_sub(3).min(total.saturating_sub(6));
                for (i, c) in self.candidates.iter().enumerate().skip(start).take(6) {
                    let marker = if i == self.selected { "▸ " } else { "  " };
                    let style = if i == self.selected {
                        theme::accent()
                    } else {
                        theme::dim()
                    };
                    let preview: String = c
                        .text
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(60)
                        .collect();
                    out.push(Line::styled(format!("  {marker}› {preview}"), style));
                }
                out.push(Line::styled(
                    "  ↑↓ choose · enter rewind · esc cancel",
                    theme::dim(),
                ));
            }
            Stage::ChooseScope => {
                out.push(Line::styled(
                    "  files were changed after this point:",
                    theme::dim(),
                ));
                for (i, (label, on)) in [
                    ("conversation + restore files", self.scope_files),
                    (
                        "conversation only (keep files as they are)",
                        !self.scope_files,
                    ),
                ]
                .iter()
                .enumerate()
                {
                    let marker = if *on { "▸ " } else { "  " };
                    let style = if *on { theme::accent() } else { theme::dim() };
                    out.push(Line::styled(format!("  {marker}{}. {label}", i + 1), style));
                }
                out.push(Line::styled(
                    "  ↑↓/1-2 choose · enter confirm · esc back",
                    theme::dim(),
                ));
            }
        }
        out
    }

    pub fn height(&self) -> u16 {
        match self.stage {
            Stage::ChooseEntry => (2 + self.candidates.len().min(6)) as u16,
            Stage::ChooseScope => 5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn picker(dirty: bool) -> Picker {
        Picker::new(vec![
            Candidate {
                index: 0,
                text: "first".into(),
                dirty,
            },
            Candidate {
                index: 4,
                text: "second".into(),
                dirty,
            },
        ])
        .unwrap()
    }

    #[test]
    fn clean_rewind_skips_scope_question() {
        let mut p = picker(false);
        // Starts at the most recent candidate.
        let PickResult::Rewind {
            index,
            restore_files,
            text,
        } = p.handle_key(key(KeyCode::Enter))
        else {
            panic!("expected rewind");
        };
        assert_eq!(index, 4);
        assert!(!restore_files);
        assert_eq!(text, "second");
    }

    #[test]
    fn dirty_rewind_asks_scope() {
        let mut p = picker(true);
        p.handle_key(key(KeyCode::Up)); // select the first entry
        assert!(matches!(
            p.handle_key(key(KeyCode::Enter)),
            PickResult::Pending
        ));
        p.handle_key(key(KeyCode::Char('2'))); // conversation only
        let PickResult::Rewind {
            index,
            restore_files,
            ..
        } = p.handle_key(key(KeyCode::Enter))
        else {
            panic!("expected rewind");
        };
        assert_eq!(index, 0);
        assert!(!restore_files);
    }

    #[test]
    fn esc_cancels() {
        let mut p = picker(false);
        assert!(matches!(
            p.handle_key(key(KeyCode::Esc)),
            PickResult::Cancelled
        ));
    }
}
