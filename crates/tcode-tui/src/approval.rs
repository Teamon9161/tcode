//! Approval dialog with Tab-annotation: any option can carry a free-text
//! note. "Yes + note" lets the model adjust without redoing the work;
//! "No + note" tells it why. A change proposal's full diff is baked into
//! the transcript (scrollable there) while this dialog is open, so the
//! dialog itself carries only the choices; a decline retracts the diff.
//!
//! The same widget also serves `ask_user`: one or more questions rendered
//! as a paged form (←→ to switch questions, ↑↓ to choose, space to toggle
//! multi-select). All answers aggregate into a single note comment.

use ratatui::text::{Line, Span};
use serde_json::Value;
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
    /// Present iff this is an `ask_user` question form (else a consent prompt).
    questions: Option<Questions>,
}

/// A paged set of `ask_user` questions plus the currently shown page.
struct Questions {
    pages: Vec<QuestionPage>,
    page: usize,
}

/// One question: its options, selection state, and its own note editor so
/// paging back and forth preserves each answer.
struct QuestionPage {
    question: String,
    options: Vec<Choice>,
    multi: bool,
    /// Highlighted option (also the selected one for single-select).
    cursor: usize,
    /// Membership set for multi-select; ignored when `multi` is false.
    chosen: Vec<bool>,
    note: Editor,
}

/// One option. `preview` is the artifact the option would produce — a
/// mockup, a snippet, a config — shown beside the list so a choice between
/// concrete things is made by looking rather than by imagining.
struct Choice {
    label: String,
    description: String,
    preview: Option<String>,
    /// The escape hatch the harness appends to every question. The model
    /// never supplies it: a menu it wrote cannot contain the answer it
    /// failed to think of.
    other: bool,
}

impl Choice {
    /// Tolerates a bare string option: earlier sessions (and the plain
    /// approver) only ever had a label.
    fn from_value(v: &Value) -> Self {
        if let Some(label) = v.as_str() {
            return Self {
                label: label.to_string(),
                description: String::new(),
                preview: None,
                other: false,
            };
        }
        Self {
            label: v["label"].as_str().unwrap_or_default().to_string(),
            description: v["description"].as_str().unwrap_or_default().to_string(),
            preview: v["preview"]
                .as_str()
                .filter(|p| !p.trim().is_empty())
                .map(str::to_owned),
            other: false,
        }
    }

    fn other() -> Self {
        Self {
            label: OTHER_LABEL.into(),
            description: "none of these — type your own answer".into(),
            preview: None,
            other: true,
        }
    }
}

const OPTIONS: [(&str, ApprovalDecision); 3] = [
    ("Yes", ApprovalDecision::Yes),
    ("Yes, don't ask again for this", ApprovalDecision::YesAlways),
    ("No", ApprovalDecision::No),
];

/// "  note: " prefix width; continuation rows are indented to match.
const NOTE_INDENT: usize = 8;

/// The dialog grows downward at the transcript's expense, so a preview is
/// capped rather than allowed to push the conversation off screen. A longer
/// artifact is a sign the choice wants a diff or a file, not a dialog.
const MAX_PREVIEW_ROWS: usize = 14;
/// Below this the two columns stop being readable; the compact list is used.
const MIN_COLUMNS: usize = 50;
const OPTION_COLUMN_MIN: usize = 16;

const OTHER_LABEL: &str = "Other";

pub enum DialogResult {
    Pending,
    Done(Approval),
}

impl QuestionPage {
    fn from_value(v: &Value) -> Self {
        let question = v["question"].as_str().unwrap_or_default().to_string();
        let options: Vec<Choice> = v["options"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(Choice::from_value)
                    .filter(|c| !c.label.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let mut options = if options.is_empty() {
            vec![Choice::from_value(&Value::String("Continue".into()))]
        } else {
            options
        };
        options.push(Choice::other());
        let chosen = vec![false; options.len()];
        Self {
            question,
            multi: v["multiSelect"].as_bool().unwrap_or(false),
            cursor: 0,
            chosen,
            note: Editor::new(),
            options,
        }
    }

    /// A preview panel opens only when this question actually has artifacts to
    /// compare. Several selections have no single preview, so a multi-select
    /// keeps the compact list even if the model supplied previews.
    fn previewing(&self) -> bool {
        !self.multi && self.options.iter().any(|o| o.preview.is_some())
    }

    /// Options the answer is built from: the ticked ones on a multi-select
    /// (falling back to the highlighted one), else the highlighted one.
    fn picked(&self) -> Vec<usize> {
        if self.multi {
            let ticked: Vec<usize> = (0..self.options.len())
                .filter(|i| self.chosen[*i])
                .collect();
            if !ticked.is_empty() {
                return ticked;
            }
        }
        vec![self.cursor]
    }

    fn chose_other(&self) -> bool {
        self.picked().iter().any(|i| self.options[*i].other)
    }

    /// "Other" with nothing typed is not an answer. Submitting it would tell
    /// the model the user chose something they explicitly did not choose.
    fn needs_text(&self) -> bool {
        self.chose_other() && self.note.text().trim().is_empty()
    }

    /// This question's answer: the selected option(s) plus any note.
    /// "Other" is the exception — there the typed text *is* the answer, and
    /// no label may be reported alongside it, or the model would read the
    /// rejected menu item as the user's choice.
    fn answer(&self) -> String {
        let note = self.note.text().trim().to_string();
        let picked = self.picked();
        let mut picks: Vec<&str> = picked
            .iter()
            .map(|i| &self.options[*i])
            .filter(|o| !o.other)
            .map(|o| o.label.as_str())
            .collect();
        if self.chose_other() {
            picks.push(note.as_str());
            return picks.join(", ");
        }
        let mut ans = picks.join(", ");
        if !note.is_empty() {
            ans.push_str(&format!(" — {note}"));
        }
        ans
    }
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
            questions: None,
        }
    }

    /// Build the `ask_user` form from the tool input. Accepts the `questions`
    /// array; tolerates a legacy single `question` + `options` shape.
    pub fn questions(summary: String, input: &Value) -> Self {
        let raw = input["questions"].as_array().cloned().unwrap_or_else(|| {
            input
                .get("question")
                .map(|_| vec![input.clone()])
                .unwrap_or_default()
        });
        let mut pages: Vec<QuestionPage> = raw.iter().map(QuestionPage::from_value).collect();
        if pages.is_empty() {
            let mut page = QuestionPage::from_value(&Value::Null);
            page.question = summary.clone();
            pages.push(page);
        }
        Self {
            summary,
            descriptor: "ask_user".into(),
            call_summary: String::new(),
            selected: 0,
            note: Editor::new(),
            note_focused: false,
            questions: Some(Questions { pages, page: 0 }),
        }
    }

    pub fn is_question(&self) -> bool {
        self.questions.is_some()
    }

    fn note_text(&self) -> String {
        self.note.text().trim().to_string()
    }

    fn cur_page(&mut self) -> &mut QuestionPage {
        let q = self.questions.as_mut().expect("question dialog");
        &mut q.pages[q.page]
    }

    fn cur_page_multi(&self) -> bool {
        let q = self.questions.as_ref().expect("question dialog");
        q.pages[q.page].multi
    }

    pub fn paste_text(&mut self, text: String) {
        // Dialog notes are a single logical line: terminal bracketed paste can
        // contain newlines, but preserving them would make the bottom panel
        // grow without bound and obscure the transcript. Keep every word and
        // make the note editor the explicit paste target.
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            return;
        }
        self.note_focused = true;
        if self.questions.is_some() {
            self.cur_page().note.insert_str(&text);
        } else {
            self.note.insert_str(&text);
        }
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> DialogResult {
        if self.questions.is_some() {
            return self.handle_question_key(key);
        }
        use crossterm::event::KeyCode as K;
        match key.code {
            K::Tab => self.note_focused = !self.note_focused,
            K::Enter => {
                let note = self.note_text();
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
                self.selected = self.selected.checked_sub(1).unwrap_or(OPTIONS.len() - 1)
            }
            K::Down if !self.note_focused => {
                self.selected = (self.selected + 1) % OPTIONS.len();
            }
            K::Char(c) if !self.note_focused && c.is_ascii_digit() => {
                let index = (c as usize).wrapping_sub('1' as usize);
                if index < OPTIONS.len() {
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

    fn handle_question_key(&mut self, key: crossterm::event::KeyEvent) -> DialogResult {
        use crossterm::event::KeyCode as K;
        let focused = self.note_focused;
        match key.code {
            K::Tab => self.note_focused = !self.note_focused,
            K::Enter => return self.submit_or_advance(),
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
            K::Left if focused => self.cur_page().note.left(),
            K::Right if focused => self.cur_page().note.right(),
            K::Home if focused => self.cur_page().note.home(),
            K::End if focused => self.cur_page().note.end(),
            K::Delete if focused => self.cur_page().note.delete(),
            K::Backspace if focused => self.cur_page().note.backspace(),
            // Not editing a note: ←→ page between questions.
            K::Left => self.page_by(-1),
            K::Right => self.page_by(1),
            K::Up => {
                let p = self.cur_page();
                p.cursor = p.cursor.checked_sub(1).unwrap_or(p.options.len() - 1);
                self.focus_note_on_other();
            }
            K::Down => {
                let p = self.cur_page();
                p.cursor = (p.cursor + 1) % p.options.len();
                self.focus_note_on_other();
            }
            // Only a space aimed at the list toggles: while the note has focus
            // a space is a space, or a multi-select note could hold no words.
            K::Char(' ') if !focused && self.cur_page_multi() => {
                let p = self.cur_page();
                let c = p.cursor;
                p.chosen[c] = !p.chosen[c];
            }
            K::Char(c) if !focused && c.is_ascii_digit() => {
                let index = (c as usize).wrapping_sub('1' as usize);
                let p = self.cur_page();
                if index < p.options.len() {
                    p.cursor = index;
                    if p.multi {
                        p.chosen[index] = !p.chosen[index];
                    }
                    self.focus_note_on_other();
                } else {
                    self.note_focused = true;
                    self.cur_page().note.insert_char(c);
                }
            }
            K::Char(c) if focused => self.cur_page().note.insert_char(c),
            K::Char(c) => {
                self.note_focused = true;
                self.cur_page().note.insert_char(c);
            }
            _ => {}
        }
        DialogResult::Pending
    }

    /// On "Other" the note editor *is* the answer field, so landing there
    /// aims the keyboard at it. Moving off leaves the focus alone: the text
    /// stays a note on whatever the user settles on.
    fn focus_note_on_other(&mut self) {
        let p = self.cur_page();
        if p.options[p.cursor].other {
            self.note_focused = true;
        }
    }

    /// Move to an adjacent question (clamped); paging always leaves the note
    /// unfocused so ←→ resumes paging immediately.
    fn page_by(&mut self, delta: isize) {
        let q = self.questions.as_mut().expect("question dialog");
        let last = q.pages.len() as isize - 1;
        q.page = (q.page as isize + delta).clamp(0, last) as usize;
        self.note_focused = false;
    }

    /// Enter advances to the next question, or submits every answer on the
    /// last page. Answers aggregate into one comment the harness turns into
    /// a single note.
    fn submit_or_advance(&mut self) -> DialogResult {
        let q = self.questions.as_mut().expect("question dialog");
        // An empty "Other" is not an answer: hold the dialog on the offending
        // page with the cursor in the note rather than send the model a
        // choice the user did not make.
        if let Some(page) = q.pages.iter().position(QuestionPage::needs_text) {
            q.page = page;
            self.note_focused = true;
            return DialogResult::Pending;
        }
        if q.page + 1 < q.pages.len() {
            q.page += 1;
            self.note_focused = false;
            return DialogResult::Pending;
        }
        let comment = if q.pages.len() == 1 {
            q.pages[0].answer()
        } else {
            q.pages
                .iter()
                .enumerate()
                .map(|(i, p)| format!("{}. {} → {}", i + 1, p.question, p.answer()))
                .collect::<Vec<_>>()
                .join("\n")
        };
        DialogResult::Done(Approval {
            decision: ApprovalDecision::Yes,
            comment: Some(comment),
        })
    }

    /// The note as display rows: cursor bar inserted when focused, then
    /// soft-wrapped to the available width so long notes stay visible.
    fn note_rows(&self, note: &Editor, width: u16) -> Vec<String> {
        let text = note.text();
        let display = if self.note_focused {
            let (_, col) = note.cursor();
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

    fn render_note(&self, note: &Editor, width: u16, out: &mut Vec<Line<'static>>) {
        let note_style = if self.note_focused {
            theme::accent()
        } else {
            theme::dim()
        };
        for (i, row) in self.note_rows(note, width).iter().enumerate() {
            let prefix = if i == 0 { "  note: " } else { "        " };
            out.push(Line::from(vec![
                Span::styled(prefix.to_string(), note_style),
                Span::raw(row.clone()),
            ]));
        }
    }

    pub fn render(&self, width: u16) -> Vec<Line<'static>> {
        if self.questions.is_some() {
            return self.render_questions(width);
        }
        // Render the summary line-by-line and wrapped, so a long or multi-line
        // shell command shows in full instead of corrupting a single Line.
        let avail = (width as usize).saturating_sub(4).max(20);
        let summary_rows: Vec<String> = self
            .summary
            .lines()
            .flat_map(|line| wrap_cells(line, avail))
            .collect();
        let mut out: Vec<Line<'static>> = Vec::new();
        for (i, row) in summary_rows.into_iter().enumerate() {
            if i == 0 {
                out.push(Line::from(vec![
                    Span::styled("● ", theme::accent()),
                    Span::styled(row, theme::bold()),
                ]));
            } else {
                out.push(Line::styled(format!("  {row}"), theme::dim()));
            }
        }
        if out.is_empty() {
            out.push(Line::styled("● ", theme::accent()));
        }
        for (i, (label, _)) in OPTIONS.iter().enumerate() {
            let marker = if i == self.selected { "▸ " } else { "  " };
            let label = if i == 1 {
                format!("{label} ({})", self.descriptor)
            } else {
                (*label).to_string()
            };
            // Consent colours: approve is green, standing approval cyan,
            // decline red.
            let color = match i {
                0 => theme::OK,
                1 => theme::ACCENT,
                _ => theme::ERROR,
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
        self.render_note(&self.note, width, &mut out);
        out.push(Line::styled(
            format!(
                "  ↑↓/1-{} choose · type/tab note · enter confirm · esc = no",
                OPTIONS.len()
            ),
            theme::dim(),
        ));
        out
    }

    fn render_questions(&self, width: u16) -> Vec<Line<'static>> {
        let q = self.questions.as_ref().expect("question dialog");
        let page = &q.pages[q.page];
        let total = q.pages.len();
        let mut out: Vec<Line<'static>> = Vec::new();
        let avail = (width as usize).saturating_sub(4).max(20);
        let rows: Vec<String> = page
            .question
            .lines()
            .flat_map(|line| wrap_cells(line, avail))
            .collect();
        for (i, row) in rows.into_iter().enumerate() {
            if i == 0 {
                let mut spans = vec![
                    Span::styled("● ", theme::accent()),
                    Span::styled(row, theme::bold()),
                ];
                if total > 1 {
                    spans.push(Span::styled(
                        format!("  ({}/{})", q.page + 1, total),
                        theme::dim(),
                    ));
                }
                out.push(Line::from(spans));
            } else {
                out.push(Line::styled(format!("  {row}"), theme::dim()));
            }
        }
        // A narrow terminal cannot hold two readable columns; the compact list
        // is the graceful degradation, not a squeezed preview.
        if page.previewing() && (width as usize) >= MIN_COLUMNS {
            out.extend(option_columns(page, width));
        } else {
            for (i, opt) in page.options.iter().enumerate() {
                let marker = if i == page.cursor { "▸ " } else { "  " };
                let check = if page.multi {
                    if page.chosen[i] {
                        "[x] "
                    } else {
                        "[ ] "
                    }
                } else {
                    ""
                };
                out.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("{marker}{}. {check}{}", i + 1, opt.label),
                        option_style(i == page.cursor),
                    ),
                ]));
                // Only the highlighted option explains itself: showing every
                // description at once turns the dialog into a wall of text.
                if i == page.cursor {
                    for row in wrap_cells(&opt.description, avail.saturating_sub(6)) {
                        if !row.is_empty() {
                            out.push(Line::styled(format!("      {row}"), theme::dim()));
                        }
                    }
                }
            }
        }
        self.render_note(&page.note, width, &mut out);
        let enter = if q.page + 1 == total {
            "enter answer"
        } else {
            "enter next"
        };
        let nav = if total > 1 { "←→ switch · " } else { "" };
        let toggle = if page.multi { "space toggle · " } else { "" };
        out.push(Line::styled(
            format!("  ↑↓ choose · {nav}{toggle}tab note · {enter} · esc cancel"),
            theme::dim(),
        ));
        out
    }
}

/// Split into rows of at most `width` display cells (never mid-char).
fn option_style(cursor: bool) -> ratatui::style::Style {
    if cursor {
        ratatui::style::Style::default()
            .fg(theme::ACCENT)
            .add_modifier(ratatui::style::Modifier::BOLD)
    } else {
        theme::dim()
    }
}

/// Options on the left, the highlighted option's preview on the right. The
/// dialog is a flat line list inside a bordered box, so the two columns are
/// composed here rather than laid out as widgets — which also keeps the
/// preview honest about its cost: every row it adds is a row the transcript
/// above loses.
fn option_columns(page: &QuestionPage, width: u16) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthStr;

    let total = (width as usize).saturating_sub(4).max(20);
    // Wide enough for the labels, but never so wide that the artifact it
    // exists to show gets squeezed. Descriptions are prose and need room to
    // breathe, so a question that has them spends the whole left budget;
    // bare labels give their slack back to the preview.
    let budget = total * 2 / 5;
    let widest = page
        .options
        .iter()
        .map(|o| o.label.width() + 5)
        .max()
        .unwrap_or(OPTION_COLUMN_MIN);
    let wanted = if page.options.iter().any(|o| !o.description.is_empty()) {
        widest.max(budget)
    } else {
        widest
    };
    let left = wanted.clamp(OPTION_COLUMN_MIN, budget);
    let right = total.saturating_sub(left + 3).max(1);

    let mut rows: Vec<(Vec<Span<'static>>, usize)> = Vec::new();
    for (i, opt) in page.options.iter().enumerate() {
        let marker = if i == page.cursor { "▸ " } else { "  " };
        let head = format!("{marker}{}. {}", i + 1, opt.label);
        for row in wrap_cells(&head, left) {
            let used = row.width();
            rows.push((
                vec![Span::styled(row, option_style(i == page.cursor))],
                used,
            ));
        }
        if i == page.cursor {
            for row in wrap_cells(&opt.description, left.saturating_sub(4)) {
                if row.is_empty() {
                    continue;
                }
                let row = format!("    {row}");
                let used = row.width();
                rows.push((vec![Span::styled(row, theme::dim())], used));
            }
        }
    }

    // An option with no artifact to show leaves the panel blank rather than
    // filling it with prose; "Other" says what the panel is for instead.
    let current = &page.options[page.cursor];
    let (body, body_style) = match current.preview.as_deref() {
        Some(preview) => (preview, ratatui::style::Style::default()),
        None if current.other => ("type your answer in the note below", theme::dim()),
        None => ("", ratatui::style::Style::default()),
    };
    let mut preview: Vec<String> = body
        .lines()
        .flat_map(|line| wrap_cells(line, right))
        .collect();
    if preview.len() > MAX_PREVIEW_ROWS {
        preview.truncate(MAX_PREVIEW_ROWS);
        preview.push("…".into());
    }

    let mut out = Vec::new();
    for i in 0..rows.len().max(preview.len()) {
        let mut spans = vec![Span::raw("  ")];
        let used = match rows.get(i) {
            Some((option, used)) => {
                spans.extend(option.iter().cloned());
                *used
            }
            None => 0,
        };
        spans.push(Span::raw(" ".repeat(left.saturating_sub(used) + 1)));
        spans.push(Span::styled("│ ", theme::dim()));
        if let Some(row) = preview.get(i) {
            spans.push(Span::styled(row.clone(), body_style));
        }
        out.push(Line::from(spans));
    }
    out
}

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
    use serde_json::json;

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

    fn screen(d: &Dialog, width: u16) -> String {
        d.render(width)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn previewed() -> Dialog {
        Dialog::questions(
            "Which rail?".into(),
            &json!({"questions": [{
                "question": "Which rail?",
                "options": [
                    { "label": "Bar", "description": "a left rail", "preview": "BAR-PREVIEW" },
                    { "label": "Caret", "description": "a caret", "preview": "CARET-PREVIEW" }
                ]
            }]}),
        )
    }

    #[test]
    fn a_previewed_option_renders_beside_the_list_and_follows_the_cursor() {
        let mut d = previewed();
        let text = screen(&d, 80);
        assert!(text.contains('│'), "previews open a second column");
        assert!(text.contains("BAR-PREVIEW"), "the highlighted option shows");
        assert!(
            !text.contains("CARET-PREVIEW"),
            "only one preview at a time: {text}"
        );

        d.handle_key(key(KeyCode::Down));
        let text = screen(&d, 80);
        assert!(text.contains("CARET-PREVIEW"), "moving swaps the preview");
        assert!(!text.contains("BAR-PREVIEW"));
    }

    fn menu() -> Dialog {
        Dialog::questions(
            "Pick one".into(),
            &json!({"questions": [{ "question": "Pick one", "options": ["A", "B"] }]}),
        )
    }

    /// Down past the last model-supplied option lands on the appended "Other".
    fn to_other(d: &mut Dialog) {
        d.handle_key(key(KeyCode::Down));
        d.handle_key(key(KeyCode::Down));
    }

    #[test]
    fn other_reports_the_typed_text_alone_and_never_a_rejected_option() {
        let mut d = menu();
        to_other(&mut d);
        type_str(&mut d, "neither, do X");
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter should answer");
        };
        // Not "A — neither, do X": the user rejected the menu, and the model
        // must not read a menu item as their choice.
        assert_eq!(a.comment.as_deref(), Some("neither, do X"));
    }

    #[test]
    fn landing_on_other_aims_the_keyboard_at_the_note() {
        let mut d = menu();
        to_other(&mut d);
        // No Tab needed: typing goes straight into the answer field.
        type_str(&mut d, "mine");
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter should answer");
        };
        assert_eq!(a.comment.as_deref(), Some("mine"));
    }

    #[test]
    fn an_empty_other_holds_the_dialog_instead_of_answering() {
        let mut d = menu();
        to_other(&mut d);
        assert!(
            matches!(d.handle_key(key(KeyCode::Enter)), DialogResult::Pending),
            "an empty Other is not an answer"
        );
        type_str(&mut d, "now it is");
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter should answer once text exists");
        };
        assert_eq!(a.comment.as_deref(), Some("now it is"));
    }

    #[test]
    fn other_ticked_beside_a_real_option_contributes_its_text() {
        let mut d = Dialog::questions(
            "Pick many".into(),
            &json!({"questions": [
                { "question": "Pick many", "options": ["A", "B"], "multiSelect": true }
            ]}),
        );
        d.handle_key(key(KeyCode::Char('1'))); // tick A
        to_other(&mut d);
        d.handle_key(key(KeyCode::Tab)); // leave the note to tick the row
        d.handle_key(key(KeyCode::Char(' ')));
        d.handle_key(key(KeyCode::Tab));
        type_str(&mut d, "and Z");
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter should answer");
        };
        assert_eq!(a.comment.as_deref(), Some("A, and Z"));
    }

    #[test]
    fn the_answer_is_the_label_not_the_preview() {
        let mut d = previewed();
        d.handle_key(key(KeyCode::Down));
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter should answer");
        };
        assert_eq!(a.comment.as_deref(), Some("Caret"));
    }

    #[test]
    fn a_narrow_terminal_falls_back_to_the_compact_list() {
        let d = previewed();
        let text = screen(&d, 40);
        assert!(!text.contains('│'));
        assert!(!text.contains("BAR-PREVIEW"));
        assert!(text.contains("Bar"), "the options are still choosable");
    }

    #[test]
    fn a_multi_select_keeps_the_compact_list_since_previews_cannot_be_merged() {
        let d = Dialog::questions(
            "Which?".into(),
            &json!({"questions": [{
                "question": "Which?",
                "multiSelect": true,
                "options": [
                    { "label": "Bar", "preview": "BAR-PREVIEW" },
                    { "label": "Caret", "preview": "CARET-PREVIEW" }
                ]
            }]}),
        );
        let text = screen(&d, 80);
        assert!(!text.contains("BAR-PREVIEW"), "{text}");
        assert!(text.contains("[ ] Bar"));
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
    fn dialog_paste_flattens_newlines_into_its_note() {
        let mut d = dialog();
        d.paste_text("keep this\nwith the details".into());
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter should confirm");
        };
        assert_eq!(a.comment.as_deref(), Some("keep this with the details"));
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

    fn question_dialog(input: Value) -> Dialog {
        Dialog::questions("q".into(), &input)
    }

    #[test]
    fn single_question_returns_selected_option() {
        let mut d = question_dialog(json!({
            "questions": [{ "question": "Pick one", "options": ["A", "B", "C"] }]
        }));
        assert!(d.is_question());
        d.handle_key(key(KeyCode::Down)); // highlight B
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter answers on the only page");
        };
        assert_eq!(a.decision, ApprovalDecision::Yes);
        assert_eq!(a.comment.as_deref(), Some("B"));
    }

    #[test]
    fn single_question_with_note() {
        let mut d = question_dialog(json!({
            "questions": [{ "question": "Pick one", "options": ["A", "B"] }]
        }));
        d.handle_key(key(KeyCode::Tab));
        type_str(&mut d, "because reasons");
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter answers");
        };
        assert_eq!(a.comment.as_deref(), Some("A — because reasons"));
    }

    #[test]
    fn multi_select_toggles_with_space() {
        let mut d = question_dialog(json!({
            "questions": [{ "question": "Pick many", "options": ["A", "B", "C"], "multiSelect": true }]
        }));
        d.handle_key(key(KeyCode::Char(' '))); // tick A (cursor at 0)
        d.handle_key(key(KeyCode::Down));
        d.handle_key(key(KeyCode::Down));
        d.handle_key(key(KeyCode::Char(' '))); // tick C
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter answers");
        };
        assert_eq!(a.comment.as_deref(), Some("A, C"));
    }

    #[test]
    fn multiple_questions_advance_and_aggregate() {
        let mut d = question_dialog(json!({
            "questions": [
                { "question": "First?", "options": ["A", "B"] },
                { "question": "Second?", "options": ["X", "Y"] }
            ]
        }));
        // Page 1: highlight B, Enter advances (does not submit).
        d.handle_key(key(KeyCode::Down));
        assert!(matches!(
            d.handle_key(key(KeyCode::Enter)),
            DialogResult::Pending
        ));
        // Page 2: default X, Enter submits both.
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter on last page answers");
        };
        assert_eq!(a.comment.as_deref(), Some("1. First? → B\n2. Second? → X"));
    }

    #[test]
    fn arrows_switch_questions_preserving_answers() {
        let mut d = question_dialog(json!({
            "questions": [
                { "question": "First?", "options": ["A", "B"] },
                { "question": "Second?", "options": ["X", "Y"] }
            ]
        }));
        d.handle_key(key(KeyCode::Right)); // to page 2
        d.handle_key(key(KeyCode::Down)); // highlight Y
        d.handle_key(key(KeyCode::Left)); // back to page 1
        d.handle_key(key(KeyCode::Down)); // highlight B
        d.handle_key(key(KeyCode::Enter)); // advance to page 2
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter on last page answers");
        };
        assert_eq!(a.comment.as_deref(), Some("1. First? → B\n2. Second? → Y"));
    }

    #[test]
    fn question_esc_cancels() {
        let mut d = question_dialog(json!({
            "questions": [{ "question": "Pick one", "options": ["A", "B"] }]
        }));
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Esc)) else {
            panic!("esc cancels");
        };
        assert_eq!(a.decision, ApprovalDecision::No);
        assert_eq!(a.comment, None);
    }
}
