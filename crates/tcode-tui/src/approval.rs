//! Approval dialog with Tab-annotation: any option can carry a free-text
//! note. "Yes + note" lets the model adjust without redoing the work;
//! "No + note" tells it why.

use ratatui::text::{Line, Span};
use tcode_core::{Approval, ApprovalDecision};

use crate::theme;

pub struct Dialog {
    pub summary: String,
    pub descriptor: String,
    selected: usize,
    note: String,
    note_focused: bool,
}

const OPTIONS: [(&str, ApprovalDecision); 3] = [
    ("Yes", ApprovalDecision::Yes),
    ("Yes, don't ask again for this", ApprovalDecision::YesAlways),
    ("No", ApprovalDecision::No),
];

pub enum DialogResult {
    Pending,
    Done(Approval),
}

impl Dialog {
    pub fn new(summary: String, descriptor: String) -> Self {
        Self {
            summary,
            descriptor,
            selected: 0,
            note: String::new(),
            note_focused: false,
        }
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> DialogResult {
        use crossterm::event::KeyCode as K;
        match key.code {
            K::Tab => self.note_focused = !self.note_focused,
            K::Enter => {
                let decision = OPTIONS[self.selected].1;
                return DialogResult::Done(Approval {
                    decision,
                    comment: Some(self.note.trim().to_string()).filter(|s| !s.is_empty()),
                });
            }
            K::Esc => {
                if self.note_focused {
                    self.note_focused = false;
                } else {
                    return DialogResult::Done(Approval {
                        decision: ApprovalDecision::No,
                        comment: None,
                    });
                }
            }
            K::Up if !self.note_focused => {
                self.selected = self.selected.checked_sub(1).unwrap_or(OPTIONS.len() - 1)
            }
            K::Down if !self.note_focused => self.selected = (self.selected + 1) % OPTIONS.len(),
            K::Char(c) if !self.note_focused && ('1'..='3').contains(&c) => {
                self.selected = c as usize - '1' as usize;
            }
            K::Char(c) if self.note_focused => self.note.push(c),
            K::Backspace if self.note_focused => {
                self.note.pop();
            }
            K::Char(c) if !self.note_focused => {
                // Any other typing implies annotating: focus the note.
                self.note_focused = true;
                self.note.push(c);
            }
            _ => {}
        }
        DialogResult::Pending
    }

    pub fn render(&self) -> Vec<Line<'static>> {
        let mut out = vec![Line::from(vec![
            Span::styled("● ", theme::accent()),
            Span::styled(self.summary.clone(), theme::bold()),
        ])];
        for (i, (label, _)) in OPTIONS.iter().enumerate() {
            let marker = if i == self.selected { "▸ " } else { "  " };
            let label = if i == 1 {
                format!("{label} ({})", self.descriptor)
            } else {
                (*label).to_string()
            };
            let style = if i == self.selected {
                theme::accent()
            } else {
                ratatui::style::Style::default()
            };
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{marker}{}. {label}", i + 1), style),
            ]));
        }
        let note_style = if self.note_focused {
            theme::accent()
        } else {
            theme::dim()
        };
        let cursor = if self.note_focused { "▏" } else { "" };
        out.push(Line::from(vec![
            Span::styled("  note: ", note_style),
            Span::raw(self.note.clone()),
            Span::styled(cursor.to_string(), theme::accent()),
        ]));
        out.push(Line::styled(
            "  ↑↓/1-3 choose · type/tab to add a note · enter confirm · esc = no",
            theme::dim(),
        ));
        out
    }

    pub fn height(&self) -> u16 {
        (OPTIONS.len() + 3) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent};

    fn dialog() -> Dialog {
        Dialog::new("edit src/main.rs".into(), "edit(src/main.rs)".into())
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn type_str(d: &mut Dialog, s: &str) {
        for c in s.chars() {
            d.handle_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn plain_yes() {
        let mut d = dialog();
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter should confirm");
        };
        assert_eq!(a.decision, ApprovalDecision::Yes);
        assert_eq!(a.comment, None);
    }

    #[test]
    fn yes_with_tab_annotation() {
        let mut d = dialog();
        d.handle_key(key(KeyCode::Tab));
        type_str(&mut d, "use 4 spaces");
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter should confirm");
        };
        assert_eq!(a.decision, ApprovalDecision::Yes);
        assert_eq!(a.comment.as_deref(), Some("use 4 spaces"));
    }

    #[test]
    fn typing_focuses_note_and_no_keeps_reason() {
        let mut d = dialog();
        // Select "No" via digit, then typing implies annotating.
        d.handle_key(key(KeyCode::Char('3')));
        type_str(&mut d, "wrong file");
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter should confirm");
        };
        assert_eq!(a.decision, ApprovalDecision::No);
        // '3' selected the option; only the later chars are the note.
        assert_eq!(a.comment.as_deref(), Some("wrong file"));
    }

    #[test]
    fn esc_declines_but_first_unfocuses_note() {
        let mut d = dialog();
        d.handle_key(key(KeyCode::Tab));
        assert!(matches!(
            d.handle_key(key(KeyCode::Esc)),
            DialogResult::Pending
        ));
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Esc)) else {
            panic!("second esc should decline");
        };
        assert_eq!(a.decision, ApprovalDecision::No);
    }

    #[test]
    fn arrows_and_always() {
        let mut d = dialog();
        d.handle_key(key(KeyCode::Down));
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter should confirm");
        };
        assert_eq!(a.decision, ApprovalDecision::YesAlways);
    }
}
