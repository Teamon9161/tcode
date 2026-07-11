//! Approval dialog with Tab-annotation: any option can carry a free-text
//! note. "Yes + note" lets the model adjust without redoing the work;
//! "No + note" tells it why. A change proposal's full diff is baked into
//! the transcript (scrollable there) while this dialog is open, so the
//! dialog itself carries only the choices; a decline retracts the diff.

use ratatui::text::{Line, Span};
use tcode_core::{Approval, ApprovalDecision};

use crate::editor::Editor;
use crate::theme;

pub struct Dialog {
    pub summary: String,
    pub descriptor: String,
    /// ToolStart-format call summary. A declined call never emits
    /// ToolStart, so the dialog supplies the line to bake instead.
    pub call_summary: String,
    selected: usize,
    /// Single-line note editor: full cursor movement, wraps on render.
    note: Editor,
    note_focused: bool,
    question_options: Option<Vec<String>>,
}

const OPTIONS: [(&str, ApprovalDecision); 3] = [
    ("Yes", ApprovalDecision::Yes),
    ("Yes, don't ask again for this", ApprovalDecision::YesAlways),
    ("No", ApprovalDecision::No),
];

/// "  note: " prefix width; continuation rows are indented to match.
const NOTE_INDENT: usize = 8;

pub enum DialogResult {
    Pending,
    Done(Approval),
}

impl Dialog {
    pub fn new(summary: String, descriptor: String, call_summary: String) -> Self {
        Self {
            summary,
            descriptor,
            call_summary,
            selected: 0,
            note: Editor::new(),
            note_focused: false,
            question_options: None,
        }
    }

    pub fn question(summary: String, options: Vec<String>) -> Self {
        Self {
            summary,
            descriptor: "ask_user".into(),
            call_summary: String::new(),
            selected: 0,
            note: Editor::new(),
            note_focused: false,
            question_options: Some(if options.is_empty() {
                vec!["Continue".into()]
            } else {
                options
            }),
        }
    }

    pub fn is_question(&self) -> bool {
        self.question_options.is_some()
    }

    fn note_text(&self) -> String {
        self.note.text().trim().to_string()
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> DialogResult {
        use crossterm::event::KeyCode as K;
        match key.code {
            K::Tab => self.note_focused = !self.note_focused,
            K::Enter => {
                let note = self.note_text();
                if let Some(options) = &self.question_options {
                    let mut answer = options
                        .get(self.selected.min(options.len().saturating_sub(1)))
                        .cloned()
                        .unwrap_or_default();
                    if !note.is_empty() {
                        answer.push_str(&format!(" — {note}"));
                    }
                    return DialogResult::Done(Approval {
                        decision: ApprovalDecision::Yes,
                        comment: Some(answer),
                    });
                }
                let decision = OPTIONS[self.selected].1;
                return DialogResult::Done(Approval {
                    decision,
                    comment: Some(note).filter(|s| !s.is_empty()),
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
            K::Left if self.note_focused => self.note.left(),
            K::Right if self.note_focused => self.note.right(),
            K::Home if self.note_focused => self.note.home(),
            K::End if self.note_focused => self.note.end(),
            K::Delete if self.note_focused => self.note.delete(),
            K::Backspace if self.note_focused => self.note.backspace(),
            K::Up if !self.note_focused => {
                let len = self
                    .question_options
                    .as_ref()
                    .map_or(OPTIONS.len(), Vec::len);
                self.selected = self.selected.checked_sub(1).unwrap_or(len - 1)
            }
            K::Down if !self.note_focused => {
                let len = self
                    .question_options
                    .as_ref()
                    .map_or(OPTIONS.len(), Vec::len);
                self.selected = (self.selected + 1) % len;
            }
            K::Char(c) if !self.note_focused && c.is_ascii_digit() => {
                let index = (c as usize).wrapping_sub('1' as usize);
                let len = self
                    .question_options
                    .as_ref()
                    .map_or(OPTIONS.len(), Vec::len);
                if index < len {
                    self.selected = index;
                } else {
                    // A digit with no matching option is note text, not a hotkey.
                    self.note_focused = true;
                    self.note.insert_char(c);
                }
            }
            K::Char(c) if self.note_focused => self.note.insert_char(c),
            K::Char(c) if !self.note_focused => {
                // Any other typing implies annotating: focus the note.
                self.note_focused = true;
                self.note.insert_char(c);
            }
            _ => {}
        }
        DialogResult::Pending
    }

    /// The note as display rows: cursor bar inserted when focused, then
    /// soft-wrapped to the available width so long notes stay visible.
    fn note_rows(&self, width: u16) -> Vec<String> {
        let text = self.note.text();
        let display = if self.note_focused {
            let (_, col) = self.note.cursor();
            let byte = text
                .char_indices()
                .nth(col)
                .map(|(b, _)| b)
                .unwrap_or(text.len());
            format!("{}▏{}", &text[..byte], &text[byte..])
        } else {
            text
        };
        let avail = (width as usize).saturating_sub(NOTE_INDENT + 2).max(10);
        wrap_cells(&display, avail)
    }

    pub fn render(&self, width: u16) -> Vec<Line<'static>> {
        let mut out = vec![Line::from(vec![
            Span::styled("● ", theme::accent()),
            Span::styled(self.summary.clone(), theme::bold()),
        ])];
        let choices: Vec<String> = match &self.question_options {
            Some(options) => options.clone(),
            None => OPTIONS
                .iter()
                .map(|(label, _)| (*label).to_string())
                .collect(),
        };
        for (i, label) in choices.iter().enumerate() {
            let marker = if i == self.selected { "▸ " } else { "  " };
            let label = if self.question_options.is_none() && i == 1 {
                format!("{label} ({})", self.descriptor)
            } else {
                label.to_string()
            };
            // Consent colours: approve is green, standing approval cyan,
            // decline red. Model questions carry no such semantics.
            let color = match (&self.question_options, i) {
                (Some(_), _) => theme::ACCENT,
                (None, 0) => theme::OK,
                (None, 1) => theme::ACCENT,
                (None, _) => theme::ERROR,
            };
            let style = if i == self.selected {
                ratatui::style::Style::default()
                    .fg(color)
                    .add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                theme::dim()
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
        for (i, row) in self.note_rows(width).iter().enumerate() {
            let prefix = if i == 0 { "  note: " } else { "        " };
            out.push(Line::from(vec![
                Span::styled(prefix.to_string(), note_style),
                Span::raw(row.clone()),
            ]));
        }
        let option_count = self
            .question_options
            .as_ref()
            .map_or(OPTIONS.len(), Vec::len);
        out.push(Line::styled(
            if self.question_options.is_some() {
                format!("  ↑↓/1-{option_count} choose · type/tab add note · enter answer · esc = cancel")
            } else {
                format!("  ↑↓/1-{option_count} choose · type/tab note · enter confirm · esc = no")
            },
            theme::dim(),
        ));
        out
    }
}

/// Split into rows of at most `width` display cells (never mid-char).
fn wrap_cells(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let mut rows = vec![String::new()];
    let mut used = 0usize;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width && rows.last().is_some_and(|row| !row.is_empty()) {
            rows.push(String::new());
            used = 0;
        }
        rows.last_mut().expect("rows never empty").push(c);
        used += w;
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent};

    fn dialog() -> Dialog {
        Dialog::new(
            "edit src/main.rs".into(),
            "edit(src/main.rs)".into(),
            "edit(src/main.rs)".into(),
        )
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
    fn note_cursor_moves_and_edits_mid_string() {
        let mut d = dialog();
        d.handle_key(key(KeyCode::Tab));
        type_str(&mut d, "abc");
        d.handle_key(key(KeyCode::Left));
        type_str(&mut d, "X");
        d.handle_key(key(KeyCode::Home));
        d.handle_key(key(KeyCode::Delete));
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter should confirm");
        };
        assert_eq!(a.comment.as_deref(), Some("bXc"));
    }

    #[test]
    fn long_note_wraps_and_grows_height() {
        let mut d = dialog();
        d.handle_key(key(KeyCode::Tab));
        type_str(&mut d, &"x".repeat(60));
        // 40 cells wide leaves ~30 for the note: expect several rows.
        let rows = d
            .render(40)
            .iter()
            .filter(|l| {
                let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                text.contains('x')
            })
            .count();
        assert!(rows >= 2, "60-char note must wrap at width 40");
        assert!(d.render(40).len() > d.render(200).len());
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
