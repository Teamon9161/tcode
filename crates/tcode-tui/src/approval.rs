//! Approval dialog with Tab-annotation: any option can carry a free-text
//! note. "Yes + note" lets the model adjust without redoing the work;
//! "No + note" tells it why.

use ratatui::text::{Line, Span};
use tcode_core::{Approval, ApprovalDecision};

use crate::theme;

pub struct Dialog {
    pub summary: String,
    pub descriptor: String,
    /// Red/green change preview supplied with the approval request.
    preview: Vec<Line<'static>>,
    preview_scroll: usize,
    selected: usize,
    note: String,
    note_focused: bool,
    question_options: Option<Vec<String>>,
}

const OPTIONS: [(&str, ApprovalDecision); 3] = [
    ("Yes", ApprovalDecision::Yes),
    ("Yes, don't ask again for this", ApprovalDecision::YesAlways),
    ("No", ApprovalDecision::No),
];

/// Keep the controls in view even for a long edit. PageUp/PageDown expose
/// the remaining preview before the user gives consent.
const PREVIEW_ROWS: usize = 8;

pub enum DialogResult {
    Pending,
    Done(Approval),
}

impl Dialog {
    pub fn new(summary: String, descriptor: String, preview: Vec<Line<'static>>) -> Self {
        Self {
            summary,
            descriptor,
            preview,
            preview_scroll: 0,
            selected: 0,
            note: String::new(),
            note_focused: false,
            question_options: None,
        }
    }

    pub fn question(summary: String, options: Vec<String>) -> Self {
        Self {
            summary,
            descriptor: "ask_user".into(),
            preview: Vec::new(),
            preview_scroll: 0,
            selected: 0,
            note: String::new(),
            note_focused: false,
            question_options: Some(if options.is_empty() {
                vec!["Continue".into()]
            } else {
                options
            }),
        }
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> DialogResult {
        use crossterm::event::KeyCode as K;
        match key.code {
            K::PageUp => self.preview_scroll = self.preview_scroll.saturating_sub(PREVIEW_ROWS),
            K::PageDown => {
                self.preview_scroll = (self.preview_scroll + PREVIEW_ROWS)
                    .min(self.preview.len().saturating_sub(PREVIEW_ROWS));
            }
            K::Tab => self.note_focused = !self.note_focused,
            K::Enter => {
                if let Some(options) = &self.question_options {
                    let mut answer = options
                        .get(self.selected.min(options.len().saturating_sub(1)))
                        .cloned()
                        .unwrap_or_default();
                    if !self.note.trim().is_empty() {
                        answer.push_str(&format!(" — {}", self.note.trim()));
                    }
                    return DialogResult::Done(Approval {
                        decision: ApprovalDecision::Yes,
                        comment: Some(answer),
                    });
                }
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
                let len = self.question_options.as_ref().map_or(OPTIONS.len(), Vec::len);
                self.selected = self.selected.checked_sub(1).unwrap_or(len - 1)
            }
            K::Down if !self.note_focused => {
                let len = self.question_options.as_ref().map_or(OPTIONS.len(), Vec::len);
                self.selected = (self.selected + 1) % len;
            }
            K::Char(c) if !self.note_focused && ('1'..='3').contains(&c) => {
                let index = c as usize - '1' as usize;
                let len = self.question_options.as_ref().map_or(OPTIONS.len(), Vec::len);
                if index < len {
                    self.selected = index;
                }
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
        if !self.preview.is_empty() {
            out.push(Line::styled("  proposed change:", theme::dim()));
            let end = (self.preview_scroll + PREVIEW_ROWS).min(self.preview.len());
            out.extend(self.preview[self.preview_scroll..end].iter().cloned());
            if self.preview.len() > PREVIEW_ROWS {
                out.push(Line::styled(
                    format!(
                        "  {}–{} / {} lines · page up/down to scroll",
                        self.preview_scroll + 1,
                        end,
                        self.preview.len()
                    ),
                    theme::dim(),
                ));
            }
        }
        let choices: Vec<String> = match &self.question_options {
            Some(options) => options.clone(),
            None => OPTIONS.iter().map(|(label, _)| (*label).to_string()).collect(),
        };
        for (i, label) in choices.iter().enumerate() {
            let marker = if i == self.selected { "▸ " } else { "  " };
            let label = if self.question_options.is_none() && i == 1 {
                format!("{label} ({})", self.descriptor)
            } else {
                label.to_string()
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
            if self.question_options.is_some() {
                "  ↑↓/1-4 choose · type/tab add note · enter answer · esc = cancel"
            } else {
                "  ↑↓/1-3 choose · pgup/pgdn diff · type/tab note · enter confirm · esc = no"
            },
            theme::dim(),
        ));
        out
    }

    pub fn height(&self) -> u16 {
        let preview = if self.preview.is_empty() {
            0
        } else {
            1 + self.preview.len().min(PREVIEW_ROWS) + usize::from(self.preview.len() > PREVIEW_ROWS)
        };
        (preview + self.question_options.as_ref().map_or(OPTIONS.len(), Vec::len) + 3) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent};

    fn dialog() -> Dialog {
        Dialog::new("edit src/main.rs".into(), "edit(src/main.rs)".into(), Vec::new())
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

    #[test]
    fn preview_is_visible_and_scrollable() {
        let preview = (0..10)
            .map(|i| Line::raw(format!("change {i}")))
            .collect();
        let mut d = Dialog::new("edit f".into(), "edit(f)".into(), preview);
        let first = d
            .render()
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(first.contains("change 0"));
        assert!(first.contains("page up/down"));
        d.handle_key(key(KeyCode::PageDown));
        let second = d
            .render()
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(second.contains("change 9"));
    }
}
